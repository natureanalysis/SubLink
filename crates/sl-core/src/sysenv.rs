//! 系统设置模式：直接改注册表里的环境变量，而不是在源路径上挂 Junction。
//!
//! ## 为什么需要这一层
//!
//! 有些系统目录（`%TEMP%` / `%TMP%`、`C:\Windows\Temp`）**本来就是环境变量
//! 决定的**。Windows 上进程读到的临时目录来自 `TEMP` / `TMP` 两个变量，
//! 它们存在注册表里：
//!
//! | 作用域 | 注册表位置 | 谁能写 |
//! |---|---|---|
//! | 用户 | `HKEY_CURRENT_USER\Environment` | 当前用户，免提权 |
//! | 系统 | `HKEY_LOCAL_MACHINE\SYSTEM\CurrentControlSet\Control\Session Manager\Environment` | 需要管理员 |
//!
//! 对这类目录，**改注册表才是"对的地方"**：它对所有按环境变量取路径的程序生效
//! （包括不读 `%LOCALAPPDATA%\Temp` 这种硬编码路径的），而且不会在 `C:\` 上
//! 留下任何重解析点 —— 也就没有"链接被软件更新覆盖""断链"这些麻烦。
//!
//! ## 与链接模式的对比
//!
//! | | 链接模式（Junction） | 系统设置模式（本模块） |
//! |---|---|---|
//! | 生效面 | 只有打开该路径的程序 | 所有读环境变量的程序 |
//! | 源路径 | 变成重解析点 | 保持原样（内容被搬走） |
//! | 断链风险 | 有（RAM 盘掉了就断） | 无（环境变量是字符串，指向哪都行） |
//! | 需要提权 | 仅受保护源路径 | 用户级不需要；系统级需要 |
//!
//! ## 写完必须广播 `WM_SETTINGCHANGE`
//!
//! 只改注册表的话，**已经运行的程序**和**已经启动的 explorer** 仍然拿着旧的环境块，
//! 新建的临时文件还会往老地方写。必须广播一次 `WM_SETTINGCHANGE("Environment")`，
//! explorer 才会重新读取环境块、把它传给之后启动的程序。
//! 已经启动的程序不会更新（它们的进程环境块在创建时就固定了）—— 这是 Windows 的
//! 行为，不是本工具能改变的，所以界面上必须提示"需要重启相关软件"。
//!
//! ## 值的类型
//!
//! 环境变量在注册表里通常是 `REG_EXPAND_SZ`（可含 `%VAR%`）或 `REG_SZ`。
//! 写的时候**保持原类型**：把 `REG_EXPAND_SZ` 写成 `REG_SZ` 会让依赖变量展开的
//! 程序拿不到展开后的路径；反过来把 `REG_SZ` 写成 `REG_EXPAND_SZ` 通常无害，
//! 但保持一致最省心。
//!
//! ## 只在 Windows 上编译
//!
//! 本工具只支持 Windows；`#[cfg(not(windows))]` 下本模块退化成"永远失败"，
//! 以便跨平台 CI 至少能编译通过。

use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LPARAM, WPARAM};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_EXPAND_SZ,
    REG_SZ, REG_VALUE_TYPE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    SendMessageTimeoutW, HWND_BROADCAST, SMTO_ABORTIFHUNG, WM_SETTINGCHANGE,
};

use crate::error::{SlError, SlResult};
use crate::i18n::{t, tf};
use crate::util;

/// 用户级环境变量的注册表子键。
const USER_ENV_KEY: &str = "Environment";
/// 系统级环境变量的注册表子键。
const MACHINE_ENV_KEY: &str = r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment";

/// 注册表里"某个值不存在"的错误码。
const ERROR_FILE_NOT_FOUND: u32 = 2;
/// 权限不足。
const ERROR_ACCESS_DENIED: u32 = 5;

/// 广播 `WM_SETTINGCHANGE` 的超时（毫秒）。
const BROADCAST_TIMEOUT_MS: u32 = 5000;

/// 环境变量的写入作用域。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvScope {
    /// `HKEY_CURRENT_USER\Environment` —— 当前用户，免提权。
    User,
    /// `HKEY_LOCAL_MACHINE\...\Session Manager\Environment` —— 全机，需管理员。
    Machine,
}

