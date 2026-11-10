//! 目录 DACL：导出 / 回灌 / **给"对应使用者"完全控制**。
//!
//! ## 为什么需要这一层
//!
//! SubLink 会在两处产生目录：
//!
//! * **目标目录**（RAM 盘上的重定向落点）—— 开机重建时以管理员身份创建；
//! * **链接条目**（源路径上的 Junction / 目录符号链接）。
//!
//! 提权身份建出来的目录，属主是 `Administrators`、DACL 里没有当前用户，
//! 应用写缓存时会拿到 "Access Denied"，表现出来就是**软件莫名报错、缓存写不进去**。
//!
//! ## 三条手段
//!
//! 1. **以用户身份重建**（首选）—— 目录继承父目录 ACL，天然正确，无需 SDDL；
//! 2. **显式授权**（本模块的 [`grant_full_control`]）—— 不管是谁建的，
//!    给"对应使用者"补一条"完全控制"ACE，并让子对象继承；
//! 3. **SDDL 回灌**（[`export_sddl`] / [`apply_sddl`]）—— 确实需要提权时，
//!    把"首次重定向那一刻源目录的 DACL"原样贴回去。
//!
//! ## "对应使用者"是谁
//!
//! 就是**当前进程令牌里的那个用户**（[`current_user_sid`]）。
//!
//! 开机重建跑在登录任务里（`InteractiveToken`，高权限那条是同一个用户的提权令牌），
//! 提权 worker 也是同一个用户的管理员令牌 —— 所以令牌里的 SID 就是使用者本人，
//! 即使进程被提权也不会变成 SYSTEM。
//!
//! ⚠️ 只有当进程**真的以 SYSTEM 身份**跑（例如将来的后台服务）时，
//! 令牌里的 SID 才是 SYSTEM；那时必须显式传入使用者 SID
//! （[`grant_full_control_to_sid`]），否则会把权限授给 SYSTEM 而不是人。
//!
//! ## 安全边界
//!
//! 授权只加 ACE、**不改属主、不清空原有 ACE**（`SetEntriesInAclW` 是合并语义），
//! 所以不会把别人的权限抢走，也不会动 `C:\Windows` 这类受保护目录的继承链。

use std::path::Path;
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW, ConvertSidToStringSidW,
    ConvertStringSecurityDescriptorToSecurityDescriptorW, ConvertStringSidToSidW, GetNamedSecurityInfoW,
    GetSecurityInfo, SetEntriesInAclW, SetNamedSecurityInfoW, SetSecurityInfo, EXPLICIT_ACCESS_W, GRANT_ACCESS,
    NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT, TRUSTEE_IS_SID, TRUSTEE_IS_USER, TRUSTEE_W,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, LookupAccountSidW, SetFileSecurityW, SidTypeUser, TokenUser, ACL, CONTAINER_INHERIT_ACE,
    OBJECT_INHERIT_ACE, PSECURITY_DESCRIPTOR, PSID, SID_NAME_USE, TOKEN_USER,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Threading::OpenProcessToken;

use crate::error::{io_ctx, SlError, SlResult};
use crate::i18n::{t, tf};
use crate::util;

/// `DACL_SECURITY_INFORMATION`
const DACL_SECURITY_INFORMATION: u32 = 0x0000_0004;
/// `SDDL_REVISION_1`
const SDDL_REVISION_1: u32 = 1;
/// `FILE_ALL_ACCESS` —— 与 SDDL 里的 `FA` 等价。
const FILE_ALL_ACCESS: u32 = 0x001F_01FF;
/// `READ_CONTROL`
const READ_CONTROL: u32 = 0x0002_0000;
/// `WRITE_DAC`
const WRITE_DAC: u32 = 0x0004_0000;
/// `TOKEN_QUERY`
const TOKEN_QUERY_ACCESS: u32 = 0x0008;
/// 当前进程伪句柄 —— 与 `GetCurrentProcess()` 等价，省一次导入。
const PSEUDO_CURRENT_PROCESS: windows_sys::Win32::Foundation::HANDLE = -1isize as windows_sys::Win32::Foundation::HANDLE;

