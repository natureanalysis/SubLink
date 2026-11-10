//! 「系统设置模式」可实现性的判定测试。
//!
//! ## 这条判定为什么重要
//!
//! 界面上的「重定向方式」下拉里，「系统设置」这个选项**只对能落地的规则出现**。
//! 判据是：源路径正好是某个 `TEMP` / `TMP` 当前指向的目录 —— 因为这个模式
//! 改的是注册表里的环境变量值，变量指向别处就改不动（apply 时会以
//! `source_mismatch` 被安全检查拒掉）。
//!
//! 判定一旦写错，用户就会拿到一个**点下去必然失败**的选项，而且报错在 apply
//! 阶段才出现，很难回溯到"当初那个下拉框就不该给我这个选项"。
//!
//! 这里不真的改注册表（理由见 `setting_tests.rs` 的模块说明）：只测纯比较逻辑，
//! 注册表那一层由 `sysenv` 的单元测试覆盖到"读不存在的变量返回 None"。

use std::path::Path;

use crate::engine;

/// 一个变量的当前值是不是指向给定源目录。
#[test]
fn var_points_at_accepts_the_same_path() {
    assert!(
        engine::var_points_at(r"C:\Users\me\AppData\Local\Temp", Path::new(r"C:\Users\me\AppData\Local\Temp")),
        "完全相同的路径应当匹配"
    );
}

#[test]
fn var_points_at_ignores_case_and_trailing_separator() {
    // Windows 路径大小写不敏感，末尾的 `\` 也不影响同一个目录。
    assert!(
        engine::var_points_at(
            r"c:\users\ME\appdata\local\temp\",
            Path::new(r"C:\Users\me\AppData\Local\Temp")
        ),
        "大小写与末尾分隔符的差异不应影响判定"
    );
}

#[test]
fn var_points_at_rejects_a_different_directory() {
    assert!(
        !engine::var_points_at(r"C:\Windows\Temp", Path::new(r"C:\Users\me\AppData\Local\Temp")),
        "不同的目录不应当匹配"
    );
}

#[test]
fn var_points_at_rejects_an_empty_value() {
    // 注册表里存在但值是空串（少见但合法）—— 视为"没指向任何地方"。
    assert!(
        !engine::var_points_at("   ", Path::new(r"C:\Users\me\AppData\Local\Temp")),
        "空值不应当匹配任何目录"
    );
}

/// 源路径不是任何变量的指向时，没有可行组合 —— 界面因此不显示该选项。
///
/// 这条不依赖本机注册表状态：一个**谁也不会指向**的路径必然匹配不上。
#[test]
fn no_options_for_a_path_nothing_points_at() {
    let opts = engine::setting_options_for_source(Path::new(
        r"Z:\definitely-not-a-temp-dir-8f2c1a",
    ));
    assert!(
        opts.is_empty(),
        "没有任何变量指向的路径不应有可行组合，实得 {opts:?}"
    );
}

/// 可行组合里，作用域与变量名必须成套出现 —— 前端要靠它预填下拉框。
#[test]
fn options_carry_scope_and_vars_together() {
    for opt in engine::setting_options_for_source(Path::new(
        r"Z:\definitely-not-a-temp-dir-8f2c1a",
    )) {
        assert!(
            opt.scope == "user" || opt.scope == "machine",
            "作用域只能是 user / machine，实得 {}",
            opt.scope
        );
        assert!(
            !opt.vars.is_empty(),
            "有可行组合就必须带上变量名，否则前端无从预填"
        );
        for v in &opt.vars {
            assert!(
                v == "TEMP" || v == "TMP",
                "变量名必须在白名单内，实得 {v}"
            );
        }
    }
}

/// 白名单只有 TEMP / TMP —— 这条是「系统设置」模式的入口，
/// 加变量必须同时改判定逻辑，不能只改白名单。
#[test]
fn supported_vars_are_only_temp_and_tmp() {
    assert_eq!(crate::sysenv::SUPPORTED_VARS, ["TEMP", "TMP"]);
    assert!(crate::sysenv::is_supported_var("temp"));
    assert!(crate::sysenv::is_supported_var(" TMP "));
    assert!(!crate::sysenv::is_supported_var("LOCALAPPDATA"));
    assert!(!crate::sysenv::is_supported_var("PATH"));
}