impl EnvScope {
    pub fn as_str(self) -> &'static str {
        match self {
            EnvScope::User => "user",
            EnvScope::Machine => "machine",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "user" | "hkcu" | "current_user" => Some(EnvScope::User),
            "machine" | "hklm" | "local_machine" | "system" => Some(EnvScope::Machine),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            EnvScope::User => t("sysenv.scope.user"),
            EnvScope::Machine => t("sysenv.scope.machine"),
        }
    }

    /// 该作用域的写入是否需要管理员权限。
    ///
    /// 系统级环境变量写在 HKLM 下，普通用户写会拿到 `ERROR_ACCESS_DENIED`；
    /// 用户级写在 HKCU 下，自己就能写。
    pub fn requires_elevation(self) -> bool {
        matches!(self, EnvScope::Machine)
    }

    /// 注册表根键 + 子键路径。
    fn reg_path(self) -> (HKEY, &'static str) {
        match self {
            EnvScope::User => (HKEY_CURRENT_USER, USER_ENV_KEY),
            EnvScope::Machine => (HKEY_LOCAL_MACHINE, MACHINE_ENV_KEY),
        }
    }
}

/// 一个环境变量当前的值，以及它在注册表里的类型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvValue {
    /// 注册表里的原始文本（未展开 `%VAR%`）。
    pub raw: String,
    /// 注册表类型：`REG_SZ` / `REG_EXPAND_SZ`。
    pub reg_type: REG_VALUE_TYPE,
}

impl EnvValue {
    /// 展开后的值（`%VAR%` 已替换）。展开失败时退回原始文本。
    pub fn expanded(&self) -> String {
        let (s, unknown) = util::expand_env(&self.raw);
        if unknown.is_empty() {
            s
        } else {
            self.raw.clone()
        }
    }

    /// 注册表类型的可读名（`REG_SZ` / `REG_EXPAND_SZ`）。
    ///
    /// 自检与诊断要用：类型被降级（`REG_EXPAND_SZ` → `REG_SZ`）是真实故障 ——
    /// 依赖 `%VAR%` 展开的程序会拿不到展开后的路径。
    pub fn type_name(&self) -> &'static str {
        if self.reg_type == REG_SZ {
            "REG_SZ"
        } else {
            "REG_EXPAND_SZ"
        }
    }
}

/// 读一个环境变量的注册表原值。值不存在返回 `Ok(None)`。
pub fn read_env(scope: EnvScope, name: &str) -> SlResult<Option<EnvValue>> {
    let (root, sub) = scope.reg_path();
    let key = open_key(root, sub, KEY_QUERY_VALUE)?;
    let Some(key) = key else { return Ok(None) };
    let result = query_value(key, name);
    close_key(key);
    result
}

/// 读一个环境变量并展开（等价于进程里看到的值）。
///
/// 读的是**注册表**而不是 `std::env` —— 后者是本进程启动时的快照，
/// 早就被别的改动污染了，不能用来判断"系统设置现在是什么"。
pub fn read_env_expanded(scope: EnvScope, name: &str) -> SlResult<Option<String>> {
    Ok(read_env(scope, name)?.map(|v| v.expanded()))
}

/// 这个变量当前**是否指向**给定路径。
///
/// 单一判定入口：`engine::setting_state`（整组变量）与 `doctor` 自检
/// （逐个变量）都走它，避免两处的路径比较规则漂移。
///
/// 变量不存在、或读取失败 → 一律 `false`（"没指向目标"）。
pub fn env_points_to(scope: EnvScope, name: &str, target: &Path) -> bool {
    match read_env_expanded(scope, name) {
        Ok(Some(cur)) => util::path_eq(&PathBuf::from(cur.trim()), target),
        _ => false,
    }
}

/// 写一个环境变量。**保持原类型**：原来没有就写 `REG_EXPAND_SZ`（Windows 的惯例）。
pub fn write_env(scope: EnvScope, name: &str, value: &str) -> SlResult<()> {
    let (root, sub) = scope.reg_path();
    let key = open_key(root, sub, KEY_SET_VALUE | KEY_QUERY_VALUE)?;
    let Some(key) = key else {
        return Err(SlError::Other(tf("sysenv.open_failed", &[
            ("scope", scope.label()),
            ("err", &t("sysenv.open_failed")),
        ])));
    };

    // 类型跟随原值；原来没有就用 REG_EXPAND_SZ（与 Windows 写环境变量的惯例一致）。
    let reg_type = match query_value(key, name) {
        Ok(Some(v)) => v.reg_type,
        Ok(None) => REG_EXPAND_SZ,
        Err(_) => REG_EXPAND_SZ,
    };

    let wide_name = util::to_wide(name);
    let wide_data = util::to_wide(value);
    // 字节数含结尾的 NUL。
    let cb = (wide_data.len() * 2) as u32;
    let rc = unsafe {
        RegSetValueExW(
            key,
            wide_name.as_ptr(),
            0,
            reg_type,
            wide_data.as_ptr().cast::<u8>(),
            cb,
        )
    };
    close_key(key);
    if rc != ERROR_SUCCESS {
        return Err(reg_error(rc, scope, name));
    }
    Ok(())
}