/// 导出一个目录的 DACL（SDDL 字符串）。
pub fn export_sddl(path: &Path) -> SlResult<String> {
    let wide = util::to_wide(&util::to_verbatim(path).to_string_lossy());
    unsafe {
        let mut sd: *mut core::ffi::c_void = std::ptr::null_mut();
        let rc = GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut sd,
        );
        if rc != 0 || sd.is_null() {
            return Err(SlError::Other(tf("acl.export_failed", &[
                ("rc", &rc.to_string()),
                ("path", &path.display().to_string()),
            ])));
        }

        let mut out: *mut u16 = std::ptr::null_mut();
        let mut len: u32 = 0;
        let ok = ConvertSecurityDescriptorToStringSecurityDescriptorW(
            sd,
            SDDL_REVISION_1,
            DACL_SECURITY_INFORMATION,
            &mut out,
            &mut len,
        );
        let result = if ok == 0 || out.is_null() {
            Err(SlError::Other(tf("acl.to_sddl_failed", &[
                ("path", &path.display().to_string()),
            ])))
        } else {
            let mut n = 0usize;
            while *out.add(n) != 0 {
                n += 1;
            }
            Ok(String::from_utf16_lossy(std::slice::from_raw_parts(out, n)))
        };

        if !out.is_null() {
            LocalFree(out as *mut core::ffi::c_void);
        }
        LocalFree(sd);
        result
    }
}

/// 把一个 SDDL 里的 DACL 贴到目录上。
pub fn apply_sddl(path: &Path, sddl: &str) -> SlResult<()> {
    if sddl.trim().is_empty() {
        return Ok(());
    }
    let wide = util::to_wide(&util::to_verbatim(path).to_string_lossy());
    let sddl_wide = util::to_wide(sddl.trim());
    unsafe {
        let mut sd: *mut core::ffi::c_void = std::ptr::null_mut();
        let ok = ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl_wide.as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            std::ptr::null_mut(),
        );
        if ok == 0 || sd.is_null() {
            return Err(SlError::Other(tf("acl.parse_failed", &[("sddl", sddl)])));
        }
        let applied = SetFileSecurityW(wide.as_ptr(), DACL_SECURITY_INFORMATION, sd);
        LocalFree(sd);
        if applied == 0 {
            let e = std::io::Error::last_os_error();
            return Err(io_ctx(e, path, t("io.action.restore_acl")));
        }
    }
    Ok(())
}

/// 当前进程令牌里的用户 SID（SDDL 形式，例如 `S-1-5-21-...-1001`）。
///
/// 提权不会改变它 —— UAC 提权后的令牌，用户 SID 与普通令牌一致。
/// 只有以 SYSTEM 身份运行时才会拿到 `S-1-5-18`。
pub fn current_user_sid() -> SlResult<String> {
    static CACHE: OnceLock<String> = OnceLock::new();
    if let Some(v) = CACHE.get() {
        return Ok(v.clone());
    }
    let sid = query_current_user_sid()?;
    let _ = CACHE.set(sid.clone());
    Ok(sid)
}

/// 真正去问系统要 SID（不走缓存）。
fn query_current_user_sid() -> SlResult<String> {
    unsafe {
        let mut token: windows_sys::Win32::Foundation::HANDLE = std::ptr::null_mut();
        if OpenProcessToken(PSEUDO_CURRENT_PROCESS, TOKEN_QUERY_ACCESS, &mut token) == 0 {
            let e = std::io::Error::last_os_error();
            return Err(SlError::Other(tf("acl.sid_failed", &[("err", &e.to_string())])));
        }

        let mut needed: u32 = 0;
        // 第一次调用只为拿长度
        GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed);
        if needed == 0 {
            let e = std::io::Error::last_os_error();
            CloseHandle(token);
            return Err(SlError::Other(tf("acl.sid_failed", &[("err", &e.to_string())])));
        }

        let mut buf = vec![0u8; needed as usize];
        let ok = GetTokenInformation(
            token,
            TokenUser,
            buf.as_mut_ptr() as *mut core::ffi::c_void,
            needed,
            &mut needed,
        );
        CloseHandle(token);
        if ok == 0 {
            let e = std::io::Error::last_os_error();
            return Err(SlError::Other(tf("acl.sid_failed", &[("err", &e.to_string())])));
        }

        let token_user = &*(buf.as_ptr() as *const TOKEN_USER);
        sid_to_string(token_user.User.Sid)
    }
}

