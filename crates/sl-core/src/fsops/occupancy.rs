//! 占用检测。
//!
//! ## 为什么要分两层
//!
//! * **第一层：独占打开探测。** 用 `CreateFileW(..., dwShareMode = 0, ...)` 打开文件，
//!   只要有任何进程持有句柄就会拿到 `ERROR_SHARING_VIOLATION`。廉价、无数量限制，
//!   能快速扫出"哪些文件现在动不了"。
//! * **第二层：Restart Manager。** 只对第一层筛出来的那批文件调用，
//!   直接拿到**是哪个进程占着**（`chrome.exe` / 某个服务），
//!   这样提示就能写成"请先关闭 Chrome"而不是没用的"文件被占用"。
//!
//! 反过来做是行不通的：Restart Manager 要注册文件清单，十万个缓存文件注册不动。
//!
//! ## 为什么要丢到独立线程
//!
//! Restart Manager 要求调用线程处于 **MTA** 公寓。GUI 主线程很可能是 STA，
//! 所以这里固定把 RM 调用丢进一个临时线程并显式 `CoInitializeEx(MTA)`，
//! 不污染调用方的公寓状态。

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, OPEN_EXISTING,
};
use windows_sys::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

use crate::error::SlResult;
use crate::util;

const GENERIC_READ: u32 = 0x8000_0000;
const ERROR_SHARING_VIOLATION: u32 = 32;
const ERROR_ACCESS_DENIED: u32 = 5;
const ERROR_MORE_DATA: u32 = 234;
const RM_INVALID_SESSION: u32 = u32::MAX;

/// 单个文件的独占打开探测结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    /// 能独占打开：没被占用。
    Free,
    /// 被占用（共享冲突）。
    Locked,
    /// 连读都读不了：通常是权限问题，需要提权而不是关软件。
    Denied,
}

/// 以独占方式探测单个文件。
pub fn probe_exclusive(path: &Path) -> Probe {
    let wide = util::to_wide(&util::to_verbatim(path).to_string_lossy());
    unsafe {
        let handle = CreateFileW(
            wide.as_ptr(),
            GENERIC_READ,
            0, // 不允许任何共享 —— 有句柄就失败
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        );
        if handle == INVALID_HANDLE_VALUE {
            return match GetLastError() {
                ERROR_SHARING_VIOLATION => Probe::Locked,
                ERROR_ACCESS_DENIED => Probe::Denied,
                _ => Probe::Free, // 文件不存在等：不算"被占用"
            };
        }
        CloseHandle(handle);
        Probe::Free
    }
}

/// 占用它的进程 / 服务。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Blocker {
    pub app: String,
    pub pid: u32,
    pub is_service: bool,
}

