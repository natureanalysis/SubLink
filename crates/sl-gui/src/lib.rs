//! SubLink 桌面端入口。
//!
//! 启动时做三件事（顺序有讲究）：
//! 1. 打开规则库（失败则降级成内存库 + 在界面上报错，而不是静默退出）；
//! 2. 清理上次崩溃留下的"进行中"操作流水（`mark_stale_ops`），
//!    否则它们会永远挂在"待修复"列表里；
//! 3. 装托盘。
//!
//! 界面本身是**静态文件**（`ui/` 目录，随二进制一起内嵌），不依赖 npm / 打包器 ——
//! 这个工具的前端就是一张表格加几个弹窗，引入一整条前端工具链得不偿失。

pub mod api;
pub mod commands;
pub mod notify;
pub mod submode;
pub mod tray;

use tauri::Manager;

use sl_core::store::Store;

/// 构建并运行应用。
pub fn run() {
    // ---- 0. 无界面子模式优先 ----
    // `boot-rebuild`（登录任务）与 `elevated <job>`（提权 worker）都必须在这里拦下来，
    // 不能让 Tauri 起来（否则登录任务会弹出一个窗口）。
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(code) = submode::dispatch(&args) {
        std::process::exit(code);
    }

    // ---- 1. 规则库 ----
    let (store, init_error) = match Store::open_default() {
        Ok(s) => (s, None),
        Err(e) => {
            // 降级到内存库：界面还能打开，并在顶部横幅显示真实原因，
            // 总比"双击没反应"或者一闪而过要好。
            let fallback =
                Store::open_in_memory().expect("连内存数据库都打不开，说明 SQLite 链接失败");
            (fallback, Some(e.to_string()))
        }
    };

    // ---- 2. 清理上次中断的操作 ----
    if let Ok(n) = store.mark_stale_ops() {
        if n > 0 {
            let _ = store.log_event(
                None,
                "warn",
                "stale_ops",
                &format!("启动时发现 {n} 条上次中断的操作，已标记为失败"),
            );
        }
    }

    let state = commands::AppState::new(store, init_error);

    tauri::Builder::default()
        // 单实例：第二个实例把已有窗口叫到前面，而不是再开一套 DB 连接
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.unminimize();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .manage(state)
        // 关闭窗口 ≠ 退出应用：默认静默隐藏到托盘，后台继续水位巡检 / 预授权自动清理
        // （否则这些"无人值守"能力只在窗口开着时才生效）。用户可在设置里改成直接退出。
        //
        // 这里刻意不发任何系统通知：点击 ✕ 只是收起窗口，不该用弹窗 / Toast 打断用户，
        // 也避免通知链路拉起额外的宿主进程。托盘图标本身就是"程序还在跑"的提示。
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if commands::close_to_tray(window.app_handle()) {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::ui_snapshot,
            commands::recent_events,
            commands::recent_ops,
            commands::savings,
            commands::add_root,
            commands::delete_root,
            commands::add_rule,
            commands::update_rule,
            commands::delete_rules,
            commands::undo_last,
            commands::set_rules_enabled,
            commands::set_rules_category,
            commands::set_rules_hidden,
            commands::reorder_rules,
            commands::capture_acl,
            commands::clear_acl,
            commands::inspect_source,
            commands::inspect_target,
            commands::preset_rules,
            commands::apply_rules,
            commands::restore_rules,
            commands::relink_rule,
            commands::check_locks,
            commands::clean_preview,
            commands::clean_run,
            commands::disk_usage,
            commands::water_poll,
            commands::boot_rebuild,
            commands::autostart_status,
            commands::autostart_enable,
            commands::autostart_disable,
            commands::autostart_run_now,
            commands::autostart_elevated_enable,
            commands::autostart_elevated_disable,
            commands::autostart_startup_enable,
            commands::autostart_startup_disable,
            commands::run_elevated_job,
            commands::export_config,
            commands::import_config,
            commands::pick_folder,
            commands::pick_save_file,
            commands::pick_open_file,
            commands::reveal_in_explorer,
            commands::show_main_window,
            commands::hide_main_window,
            commands::quit_app,
            commands::ui_settings,
            commands::set_close_to_tray,
            commands::set_allow_merge,
            commands::set_locale,
            commands::get_locale,
            commands::scheduled_clean_poll,
            commands::scheduled_clean_settings,
            commands::set_scheduled_clean,
        ])
        .setup(|app| {
            tray::install(app.handle())?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("SubLink 启动失败");
}