/// SID 指针 → SDDL 字符串。
fn sid_to_string(sid: PSID) -> SlResult<String> {
    unsafe {
        let mut out: *mut u16 = std::ptr::null_mut();
        if ConvertSidToStringSidW(sid, &mut out) == 0 || out.is_null() {
            let e = std::io::Error::last_os_error();
            return Err(SlError::Other(tf("acl.sid_failed", &[("err", &e.to_string())])));
        }
        let mut n = 0usize;
        while *out.add(n) != 0 {
            n += 1;
        }
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(out, n));
        LocalFree(out as *mut core::ffi::c_void);
        Ok(s)
    }
}

/// 当前进程令牌用户的可读账号名（`域\用户`）。拿不到时返回 `None`。
///
/// 用途：让用户确认"被授权的到底是不是我" —— SID 精确但不好读，账号名一眼能认。
pub fn current_user_name() -> Option<String> {
    unsafe {
        let mut token: windows_sys::Win32::Foundation::HANDLE = std::ptr::null_mut();
        if OpenProcessToken(PSEUDO_CURRENT_PROCESS, TOKEN_QUERY_ACCESS, &mut token) == 0 {
            return None;
        }
        let mut needed: u32 = 0;
        GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed);
        if needed == 0 {
            CloseHandle(token);
            return None;
        }
        let mut buf = vec![0u8; needed as usize];
        let ok = GetTokenInformation(
            token,
            TokenUser,
            buf.as_mut_ptr() as *mut core::ffi::c_void,
            needed,
            &mut needed,
        );
        CloseHandle(token);
        if ok == 0 {
            return None;
        }
        let token_user = &*(buf.as_ptr() as *const TOKEN_USER);
        lookup_account_name(token_user.User.Sid)
    }
}

/// SID 指针 → `域\用户`（拿不到就只给用户名）。
fn lookup_account_name(sid: PSID) -> Option<String> {
    unsafe {
        let mut name: Vec<u16> = vec![0; 256];
        let mut name_len: u32 = name.len() as u32;
        let mut domain: Vec<u16> = vec![0; 256];
        let mut domain_len: u32 = domain.len() as u32;
        let mut sid_type: SID_NAME_USE = SidTypeUser;
        let ok = LookupAccountSidW(
            std::ptr::null(),
            sid,
            name.as_mut_ptr(),
            &mut name_len,
            domain.as_mut_ptr(),
            &mut domain_len,
            &mut sid_type,
        );
        if ok == 0 {
            return None;
        }
        let user = String::from_utf16_lossy(&name[..name_len as usize]);
        let domain = String::from_utf16_lossy(&domain[..domain_len as usize]);
        Some(if domain.trim().is_empty() {
            user
        } else {
            format!("{}\\{}", domain.trim(), user)
        })
    }
}

/// 路径是不是**卷根**（`R:\`、`C:\` 这种）。
///
/// 卷根的 DACL 属于**整块盘**的安全策略，不是"重定向目录"该顺手改的东西：
/// 给它授权要么失败（普通用户没权限），要么把整块盘的安全边界改了。
/// 所以凡是要授权的地方都先过这一关，卷根一律跳过。
pub fn is_volume_root(path: &Path) -> bool {
    path.parent().is_none()
}

/// 给当前用户授权：确保它在 `path` 的 DACL 里有一条**完全控制**的允许 ACE。
///
/// 语义是**合并**，不是覆盖：原有的 ACE 一条不少，只是多出一条给使用者。
/// 新 ACE 带 `OI|CI` 继承标志，所以之后建在里面的子目录 / 文件也会带上它。
///
/// 已经有一条"完全控制"给该 SID 时**直接返回**（幂等，不会越跑 ACE 越多）。
pub fn grant_full_control(path: &Path) -> SlResult<()> {
    let sid = current_user_sid()?;
    grant_full_control_to_sid(path, &sid)
}

/// 给指定 SID 授权"完全控制"。语义与 [`grant_full_control`] 相同。
pub fn grant_full_control_to_sid(path: &Path, sid: &str) -> SlResult<()> {
    if !path.exists() {
        return Err(SlError::InvalidPath(tf("acl.grant_missing", &[(
            "path",
            &path.display().to_string(),
        )])));
    }
    // 已经有就别再写一遍 DACL：既省一次内核调用，也避免 ACE 越堆越多。
    if let Ok(sddl) = export_sddl(path) {
        if sddl_grants_full_control(&sddl, sid) {
            return Ok(());
        }
    }
    grant_on_path(path, sid)
}

