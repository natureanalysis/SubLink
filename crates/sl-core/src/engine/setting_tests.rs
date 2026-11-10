//! 「系统设置模式」的单元测试。
//!
//! ## 为什么不在这里真的改注册表
//!
//! 本工具的白名单只允许改 `TEMP` / `TMP` / `LOCALAPPDATA` 三个变量 ——
//! 它们是**用户真实在用的**环境变量。真去改它们会把测试跑成"用户的临时目录
//! 被测试程序改了"，而且 `cargo test` 的多进程并行会让这种测试极难收敛。
//!
//! 所以这里的测试覆盖的是**不碰注册表也能验证的部分**：
//!
//! * 变量名白名单与作用域解析（`sysenv` 单元测试）；
//! * 规则库里新列的读写与迁移（`store` 单元测试）；
//! * 分流判定：什么算「系统设置模式」、什么需要提权（`engine` 单元测试）；
//! * 表单校验：非法变量名 / 非法作用域必须被挡下（`usecase` 单元测试）。
//!
//! 真正读写注册表的路径（`RegSetValueExW` / `RegQueryValueExW` /
//! `WM_SETTINGCHANGE` 广播）由 `sysenv` 模块的单元测试覆盖到"读不存在的
//! 变量返回 None"这一层 —— 它不写入任何值。

use crate::engine;
use crate::store::{NewRule, Rule, Store, REDIRECT_MODE_LINK, REDIRECT_MODE_SETTING};
use crate::sysenv::{self, EnvScope};

/// 造一条「系统设置模式」的规则。
fn setting_rule(name: &str, src: &str, sub: &str, scope: &str, vars: &[&str]) -> NewRule {
    NewRule {
        name: name.into(),
        category: "system".into(),
        source_template: src.into(),
        target_root_id: 1,
        target_subpath: sub.into(),
        link_type: "junction".into(),
        redirect_mode: REDIRECT_MODE_SETTING.into(),
        env_vars: vars.iter().map(|s| s.to_string()).collect(),
        env_scope: scope.into(),
        want_enabled: true,
        rebuild_on_boot: true,
        notes: None,
        excludes: Vec::new(),
        auto_clean_policy: String::new(),
        auto_clean_limit_bytes: 0,
    }
}

fn link_rule(name: &str, src: &str, sub: &str) -> NewRule {
    NewRule {
        redirect_mode: REDIRECT_MODE_LINK.into(),
        env_vars: Vec::new(),
        env_scope: "user".into(),
        ..setting_rule(name, src, sub, "user", &[])
    }
}

// ---------------------------------------------------------------------
// 规则库：新列的读写
// ---------------------------------------------------------------------

#[test]
fn setting_rule_round_trips_through_store() {
    let store = Store::open_in_memory().unwrap();
    store.insert_root("RAM", "R:", true, 90000).unwrap();

    let id = store
        .insert_rule(&setting_rule(
            "用户 TEMP",
            r"C:\Users\a\AppData\Local\Temp",
            "system\\user-temp",
            "user",
            &["TEMP", "TMP"],
        ))
        .unwrap();

    let rule = store.get_rule(id).unwrap();
    assert_eq!(rule.redirect_mode, REDIRECT_MODE_SETTING);
    assert_eq!(rule.env_vars, vec!["TEMP".to_string(), "TMP".to_string()]);
    assert_eq!(rule.env_scope, "user");
    // link_type 在这个模式下不生效，但仍应保持原值（不污染别的字段）。
    assert_eq!(rule.link_type, "junction");
}

#[test]
fn link_rule_defaults_to_link_mode() {
    let store = Store::open_in_memory().unwrap();
    store.insert_root("RAM", "R:", true, 90000).unwrap();
    let id = store
        .insert_rule(&link_rule("Chrome", r"C:\Users\a\AppData\Local\Cache", "browsers\\chrome"))
        .unwrap();
    let rule = store.get_rule(id).unwrap();
    assert_eq!(rule.redirect_mode, REDIRECT_MODE_LINK);
    assert!(rule.env_vars.is_empty());
    assert_eq!(rule.env_scope, "user");
    // 用户级作用域不需要提权。
    assert!(!rule.needs_elevation);
}

