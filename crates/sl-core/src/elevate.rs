//! 提权启动。
//!
//! 用 `ShellExecuteExW(verb = "runas")` 拉起自己（或指定的 exe），等它跑完并取回退出码。
//!
//! ## 为什么不用 `runas` 之外的方案
//!
//! * 让整个 GUI 常驻管理员：不可接受（界面进程不该有特权）。
//! * 清单文件里写 `requireAdministrator`：那连不需要提权的常规操作都会弹 UAC。
//! * 常驻一个高权限服务：M4 的事，且仍要先解决"谁来安装服务"（需要一次提权）。
//!
//! 所以是"需要时才弹一次 UAC"。用户点「否」会拿到 `ERROR_CANCELLED(1223)`，
//! 我们把它翻译成明确的中文提示，而不是一个孤零零的错误码。

use std::path::Path;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};

use crate::error::{io_ctx, SlError, SlResult};
use crate::i18n::{t, tf};
use crate::util;

/// 当前进程是否已提权（查令牌而不是"试着开一下物理磁盘"）。
///
/// 直接用 kernel32 的伪句柄 `(HANDLE)-1` 表示当前进程 —— 这正是
/// `GetCurrentProcess()` 的返回值，省一次导入。
const PSEUDO_CURRENT_PROCESS: HANDLE = -1isize as HANDLE;

const TOKEN_QUERY: u32 = 0x0008;
const TOKEN_ELEVATION_CLASS: u32 = 20; // TokenElevation
const SW_SHOWNORMAL: i32 = 1;
const INFINITE: u32 = 0xFFFF_FFFF;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct TokenElevationRaw {
    token_is_elevated: u32,
}

#[link(name = "advapi32")]
extern "system" {
    fn OpenProcessToken(process_handle: HANDLE, desired_access: u32, token: *mut HANDLE) -> i32;
    fn GetTokenInformation(
        token: HANDLE,
        class: u32,
        info: *mut core::ffi::c_void,
        info_len: u32,
        returned: *mut u32,
    ) -> i32;
}

#[link(name = "kernel32")]
extern "system" {
    fn WaitForSingleObject(handle: HANDLE, milliseconds: u32) -> u32;
    fn GetExitCodeProcess(process: HANDLE, exit_code: *mut u32) -> i32;
}

/// 当前进程是否已提权。
pub fn is_elevated() -> bool {
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(PSEUDO_CURRENT_PROCESS, TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut info = TokenElevationRaw::default();
        let mut returned: u32 = 0;
        let ok = GetTokenInformation(
            token,
            TOKEN_ELEVATION_CLASS,
            &mut info as *mut _ as *mut core::ffi::c_void,
            std::mem::size_of::<TokenElevationRaw>() as u32,
            &mut returned,
        );
        let _ = CloseHandle(token);
        ok != 0 && info.token_is_elevated != 0
    }
}

/// 以管理员身份启动 `exe` 并等待它结束，返回退出码。
///
/// `args` 是一整行参数字符串（`ShellExecuteEx` 的语义就是单行字符串），
/// 所以带空格的路径必须自己加引号 —— 见 [`quote_arg`]。
pub fn run_elevated_wait(exe: &Path, args: &str) -> SlResult<u32> {
    if !exe.is_file() {
        return Err(SlError::InvalidPath(tf("elevate.exe_not_found", &[
            ("path", &exe.display().to_string()),
        ])));
    }

    let verb = util::to_wide("runas");
    let file = util::to_wide(&exe.to_string_lossy());
    let params = util::to_wide(args);

    // ShellExecuteEx 需要 COM 已初始化；失败说明当前线程本来就已经初始化过，忽略即可。
    unsafe {
        let _ = windows_sys::Win32::System::Com::CoInitializeEx(
            std::ptr::null(),
            windows_sys::Win32::System::Com::COINIT_APARTMENTTHREADED as u32,
        );
    }

    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS | 0x0000_0100; // SEE_MASK_NOASYNC
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = params.as_ptr();
    info.nShow = SW_SHOWNORMAL;

    let ok = unsafe { ShellExecuteExW(&mut info) };
    if ok == 0 {
        let e = std::io::Error::last_os_error();
        return match e.raw_os_error() {
            // 用户点了 UAC 的「否」
            Some(1223) => Err(SlError::Other(t("elevate.cancelled").into())),
            _ => Err(io_ctx(e, exe, t("io.action.elev_request"))),
        };
    }

    let process = info.hProcess;
    if process.is_null() {
        // 没有拿到进程句柄（理论上 fMask 带了 NOCLOSEPROCESS 就不会），无法等待
        return Ok(0);
    }
    unsafe {
        WaitForSingleObject(process, INFINITE);
        let mut code: u32 = 0;
        let _ = GetExitCodeProcess(process, &mut code);
        let _ = CloseHandle(process);
        Ok(code)
    }
}