/// 给**链接条目自身**（不是它指向的目标）授权"完全控制"。
///
/// 关键在 `FILE_FLAG_OPEN_REPARSE_POINT`：不加这个标志，句柄打开的是链接**指向的目录**，
/// 改的就变成目标目录的权限了。链接条目自己的权限决定使用者能否穿过它。
pub fn grant_full_control_on_link(path: &Path) -> SlResult<()> {
    let sid = current_user_sid()?;
    let wide = util::to_wide(&util::to_verbatim(path).to_string_lossy());
    unsafe {
        let handle = CreateFileW(
            wide.as_ptr(),
            READ_CONTROL | WRITE_DAC,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        );
        if handle == INVALID_HANDLE_VALUE {
            let e = std::io::Error::last_os_error();
            return Err(io_ctx(e, path, t("io.action.open_link_acl")));
        }

        let result = grant_on_handle(handle, path, &sid);
        CloseHandle(handle);
        result
    }
}

/// 建目录（不存在才建）+ 确保当前用户对它有完全控制。
///
/// 返回是否真的新建了目录。
pub fn ensure_dir_full_control(path: &Path) -> SlResult<bool> {
    let created = crate::link::create::ensure_dir(path)?;
    grant_full_control(path)?;
    Ok(created)
}

/// 路径的 DACL 里是否已经有"给该 SID 的完全控制"。
pub fn has_full_control(path: &Path, sid: &str) -> SlResult<bool> {
    let sddl = export_sddl(path)?;
    Ok(sddl_grants_full_control(&sddl, sid))
}

/// 链接条目**自身**是否有"给该 SID 的完全控制"。
///
/// 与 [`has_full_control`] 的区别：后者走路径 API，会**跟随**链接读到**目标**的 DACL；
/// 这里读的是链接条目自己（`FILE_FLAG_OPEN_REPARSE_POINT`）。
pub fn has_full_control_on_link(path: &Path, sid: &str) -> SlResult<bool> {
    let sddl = link_own_sddl(path)?;
    Ok(sddl_grants_full_control(&sddl, sid))
}

/// 读链接条目自身的 DACL（SDDL）。
fn link_own_sddl(path: &Path) -> SlResult<String> {
    let wide = util::to_wide(&util::to_verbatim(path).to_string_lossy());
    unsafe {
        let handle = CreateFileW(
            wide.as_ptr(),
            READ_CONTROL,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        );
        if handle == INVALID_HANDLE_VALUE {
            let e = std::io::Error::last_os_error();
            return Err(io_ctx(e, path, t("io.action.open_link_acl")));
        }
        let result = handle_sddl(handle);
        CloseHandle(handle);
        result
    }
}

/// 读句柄上的 DACL（SDDL 字符串）。
fn handle_sddl(handle: windows_sys::Win32::Foundation::HANDLE) -> SlResult<String> {
    unsafe {
        let (old_dacl, sd) = read_handle_dacl(handle)?;
        let _ = old_dacl;

        let mut out: *mut u16 = std::ptr::null_mut();
        let mut len: u32 = 0;
        let ok = ConvertSecurityDescriptorToStringSecurityDescriptorW(
            sd,
            SDDL_REVISION_1,
            DACL_SECURITY_INFORMATION,
            &mut out,
            &mut len,
        );
        if ok == 0 || out.is_null() {
            if !sd.is_null() {
                LocalFree(sd as *mut core::ffi::c_void);
            }
            return Err(SlError::Other(t("acl.to_sddl_failed").into()));
        }
        let mut n = 0usize;
        while *out.add(n) != 0 {
            n += 1;
        }
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(out, n));
        LocalFree(out as *mut core::ffi::c_void);
        LocalFree(sd as *mut core::ffi::c_void);
        Ok(s)
    }
}