#[test]
fn machine_scope_marks_rule_as_needing_elevation() {
    let store = Store::open_in_memory().unwrap();
    store.insert_root("RAM", "R:", true, 90000).unwrap();
    // 源路径是普通目录（不需要提权），但机器级作用域要写 HKLM → 需要提权。
    let id = store
        .insert_rule(&setting_rule(
            "系统 TEMP",
            r"C:\Windows\Temp",
            "system\\windows-temp",
            "machine",
            &["TEMP", "TMP"],
        ))
        .unwrap();
    assert!(
        store.get_rule(id).unwrap().needs_elevation,
        "机器级环境变量写在 HKLM 下，必须标记为需要提权，否则既不弹 UAC 也写不进去"
    );
}

#[test]
fn switching_to_machine_scope_updates_elevation_flag() {
    let store = Store::open_in_memory().unwrap();
    store.insert_root("RAM", "R:", true, 90000).unwrap();
    let id = store
        .insert_rule(&setting_rule(
            "用户 TEMP",
            r"C:\Users\a\AppData\Local\Temp",
            "system\\user-temp",
            "user",
            &["TEMP"],
        ))
        .unwrap();
    assert!(!store.get_rule(id).unwrap().needs_elevation);

    // 改成机器级 → 提权标记必须跟着变（否则改了作用域也走不上提权流程）。
    let mut nr = setting_rule(
        "用户 TEMP",
        r"C:\Users\a\AppData\Local\Temp",
        "system\\user-temp",
        "machine",
        &["TEMP"],
    );
    nr.target_root_id = 1;
    store.update_rule(id, &nr).unwrap();
    assert!(store.get_rule(id).unwrap().needs_elevation);

    // 改回用户级 → 标记也要跟着回落。
    nr.env_scope = "user".into();
    store.update_rule(id, &nr).unwrap();
    assert!(!store.get_rule(id).unwrap().needs_elevation);
}

// ---------------------------------------------------------------------
// 分流判定
// ---------------------------------------------------------------------

#[test]
fn mode_detection_routes_to_the_right_pipeline() {
    let store = Store::open_in_memory().unwrap();
    store.insert_root("RAM", "R:", true, 90000).unwrap();

    let link_id = store
        .insert_rule(&link_rule("Chrome", r"C:\Users\a\AppData\Local\Cache", "browsers\\chrome"))
        .unwrap();
    let setting_id = store
        .insert_rule(&setting_rule(
            "用户 TEMP",
            r"C:\Users\a\AppData\Local\Temp",
            "system\\user-temp",
            "user",
            &["TEMP", "TMP"],
        ))
        .unwrap();

    let link_rule_row = store.get_rule(link_id).unwrap();
    let setting_rule_row = store.get_rule(setting_id).unwrap();

    assert!(!engine::is_setting_mode(&link_rule_row));
    assert!(engine::is_setting_mode(&setting_rule_row));
}

#[test]
fn machine_scope_rule_requires_elevation_even_for_normal_source() {
    let store = Store::open_in_memory().unwrap();
    store.insert_root("RAM", "R:", true, 90000).unwrap();

    // 源路径是用户目录下（不受保护），但作用域是机器级 → 仍需提权。
    let id = store
        .insert_rule(&setting_rule(
            "机器级 TEMP",
            r"C:\Users\a\AppData\Local\Temp",
            "system\\user-temp",
            "machine",
            &["TEMP"],
        ))
        .unwrap();
    let rule = store.get_rule(id).unwrap();
    assert!(engine::rule_requires_elevation(&store, &rule));

    // 用户级 → 不需要提权。
    let mut nr = setting_rule(
        "机器级 TEMP",
        r"C:\Users\a\AppData\Local\Temp",
        "system\\user-temp",
        "user",
        &["TEMP"],
    );
    nr.target_root_id = 1;
    store.update_rule(id, &nr).unwrap();
    let rule = store.get_rule(id).unwrap();
    assert!(!engine::rule_requires_elevation(&store, &rule));
}

// ---------------------------------------------------------------------
// 变量名白名单与作用域
// ---------------------------------------------------------------------

