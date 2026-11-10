//! 「系统设置」选项的前后端接缝必须对得上。
//!
//! ## 这条接缝钉的是什么
//!
//! 「重定向方式」下拉里的「系统设置」选项**只对能落地的规则出现** ——
//! 判据是源路径正好是某个 `TEMP` / `TMP` 当前指向的目录（这个模式改的是
//! 注册表里的环境变量值，变量指向别处就改不动）。
//!
//! 这个判定放在后端（`sl-core`）算，前端只负责按它给不给选项。这里钉住：
//! 前端读的字段名、后端给的字段名、mock 后端提供的字段名三者一致，
//! 以及「不可用时要有说明」这句提示不会在翻译表里缺项。
//!
//! 字段名对不上时**界面不报错**：选项要么永远不出现，要么永远出现，
//! 用户只会觉得"这个功能坏了"，很难查。

use std::fs;
use std::path::PathBuf;

fn ui_file(name: &str) -> String {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("ui");
    p.push(name);
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("读 {}: {e}", p.display()))
}

#[test]
fn frontend_reads_the_setting_options_field() {
    let js = ui_file("app.js");
    assert!(
        js.contains("setting_options"),
        "前端应读后端给的 setting_options 字段"
    );
}

#[test]
fn frontend_only_offers_setting_mode_when_it_is_feasible() {
    let js = ui_file("app.js");
    assert!(
        js.contains("const canSetting = settingOptions.length > 0;"),
        "「系统设置」选项必须按可行组合的数量来给，不能无条件出现"
    );
    assert!(
        js.contains("if (canSetting) modePairs.push(['setting'"),
        "不可行时不应把「系统设置」加进下拉选项"
    );
}

#[test]
fn frontend_explains_why_setting_mode_is_unavailable() {
    let js = ui_file("app.js");
    assert!(
        js.contains("inline.setting_unavailable"),
        "不可用时应给出说明（用户看不到这个选项，得知道为什么）"
    );
    assert!(
        js.contains("inline.setting_available"),
        "可用时也应有一句说明"
    );
}

#[test]
fn mock_backend_provides_the_same_field() {
    let js = ui_file("app.js");
    assert!(
        js.contains("setting_options: e.setting_options || []"),
        "mock 后端也要提供 setting_options，否则 mock 模式与真机行为不一致"
    );
}

#[test]
fn both_locales_define_the_new_keys() {
    let js = ui_file("i18n.js");
    for key in ["inline.setting_available", "inline.setting_unavailable"] {
        let hits = js.matches(&format!("'{key}'")).count();
        assert_eq!(hits, 2, "{key} 应在中英文各出现一次，实得 {hits} 次");
    }
}

#[test]
fn backend_view_carries_the_field() {
    // 后端视图里必须有这个字段 —— 前端按它过滤选项，缺了就会永远不给选项。
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.push("sl-core");
    p.push("src");
    p.push("usecase");
    p.push("view.rs");
    let src = fs::read_to_string(&p).unwrap_or_else(|e| panic!("读 {}: {e}", p.display()));
    assert!(
        src.contains("pub setting_options: Vec<SettingOption>"),
        "RuleView / PresetView 必须带 setting_options 字段"
    );
}