/// 拼提权 worker 的命令行（`elevated <job>` + 可选的数据目录参数）。
///
/// 便携模式下必须显式带上 `--data-dir`：提权进程拿到的是新的环境块，
/// `SUBLINK_DATA_DIR` 不会传过去，worker 会去 ProgramData 找 job 并把它拒掉。
pub fn worker_args(job_path: &Path, extra_args: &str) -> String {
    let mut s = format!("elevated {}", quote_arg(&job_path.to_string_lossy()));
    if !extra_args.trim().is_empty() {
        s.push(' ');
        s.push_str(extra_args.trim());
    }
    s
}

/// 把单个参数按命令行规则加引号（路径带空格时必需）。
pub fn quote_arg(s: &str) -> String {
    if s.is_empty() {
        return "\"\"".into();
    }
    if !s.contains([' ', '\t', '"']) {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for c in s.chars() {
        match c {
            '\\' => {
                backslashes += 1;
                out.push(c);
            }
            '"' => {
                // 反斜杠数量翻倍，再转义引号
                for _ in 0..backslashes {
                    out.push('\\');
                }
                backslashes = 0;
                out.push('\\');
                out.push('"');
            }
            _ => {
                backslashes = 0;
                out.push(c);
            }
        }
    }
    for _ in 0..backslashes {
        out.push('\\');
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_arg_basics() {
        assert_eq!(quote_arg("boot-rebuild"), "boot-rebuild");
        assert_eq!(quote_arg(r"C:\a b\c.exe"), r#""C:\a b\c.exe""#);
        assert_eq!(quote_arg(""), "\"\"");
        // 结尾的反斜杠在引号内必须翻倍，否则会转义掉收尾引号
        assert_eq!(quote_arg(r"C:\dir with space\"), r#""C:\dir with space\\""#);
    }

    #[test]
    fn worker_args_carry_data_dir() {
        let job = Path::new(r"C:\ProgramData\SubLink\jobs\apply-1.job.json");
        assert_eq!(
            worker_args(job, ""),
            r#"elevated C:\ProgramData\SubLink\jobs\apply-1.job.json"#
        );
        assert_eq!(
            worker_args(job, r#"--data-dir "D:\portable""#),
            r#"elevated C:\ProgramData\SubLink\jobs\apply-1.job.json --data-dir "D:\portable""#
        );
        let spaced = Path::new(r"D:\my data\jobs\apply-1.job.json");
        assert!(worker_args(spaced, "").starts_with(r#"elevated "D:\my data\jobs\apply-1.job.json""#));
    }

    #[test]
    fn elevation_probe_returns_bool() {
        // 不做断言（取决于运行环境），只要求不 panic
        let _ = is_elevated();
    }

    #[test]
    fn elevated_launch_missing_exe_is_clear() {
        let err = run_elevated_wait(Path::new(r"Z:\definitely\missing.exe"), "").unwrap_err();
        assert!(err.to_string().contains("找不到"), "{err}");
    }
}