#[test]
fn only_whitelisted_vars_are_accepted() {
    assert!(sysenv::is_supported_var("TEMP"));
    assert!(sysenv::is_supported_var("TMP"));
    // 绝不能放行 PATH 这类关键变量 —— 改错会直接把用户的系统搞坏。
    assert!(!sysenv::is_supported_var("PATH"));
    assert!(!sysenv::is_supported_var("PATHEXT"));
    assert!(!sysenv::is_supported_var("COMSPEC"));
    // LOCALAPPDATA 也不放行：它由 Shell 计算、不在注册表的环境变量里，
    // 且装的是书签密码这类不可再生的用户数据（详见 sysenv::is_supported_var）。
    assert!(!sysenv::is_supported_var("LOCALAPPDATA"));
}

#[test]
fn normalize_vars_rejects_path_and_dedupes() {
    let v = sysenv::normalize_vars(&["temp".into(), "TEMP".into(), "tmp".into()]).unwrap();
    assert_eq!(v, vec!["TEMP".to_string(), "TMP".to_string()]);
    assert!(sysenv::normalize_vars(&["PATH".into()]).is_err());
    assert!(sysenv::normalize_vars(&[]).is_err());
}

#[test]
fn scope_parsing_and_elevation() {
    assert_eq!(EnvScope::parse("user"), Some(EnvScope::User));
    assert_eq!(EnvScope::parse("HKCU"), Some(EnvScope::User));
    assert_eq!(EnvScope::parse("machine"), Some(EnvScope::Machine));
    assert_eq!(EnvScope::parse("HKLM"), Some(EnvScope::Machine));
    assert_eq!(EnvScope::parse("bogus"), None);

    // 用户级写 HKCU，自己就能写；机器级写 HKLM，要管理员。
    assert!(!EnvScope::User.requires_elevation());
    assert!(EnvScope::Machine.requires_elevation());
}

// ---------------------------------------------------------------------
// 表单校验：非法输入必须被挡在入库之前
// ---------------------------------------------------------------------

#[test]
fn form_validation_rejects_bad_setting_fields() {
    use crate::usecase::view::RuleForm;

    let mut form = RuleForm {
        name: "TEMP".into(),
        category: "system".into(),
        source_template: r"C:\Users\a\AppData\Local\Temp".into(),
        target_root_id: 1,
        target_subpath: "system\\user-temp".into(),
        link_type: "junction".into(),
        redirect_mode: REDIRECT_MODE_SETTING.into(),
        env_vars: vec!["TEMP".into()],
        env_scope: "user".into(),
        want_enabled: true,
        rebuild_on_boot: true,
        notes: None,
        excludes: Vec::new(),
        auto_clean_policy: String::new(),
        auto_clean_limit_bytes: 0,
    };

    // 合法组合通过。
    assert!(crate::usecase::rules::validate_form(&form).is_ok());

    // 非白名单变量名被拒。
    form.env_vars = vec!["PATH".into()];
    assert!(crate::usecase::rules::validate_form(&form).is_err());

    // 空变量列表被拒。
    form.env_vars = Vec::new();
    assert!(crate::usecase::rules::validate_form(&form).is_err());

    // 非法作用域被拒。
    form.env_vars = vec!["TEMP".into()];
    form.env_scope = "bogus".into();
    assert!(crate::usecase::rules::validate_form(&form).is_err());

    // 非法重定向方式被拒。
    form.env_scope = "user".into();
    form.redirect_mode = "weird".into();
    assert!(crate::usecase::rules::validate_form(&form).is_err());
}

#[test]
fn link_mode_ignores_setting_fields() {
    use crate::usecase::view::RuleForm;

    // 链接模式下 env_vars / env_scope 不参与校验 —— 空值也合法。
    let form = RuleForm {
        name: "Chrome".into(),
        category: "browser".into(),
        source_template: r"C:\Users\a\AppData\Local\Cache".into(),
        target_root_id: 1,
        target_subpath: "browsers\\chrome".into(),
        link_type: "junction".into(),
        redirect_mode: REDIRECT_MODE_LINK.into(),
        env_vars: Vec::new(),
        env_scope: "user".into(),
        want_enabled: true,
        rebuild_on_boot: true,
        notes: None,
        excludes: Vec::new(),
        auto_clean_policy: String::new(),
        auto_clean_limit_bytes: 0,
    };
    assert!(crate::usecase::rules::validate_form(&form).is_ok());
}

// ---------------------------------------------------------------------
// 导入导出：新字段必须随配置一起走
// ---------------------------------------------------------------------