/// 删除一个环境变量（恢复"没有这一项"的状态）。
///
/// 值本来就不存在时返回 `Ok(false)`（不算错）。
pub fn delete_env(scope: EnvScope, name: &str) -> SlResult<bool> {
    let (root, sub) = scope.reg_path();
    let key = open_key(root, sub, KEY_SET_VALUE)?;
    let Some(key) = key else { return Ok(false) };
    let wide_name = util::to_wide(name);
    let rc = unsafe { RegDeleteValueW(key, wide_name.as_ptr()) };
    close_key(key);
    if rc == ERROR_SUCCESS {
        Ok(true)
    } else if rc == ERROR_FILE_NOT_FOUND {
        Ok(false)
    } else {
        Err(reg_error(rc, scope, name))
    }
}

/// 广播 `WM_SETTINGCHANGE("Environment")`，让 explorer 重新读取环境块。
///
/// 失败**不阻断**主流程：注册表已经写对了，只是新启动的程序可能晚一点才拿到新值。
/// 返回 `Ok(false)` 表示广播没成功（调用方据此留一条 warn）。
pub fn broadcast_environment_change() -> SlResult<bool> {
    let payload = util::to_wide("Environment");
    let mut result: usize = 0;
    let ok = unsafe {
        SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            0 as WPARAM,
            payload.as_ptr() as LPARAM,
            SMTO_ABORTIFHUNG,
            BROADCAST_TIMEOUT_MS,
            &mut result,
        )
    };
    Ok(ok != 0)
}

/// 打开注册表键。键不存在返回 `Ok(None)`（`KEY_SET_VALUE` 不会自动创建键）。
fn open_key(root: HKEY, sub: &str, access: u32) -> SlResult<Option<HKEY>> {
    let wide = util::to_wide(sub);
    let mut handle: HKEY = std::ptr::null_mut();
    let rc = unsafe { RegOpenKeyExW(root, wide.as_ptr(), 0, access, &mut handle) };
    if rc == ERROR_SUCCESS {
        Ok(Some(handle))
    } else if rc == ERROR_FILE_NOT_FOUND {
        Ok(None)
    } else {
        Err(SlError::Other(tf("sysenv.open_failed", &[
            ("scope", &sub.to_string()),
            ("err", &format!("{rc}")),
        ])))
    }
}

fn close_key(key: HKEY) {
    unsafe {
        RegCloseKey(key);
    }
}

