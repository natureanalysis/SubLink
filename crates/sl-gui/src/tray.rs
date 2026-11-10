//! 托盘图标。
//!
//! 托盘提供两个"不用打开窗口就能做"的动作：
//! * 立即执行开机重建（用户手动补一次，比如 RAM 盘是登录后才挂上的）；
//! * **双击**图标（或菜单里的"显示主窗口"）唤出主窗口，菜单里还能退出。
//!
//! 重建动作放到后台线程跑：托盘事件是在主线程派发的，在这里做耗时 IO 会把界面卡死。
//!
//! ## 双击为什么要自己判定
//!
//! Tauri 2.11 的托盘后端是 `tray-icon` 0.24.2，它在 Windows 上注册托盘窗口类时
//! **没有置 `CS_DBLCLKS` 样式**（`platform_impl/windows/mod.rs` 里的 `WNDCLASSW`
//! 用 `..std::mem::zeroed()` 补全，`style` 留在 0）。Win32 的规则是：窗口类没有这个
//! 样式，系统就不会派发 `WM_LBUTTONDBLCLK`，于是 `TrayIconEvent::DoubleClick` 在
//! Windows 上**永远不会触发** —— 只挂这一个分支，双击到手就是"没反应"。
//!
//! 因此这里改成监听收得到的左键抬起（`WM_LBUTTONUP` 正常派发），按两次点击的**间隔**
//! 自己判定双击。显式的 `DoubleClick` 分支一并留着：macOS / Linux，或上游补上
//! `CS_DBLCLKS` 之后，它仍然会生效。

use std::sync::atomic::{AtomicU64, Ordering};

use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager};

use crate::commands::{self, AppState};

/// 托盘动作完成后广播给前端的事件名。
pub const EV_TRAY_BOOT: &str = "td-tray-boot";

/// 两次左键单击的间隔不超过它，就算一次双击。
///
/// 取固定的 500 ms：普通"连点两下"的手势（间隔通常 < 300 ms）必然命中，而刻意分开的
/// 两次点击不会误判。没为此去调 Win32 的 `GetDoubleClickTime()` —— 为一个手感参数拖进
/// winapi 依赖不划算。若用户把系统双击间隔调得特别长，两下会被认成两次单击，此时仍可
/// 用托盘菜单的"显示主窗口"，功能不会丢。
const DOUBLE_CLICK_MS: u64 = 500;

/// 上一次左键单击的时间戳（Unix 毫秒）。0 = 还没有单击过。
static LAST_LEFT_CLICK_MS: AtomicU64 = AtomicU64::new(0);

pub fn install(app: &AppHandle) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "显示主窗口", true, None::<&str>)?;
    let boot = MenuItem::with_id(app, "boot", "立即执行开机重建", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &boot, &quit])?;

    let mut builder = TrayIconBuilder::with_id("main-tray")
        .tooltip("SubLink")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => focus_main(app),
            "boot" => spawn_boot(app.clone()),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            match event {
                // 上游补上 CS_DBLCLKS 后（或非 Windows 平台）走这条。
                TrayIconEvent::DoubleClick {
                    button: MouseButton::Left,
                    ..
                } => focus_main(tray.app_handle()),
                // Windows 上真正收得到的是这条：左键抬起。自己判两次的间隔。
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } => {
                    if left_click_is_double(now_millis()) {
                        focus_main(tray.app_handle());
                    }
                }
                _ => {}
            }
        });

    if let Some(icon) = app.default_window_icon().cloned() {
        builder = builder.icon(icon);
    }
    builder.build(app)?;
    Ok(())
}

fn focus_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

/// 把这次左键单击记进状态，并回答"它和上一次是否构成双击"。
fn left_click_is_double(now_ms: u64) -> bool {
    let prev = LAST_LEFT_CLICK_MS.load(Ordering::SeqCst);
    if is_double_click(prev, now_ms) {
        // 消费掉这次双击：连点三下不该被算成"第二次双击"。
        LAST_LEFT_CLICK_MS.store(0, Ordering::SeqCst);
        true
    } else {
        LAST_LEFT_CLICK_MS.store(now_ms, Ordering::SeqCst);
        false
    }
}

/// 纯判定：`prev_ms == 0` 表示"上一次不存在"，此时不算双击。
fn is_double_click(prev_ms: u64, now_ms: u64) -> bool {
    prev_ms != 0 && now_ms >= prev_ms && now_ms - prev_ms <= DOUBLE_CLICK_MS
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn spawn_boot(app: AppHandle) {
    focus_main(&app);
    std::thread::spawn(move || {
        let state = app.state::<AppState>();
        let payload = match commands::lock_store(&state) {
            Ok(store) => match crate::api::boot_rebuild(&store) {
                Ok(report) => serde_json::json!({
                    "ok": !report.has_problem(),
                    "message": report.one_line(),
                    "warnings": report.warnings,
                    "failed": report.failed,
                }),
                Err(e) => {
                    serde_json::json!({ "ok": false, "message": e.to_string(), "warnings": [], "failed": [] })
                }
            },
            Err(e) => {
                serde_json::json!({ "ok": false, "message": e, "warnings": [], "failed": [] })
            }
        };
        let _ = app.emit(EV_TRAY_BOOT, payload);
    });
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn double_click_requires_a_previous_click_within_the_window() {
        assert!(!is_double_click(0, 10_000), "首次单击没有可比对象");
        assert!(is_double_click(10_000, 10_300), "间隔 300ms 内 = 双击");
        assert!(is_double_click(10_000, 10_500), "正好到阈值 = 双击");
        assert!(!is_double_click(10_000, 10_501), "超过阈值 = 两次单击");
        assert!(is_double_click(10_000, 10_000), "同一毫秒内连点也算双击");
    }

    #[test]
    fn double_click_rejects_missing_and_backwards_clocks() {
        assert!(!is_double_click(0, 0), "从未单击过");
        assert!(!is_double_click(10_000, 9_000), "时间回拨不算双击");
    }
}