#[test]
fn export_import_carries_setting_fields() {
    use crate::portable::{export_bundle, import_bundle};

    let src = Store::open_in_memory().unwrap();
    src.insert_root("RAM", "R:", true, 90000).unwrap();
    src.insert_rule(&setting_rule(
        "用户 TEMP",
        r"C:\Users\a\AppData\Local\Temp",
        "system\\user-temp",
        "machine",
        &["TEMP", "TMP"],
    ))
    .unwrap();

    let bundle = export_bundle(&src).unwrap();
    assert_eq!(bundle.rules[0].redirect_mode, REDIRECT_MODE_SETTING);
    assert_eq!(bundle.rules[0].env_vars, vec!["TEMP".to_string(), "TMP".to_string()]);
    assert_eq!(bundle.rules[0].env_scope, "machine");

    let dst = Store::open_in_memory().unwrap();
    let rep = import_bundle(&dst, &bundle, None, false).unwrap();
    assert_eq!(rep.rules_created, 1);
    let imported = &dst.list_rules().unwrap()[0];
    assert_eq!(imported.redirect_mode, REDIRECT_MODE_SETTING);
    assert_eq!(imported.env_vars, vec!["TEMP".to_string(), "TMP".to_string()]);
    assert_eq!(imported.env_scope, "machine");
}

#[test]
fn old_bundle_without_setting_fields_imports_as_link_mode() {
    use crate::portable::{import_bundle, Bundle, BundleRoot, BundleRule};

    // 手工构造一份"旧格式"配置（没有 redirect_mode / env_vars / env_scope）。
    let json = r#"{
      "schema": 1,
      "app": "SubLink",
      "exported_at": "2026-01-01",
      "roots": [
        { "ref": "r0", "label": "RAM", "path": "R:", "is_ramdisk": true, "wait_timeout_ms": 90000 }
      ],
      "rules": [
        { "name": "Chrome", "category": "browser", "source_template": "C:\\Temp\\Chrome",
          "root_ref": "r0", "target_subpath": "browsers\\chrome", "link_type": "junction",
          "want_enabled": true, "rebuild_on_boot": true }
      ]
    }"#;
    let bundle: Bundle = serde_json::from_str(json).unwrap();
    // 反序列化时缺字段 → 走默认值（link 模式）。
    assert_eq!(bundle.rules[0].redirect_mode, REDIRECT_MODE_LINK);
    assert!(bundle.rules[0].env_vars.is_empty());
    assert_eq!(bundle.rules[0].env_scope, "user");

    let dst = Store::open_in_memory().unwrap();
    let rep = import_bundle(&dst, &bundle, None, false).unwrap();
    assert_eq!(rep.rules_created, 1);
    let imported = &dst.list_rules().unwrap()[0];
    assert_eq!(imported.redirect_mode, REDIRECT_MODE_LINK);

    // 构造器本身也要能编译（BundleRoot / BundleRule 字段齐全）。
    let _ = BundleRoot {
        root_ref: "r0".into(),
        label: "RAM".into(),
        path: "R:".into(),
        is_ramdisk: true,
        wait_timeout_ms: 90000,
    };
    let _ = BundleRule {
        name: "x".into(),
        category: "other".into(),
        source_template: "C:\\x".into(),
        root_ref: "r0".into(),
        target_subpath: "x".into(),
        link_type: "junction".into(),
        redirect_mode: REDIRECT_MODE_LINK.into(),
        env_vars: Vec::new(),
        env_scope: "user".into(),
        want_enabled: true,
        rebuild_on_boot: true,
        acl_sddl: None,
        notes: None,
        excludes: Vec::new(),
        auto_clean_policy: String::new(),
        auto_clean_limit_bytes: 0,
        hidden: false,
    };
}

// ---------------------------------------------------------------------
// 状态检测：变量指向哪儿决定状态
// ---------------------------------------------------------------------

#[test]
fn setting_state_detects_mismatch_when_vars_point_elsewhere() {
    // 变量指向一个既不是目标、也不是"完全没设置"的路径 → 状态不匹配。
    // 用一个几乎不可能被设置、但也不会指向目标的值来构造。
    let target = std::path::Path::new(r"R:\system\user-temp");
    let st = engine::setting_state(EnvScope::User, &["LOCALAPPDATA".to_string()], target);
    // LOCALAPPDATA 在任何 Windows 上都存在，且不等于我们的目标 → 不匹配。
    assert!(
        matches!(st, crate::link::detect::LinkState::SettingMismatch { .. }),
        "{st:?}"
    );
    // 不匹配是需要用户处理的状态。
    assert!(st.needs_attention());
}

