//! 「合并到已有目标目录」开关的前后端接缝必须对得上。
//!
//! ## 这条接缝钉的是什么
//!
//! 目标目录**已经有内容**时，普通应用会直接中止（"目标目录已有内容"）。
//! CLI 一直有 `apply --merge`，界面上却没有对应的开关 —— `allow_merge`
//! 在前端恒为 `false`，于是这道坎在界面上过不去。
//!
//! 这里钉住：
//! * 前端读到的偏好字段名（`allow_merge`）与后端下发的一致；
//! * 应用时这个值**真的**进了 `apply_rules` 的选项里（普通路径与提权路径都要）；
//! * 默认关闭（不给选项 → 目标非空时中止），且只认严格的 `"1"`；
//! * mock 后端提供同样的字段与命令（否则 mock 模式与真机行为不一致）。
//!
//! 字段名对不上时**界面不报错**：合并要么永远不发生，要么在用户不知情时
//! 覆盖目标里的旧文件 —— 后者更糟。

use std::fs;
use std::path::PathBuf;

fn ui_file(name: &str) -> String {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("ui");
    p.push(name);
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("读 {}: {e}", p.display()))
}

fn gui_src(name: &str) -> String {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("src");
    p.push(name);
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("读 {}: {e}", p.display()))
}

#[test]
fn frontend_reads_the_allow_merge_preference() {
    let js = ui_file("app.js");
    assert!(
        js.contains("state.uiSettings = await invoke('ui_settings', {})"),
        "前端应读后端下发的界面偏好"
    );
    assert!(
        js.contains("state.uiSettings.allow_merge"),
        "前端应从界面偏好里取 allow_merge"
    );
}

#[test]
fn apply_passes_allow_merge_in_both_paths() {
    let js = ui_file("app.js");
    // 普通路径
    assert!(
        js.contains("invoke('apply_rules', { ids: ids, opts: applyOpts(dryRun) })"),
        "普通应用路径必须走 applyOpts（带 allow_merge）"
    );
    // 提权路径
    assert!(
        js.contains("opts: elevatedOpts()"),
        "提权应用路径必须走 elevatedOpts（带 allow_merge）"
    );
    assert!(
        js.contains("allow_merge: mergeEnabled()"),
        "两个路径的选项都要把用户的合并开关带上"
    );
}

#[test]
fn the_switch_is_off_by_default() {
    let js = ui_file("app.js");
    assert!(
        js.contains("uiSettings: { close_to_tray: true, allow_merge: false }"),
        "合并开关缺省必须是关闭的"
    );
}

#[test]
fn backend_exposes_the_preference_and_the_command() {
    let src = gui_src("commands.rs");
    assert!(
        src.contains("pub const KEY_ALLOW_MERGE: &str = \"apply_allow_merge\""),
        "后端必须有这个 settings 键"
    );
    assert!(
        src.contains("pub allow_merge: bool"),
        "界面偏好结构里必须有 allow_merge 字段"
    );
    assert!(
        src.contains("pub fn set_allow_merge("),
        "必须有写这个偏好的命令"
    );
    // 只认严格的 "1"：放宽解析会让一次文件覆盖在用户不知情时发生。
    assert!(
        src.contains(".map(|v| v.trim() == \"1\")"),
        "合并开关只认严格的 \"1\"，脏数据一律按关闭处理"
    );
}

#[test]
fn command_is_registered() {
    let src = gui_src("lib.rs");
    assert!(
        src.contains("commands::set_allow_merge,"),
        "命令必须注册进 invoke_handler，否则前端调不动"
    );
}

#[test]
fn mock_backend_implements_the_same_contract() {
    let js = ui_file("app.js");
    assert!(
        js.contains("case 'set_allow_merge':"),
        "mock 后端也要实现 set_allow_merge"
    );
    assert!(
        js.contains("allow_merge: mockAllowMerge"),
        "mock 后端也要下发 allow_merge 字段"
    );
}

#[test]
fn both_locales_define_the_new_keys() {
    let js = ui_file("i18n.js");
    for key in [
        "settings.merge_when",
        "settings.merge_on",
        "settings.merge_saved_on",
        "settings.merge_saved_off",
        "settings.merge_hint",
    ] {
        let hits = js.matches(&format!("'{key}'")).count();
        assert_eq!(hits, 2, "{key} 应在中英文各出现一次，实得 {hits} 次");
    }
}