/// 读一个注册表值。
fn query_value(key: HKEY, name: &str) -> SlResult<Option<EnvValue>> {
    let wide_name = util::to_wide(name);
    let mut reg_type: REG_VALUE_TYPE = 0;
    let mut size: u32 = 0;
    let rc = unsafe {
        RegQueryValueExW(
            key,
            wide_name.as_ptr(),
            std::ptr::null(),
            &mut reg_type,
            std::ptr::null_mut(),
            &mut size,
        )
    };
    if rc == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    if rc != ERROR_SUCCESS {
        return Err(reg_error(rc, EnvScope::User, name));
    }
    if size == 0 {
        return Ok(Some(EnvValue {
            raw: String::new(),
            reg_type,
        }));
    }

    // REG_SZ / REG_EXPAND_SZ 按 UTF-16 处理；其它类型（如 REG_DWORD）不是路径。
    if reg_type != REG_SZ && reg_type != REG_EXPAND_SZ {
        return Err(SlError::Other(tf("sysenv.bad_type", &[
            ("name", name),
            ("type", &format!("{reg_type}")),
        ])));
    }

    let mut buf = vec![0u8; size as usize];
    let mut size = size;
    let rc = unsafe {
        RegQueryValueExW(
            key,
            wide_name.as_ptr(),
            std::ptr::null(),
            &mut reg_type,
            buf.as_mut_ptr(),
            &mut size,
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(reg_error(rc, EnvScope::User, name));
    }
    // 宽字符按 u16 读回（去掉结尾 NUL）。
    let words = size as usize / 2;
    let units: Vec<u16> = buf[..words * 2]
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    Ok(Some(EnvValue {
        raw: util::from_wide_buf(&units),
        reg_type,
    }))
}

/// 把注册表错误码翻译成带上下文的错误。
fn reg_error(rc: u32, scope: EnvScope, name: &str) -> SlError {
    if rc == ERROR_ACCESS_DENIED {
        return SlError::ElevationRequired(tf("sysenv.access_denied", &[
            ("scope", scope.label()),
            ("name", name),
        ]));
    }
    SlError::Other(tf("sysenv.reg_failed", &[
        ("scope", scope.label()),
        ("name", name),
        ("err", &format!("{rc}")),
    ]))
}

/// 系统设置模式下要改的环境变量名。
///
/// 只接受白名单里的名字：这是**能改系统设置**的入口，名字必须是我们明确支持的，
/// 不能让用户随便写一个名字就去改注册表。
///
/// ## 允许的变量
///
/// * `TEMP` / `TMP` —— 临时目录（本工具的主场景）。
///
/// ## 为什么**没有** `LOCALAPPDATA`
///
/// 它的性质和 TEMP 完全不同，不能一并放行：
///
/// 1. **它不在 `HKCU\Environment` 里** —— 权威来源是 Shell 的
///    `User Shell Folders\Local AppData`。往注册表里写一个 `LOCALAPPDATA`
///    只是**新增**一个同名环境变量，与 Shell 的算法并存、两套值打架，
///    到底哪个生效取决于程序怎么取值 —— 不是"改设置"，是"制造歧义"。
/// 2. **它装的是不可再生的用户数据**，不是缓存：本机实测
///    `%LOCALAPPDATA%\Google\Chrome\User Data\Default\` 下有 `Bookmarks`、
///    `Login Data`（密码）、`Preferences`。把它重定向到 RAM 盘，
///    等于每次重启都丢书签和密码 —— 这与本工具"只搬缓存"的定位相反。
///
/// 所以宁可少支持一个变量，也不给"下次开机发现书签没了"留口子。
pub fn is_supported_var(name: &str) -> bool {
    SUPPORTED_VARS
        .iter()
        .any(|v| v.eq_ignore_ascii_case(name.trim()))
}

/// 本工具允许改写的环境变量名（白名单全集）。
///
/// [`is_supported_var`] 与「系统设置模式」的可行性判定共用这一份清单，
/// 避免出现「白名单加了变量、判定逻辑还停在旧名单」的两处不同步。
pub const SUPPORTED_VARS: [&str; 2] = ["TEMP", "TMP"];

/// 校验一组环境变量名。返回规范化（大写、去空白）后的列表。
pub fn normalize_vars(names: &[String]) -> SlResult<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for n in names {
        let name = n.trim().to_ascii_uppercase();
        if name.is_empty() {
            continue;
        }
        if !is_supported_var(&name) {
            // LOCALAPPDATA 单独给一条说明：它曾经被支持过（存量规则里可能还留着），
            // 用户看到"不支持"会疑惑为什么 —— 要讲清楚**为什么**不支持。
            let msg = if name == "LOCALAPPDATA" {
                t("sysenv.unsupported_var_localappdata").to_string()
            } else {
                tf("sysenv.unsupported_var", &[("name", &name)])
            };
            return Err(SlError::InvalidPath(msg));
        }
        if !out.contains(&name) {
            out.push(name);
        }
    }
    if out.is_empty() {
        return Err(SlError::InvalidPath(t("sysenv.no_vars").into()));
    }
    Ok(out)
}

/// 把「某个环境变量当前指向的目录」解析成路径。
///
/// 读不到（变量不存在 / 值为空）返回 `None`。
pub fn env_dir(scope: EnvScope, name: &str) -> SlResult<Option<std::path::PathBuf>> {
    let raw = read_env_expanded(scope, name)?;
    Ok(raw
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from))
}