#[test]
fn setting_state_reports_empty_target_distinctly() {
    // TEMP 指向目标、但目标目录不存在（RAM 盘刚重启）→ "已生效但目标为空"。
    let base = std::env::temp_dir().join(format!("td_setting_state_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let target = base.join("target");
    std::fs::create_dir_all(&target).unwrap();

    // TEMP 当前指向别处 → 不匹配（用 LOCALAPPDATA 保证不等于 target）。
    let st = engine::setting_state(EnvScope::User, &["LOCALAPPDATA".to_string()], &target);
    assert!(matches!(
        st,
        crate::link::detect::LinkState::SettingMismatch { .. }
    ));

    let _ = std::fs::remove_dir_all(&base);
}

// ---------------------------------------------------------------------
// 回归护栏：存量规则里的非法变量不能让整个体检崩掉
// ---------------------------------------------------------------------

#[test]
fn stale_rule_with_removed_var_degrades_to_state_not_error() {
    // 这条护栏对应一个真实风险：
    // 白名单里的变量被移出后（LOCALAPPDATA），**存量规则**里可能还留着它。
    // 若体检时直接报错冒泡，整个 status / doctor 都会失败 ——
    // 一条坏规则不该拖垮全部体检。
    //
    // 期望：这条规则单独显示为「配置无效」状态，其余规则照常体检。
    let store = Store::open_in_memory().unwrap();
    store.insert_root("RAM", "R:", true, 90000).unwrap();

    // 直接写库，绕过表单校验（模拟旧版本存进来的规则）。
    let stale_id = store
        .insert_rule(&NewRule {
            env_vars: vec!["LOCALAPPDATA".into()],
            ..setting_rule(
                "旧规则",
                r"C:\Users\a\AppData\Local\OldTemp",
                "system\\old",
                "user",
                &[],
            )
        })
        .unwrap();
    let good_id = store
        .insert_rule(&setting_rule(
            "正常规则",
            r"C:\Users\a\AppData\Local\Temp",
            "system\\ok",
            "user",
            &["TEMP"],
        ))
        .unwrap();

    // 坏规则单独体检 → 报出配置无效，而不是让调用方炸掉。
    let stale = store.get_rule(stale_id).unwrap();
    let st = engine::rule_status(&store, &stale).expect("坏规则不该让体检函数整体失败");
    assert!(
        matches!(
            st.state,
            crate::link::detect::LinkState::ConfigInvalid { .. }
        ),
        "应为配置无效状态：{:?}",
        st.state
    );
    assert!(st.needs_attention, "配置无效需要用户处理");
    assert_eq!(st.badge, "❌");

    // 关键：批量体检里，坏规则不能拖垮好的规则。
    let all = engine::status_all(&store, None, false, None, false)
        .expect("批量体检不应因一条坏规则而失败");
    assert_eq!(all.len(), 2, "两条规则都应出现在结果里");
    let good = all.iter().find(|s| s.rule_id == good_id).expect("好规则应在结果里");
    assert!(
        !matches!(
            good.state,
            crate::link::detect::LinkState::ConfigInvalid { .. }
        ),
        "好规则不该被标成配置无效"
    );

    let _ = store.get_rule(stale_id).unwrap();
}

#[test]
fn setting_rule_reads_scope_and_vars_from_row() {    let store = Store::open_in_memory().unwrap();
    store.insert_root("RAM", "R:", true, 90000).unwrap();
    let id = store
        .insert_rule(&setting_rule(
            "用户 TEMP",
            r"C:\Users\a\AppData\Local\Temp",
            "system\\user-temp",
            "user",
            &["TEMP", "TMP"],
        ))
        .unwrap();
    let rule: Rule = store.get_rule(id).unwrap();
    assert_eq!(engine::rule_scope(&rule).unwrap(), EnvScope::User);
    assert_eq!(
        engine::rule_vars(&rule).unwrap(),
        vec!["TEMP".to_string(), "TMP".to_string()]
    );
}

// ---------------------------------------------------------------------
// 回归护栏：原值必须持久化，不能在 restore 时现场读
// ---------------------------------------------------------------------

#[test]
fn env_prev_is_persisted_and_cleared() {
    // 这条护栏对应一个真实缺陷：
    // restore 时若**现场**读注册表当"原值"，读到的已经是"指向目标"的值，
    // 写回去等于什么都没还原（变量仍然指向 RAM 盘）。
    // 原值必须在 apply 之前存进规则库。
    use crate::engine::{
        env_prev_key_for_test, load_env_prev_for_test, save_env_prev_for_test, VarSnapshotForTest,
    };

    let store = Store::open_in_memory().unwrap();
    let rule_id = 42i64;

    // 没有记录时读出来是 None。
    assert!(load_env_prev_for_test(&store, rule_id).unwrap().is_none());

    // 写入一条记录（模拟 apply 时存的原值）。
    let snaps = vec![VarSnapshotForTest {
        name: "TEMP".into(),
        original_raw: Some("%LOCALAPPDATA%\\Temp".into()),
    }];
    save_env_prev_for_test(&store, rule_id, &snaps).unwrap();

    let loaded = load_env_prev_for_test(&store, rule_id).unwrap().expect("应能读回记录");
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].name, "TEMP");
    assert_eq!(
        loaded[0].original_raw.as_deref(),
        Some("%LOCALAPPDATA%\\Temp"),
        "读回的原值必须与写入时逐字相同（否则还原会把路径改坏）"
    );

    // 重复写入不应覆盖已有记录 —— 否则第二次 apply 会把"目标路径"当成原值存下，
    // 之后还原就永远回不去了。
    let snaps2 = vec![VarSnapshotForTest {
        name: "TEMP".into(),
        original_raw: Some("R:\\target".into()),
    }];
    save_env_prev_for_test(&store, rule_id, &snaps2).unwrap();
    let loaded = load_env_prev_for_test(&store, rule_id).unwrap().expect("记录仍在");
    assert_eq!(
        loaded[0].original_raw.as_deref(),
        Some("%LOCALAPPDATA%\\Temp"),
        "已有记录时不得被覆盖"
    );

    // 键名按规则 id 分开 —— 两条规则不能串味。
    assert_eq!(env_prev_key_for_test(1), "env_prev:1");
    assert_eq!(env_prev_key_for_test(2), "env_prev:2");
}

