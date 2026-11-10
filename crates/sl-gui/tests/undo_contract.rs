//! 前端与后端的「撤销」接缝必须对得上。
//!
//! 这一条钉住的是最容易出错的接缝：前端 `invoke` 的命令名、读的快照字段，
//! 与后端注册的命令、下发的字段必须一致。名字对不上时**界面不报错**，
//! 只是「撤销」按钮一直灰着 —— 用户只会觉得"这功能坏了"，很难查。
//!
//! （同一类护栏还有 `sl-core` 里的 `rule_form_wire_contract`。）

use std::fs;
use std::path::PathBuf;

fn ui_file(name: &str) -> String {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("ui");
    p.push(name);
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("读 {}: {e}", p.display()))
}

#[test]
fn frontend_calls_the_undo_last_command() {
    let js = ui_file("app.js");
    assert!(
        js.contains("invoke('undo_last'"),
        "前端应调用 undo_last 命令"
    );
    assert!(
        !js.contains("undo_delete"),
        "前端不该再调用旧的 undo_delete（命令已改名为 undo_last）"
    );
}

#[test]
fn frontend_reads_the_undo_snapshot_field() {
    let js = ui_file("app.js");
    assert!(
        js.contains("state.snap.undo"),
        "前端应读快照里的 undo 字段"
    );
    assert!(
        !js.contains("state.snap.trash"),
        "旧的 trash 字段已不存在（撤销栈合并成 undo）"
    );
}

#[test]
fn mock_backend_implements_the_same_contract() {
    let js = ui_file("app.js");
    assert!(
        js.contains("case 'undo_last'"),
        "mock 后端也要实现 undo_last（否则 mock 模式与真机行为不一致）"
    );
    assert!(
        js.contains("undo: mockUndoView()"),
        "mock 快照也要给出 undo 字段"
    );
}

#[test]
fn backend_snapshot_carries_the_undo_view() {
    let store = sl_core::store::Store::open_in_memory().unwrap();
    let snap = sl_core::usecase::snapshot(&store).unwrap();
    let json = serde_json::to_string(&snap).unwrap();
    assert!(json.contains("\"undo\""), "快照里要有 undo 字段：{json}");
    assert!(
        !json.contains("\"trash\""),
        "快照里不该再有 trash 字段：{json}"
    );
}