/// 把目标路径写成环境变量的值（展开后的绝对路径）。
pub fn set_env_to_path(scope: EnvScope, name: &str, path: &Path) -> SlResult<()> {
    write_env(scope, name, &path.to_string_lossy())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_parsing() {
        assert_eq!(EnvScope::parse("user"), Some(EnvScope::User));
        assert_eq!(EnvScope::parse("HKCU"), Some(EnvScope::User));
        assert_eq!(EnvScope::parse(" Machine "), Some(EnvScope::Machine));
        assert_eq!(EnvScope::parse("hklm"), Some(EnvScope::Machine));
        assert_eq!(EnvScope::parse("nope"), None);
    }

    #[test]
    fn only_machine_scope_needs_elevation() {
        assert!(!EnvScope::User.requires_elevation());
        assert!(EnvScope::Machine.requires_elevation());
    }

    #[test]
    fn supported_vars_are_whitelisted() {
        assert!(is_supported_var("TEMP"));
        assert!(is_supported_var("tmp"));
        assert!(is_supported_var(" TMP "));
        assert!(!is_supported_var("PATH"));
        assert!(!is_supported_var("PATHEXT"));
        assert!(!is_supported_var(""));
    }

    #[test]
    fn localappdata_is_not_supported() {
        // LOCALAPPDATA 曾经被支持过，后来移出白名单 —— 它由 Shell 计算、
        // 不在注册表的环境变量里，且装的是书签密码这类不可再生的用户数据。
        // 这条护栏钉住这个决定：谁想把它加回来，先看看这里。
        assert!(!is_supported_var("LOCALAPPDATA"));
        assert!(!is_supported_var("localappdata"));
        let err = normalize_vars(&["LOCALAPPDATA".into()]).unwrap_err();
        assert!(matches!(err, SlError::InvalidPath(_)), "{err}");
        // 错误文案要说明**为什么**不支持（存量规则里可能还留着它）。
        let msg = err.to_string();
        assert!(
            msg.contains("LOCALAPPDATA") && msg.contains("书签"),
            "错误文案应点名变量并说明原因：{msg}"
        );
    }

    #[test]
    fn normalize_dedupes_and_uppercases() {
        let v = normalize_vars(&["temp".into(), " TEMP ".into(), "Tmp".into()]).unwrap();
        assert_eq!(v, vec!["TEMP".to_string(), "TMP".into()]);
    }

    #[test]
    fn normalize_rejects_unsupported() {
        let err = normalize_vars(&["PATH".into()]).unwrap_err();
        assert!(matches!(err, SlError::InvalidPath(_)), "{err}");
    }

    #[test]
    fn normalize_rejects_empty_list() {
        let err = normalize_vars(&["  ".into()]).unwrap_err();
        assert!(matches!(err, SlError::InvalidPath(_)), "{err}");
    }

    #[test]
    fn reading_absent_user_var_is_none_not_error() {
        // 这个名字几乎不可能被设置过；读不到必须是 None 而不是错误。
        let v = read_env(EnvScope::User, "TD_SYSENV_ABSENT_XYZ").unwrap();
        assert_eq!(v, None);
    }

    #[test]
    fn env_points_to_is_false_for_absent_var() {
        // 变量不存在 → "没有指向目标"，而不是报错。
        assert!(!env_points_to(
            EnvScope::User,
            "TD_SYSENV_ABSENT_XYZ",
            Path::new(r"C:\whatever")
        ));
    }

    #[test]
    fn env_points_to_matches_current_user_temp() {
        // 用真实存在的 TEMP 验证判定本身是通的（只读，不改任何值）。
        // 断言"指向自己当前值"为真、"指向别处"为假。
        let cur = read_env_expanded(EnvScope::User, "TEMP")
            .unwrap()
            .expect("TEMP 应当存在于 HKCU\\Environment");
        let trimmed = cur.trim();
        assert!(
            env_points_to(EnvScope::User, "TEMP", Path::new(trimmed)),
            "变量当前值应当被判定为'指向自己'"
        );
        assert!(
            !env_points_to(
                EnvScope::User,
                "TEMP",
                Path::new(r"Z:\definitely\not\here")
            ),
            "指向别处必须判为 false"
        );
    }

    #[test]
    fn env_value_expands_percent_vars() {
        let v = EnvValue {
            raw: "%TD_TEST_EXPAND%\\sub".into(),
            reg_type: REG_EXPAND_SZ,
        };
        std::env::set_var("TD_TEST_EXPAND", "VALUE");
        assert_eq!(v.expanded(), "VALUE\\sub");
    }

    #[test]
    fn env_value_keeps_raw_when_expansion_fails() {
        let v = EnvValue {
            raw: "%TD_NOT_SET_XYZ%\\sub".into(),
            reg_type: REG_EXPAND_SZ,
        };
        assert_eq!(v.expanded(), "%TD_NOT_SET_XYZ%\\sub");
    }
}