/// 从句柄读出 DACL 与安全描述符（调用方负责 `LocalFree`）。
unsafe fn read_handle_dacl(
    handle: windows_sys::Win32::Foundation::HANDLE,
) -> SlResult<(*mut ACL, PSECURITY_DESCRIPTOR)> {
    let mut old_dacl: *mut ACL = std::ptr::null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    let rc = GetSecurityInfo(
        handle,
        SE_FILE_OBJECT,
        DACL_SECURITY_INFORMATION,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        &mut old_dacl,
        std::ptr::null_mut(),
        &mut sd,
    );
    if rc != 0 {
        return Err(SlError::Other(tf("acl.grant_failed", &[
            ("sid", "-"),
            ("path", "-"),
            ("err", &rc.to_string()),
        ])));
    }
    Ok((old_dacl, sd))
}

/// 从 SDDL 文本里判断有没有"给该 SID 的完全控制"允许项。
///
/// 只做这一个判断，不做通用 SDDL 解析：ACE 形如 `(A;<标志>;<权限>;<对象类型>;<SID>)`，
/// 权限位含 `F`（FILE_ALL_ACCESS）即视为完全控制。
fn sddl_grants_full_control(sddl: &str, sid: &str) -> bool {
    for chunk in sddl.split('(').skip(1) {
        let ace = chunk.split(')').next().unwrap_or("");
        let parts: Vec<&str> = ace.split(';').collect();
        if parts.len() < 3 {
            continue;
        }
        if !parts[0].eq_ignore_ascii_case("A") {
            continue;
        }
        let rights = parts[2];
        if !rights.contains('F') {
            continue;
        }
        if parts
            .iter()
            .skip(3)
            .any(|p| !p.is_empty() && p.eq_ignore_ascii_case(sid))
        {
            return true;
        }
    }
    false
}

/// 走路径的授权实现：读旧 DACL → 合并新 ACE → 写回。
fn grant_on_path(path: &Path, sid: &str) -> SlResult<()> {
    let wide = util::to_wide(&util::to_verbatim(path).to_string_lossy());
    unsafe {
        let mut old_dacl: *mut ACL = std::ptr::null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        let rc = GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut old_dacl,
            std::ptr::null_mut(),
            &mut sd,
        );
        if rc != 0 {
            return Err(SlError::Other(tf("acl.grant_failed", &[
                ("sid", sid),
                ("path", &path.display().to_string()),
                ("err", &rc.to_string()),
            ])));
        }

        let psid = sid_to_psid(sid)?;
        let new_dacl = merge_full_control(psid, old_dacl, path, sid)?;
        LocalFree(psid as *mut core::ffi::c_void);
        if !sd.is_null() {
            LocalFree(sd as *mut core::ffi::c_void);
        }

        let rc = SetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            new_dacl,
            std::ptr::null_mut(),
        );
        LocalFree(new_dacl as *mut core::ffi::c_void);
        if rc != 0 {
            return Err(SlError::Other(tf("acl.grant_failed", &[
                ("sid", sid),
                ("path", &path.display().to_string()),
                ("err", &rc.to_string()),
            ])));
        }
    }
    Ok(())
}

/// 走句柄的授权实现（用于重解析点自身）。
fn grant_on_handle(
    handle: windows_sys::Win32::Foundation::HANDLE,
    path: &Path,
    sid: &str,
) -> SlResult<()> {
    unsafe {
        let (old_dacl, sd) = read_handle_dacl(handle).map_err(|e| {
            SlError::Other(tf("acl.grant_failed", &[
                ("sid", sid),
                ("path", &path.display().to_string()),
                ("err", &e.to_string()),
            ]))
        })?;

        let psid = sid_to_psid(sid)?;
        let new_dacl = merge_full_control(psid, old_dacl, path, sid)?;
        LocalFree(psid as *mut core::ffi::c_void);
        if !sd.is_null() {
            LocalFree(sd as *mut core::ffi::c_void);
        }

        let rc = SetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            new_dacl,
            std::ptr::null_mut(),
        );
        LocalFree(new_dacl as *mut core::ffi::c_void);
        if rc != 0 {
            return Err(SlError::Other(tf("acl.grant_failed", &[
                ("sid", sid),
                ("path", &path.display().to_string()),
                ("err", &rc.to_string()),
            ])));
        }
    }
    Ok(())
}

