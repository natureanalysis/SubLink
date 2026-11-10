//! 系统通知（Windows Toast）。
//!
//! 走 `tauri-plugin-notification` 的 **Rust API**（而非前端 `plugin:notification|*`），
//! 原因有两个：
//! 1. 应用自定义命令不受 Tauri ACL 限制，无需额外 capabilities 声明；
//! 2. 通知的触发点（水位越线、自动清理、重建失败）都在后端，前端窗口可能已隐藏，
//!    由后端直接发通知比"前端轮询到再转调"更可靠。
//!
//! **静默失败**是刻意的：用户关掉了通知权限、专注助手开着、系统版本不支持 ——
//! 这些都不该影响主流程，更不能因为通知失败把一次成功的巡检/清理判为失败。

use tauri::AppHandle;
use tauri_plugin_notification::NotificationExt;

/// 发一条系统通知；失败静默（见模块文档）。
pub fn notify(app: &AppHandle, title: &str, body: &str) {
    let _ = app.notification().builder().title(title).body(body).show();
}
