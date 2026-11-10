//! 统一错误类型。
//!
//! 设计原则：错误信息**本身就是给用户看的文案**（带具体路径、带下一步建议）。
//! UI 与 CLI 都原样展示，不做二次包装，避免出现"操作失败（错误 0x80070005）"这种没用的提示。
//!
//! 文案按[当前语言](crate::i18n)在 `Display` 时解析：同一份错误值，
//! 中文环境显示中文、英文环境显示英文，调用点无需感知语言。

use crate::i18n;
use std::path::Path;

/// SubLink 统一错误。
#[derive(Debug)]
pub enum SlError {
    Io(std::io::Error),
    Db(rusqlite::Error),
    Json(serde_json::Error),
    InvalidPath(String),
    SafetyBlocked(String),
    Busy(String),
    NoSpace { need: String, free: String },
    VerifyFailed(String),
    ElevationRequired(String),
    NotFound(String),
    Other(String),
}

impl std::fmt::Display for SlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            SlError::Io(e) => i18n::tf("err.io", &[("e", &e.to_string())]),
            SlError::Db(e) => i18n::tf("err.db", &[("e", &e.to_string())]),
            SlError::Json(e) => i18n::tf("err.json", &[("e", &e.to_string())]),
            SlError::InvalidPath(m) => i18n::tf("err.invalid_path", &[("e", m)]),
            SlError::SafetyBlocked(m) => i18n::tf("err.safety", &[("e", m)]),
            SlError::Busy(m) => i18n::tf("err.busy", &[("e", m)]),
            SlError::NoSpace { need, free } => {
                i18n::tf("err.no_space", &[("need", need), ("free", free)])
            }
            SlError::VerifyFailed(m) => i18n::tf("err.verify", &[("e", m)]),
            SlError::ElevationRequired(m) => i18n::tf("err.elevation", &[("e", m)]),
            SlError::NotFound(m) => i18n::tf("err.not_found", &[("e", m)]),
            SlError::Other(m) => m.clone(),
        };
        f.write_str(&s)
    }
}

impl std::error::Error for SlError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SlError::Io(e) => Some(e),
            SlError::Db(e) => Some(e),
            SlError::Json(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for SlError {
    fn from(e: std::io::Error) -> Self {
        SlError::Io(e)
    }
}

impl From<rusqlite::Error> for SlError {
    fn from(e: rusqlite::Error) -> Self {
        SlError::Db(e)
    }
}

impl From<serde_json::Error> for SlError {
    fn from(e: serde_json::Error) -> Self {
        SlError::Json(e)
    }
}

pub type SlResult<T> = Result<T, SlError>;

/// 把裸 IO 错误翻译成「动作 + 路径 + 建议」三段式文案。
pub fn io_ctx(e: std::io::Error, path: &Path, action: &str) -> SlError {
    let hint_key = match e.raw_os_error() {
        Some(32) => "hint.busy",
        Some(5) => "hint.denied",
        Some(2) | Some(3) => "hint.not_exist",
        Some(112) => "hint.no_space",
        Some(206) => "hint.too_long",
        Some(4390) => "hint.not_reparse",
        _ => "hint.none",
    };
    let hint = i18n::t(hint_key);
    SlError::Other(i18n::tf(
        "io.failed",
        &[
            ("action", action),
            ("err", &e.to_string()),
            ("hint", hint),
            ("path", &path.display().to_string()),
        ],
    ))
}

/// 该 IO 错误是否属于"换个提权进程就能解决"。
pub fn needs_elevation(e: &std::io::Error) -> bool {
    // 5 = ERROR_ACCESS_DENIED，1314 = ERROR_PRIVILEGE_NOT_HELD
    matches!(e.raw_os_error(), Some(5) | Some(1314))
}

/// 该 IO 错误是否属于"被别的进程占着"。
pub fn is_busy(e: &std::io::Error) -> bool {
    e.raw_os_error() == Some(32)
}