/// SDDL 字符串 → SID 指针。
fn sid_to_psid(sid: &str) -> SlResult<PSID> {
    let wide = util::to_wide(sid.trim());
    unsafe {
        let mut psid: PSID = std::ptr::null_mut();
        if ConvertStringSidToSidW(wide.as_ptr(), &mut psid) == 0 || psid.is_null() {
            return Err(SlError::Other(tf("acl.sid_invalid", &[("sid", sid)])));
        }
        Ok(psid)
    }
}

/// 往旧 DACL 里合并一条"完全控制"ACE，返回新 DACL（调用方负责 `LocalFree`）。
fn merge_full_control(
    psid: PSID,
    old_dacl: *const ACL,
    path: &Path,
    sid: &str,
) -> SlResult<*mut ACL> {
    unsafe {
        let entry = EXPLICIT_ACCESS_W {
            grfAccessPermissions: FILE_ALL_ACCESS,
            grfAccessMode: GRANT_ACCESS,
            grfInheritance: OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE,
            // 单受托人：Trustee 自身就是受托人，`pMultipleTrustee` 留空。
            // `TrusteeForm = TRUSTEE_IS_SID` 时 `ptstrName` 指向的是 SID 指针。
            Trustee: TRUSTEE_W {
                pMultipleTrustee: std::ptr::null_mut(),
                MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_USER,
                ptstrName: psid as *mut u16,
            },
        };

        let mut new_dacl: *mut ACL = std::ptr::null_mut();
        let rc = SetEntriesInAclW(1, &entry, old_dacl, &mut new_dacl);
        if rc != 0 || new_dacl.is_null() {
            return Err(SlError::Other(tf("acl.grant_failed", &[
                ("sid", sid),
                ("path", &path.display().to_string()),
                ("err", &rc.to_string()),
            ])));
        }
        Ok(new_dacl)
    }
}

