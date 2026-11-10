//! 权限兜底：建完目录 / 建完链接后，确保**对应使用者**有完全控制。
//!
//! ## 为什么要单独一层
//!
//! 目录和链接是在 `engine` / `boot` 里建出来的，但"谁该有权限"是平台层
//! （[`crate::acl`]）的事。这层只做两件事：调用授权、把失败**记成事件**。
//!
//! ## 失败为什么不阻断主流程
//!
//! 链接已经建好、目录已经存在，这时候因为一条 DACL 写不进去就把整次应用判失败，
//! 代价远大于收益：用户看到的是"重定向失败"，而真实问题只是"权限可能不对"。
//! 但**不能静默** —— 权限不对的表现是"软件莫名报错、缓存写不进去"，
//! 没有痕迹就等于埋雷，所以每次失败都落一条 `warn` 事件，开机重建还会额外进报告。

use std::path::Path;

use crate::acl;
use crate::i18n::tf;
use crate::store::Store;

/// 确保"对应使用者"对**目标目录**有完全控制。失败时返回错误文案（并已落事件）。
pub fn ensure_dir_access(store: &Store, rule_id: Option<i64>, path: &Path) -> Option<String> {
    match acl::grant_full_control(path) {
        Ok(()) => None,
        Err(e) => {
            let msg = tf("acl.dir_failed", &[
                ("path", &path.display().to_string()),
                ("err", &e.to_string()),
            ]);
            let _ = store.log_event(rule_id, "warn", "acl_dir_failed", &msg);
            Some(msg)
        }
    }
}

/// 确保"对应使用者"对**链接条目**有完全控制。失败时返回错误文案（并已落事件）。
pub fn ensure_link_access(store: &Store, rule_id: Option<i64>, link: &Path) -> Option<String> {
    match acl::grant_full_control_on_link(link) {
        Ok(()) => None,
        Err(e) => {
            let msg = tf("acl.link_failed", &[
                ("path", &link.display().to_string()),
                ("err", &e.to_string()),
            ]);
            let _ = store.log_event(rule_id, "warn", "acl_link_failed", &msg);
            Some(msg)
        }
    }
}
