//! 前端与后端的「开机自启」接缝必须对得上。
//!
//! 这一条钉住的是最容易出错的接缝：前端 `invoke` 的命令名、读的字段，
//! 与后端注册的命令、下发的字段必须一致。名字对不上时**界面不报错**，
//! 只是按钮点了没反应 —— 用户只会觉得"这功能坏了"，很难查。
//!
//! 重点覆盖第三个任务「系统启动时」：它以 SYSTEM 身份在登录前跑，
//! 是机器级 TEMP/TMP 指向 RAM 盘时能开机就用的关键。

use std::fs;
use std::path::PathBuf;

fn ui_file(name: &str) -> String {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("ui");
    p.push(name);
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("读 {}: {e}", p.display()))
}

#[test]
fn frontend_calls_the_startup_task_commands() {
    let js = ui_file("app.js");
    assert!(
        js.contains("invoke('autostart_startup_enable')"),
        "前端应调用 autostart_startup_enable"
    );
    assert!(
        js.contains("invoke('autostart_startup_disable')"),
        "前端应调用 autostart_startup_disable"
    );
}

#[test]
fn frontend_reads_the_startup_task_fields() {
    let js = ui_file("app.js");
    assert!(
        js.contains("auto.startup_registered"),
        "前端应读 startup_registered 字段"
    );
    assert!(
        js.contains("auto.startup_task_name"),
        "前端应读 startup_task_name 字段"
    );
}

#[test]
fn mock_backend_implements_the_same_contract() {
    let js = ui_file("app.js");
    assert!(
        js.contains("case 'autostart_startup_enable'"),
        "mock 后端也要实现 autostart_startup_enable（否则 mock 模式与真机行为不一致）"
    );
    assert!(
        js.contains("case 'autostart_startup_disable'"),
        "mock 后端也要实现 autostart_startup_disable"
    );
}

#[test]
fn backend_view_carries_the_startup_task_state() {
    let store = sl_core::store::Store::open_in_memory().unwrap();
    let view = sl_core::usecase::autostart_status(&store);
    let json = serde_json::to_string(&view).unwrap();
    assert!(
        json.contains("\"startup_registered\""),
        "视图里要有 startup_registered：{json}"
    );
    assert!(
        json.contains("\"startup_task_name\""),
        "视图里要有 startup_task_name：{json}"
    );
    assert!(
        view.startup_task_name == "SubLink-BootRebuild-Startup",
        "任务名不对：{}",
        view.startup_task_name
    );
}

#[test]
fn backend_registers_the_startup_commands() {
    let lib = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("lib.rs"),
    )
    .unwrap();
    assert!(
        lib.contains("commands::autostart_startup_enable"),
        "Tauri 侧必须注册 autostart_startup_enable"
    );
    assert!(
        lib.contains("commands::autostart_startup_disable"),
        "Tauri 侧必须注册 autostart_startup_disable"
    );
}