/// 目标平台不支持时给出明确说明（本工具目前仅 Windows）。
pub fn supported() -> bool {
    cfg!(windows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// 探针 DACL：显式授予 Everyone 完全控制，`P` 标志切断继承。
    /// 不引用具体账号 SID，测试不依赖运行身份。
    const PROBE_SDDL: &str = "D:P(A;OICI;FA;;;WD)";

    fn lab(tag: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!("td_acl_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn export_apply_roundtrip() {
        let base = lab("roundtrip");
        let src = base.join("src");
        let dst = base.join("dst");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();

        // %TEMP% 的继承链因机器而异：回灌不带 protected 标志时，父目录的
        // 可继承 ACE 会并入回读结果，与原始 SDDL 差一份继承条目。先用探针
        // DACL 把源目录切成"仅显式 ACE"，往返才有确定答案。
        apply_sddl(&src, PROBE_SDDL).unwrap();

        let sddl = export_sddl(&src).unwrap();
        assert!(!sddl.is_empty(), "应能导出非空 DACL");
        assert!(sddl.starts_with('D'), "SDDL 应以 D: (DACL) 开头，实际 {sddl}");

        // 贴到另一个目录上；同样以 protected 形式贴入，避免并入继承 ACE
        apply_sddl(&dst, &ensure_dacl_protected(&sddl)).unwrap();

        // 回读应一致
        let again = export_sddl(&dst).unwrap();
        assert_eq!(normalize(&sddl), normalize(&again));

        let _ = fs::remove_dir_all(&base);
    }

    /// 给 `D:(...)` 形式的 SDDL 补上 `P`（protected）标志；已带标志则原样返回。
    fn ensure_dacl_protected(sddl: &str) -> String {
        match sddl.strip_prefix("D:(") {
            Some(rest) => format!("D:P({rest}"),
            None => sddl.to_string(),
        }
    }

    #[test]
    fn empty_sddl_is_noop() {
        let base = lab("noop");
        assert!(apply_sddl(&base, "  ").is_ok());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn current_user_sid_is_a_sid() {
        let sid = current_user_sid().unwrap();
        assert!(sid.starts_with("S-1-"), "应返回 SID 字符串，实际 {sid}");
        // 提权不会改变令牌里的用户 SID，但 SYSTEM 会 —— 两种都算合法。
        assert!(sid == "S-1-5-18" || sid.starts_with("S-1-5-21") || sid.starts_with("S-1-5-32"), "{sid}");
    }

    #[test]
    fn grant_adds_full_control_for_current_user() {
        let base = lab("grant");
        let dir = base.join("target");
        fs::create_dir_all(&dir).unwrap();
        let sid = current_user_sid().unwrap();

        // 先把 DACL 换成"只有 SYSTEM"的样子，确保授权是这次改出来的
        apply_sddl(&dir, "D:P(A;OICI;FA;;;SY)").unwrap();
        assert!(
            !has_full_control(&dir, &sid).unwrap(),
            "改 DACL 之后不应还有当前用户的完全控制"
        );

        grant_full_control(&dir).unwrap();

        assert!(
            has_full_control(&dir, &sid).unwrap(),
            "授权后应能查到当前用户的完全控制：{}",
            export_sddl(&dir).unwrap()
        );
        // 原有 ACE 不能被冲掉
        let sddl = export_sddl(&dir).unwrap();
        assert!(sddl.contains("SY"), "原有 SYSTEM 条目应保留：{sddl}");

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn grant_is_idempotent() {
        let base = lab("idem");
        let dir = base.join("target");
        fs::create_dir_all(&dir).unwrap();
        let sid = current_user_sid().unwrap();

        grant_full_control(&dir).unwrap();
        let first = export_sddl(&dir).unwrap();
        grant_full_control(&dir).unwrap();
        grant_full_control(&dir).unwrap();
        let second = export_sddl(&dir).unwrap();

        assert_eq!(
            normalize(&first),
            normalize(&second),
            "重复授权不应让 DACL 越滚越大"
        );
        assert!(sddl_grants_full_control(&second, &sid));

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn granted_dir_children_inherit_permission() {
        let base = lab("inherit");
        let dir = base.join("target");
        fs::create_dir_all(&dir).unwrap();
        apply_sddl(&dir, "D:P(A;OICI;FA;;;SY)").unwrap();
        grant_full_control(&dir).unwrap();

        let child = dir.join("child");
        fs::create_dir_all(&child).unwrap();
        let sid = current_user_sid().unwrap();
        let sddl = export_sddl(&child).unwrap();
        assert!(
            sddl_grants_full_control(&sddl, &sid),
            "子目录应继承到完全控制：{sddl}"
        );

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn ensure_dir_full_control_creates_and_grants() {
        let base = lab("ensure");
        let dir = base.join("target");
        let sid = current_user_sid().unwrap();

        let created = ensure_dir_full_control(&dir).unwrap();
        assert!(created, "第一次应真的建出目录");
        assert!(dir.is_dir());
        assert!(has_full_control(&dir, &sid).unwrap());

        // 再来一次：目录已存在，不该报错，也不该重复堆 ACE
        let again = ensure_dir_full_control(&dir).unwrap();
        assert!(!again, "目录已存在时不应报告新建");
        assert!(has_full_control(&dir, &sid).unwrap());

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn grant_on_link_hits_the_link_not_the_target() {
        let base = lab("link");
        let target = base.join("target");
        let link = base.join("link");
        fs::create_dir_all(&target).unwrap();
        crate::link::create::create_dir_link(&link, &target, crate::link::LinkType::Junction).unwrap();

        let sid = current_user_sid().unwrap();
        // 目标目录先设成"没有当前用户权限"
        apply_sddl(&target, "D:P(A;OICI;FA;;;SY)").unwrap();
        assert!(!has_full_control(&target, &sid).unwrap());

        grant_full_control_on_link(&link).unwrap();

        // 目标目录的权限不能被这次授权改动
        assert!(
            !has_full_control(&target, &sid).unwrap(),
            "给链接条目授权不应改到目标目录上"
        );
        // 链接条目自己拿到了权限
        let link_sddl = link_own_sddl(&link).unwrap();
        assert!(
            sddl_grants_full_control(&link_sddl, &sid),
            "链接条目自身应有完全控制：{link_sddl}"
        );

        let _ = fs::remove_dir_all(&base);
    }

    /// 读链接**自身**（不跟随）的 DACL。
    fn link_own_sddl(link: &Path) -> SlResult<String> {
        super::link_own_sddl(link)
    }

    fn normalize(s: &str) -> String {
        // SDDL 里 ACE 顺序可能被系统重排，这里只比较字符集合
        let mut v: Vec<char> = s.chars().collect();
        v.sort_unstable();
        v.into_iter().collect()
    }
}