// ---------------------------------------------------------------------
// 回归护栏：开机重建的分流与授权
// ---------------------------------------------------------------------

#[test]
fn boot_rebuild_never_asks_for_elevation_on_setting_rules() {
    // 这条护栏对应一个真实缺陷：
    // 开机重建按"整体执行要不要提权"分流，把「系统设置模式 + 机器级作用域」
    // 的规则判成"需要提权"。但开机重建在 setting 模式下**不碰注册表**
    // （环境变量是持久值，重启不丢），只建 RAM 盘上的目标目录 —— 这一步
    // 根本不需要管理员权限。
    // 后果：普通登录任务（--skip-elevated）把它跳过了，而高权限登录任务
    // 默认没注册 → 开机后目标目录不再重建，环境变量指向不存在的目录，
    // 新进程写临时文件直接失败。
    let store = Store::open_in_memory().unwrap();
    store.insert_root("RAM", "R:", true, 90000).unwrap();

    let id = store
        .insert_rule(&setting_rule(
            "机器级 TEMP",
            r"C:\Users\a\AppData\Local\Temp",
            "system\\user-temp",
            "machine",
            &["TEMP"],
        ))
        .unwrap();
    let rule = store.get_rule(id).unwrap();

    // apply / restore 仍要提权（写 HKLM）。
    assert!(
        engine::rule_requires_elevation(&store, &rule),
        "机器级作用域写 HKLM，apply / restore 必须提权"
    );
    // 但开机重建不需要 —— 它不写注册表。
    assert!(
        !engine::boot_requires_elevation(&store, &rule),
        "开机重建只建目录、不碰注册表，不该被判成需要提权"
    );

    // 链接模式下受保护源仍然要提权（要在 C:\Windows\... 建链接）。
    let win = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
    let link_id = store
        .insert_rule(&link_rule("受保护源", &format!("{win}\\Temp\\td-boot"), "system\\protected"))
        .unwrap();
    let link_row = store.get_rule(link_id).unwrap();
    assert!(
        engine::boot_requires_elevation(&store, &link_row),
        "链接模式下受保护源在开机重建时确实需要提权"
    );
}