impl Blocker {
    /// 给用户看的文案。
    pub fn describe(&self) -> String {
        if self.is_service {
            crate::i18n::tf("blocker.service", &[("app", &self.app), ("pid", &self.pid.to_string())])
        } else {
            crate::i18n::tf("blocker.process", &[("app", &self.app), ("pid", &self.pid.to_string())])
        }
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct LockReport {
    /// 被占用的文件（最多 `max_report` 条）。
    pub locked_files: Vec<PathBuf>,
    /// 无权限读取的文件（提示用户改用提权模式）。
    pub denied_files: Vec<PathBuf>,
    /// 去重后的占用者。
    pub blockers: Vec<Blocker>,
    pub scanned: u64,
    /// 命中数量超过上限被截断。
    pub truncated: bool,
    pub elapsed_ms: u128,
}

impl LockReport {
    pub fn is_clear(&self) -> bool {
        self.locked_files.is_empty() && self.denied_files.is_empty()
    }

    /// 给用户看的一句话结论。
    pub fn summary(&self) -> String {
        if self.is_clear() {
            return crate::i18n::tf("lock.clear", &[("n", &self.scanned.to_string())]);
        }
        let who = if self.blockers.is_empty() {
            crate::i18n::t("lock.unknown").to_string()
        } else {
            self.blockers
                .iter()
                .map(|b| b.describe())
                .collect::<Vec<_>>()
                .join("、")
        };
        let trunc = if self.truncated { crate::i18n::t("lock.truncated") } else { "" };
        let mut s = crate::i18n::tf("lock.locked", &[
            ("n", &self.locked_files.len().to_string()),
            ("trunc", trunc),
            ("who", &who),
        ]);
        if !self.denied_files.is_empty() {
            s.push_str(&crate::i18n::tf("lock.denied", &[
                ("n", &self.denied_files.len().to_string()),
            ]));
        }
        s
    }
}

#[derive(Debug, Clone)]
pub struct LockScanOptions {
    /// 最多记录多少个被占用文件。
    pub max_report: usize,
    /// 每个文件的重试次数（杀软实时扫描会造成瞬时占用）。
    pub retries: u32,
    pub retry_delay: Duration,
}

impl Default for LockScanOptions {
    fn default() -> Self {
        Self {
            max_report: 64,
            retries: 3,
            retry_delay: Duration::from_millis(120),
        }
    }
}

/// 扫描整棵目录树的占用情况。
pub fn scan_locks(root: &Path, opts: &LockScanOptions) -> SlResult<LockReport> {
    let started = std::time::Instant::now();
    let mut report = LockReport::default();

    for entry in WalkDir::new(util::to_verbatim(root))
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
    {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().is_file() {
            continue;
        }
        report.scanned += 1;

        let mut probe = Probe::Denied;
        for attempt in 0..=opts.retries {
            probe = probe_exclusive(entry.path());
            if probe == Probe::Free {
                break;
            }
            if attempt < opts.retries {
                std::thread::sleep(opts.retry_delay);
            }
        }

        match probe {
            Probe::Free => {}
            Probe::Locked => {
                report.locked_files.push(util::simplify(entry.path()));
                if report.locked_files.len() >= opts.max_report {
                    report.truncated = true;
                    break;
                }
            }
            Probe::Denied => {
                report.denied_files.push(util::simplify(entry.path()));
                if report.denied_files.len() >= opts.max_report {
                    report.truncated = true;
                    break;
                }
            }
        }
    }

    if !report.locked_files.is_empty() {
        let files: Vec<String> = report
            .locked_files
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        report.blockers = rm_blockers(files);
    }
    report.elapsed_ms = started.elapsed().as_millis();
    Ok(report)
}

// ---------------------------------------------------------------------------
// Restart Manager
// ---------------------------------------------------------------------------

const CCH_RM_SESSION_KEY: usize = 32;
const CCH_RM_MAX_APP_NAME: usize = 255;
const CCH_RM_MAX_SVC_NAME: usize = 63;

#[repr(C)]
#[derive(Clone, Copy)]
struct FileTimeRaw {
    low: u32,
    high: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RmUniqueProcess {
    process_id: u32,
    start_time: FileTimeRaw,
}

#[repr(C)]
struct RmProcessInfo {
    process: RmUniqueProcess,
    app_name: [u16; CCH_RM_MAX_APP_NAME + 1],
    service_name: [u16; CCH_RM_MAX_SVC_NAME + 1],
    application_type: u32,
    app_status: u32,
    ts_session_id: u32,
    restartable: i32,
}

#[link(name = "rstrtmgr")]
extern "system" {
    fn RmStartSession(session: *mut u32, flags: u32, key: *mut u16) -> u32;
    fn RmRegisterResources(
        session: u32,
        n_files: u32,
        files: *const *const u16,
        n_apps: u32,
        apps: *const RmUniqueProcess,
        n_services: u32,
        services: *const *const u16,
    ) -> u32;
    fn RmGetList(
        session: u32,
        needed: *mut u32,
        count: *mut u32,
        info: *mut RmProcessInfo,
        reasons: *mut u32,
    ) -> u32;
    fn RmEndSession(session: u32) -> u32;
}

/// 查出持有这批文件的进程 / 服务。
///
/// 任何一步失败都返回**空列表**而不是错误 —— 占用检测是辅助信息，
/// 不能因为它失败就阻断主流程。
pub fn rm_blockers(files: Vec<String>) -> Vec<Blocker> {
    if files.is_empty() {
        return Vec::new();
    }
    std::thread::spawn(move || rm_blockers_mta(files))
        .join()
        .unwrap_or_default()
}

fn rm_blockers_mta(files: Vec<String>) -> Vec<Blocker> {
    let mut out: Vec<Blocker> = Vec::new();
    unsafe {
        // 显式进入 MTA。已初始化过会返回 RPC_E_CHANGED_MODE，忽略即可。
        // COINIT_MULTITHREADED 在 win32 元数据里是 i32，CoInitializeEx 收 u32，故显式转换。
        let _ = CoInitializeEx(std::ptr::null(), COINIT_MULTITHREADED as u32);

        let mut session: u32 = RM_INVALID_SESSION;
        let mut key = [0u16; CCH_RM_SESSION_KEY + 1];
        let seed = format!("SubLink{:016x}", unique_seed());
        for (i, c) in seed.encode_utf16().take(CCH_RM_SESSION_KEY).enumerate() {
            key[i] = c;
        }

        if RmStartSession(&mut session, 0, key.as_mut_ptr()) == 0 {
            let owned: Vec<Vec<u16>> = files.iter().map(|f| util::to_wide(f)).collect();
            let ptrs: Vec<*const u16> = owned.iter().map(|v| v.as_ptr()).collect();

            let reg = RmRegisterResources(
                session,
                ptrs.len() as u32,
                ptrs.as_ptr(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
            );
            if reg == 0 {
                let mut needed: u32 = 0;
                let mut count: u32 = 0;
                let mut reasons: u32 = 0;
                // 先问需要多大
                let mut rc = RmGetList(
                    session,
                    &mut needed,
                    &mut count,
                    std::ptr::null_mut(),
                    &mut reasons,
                );
                if rc == ERROR_MORE_DATA || (rc == 0 && needed > 0) {
                    count = needed;
                    let mut buf: Vec<RmProcessInfo> = Vec::with_capacity(count as usize);
                    rc = RmGetList(session, &mut needed, &mut count, buf.as_mut_ptr(), &mut reasons);
                    if rc == 0 {
                        buf.set_len(count as usize);
                        for info in buf.iter() {
                            let app = util::from_wide_buf(&info.app_name);
                            let svc = util::from_wide_buf(&info.service_name);
                            let name = if svc.is_empty() {
                                app.clone()
                            } else if app.is_empty() {
                                svc.clone()
                            } else {
                                format!("{app} / {svc}")
                            };
                            if name.is_empty() {
                                continue;
                            }
                            let blocker = Blocker {
                                app: name,
                                pid: info.process.process_id,
                                is_service: !svc.is_empty(),
                            };
                            if !out.contains(&blocker) {
                                out.push(blocker);
                            }
                        }
                    }
                }
            }
            RmEndSession(session);
        }
        CoUninitialize();
    }
    out
}

/// 会话 key 必须唯一，否则 `RmStartSession` 返回冲突。
fn unique_seed() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    nanos ^ ((std::process::id() as u64) << 32)
}

/// 粗略探测一个目录本身是否"有人拿着"（例如某个进程把它设为工作目录）。
///
/// Windows 上目录被占用很难给出权威答案，这里用"能否以 DELETE 访问独占打开"做启发式判断，
/// 真正的权威判断是下面逐文件的 `probe_exclusive`。
pub fn probe_dir_handle(dir: &Path) -> Probe {
    use windows_sys::Win32::Storage::FileSystem::DELETE;
    let wide = util::to_wide(&util::to_verbatim(dir).to_string_lossy());
    unsafe {
        let handle = CreateFileW(
            wide.as_ptr(),
            DELETE,
            0,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        );
        if handle == INVALID_HANDLE_VALUE {
            return match GetLastError() {
                ERROR_SHARING_VIOLATION => Probe::Locked,
                ERROR_ACCESS_DENIED => Probe::Denied,
                _ => Probe::Free,
            };
        }
        CloseHandle(handle);
        Probe::Free
    }
}

/// 目录是否为空。
pub fn dir_is_empty(dir: &Path) -> io::Result<bool> {
    let mut rd = std::fs::read_dir(util::to_verbatim(dir))?;
    Ok(rd.next().is_none())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn unlocked_file_probes_free() {
        let base = std::env::temp_dir().join(format!("sl_lock_free_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let f = base.join("a.txt");
        fs::write(&f, b"hello").unwrap();
        assert_eq!(probe_exclusive(&f), Probe::Free);

        let rep = scan_locks(&base, &LockScanOptions::default()).unwrap();
        assert!(rep.is_clear(), "{}", rep.summary());
        assert_eq!(rep.scanned, 1);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn locked_file_is_detected() {
        use std::io::Write;
        let base = std::env::temp_dir().join(format!("sl_lock_busy_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let f = base.join("locked.txt");
        {
            let mut fh = fs::File::create(&f).unwrap();
            fh.write_all(b"busy").unwrap();
            fh.sync_all().unwrap();
            // 持有独占句柄期间探测（share_mode = 0 → 冲突）
            let mut exclusive = fs::OpenOptions::new();
            let held = exclusive.write(true).open(&f).unwrap();
            assert_eq!(probe_exclusive(&f), Probe::Locked);
            drop(held);
        }
        let rep = scan_locks(&base, &LockScanOptions::default()).unwrap();
        assert!(rep.is_clear());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn empty_dir_detected() {
        let base = std::env::temp_dir().join(format!("sl_lock_empty_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        assert!(dir_is_empty(&base).unwrap());
        fs::write(base.join("x"), b"1").unwrap();
        assert!(!dir_is_empty(&base).unwrap());
        let _ = fs::remove_dir_all(&base);
    }
}