#[test]
fn best_effort_only_applies_to_protected_sources() {
    // 尽力模式（拷不动的条目跳过、放弃占用检测）只该给**受保护的源**用。
    // 「系统设置模式 + 机器级作用域」要提权是因为写 HKLM，跟搬数据的
    // 严格程度无关 —— 用户级 TEMP 规则的源目录是普通目录，不该因此
    // 放弃占用检测与逐条校验。
    let store = Store::open_in_memory().unwrap();
    store.insert_root("RAM", "R:", true, 90000).unwrap();

    let id = store
        .insert_rule(&setting_rule(
            "机器级 TEMP",
            r"C:\Users\a\AppData\Local\Temp",
            "system\\user-temp",
            "machine",
            &["TEMP"],
        ))
        .unwrap();
    let rule = store.get_rule(id).unwrap();
    assert!(
        !engine::source_requires_elevation(&store, &rule),
        "源目录在用户目录下，不受保护"
    );
    assert!(
        engine::rule_requires_elevation(&store, &rule),
        "但整体执行仍要提权（写 HKLM）"
    );
}

// ---------------------------------------------------------------------
// 回归护栏：合并有条目读不出来时，绝不能删源
// ---------------------------------------------------------------------

#[test]
fn setting_merge_failure_keeps_source_directory() {
    // 这条护栏对应一个真实缺陷：
    // setting 模式在「目标目录非空 → 合并」这条路径上，把
    // `merge_into_ex` 的 failed 条目**整个忽略**了，直接删源。
    // 那些条目根本没进目标，副本不存在 —— 删源就是丢数据。
    // 链接模式一直有这道检查（apply.rs），setting 模式漏了。
    //
    // 直接测 `merge_into_target`（不碰注册表）：锁住源里的一个文件
    // （模拟被别的程序占用），让合并读不出它，再断言源目录必须还在。
    use std::fs;

    let base = std::env::temp_dir().join(format!("td_setmerge_{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let source = base.join("src");
    let target = base.join("dst");
    fs::create_dir_all(&source).unwrap();
    fs::create_dir_all(&target).unwrap();
    fs::write(source.join("free.txt"), b"free").unwrap();
    fs::write(source.join("busy.txt"), b"busy").unwrap();
    // 目标非空 → 走合并路径。
    fs::write(target.join("existing.txt"), b"old").unwrap();

    let h = lock_no_share(&source.join("busy.txt"));

    // 尽力模式：拷不动的条目跳过 —— 正是会触发 `failed` 非空的路径。
    let mut noop = |_: crate::fsops::mover::Progress| {};
    let res = crate::engine::merge_into_target(&source, &target, true, &mut noop);

    unlock(h);

    // 合并读不出条目 → 必须报错，绝不能把源删掉。
    assert!(
        res.is_err(),
        "合并有条目读不出来时必须报错，不能当成功：{:?}",
        res.as_ref().map(|s| s.summary())
    );
    assert!(
        source.exists(),
        "源目录必须保留 —— 那些条目没有副本，删源就是丢数据"
    );
    assert!(
        source.join("busy.txt").exists(),
        "读不出来的源文件必须原地保留"
    );
    assert!(
        source.join("free.txt").exists(),
        "源里能拷走的条目也不该在合并失败时被删掉"
    );

    let _ = fs::remove_dir_all(&base);
}

/// 用不允许任何共享的句柄锁住文件，模拟"被别的程序打开"。
fn lock_no_share(path: &std::path::Path) -> windows_sys::Win32::Foundation::HANDLE {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{CreateFileW, OPEN_EXISTING};
    let wide = crate::util::to_wide(&crate::util::to_verbatim(path).to_string_lossy());
    let h = unsafe {
        CreateFileW(
            wide.as_ptr(),
            0x8000_0000, // GENERIC_READ
            0,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    assert_ne!(h, INVALID_HANDLE_VALUE, "锁文件失败");
    h
}

fn unlock(h: windows_sys::Win32::Foundation::HANDLE) {
    unsafe {
        windows_sys::Win32::Foundation::CloseHandle(h);
    }
}
