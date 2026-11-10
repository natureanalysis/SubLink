//! 用例层单元测试。
//!
//! 原先位于 `sl-gui/src/api.rs`（界面业务层）。逻辑下沉到 core 后随之迁移 ——
//! 这样这些用例测试不再需要 GUI crate，`cargo test -p sl-core` 即可覆盖。

use std::fs;
use std::path::PathBuf;

use super::presets::{load_user_presets, merge_presets, validate_preset, PRESETS_JSON};
use super::rules::validate_form;
use super::*;
use crate::error::SlError;
use crate::store::Store;

struct Env {
    base: PathBuf,
    store: Store,
    source: PathBuf,
    ram: PathBuf,
}

fn setup(tag: &str) -> Env {
    let base = std::env::temp_dir().join(format!("sl_gui_{tag}_{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let source = base.join("cache");
    let ram = base.join("ram");
    fs::create_dir_all(&source).unwrap();
    fs::create_dir_all(&ram).unwrap();
    fs::write(source.join("a.bin"), vec![1u8; 1024]).unwrap();

    let store = Store::open_in_memory().unwrap();
    store
        .insert_root("RAM", &ram.to_string_lossy(), true, 5000)
        .unwrap();
    Env {
        base,
        store,
        source,
        ram,
    }
}

fn form(env: &Env, name: &str, sub: &str) -> RuleForm {
    RuleForm {
        name: name.into(),
        category: "dev".into(),
        source_template: env.source.to_string_lossy().into_owned(),
        target_root_id: 1,
        target_subpath: sub.into(),
        link_type: "junction".into(),
        redirect_mode: "link".into(),
        env_vars: Vec::new(),
        env_scope: "user".into(),
        want_enabled: true,
        rebuild_on_boot: true,
        notes: None,
        excludes: Vec::new(),
        auto_clean_policy: String::new(),
        auto_clean_limit_bytes: 0,
    }
}

#[test]
fn rule_form_wire_contract_is_snake_case() {
    // 这段 JSON 就是前端 app.js 里实际发出来的形状（与读视图同一种命名）。
    // 字段名一旦对不上，界面点「保存」就会报 missing field ...，
    // 而这在 mock 后端里完全测不出来（mock 不校验形状）——
    // 所以这条测试专门钉住"线上真实线格式"。
    let json = r#"{
            "name": "Chrome 缓存",
            "category": "browser",
            "source_template": "%LOCALAPPDATA%\\Google\\Chrome\\User Data\\Default\\Cache",
            "target_root_id": 1,
            "target_subpath": "browsers\\chrome-cache",
            "link_type": "junction",
            "want_enabled": true,
            "rebuild_on_boot": true,
            "notes": null,
            "excludes": ["sessions", "logs/old"],
            "auto_clean_policy": "empty",
            "auto_clean_limit_bytes": 2147483648
        }"#;
    let form: RuleForm = serde_json::from_str(json).expect("前端发送的表单必须能反序列化");
    assert_eq!(form.name, "Chrome 缓存");
    assert_eq!(form.category, "browser");
    assert_eq!(form.target_root_id, 1);
    assert_eq!(form.target_subpath, r"browsers\chrome-cache");
    assert_eq!(form.link_type, "junction");
    assert!(form.want_enabled);
    assert!(form.rebuild_on_boot);
    assert!(form.notes.is_none());
    assert_eq!(form.excludes, vec!["sessions", "logs/old"]);
    assert_eq!(form.auto_clean_policy, "empty");
    assert_eq!(form.auto_clean_limit_bytes, 2147483648);

    // 老前端不带 excludes / auto_clean_policy 字段时按"无排除 / 未启用"处理
    let legacy: RuleForm = serde_json::from_str(
        r#"{"name":"x","category":"dev","source_template":"C:\\T","target_root_id":1,
            "target_subpath":"x","link_type":"junction"}"#,
    )
    .unwrap();
    assert!(legacy.excludes.is_empty());
    assert!(legacy.auto_clean_policy.is_empty());
    assert_eq!(legacy.auto_clean_limit_bytes, 0, "缺字段时默认 0 = 不启用");
}

#[test]
fn apply_opts_wire_contract_is_snake_case() {
    let json = r#"{"dry_run":true,"allow_merge":false,"skip_lock_check":true,"best_effort":true,"sha256":false}"#;
    let opts: ApplyOpts = serde_json::from_str(json).expect("选项字段名必须一致");
    assert!(opts.dry_run);
    assert!(!opts.allow_merge);
    assert!(opts.skip_lock_check);
    assert!(opts.best_effort);
    assert!(!opts.sha256);

    // 缺字段时应有默认值（前端只传关心的那几个）
    let opts: ApplyOpts = serde_json::from_str("{}").unwrap();
    assert!(!opts.dry_run && !opts.allow_merge && !opts.skip_lock_check && !opts.best_effort && !opts.sha256);
}

#[test]
fn snapshot_reports_banners_and_stats() {
    let env = setup("snapshot");
    let s = snapshot(&env.store).unwrap();
    assert_eq!(s.stats.total, 0);
    assert_eq!(s.roots.len(), 1);
    assert_eq!(s.roots[0].state, "ready");
    assert!(
        !s.banners
            .iter()
            .any(|b| b.text.contains("还没有配置目标根")),
        "目标根就绪时不应出现「未配置」横幅：{:?}",
        s.banners
    );
    let _ = fs::remove_dir_all(&env.base);
}

#[test]
fn add_rule_then_apply_then_restore_through_api() {
    let env = setup("roundtrip");
    let id = add_rule(&env.store, &form(&env, "测试", "dev\\x")).unwrap();

    let snap = snapshot(&env.store).unwrap();
    assert_eq!(snap.stats.total, 1);
    assert_eq!(snap.rules[0].badge, "❌", "初始应是未链接");

    let opts = ApplyOpts::default();
    let mut calls = 0usize;
    let res = apply_rules(&env.store, &[id], &opts, &mut |i, total, name, _p| {
        calls += 1;
        assert_eq!(i, 0);
        assert_eq!(total, 1);
        assert!(!name.is_empty());
    });
    assert_eq!(res.len(), 1);
    assert!(res[0].ok, "{}", res[0].message);
    assert_eq!(res[0].action, "moved_and_linked");
    assert!(calls > 0);

    let snap = snapshot(&env.store).unwrap();
    assert_eq!(snap.rules[0].badge, "✅");
    assert_eq!(snap.stats.ok, 1);

    let res = restore_rules(&env.store, &[id], &opts, &mut |_, _, _, _| {});
    assert!(res[0].ok, "{}", res[0].message);
    let snap = snapshot(&env.store).unwrap();
    assert_eq!(snap.stats.plain, 1);
    let _ = fs::remove_dir_all(&env.base);
}

#[test]
fn update_rule_retargets_managed_link() {
    use crate::link::detect::{detect_link, LinkState};
    use crate::util;

    let env = setup("retarget");
    let id = add_rule(&env.store, &form(&env, "改目标", "dev\\old")).unwrap();
    let res = apply_rules(&env.store, &[id], &ApplyOpts::default(), &mut |_, _, _, _| {});
    assert!(res[0].ok, "{}", res[0].message);
    let old_target = env.ram.join("dev").join("old");
    assert!(old_target.join("a.bin").exists());

    // 只改 target：列表保存后应立即删掉源路径上的旧链接并重指向新目标。
    update_rule(&env.store, id, &form(&env, "改目标", "dev\\new")).unwrap();

    let new_target = env.ram.join("dev").join("new");
    assert!(new_target.is_dir(), "新目标目录应被创建");
    match detect_link(&env.source, Some(&new_target)) {
        LinkState::LinkOk {
            target,
            target_mismatch,
            ..
        } => {
            assert!(util::path_eq(&target, &new_target), "实际指向 {target:?}");
            assert!(!target_mismatch);
        }
        other => panic!("期望链接已重建到新目标，实际 {other:?}"),
    }
    // 拆链只删链接条目：旧目标里的内容必须原封不动。
    assert!(old_target.join("a.bin").exists(), "旧目标数据必须保留");
    assert_eq!(env.store.get_rule(id).unwrap().target_subpath, "dev\\new");

    // 改回旧目标：链接重新指回去，又能看到旧数据。
    update_rule(&env.store, id, &form(&env, "改目标", "dev\\old")).unwrap();
    assert_eq!(fs::read(env.source.join("a.bin")).unwrap().len(), 1024);
    let _ = fs::remove_dir_all(&env.base);
}

#[test]
fn update_rule_without_link_only_saves_target() {
    let env = setup("retarget_plain");
    let id = add_rule(&env.store, &form(&env, "未链接", "dev\\old")).unwrap();
    update_rule(&env.store, id, &form(&env, "未链接", "dev\\new")).unwrap();

    assert_eq!(env.store.get_rule(id).unwrap().target_subpath, "dev\\new");
    // 源还是真实目录，也没有凭空建出新目标 —— 之后走「应用」时才会搬运。
    assert!(fs::symlink_metadata(&env.source).unwrap().is_dir());
    assert!(!env.ram.join("dev").join("new").exists());
    let _ = fs::remove_dir_all(&env.base);
}

/// 同一测试里加多条规则时，源路径必须互不相同 —— 规则库对 `source_norm`
/// 有 UNIQUE 约束（同一个缓存目录配两条规则是错的），这是有意的。
fn form_at(env: &Env, name: &str, src: &str, sub: &str) -> RuleForm {
    let mut f = form(env, name, sub);
    f.source_template = src.to_string();
    f
}

#[test]
fn batch_apply_collects_failures_without_aborting() {
    let env = setup("batch");
    let good_src = env.base.join("cache-good");
    fs::create_dir_all(&good_src).unwrap();
    fs::write(good_src.join("f.bin"), b"x").unwrap();
    let good = add_rule(
        &env.store,
        &form_at(&env, "好的", &good_src.to_string_lossy(), "dev\\good"),
    )
    .unwrap();
    // 源在一个不存在的盘上 → 必然失败
    let bad = add_rule(
        &env.store,
        &form_at(&env, "坏的", r"Z:\Nope\Cache", "dev\\bad"),
    )
    .unwrap();

    let res = apply_rules(
        &env.store,
        &[bad, good],
        &ApplyOpts::default(),
        &mut |_, _, _, _| {},
    );
    assert_eq!(res.len(), 2);
    assert!(!res[0].ok, "坏规则应当失败");
    assert!(res[1].ok, "好规则不受影响：{}", res[1].message);
    let _ = fs::remove_dir_all(&env.base);
}

#[test]
fn acl_capture_and_clear() {
    let env = setup("acl");
    let id = add_rule(&env.store, &form(&env, "权限", "dev\\acl")).unwrap();

    let sddl = capture_acl(&env.store, id).unwrap();
    assert!(sddl.starts_with('D'), "SDDL 应以 D 开头：{sddl}");
    assert!(snapshot(&env.store).unwrap().rules[0].has_acl);

    clear_acl(&env.store, id).unwrap();
    assert!(!snapshot(&env.store).unwrap().rules[0].has_acl);
    let _ = fs::remove_dir_all(&env.base);
}

#[test]
fn form_validation_rejects_bad_input() {
    let env = setup("validate");
    let mut f = form(&env, "x", "dev\\x");
    f.name = "  ".into();
    assert!(add_rule(&env.store, &f).is_err());

    // 分类允许自定义（批量归类可直接新建），但空名 / 超长仍是非法输入。
    let mut f = form(&env, "x", "dev\\x");
    f.category = "   ".into();
    assert!(add_rule(&env.store, &f).is_err());

    let mut f = form(&env, "x", "dev\\x");
    f.category = "x".repeat(25);
    assert!(add_rule(&env.store, &f).is_err());

    let mut f = form(&env, "自定义分类", "dev\\custom");
    f.category = "自定义".into();
    assert!(add_rule(&env.store, &f).is_ok());

    let mut f = form(&env, "x", "  ");
    f.target_subpath = "   ".into();
    assert!(add_rule(&env.store, &f).is_err());

    let mut f = form(&env, "x", "dev\\x");
    f.link_type = "hardlink".into();
    assert!(add_rule(&env.store, &f).is_err());
    let _ = fs::remove_dir_all(&env.base);
}

#[test]
fn export_import_roundtrip_through_api() {
    let env = setup("portable");
    add_rule(&env.store, &form(&env, "导出测试", "dev\\p")).unwrap();
    let file = env.base.join("cfg.json");

    let (roots, rules) = export_config(&env.store, &file).unwrap();
    assert_eq!((roots, rules), (1, 1));

    let other = Store::open_in_memory().unwrap();
    let rep = import_config(&other, &file, None, false).unwrap();
    assert_eq!(rep.rules_created, 1);
    assert_eq!(rule_views(&other).unwrap()[0].name, "导出测试");
    let _ = fs::remove_dir_all(&env.base);
}

#[test]
fn hidden_state_survives_config_file_round_trip() {
    let env = setup("portable_hidden");
    let src = |d: &str| env.base.join(d).to_string_lossy().into_owned();
    let a = add_rule(
        &env.store,
        &form_at(&env, "藏起来", &src("hide"), "dev\\hide"),
    )
    .unwrap();
    add_rule(
        &env.store,
        &form_at(&env, "留着", &src("keep"), "dev\\keep"),
    )
    .unwrap();
    set_hidden(&env.store, &[a], true).unwrap();
    let file = env.base.join("cfg.json");

    let (_, rules) = export_config(&env.store, &file).unwrap();
    assert_eq!(rules, 2);
    // 导出的文件里确实带了隐藏标记（不是只在本机 settings 里）。
    let text = fs::read_to_string(&file).unwrap();
    let bundle: crate::portable::Bundle = serde_json::from_str(&text).unwrap();
    assert_eq!(bundle.rules.iter().filter(|r| r.hidden).count(), 1);

    let other = Store::open_in_memory().unwrap();
    let rep = import_config(&other, &file, None, false).unwrap();
    assert_eq!(rep.rules_created, 2);
    assert_eq!(rep.rules_hidden, 1);

    // 界面用的读视图：导入后隐藏态与导出时一致（同名那条被隐藏，另一条可见）。
    let hidden: Vec<String> = rule_views(&other)
        .unwrap()
        .into_iter()
        .filter(|v| v.hidden)
        .map(|v| v.name)
        .collect();
    assert_eq!(hidden, vec!["藏起来".to_string()]);
    assert_eq!(other.hidden_rule_ids().unwrap().len(), 1);
    let _ = fs::remove_dir_all(&env.base);
}

#[test]
fn inspect_source_flags_system_dirs_and_elevation() {
    let ins = inspect_source(r"C:\Windows\System32").unwrap();
    assert!(!ins.blocking.is_empty());

    let ins = inspect_source(r"C:\Program Files\Foo\Cache").unwrap();
    assert!(ins.requires_elevation);
    assert!(ins.blocking.is_empty());

    let ins = inspect_source(r"%NO_SUCH_VAR_XYZ%\cache").unwrap();
    assert_eq!(ins.unknown_vars.len(), 1);
}

#[test]
fn inspect_target_detects_occupancy() {
    let env = setup("target");
    let occupied = env.ram.join("dev").join("busy");
    fs::create_dir_all(&occupied).unwrap();
    fs::write(occupied.join("x"), b"1").unwrap();

    let t = inspect_target(&env.store, 1, "dev\\busy").unwrap();
    assert!(t.root_ready);
    assert!(t.occupied, "已有内容的目录应报占用");

    let t = inspect_target(&env.store, 1, "dev\\free").unwrap();
    assert!(!t.occupied);
    assert!(!t.exists);
    let _ = fs::remove_dir_all(&env.base);
}

#[test]
fn check_locks_on_clean_dir_is_clear() {
    let env = setup("locks");
    let id = add_rule(&env.store, &form(&env, "占用", "dev\\l")).unwrap();
    let report = check_locks(&env.store, id).unwrap();
    assert!(report.is_clear(), "{}", report.summary());
    assert_eq!(report.scanned, 1);
    let _ = fs::remove_dir_all(&env.base);
}

#[test]
fn boot_rebuild_through_api_recovers_from_ram_loss() {
    let env = setup("boot");
    let id = add_rule(&env.store, &form(&env, "开机", "dev\\b")).unwrap();
    apply_rules(
        &env.store,
        &[id],
        &ApplyOpts::default(),
        &mut |_, _, _, _| {},
    );
    // 模拟 RAM 盘重启
    fs::remove_dir_all(env.ram.join("dev")).unwrap();

    let report = boot_rebuild(&env.store).unwrap();
    assert_eq!(report.dirs_created, 1);
    assert_eq!(report.links_recovered, 1);
    assert!(!report.has_problem(), "{}", report.one_line());

    let snap = snapshot(&env.store).unwrap();
    assert_eq!(snap.rules[0].badge, "⚠️", "目标为空 → 警告徽标");
    assert_eq!(snap.stats.warn, 1);
    let _ = fs::remove_dir_all(&env.base);
}

#[test]
fn delete_rules_and_set_enabled() {
    let env = setup("enabled");
    let a = add_rule(
        &env.store,
        &form_at(
            &env,
            "A",
            &env.base.join("srcA").to_string_lossy(),
            "dev\\a",
        ),
    )
    .unwrap();
    let b = add_rule(
        &env.store,
        &form_at(
            &env,
            "B",
            &env.base.join("srcB").to_string_lossy(),
            "dev\\b",
        ),
    )
    .unwrap();
    assert_eq!(set_enabled(&env.store, &[b], false).unwrap(), 1);
    assert_eq!(snapshot(&env.store).unwrap().stats.enabled, 1);
    assert_eq!(delete_rules(&env.store, &[a]).unwrap(), 1);
    assert_eq!(snapshot(&env.store).unwrap().stats.total, 1);
    let _ = fs::remove_dir_all(&env.base);
}

/// 删除 → 撤销：规则原样回到列表，快照里的回收站计数也对得上。
///
/// 这是「撤销」按钮的端到端契约：误删一批，撤销一次把那批全带回来。
#[test]
fn delete_then_undo_restores_the_batch() {
    let env = setup("undo");
    let src = |d: &str| env.base.join(d).to_string_lossy().into_owned();
    let a = add_rule(&env.store, &form_at(&env, "A", &src("uA"), "dev\\a")).unwrap();
    let b = add_rule(&env.store, &form_at(&env, "B", &src("uB"), "dev\\b")).unwrap();

    // 删之前先改一条规则，撤销要回到"删除那一刻"的内容，而不是初始值。
    let mut form = form_at(&env, "A", &src("uA"), "dev\\a");
    form.notes = Some("改过的备注".into());
    update_rule(&env.store, a, &form).unwrap();

    assert_eq!(delete_rules(&env.store, &[a, b]).unwrap(), 2);

    // 快照：规则少了，撤销栈里有一条删除操作（两条规则）。
    let snap = snapshot(&env.store).unwrap();
    assert_eq!(snap.stats.total, 0);
    assert_eq!(snap.undo.pending, 2);
    assert_eq!(snap.undo.kind, "delete");
    assert_eq!(snap.undo.names.len(), 2);

    let out = undo_last(&env.store).unwrap();
    assert_eq!(out.restored.len(), 2, "一批删的要整批恢复");
    assert!(out.skipped.is_empty(), "不该有恢复失败的条目：{:?}", out.skipped);

    let snap = snapshot(&env.store).unwrap();
    assert_eq!(snap.stats.total, 2);
    // 删除那条撤销记录已经弹掉；更早那次「改备注」还留在栈里（后进先出），
    // 所以按钮仍然亮着 —— 再点一次会把那次修改也撤掉。
    assert_eq!(snap.undo.kind, "update");
    assert_eq!(snap.undo.pending, 1);

    // 内容回到了删除那一刻的状态。
    let rules = rule_views(&env.store).unwrap();
    let back = rules.iter().find(|v| v.id == a).unwrap();
    assert_eq!(back.notes.as_deref(), Some("改过的备注"));

    let _ = fs::remove_dir_all(&env.base);
}

/// 撤销栈空着时撤销不报错（是"没什么可撤销"，不是失败）。
#[test]
fn undo_on_empty_stack_is_a_noop() {
    let env = setup("undo_empty");
    let out = undo_last(&env.store).unwrap();
    assert!(out.restored.is_empty());
    assert!(out.skipped.is_empty());
    assert_eq!(snapshot(&env.store).unwrap().undo.pending, 0);
    let _ = fs::remove_dir_all(&env.base);
}

/// 改规则 → 撤销：字段回到改之前，快照里的撤销栈指向这次修改。
///
/// 「撤销」要能撤销**对规则的修改**，不只是删除 —— 这是这一条的契约。
#[test]
fn edit_then_undo_restores_the_previous_fields() {
    let env = setup("undo_edit");
    let src = |d: &str| env.base.join(d).to_string_lossy().into_owned();
    let a = add_rule(&env.store, &form_at(&env, "A", &src("eA"), "dev\\a")).unwrap();

    let mut form = form_at(&env, "A 改名", &src("eA"), "dev\\a");
    form.notes = Some("新备注".into());
    form.category = "browser".into();
    update_rule(&env.store, a, &form).unwrap();

    let snap = snapshot(&env.store).unwrap();
    assert_eq!(snap.undo.kind, "update", "栈顶应该是那次修改");
    assert_eq!(snap.undo.pending, 1);

    let out = undo_last(&env.store).unwrap();
    assert_eq!(out.restored.len(), 1);
    assert!(out.skipped.is_empty(), "不该有跳过项：{:?}", out.skipped);

    let back = rule_views(&env.store)
        .unwrap()
        .into_iter()
        .find(|v| v.id == a)
        .unwrap();
    assert_eq!(back.name, "A", "名字应改回去");
    assert_eq!(back.notes, None, "备注应改回去");
    assert_eq!(back.category, "dev", "分类应改回去");
    assert_eq!(snapshot(&env.store).unwrap().undo.pending, 0);

    let _ = fs::remove_dir_all(&env.base);
}

/// 改 → 再改 → 撤销：先撤后一次，再撤前一次（撤销栈是后进先出）。
#[test]
fn undo_walks_back_through_several_edits() {
    let env = setup("undo_edit_stack");
    let src = |d: &str| env.base.join(d).to_string_lossy().into_owned();
    let a = add_rule(&env.store, &form_at(&env, "A", &src("sA"), "dev\\a")).unwrap();

    let mut f1 = form_at(&env, "第一步", &src("sA"), "dev\\a");
    f1.notes = Some("第一步的备注".into());
    update_rule(&env.store, a, &f1).unwrap();

    let mut f2 = form_at(&env, "第二步", &src("sA"), "dev\\a");
    f2.notes = Some("第二步的备注".into());
    update_rule(&env.store, a, &f2).unwrap();

    // 撤后一次：回到第一步。
    undo_last(&env.store).unwrap();
    let back = rule_views(&env.store)
        .unwrap()
        .into_iter()
        .find(|v| v.id == a)
        .unwrap();
    assert_eq!(back.name, "第一步");
    assert_eq!(back.notes.as_deref(), Some("第一步的备注"));

    // 再撤一次：回到最初。
    undo_last(&env.store).unwrap();
    let back = rule_views(&env.store)
        .unwrap()
        .into_iter()
        .find(|v| v.id == a)
        .unwrap();
    assert_eq!(back.name, "A");
    assert_eq!(back.notes, None);

    let _ = fs::remove_dir_all(&env.base);
}

/// 开关切换也能撤销。
#[test]
fn undo_reverts_the_enable_switch() {
    let env = setup("undo_enable");
    let src = |d: &str| env.base.join(d).to_string_lossy().into_owned();
    let a = add_rule(&env.store, &form_at(&env, "A", &src("enA"), "dev\\a")).unwrap();
    assert_eq!(set_enabled(&env.store, &[a], false).unwrap(), 1);
    assert_eq!(snapshot(&env.store).unwrap().stats.enabled, 0);

    undo_last(&env.store).unwrap();
    assert_eq!(snapshot(&env.store).unwrap().stats.enabled, 1);

    let _ = fs::remove_dir_all(&env.base);
}

#[test]
fn set_category_moves_rules_and_accepts_new_category() {
    let env = setup("categorize");
    let src = |d: &str| env.base.join(d).to_string_lossy().into_owned();
    let a = add_rule(
        &env.store,
        &form_at(&env, "A", &src("catA"), "dev\\a"),
    )
    .unwrap();
    let b = add_rule(
        &env.store,
        &form_at(&env, "B", &src("catB"), "dev\\b"),
    )
    .unwrap();

    assert_eq!(set_category(&env.store, &[a, b], "游戏工具").unwrap(), 2);
    let rules = rule_views(&env.store).unwrap();
    assert_eq!(rules.iter().filter(|v| v.category == "游戏工具").count(), 2);

    // 新建分类要出现在快照的分类清单里，界面的筛选 / 下拉才能继续选到它。
    let snap = snapshot(&env.store).unwrap();
    assert!(snap.categories.iter().any(|c| c == "游戏工具"));

    // 空名 / 纯空白被拒绝。
    assert!(set_category(&env.store, &[a], "   ").is_err());

    let _ = fs::remove_dir_all(&env.base);
}

#[test]
fn hide_and_unhide_rules_round_trip() {
    let env = setup("hidden");
    let src = |d: &str| env.base.join(d).to_string_lossy().into_owned();
    let a = add_rule(
        &env.store,
        &form_at(&env, "藏起来", &src("hide"), "dev\\hide"),
    )
    .unwrap();
    let b = add_rule(
        &env.store,
        &form_at(&env, "留着", &src("keep"), "dev\\keep"),
    )
    .unwrap();

    // 初始都可见。
    assert!(hidden_rule_ids(&env.store).unwrap().is_empty());
    assert!(rule_views(&env.store).unwrap().iter().all(|v| !v.hidden));

    // 隐藏一条：视图里只有它 hidden，且隐藏只影响展示、规则本身照旧。
    assert_eq!(set_hidden(&env.store, &[a], true).unwrap(), 1);
    assert_eq!(hidden_rule_ids(&env.store).unwrap(), vec![a]);
    let views = rule_views(&env.store).unwrap();
    assert_eq!(views.iter().filter(|v| v.hidden).count(), 1);
    assert!(views.iter().find(|v| v.id == a).unwrap().hidden);
    assert!(!views.iter().find(|v| v.id == b).unwrap().hidden);
    // 不影响功能：两条规则都还在，也都没被关掉。
    let snap = snapshot(&env.store).unwrap();
    assert_eq!(snap.stats.total, 2);
    assert_eq!(snap.stats.enabled, 2);

    // 取消隐藏：恢复可见（可恢复显示）。
    assert_eq!(set_hidden(&env.store, &[a], false).unwrap(), 0);
    assert!(hidden_rule_ids(&env.store).unwrap().is_empty());
    assert!(rule_views(&env.store).unwrap().iter().all(|v| !v.hidden));

    // 不存在的 id 静默忽略。
    assert_eq!(set_hidden(&env.store, &[99999], true).unwrap(), 0);

    // 规则删除后残留的隐藏 id 会被自动剔除，不会留给将来同号的新规则。
    assert_eq!(set_hidden(&env.store, &[b], true).unwrap(), 1);
    delete_rules(&env.store, &[b]).unwrap();
    assert!(hidden_rule_ids(&env.store).unwrap().is_empty());

    let _ = fs::remove_dir_all(&env.base);
}

#[test]
fn duplicate_source_path_is_rejected() {
    let env = setup("dup");
    add_rule(&env.store, &form(&env, "第一条", "dev\\one")).unwrap();
    let err = add_rule(&env.store, &form(&env, "第二条", "dev\\two")).unwrap_err();
    assert!(
        err.to_string().contains("已存在指向同一路径的规则"),
        "{err}"
    );
    let _ = fs::remove_dir_all(&env.base);
}

// -----------------------------------------------------------------------
// 内置预设
// -----------------------------------------------------------------------

/// 造一条合法预设的捷径。
fn preset(name: &str, src: &str, sub: &str) -> PresetDef {
    PresetDef {
        name: name.into(),
        category: "dev".into(),
        source_template: src.into(),
        target_subpath: sub.into(),
        note: None,
        redirect_mode: "link".into(),
        env_vars: Vec::new(),
        env_scope: "user".into(),
    }
}

#[test]
fn builtin_presets_parse() {
    // 内嵌 JSON 解析失败会在 builtin_presets() 里 panic，这里先显式解析拿到好报错。
    let all: Vec<PresetDef> = serde_json::from_str(PRESETS_JSON).expect("presets.json 必须能解析");
    assert!(all.len() >= 20, "预设清单不应莫名缩水：{}", all.len());

    // 分类合法、名字/模板/子路径非空且唯一 —— 这几条任何一条坏了，
    // 表单按预设填充后保存就会报错，或两条规则撞 UNIQUE 约束。
    let mut seen_src = std::collections::HashSet::new();
    let mut seen_sub = std::collections::HashSet::new();
    for p in &all {
        validate_preset(p).unwrap_or_else(|e| panic!("内置预设「{}」非法：{e}", p.name));
        assert!(
            p.source_template.starts_with('%'),
            "预设 {} 应使用 %VAR% 模板以便跨机器：{}",
            p.name,
            p.source_template
        );
        assert!(
            seen_src.insert(p.source_template.to_lowercase()),
            "预设 {} 源模板重复",
            p.name
        );
        assert!(
            seen_sub.insert(p.target_subpath.to_lowercase()),
            "预设 {} 子路径重复",
            p.name
        );
    }
    assert!(
        all.iter()
            .any(|p| p.source_template == r"%LOCALAPPDATA%\Temp"),
        "任何 Windows 机器上都该有「用户 TEMP」预设兜底"
    );
}

#[test]
fn merge_presets_overrides_by_source_and_appends() {
    let builtin = vec![
        preset("A", r"%LOCALAPPDATA%\A\cache", r"dev\a"),
        preset("B", r"%LOCALAPPDATA%\B\cache", r"dev\b"),
    ];
    // 同源覆盖（改子路径/备注），新源追加
    let user = vec![
        preset("B 改", r"%LOCALAPPDATA%\B\Cache", r"dev\b2"), // 大小写不同也应命中
        preset("C", r"%LOCALAPPDATA%\C\cache", r"dev\c"),
    ];
    let merged = merge_presets(&builtin, &user);
    assert_eq!(merged.len(), 3);
    assert_eq!(merged[0].name, "A");
    assert_eq!(merged[1].name, "B 改");
    assert_eq!(merged[1].target_subpath, r"dev\b2");
    assert_eq!(merged[2].name, "C");
}

#[test]
fn load_user_presets_skips_bad_entries_not_whole_file() {
    let dir = std::env::temp_dir().join(format!("sl_gui_preset_user_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join("presets.json");

    // 文件不存在 → 空、无告警
    let (ok, skipped) = load_user_presets(&file);
    assert!(ok.is_empty() && skipped.is_empty());

    // 一好一坏 → 好的留下，坏的跳过（不会整个文件报废）
    fs::write(
        &file,
        r#"[{"name":"好","category":"dev","source_template":"%LOCALAPPDATA%\\X","target_subpath":"dev\\x","note":null},
                {"name":"坏","category":"nonsense","source_template":"%LOCALAPPDATA%\\Y","target_subpath":"dev\\y","note":null}]"#,
    )
    .unwrap();
    let (ok, skipped) = load_user_presets(&file);
    assert_eq!(ok.len(), 1);
    assert_eq!(ok[0].name, "好");
    assert_eq!(skipped.len(), 1, "坏条目要有跳过原因：{skipped:?}");

    // 整个文件语法坏 → 全部忽略，但不 panic
    fs::write(&file, "{oops").unwrap();
    let (ok, skipped) = load_user_presets(&file);
    assert!(ok.is_empty());
    assert_eq!(skipped.len(), 1);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn preset_rules_filters_by_existence() {
    let out = preset_rules();
    assert!(
        out.iter().all(|v| v.exists),
        "返回的预设必须全部存在：{:?}",
        out.iter()
            .filter(|v| !v.exists)
            .map(|v| &v.name)
            .collect::<Vec<_>>()
    );
    // %LOCALAPPDATA%\Temp 在任何 Windows 上都有 —— 至少「用户 TEMP」会在
    assert!(
        out.iter()
            .any(|v| v.source_template == r"%LOCALAPPDATA%\Temp"),
        "本机应至少命中「用户 TEMP」预设，实际命中 {} 条",
        out.len()
    );
    // 每条预设都能直接走表单校验（预设→添加规则 这条链路不能断）
    let env = setup("presets");
    for v in &out {
        let f = RuleForm {
            name: v.name.clone(),
            category: v.category.clone(),
            source_template: v.source_template.clone(),
            target_root_id: 1,
            target_subpath: v.target_subpath.clone(),
            link_type: "junction".into(),
            redirect_mode: "link".into(),
            env_vars: Vec::new(),
            env_scope: "user".into(),
            want_enabled: true,
            rebuild_on_boot: true,
            notes: v.note.clone(),
            excludes: Vec::new(),
            auto_clean_policy: String::new(),
            auto_clean_limit_bytes: 0,
        };
        validate_form(&f).unwrap_or_else(|e| panic!("预设 {} 没通过表单校验：{e}", v.name));
    }
    let _ = fs::remove_dir_all(&env.base);
}

// -----------------------------------------------------------------------
// 文件夹清理
// -----------------------------------------------------------------------

fn empty_policy() -> CleanPolicy {
    CleanPolicy::Empty
}

#[test]
fn clean_preview_counts_folder_contents() {
    let env = setup("cleanprev");
    let dir = env.base.join("toclean");
    fs::create_dir_all(dir.join("sub")).unwrap();
    fs::write(dir.join("a.bin"), b"12345").unwrap();
    fs::write(dir.join("sub/b.bin"), b"678").unwrap();

    let t = CleanTarget::Folder {
        path: dir.to_string_lossy().into_owned(),
    };
    let pv = clean_preview(&env.store, &t, &empty_policy(), &[]).unwrap();
    assert_eq!(pv.files, 2, "递归应数到子目录里的文件");
    assert_eq!(pv.bytes, 8);
    let _ = fs::remove_dir_all(&env.base);
}

#[test]
fn clean_run_empty_removes_files_but_keeps_root() {
    let env = setup("cleanrun");
    let dir = env.base.join("toclean");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("a.bin"), b"12345").unwrap();

    let t = CleanTarget::Folder {
        path: dir.to_string_lossy().into_owned(),
    };
    let r = clean_run(&env.store, &t, &empty_policy(), &[], false, &mut |_| {}).unwrap();
    assert_eq!(r.deleted_files, 1);
    assert!(dir.is_dir(), "清理后根目录必须保留");
    assert!(!dir.join("a.bin").exists());
    // 事件流水留痕（清理是破坏性操作，必须可回溯）
    let evs = recent_events(&env.store, 10).unwrap();
    assert!(evs.iter().any(|e| e.kind == "clean"), "清理要落事件流水");
    let _ = fs::remove_dir_all(&env.base);
}

#[test]
fn clean_blocks_system_folders() {
    let env = setup("cleanblock");
    let t = CleanTarget::Folder {
        path: r"C:\Windows".into(),
    };
    let err = clean_preview(&env.store, &t, &empty_policy(), &[]).unwrap_err();
    assert!(matches!(err, SlError::SafetyBlocked(_)), "{err}");
    let _ = fs::remove_dir_all(&env.base);
}

#[test]
fn clean_rule_target_resolves_to_source_when_unlinked() {
    let env = setup("cleanrule");
    // setup 造的 env.source 是普通目录，含 1 个 1024 字节的 a.bin；规则未链接 → 清源
    let id = add_rule(&env.store, &form(&env, "清理我", "dev\\clean")).unwrap();
    let t = CleanTarget::Rule { id };
    let pv = clean_preview(&env.store, &t, &empty_policy(), &[]).unwrap();
    assert_eq!(pv.files, 1);
    assert_eq!(pv.bytes, 1024);
    let _ = fs::remove_dir_all(&env.base);
}

#[test]
fn clean_rule_respects_excludes_end_to_end() {
    let env = setup("cleanexcl");
    // 源目录里混着要保护的子树
    fs::create_dir_all(env.source.join("sessions")).unwrap();
    fs::write(env.source.join("sessions/db.sqlite"), b"keep").unwrap();
    fs::write(env.source.join("trash.bin"), b"gone").unwrap();

    let mut f = form(&env, "带排除", "dev\\excl");
    f.excludes = vec!["sessions".into()];
    let id = add_rule(&env.store, &f).unwrap();

    let t = CleanTarget::Rule { id };
    let pv = clean_preview(&env.store, &t, &empty_policy(), &[]).unwrap();
    assert_eq!(pv.files, 2, "只数到未排除的文件（a.bin + trash.bin）");
    assert!(pv.excluded_skipped >= 1, "预演要如实汇报排除计数");

    let r = clean_run(&env.store, &t, &empty_policy(), &[], false, &mut |_| {}).unwrap();
    assert_eq!(r.deleted_files, 2);
    assert!(
        env.source.join("sessions/db.sqlite").exists(),
        "排除子树必须保留"
    );
    assert!(!env.source.join("trash.bin").exists());
    let _ = fs::remove_dir_all(&env.base);
}

#[test]
fn add_rule_rejects_invalid_excludes() {
    let env = setup("badexcl");
    let mut f = form(&env, "坏排除", "dev\\bad");
    f.excludes = vec!["a/../..".into()];
    let err = add_rule(&env.store, &f).unwrap_err();
    assert!(matches!(err, SlError::InvalidPath(_)), "{err}");

    let mut f2 = form(&env, "空排除", "dev\\empty-pat");
    f2.excludes = vec!["  ".into()];
    assert!(add_rule(&env.store, &f2).is_err());
    let _ = fs::remove_dir_all(&env.base);
}

#[test]
fn clean_keeps_requested_file_types_end_to_end() {
    let env = setup("cleantypes");
    fs::write(env.source.join("script.py"), b"print(1)").unwrap();
    fs::write(env.source.join("junk.bin"), b"gone").unwrap();
    let id = add_rule(&env.store, &form(&env, "保留 py", "dev\\keep-types")).unwrap();
    let t = CleanTarget::Rule { id };
    let types = vec![".py".to_string()];

    let pv = clean_preview(&env.store, &t, &empty_policy(), &types).unwrap();
    assert_eq!(pv.files, 2, "setup 的 a.bin + junk.bin（script.py 被保留）");
    assert_eq!(pv.excluded_skipped, 1, "被保留的类型要如实计数");

    let r = clean_run(&env.store, &t, &empty_policy(), &types, false, &mut |_| {}).unwrap();
    assert_eq!(r.deleted_files, 2);
    assert!(
        env.source.join("script.py").exists(),
        "声明保留的类型必须活着"
    );
    assert!(!env.source.join("junk.bin").exists(), "其余文件照常清理");
    let _ = fs::remove_dir_all(&env.base);
}

#[test]
fn clean_rejects_bad_type_excludes() {
    let env = setup("cleanbadtype");
    let id = add_rule(&env.store, &form(&env, "坏类型", "dev\\bad-type")).unwrap();
    let t = CleanTarget::Rule { id };
    let err = clean_preview(&env.store, &t, &empty_policy(), &["tar.gz".to_string()]).unwrap_err();
    assert!(matches!(err, SlError::InvalidPath(_)), "{err}");
    let _ = fs::remove_dir_all(&env.base);
}

#[test]
fn clean_preview_on_missing_dir_is_empty_not_error() {
    let env = setup("cleanmissing");
    let t = CleanTarget::Folder {
        path: env
            .base
            .join("never-existed")
            .to_string_lossy()
            .into_owned(),
    };
    let pv = clean_preview(&env.store, &t, &empty_policy(), &[]).unwrap();
    assert_eq!(pv.files, 0, "清理一个还没内容的目录是常见的，不该报错");
    let _ = fs::remove_dir_all(&env.base);
}

#[test]
fn disk_usage_scans_rule_dir_and_volumes() {
    let env = setup("diskusage");
    add_rule(&env.store, &form(&env, "缓存", "ram\\cache")).unwrap();
    let mut seen_total = 0usize;
    let view = disk_usage(&env.store, &mut |_i, total, _name| {
        seen_total = total;
    })
    .unwrap();
    assert_eq!(view.rule_count, 1);
    assert_eq!(seen_total, 1, "应按规则条数回吐进度");
    assert_eq!(view.rules.len(), 1);
    // setup 里 source/a.bin 是 1024 字节的真实文件，未链接目录直接量源。
    assert_eq!(view.rules[0].bytes, 1024);
    assert_eq!(view.rules[0].files, 1);
    assert!(view.rules[0].exists);
    assert!(view.total_bytes >= 1024);
    assert!(!view.volumes.is_empty(), "至少应统计到一个卷的容量");
    assert!(view.volumes[0].total >= view.volumes[0].free);
    let _ = fs::remove_dir_all(&env.base);
}
#[test]
fn water_poll_records_events_only_on_level_change() {
    let env = setup("water");
    // setup 的根指向 temp 下真实目录 → 所在系统盘真实水位，默认阈值下 < 85%。
    // 用 settings 覆盖阈值来驱动级别变化，不依赖真盘实际占用。

    // 首次巡检：建立基线，ok 不落事件
    let levels = water_poll(&env.store).unwrap();
    assert_eq!(levels.len(), 1);
    assert_eq!(levels[0].level, "ok", "系统盘默认水位应低于 85%");
    assert!(levels[0].used_pct > 0.0, "水位来自真实卷容量");
    assert!(
        env.store.recent_events(10).unwrap().is_empty(),
        "首次基线 ok 不落事件"
    );

    // 压低告警线 → 级别 ok→warn，落一条 warn 事件
    env.store.set_setting("water_warn_pct", "0.5").unwrap();
    let levels = water_poll(&env.store).unwrap();
    assert_eq!(levels[0].level, "warn");
    let ev = env.store.recent_events(10).unwrap();
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].level, "warn");
    assert_eq!(ev[0].kind, "water");
    assert!(ev[0].rule_id.is_none(), "水位是根的问题，不挂在规则上");

    // 平稳期：级别不变 → 不刷事件
    let _ = water_poll(&env.store).unwrap();
    assert_eq!(
        env.store.recent_events(10).unwrap().len(),
        1,
        "平稳期不落事件"
    );

    // 抬高告警线 → 降级恢复，落一条 info 事件
    env.store.set_setting("water_warn_pct", "99.9").unwrap();
    let levels = water_poll(&env.store).unwrap();
    assert_eq!(levels[0].level, "ok");
    let ev = env.store.recent_events(10).unwrap();
    assert_eq!(ev.len(), 2);
    assert_eq!(
        ev[0].level, "info",
        "降级恢复落 info（recent_events 最新的在前）"
    );
    let _ = fs::remove_dir_all(&env.base);
}

#[test]
fn snapshot_banners_warn_on_high_water_level() {
    let env = setup("waterbanner");
    // 默认水位 ok → 快照无水位横幅
    let snap = snapshot(&env.store).unwrap();
    assert!(!snap.banners.iter().any(|b| b.text.contains("RAM 盘水位")));

    // 压低告警线 → 横幅出现（快照只读，不落事件）
    env.store.set_setting("water_warn_pct", "0.5").unwrap();
    let snap = snapshot(&env.store).unwrap();
    let hit: Vec<_> = snap
        .banners
        .iter()
        .filter(|b| b.text.contains("水位偏高"))
        .collect();
    assert_eq!(hit.len(), 1);
    assert_eq!(hit[0].level, "warn");
    assert_eq!(
        hit[0].action.as_deref(),
        Some("sweep"),
        "水位横幅要带一键清理入口"
    );
    assert!(
        env.store.recent_events(10).unwrap().is_empty(),
        "snapshot 只读，事件只能由 water_poll 落"
    );
    let _ = fs::remove_dir_all(&env.base);
}

#[test]
fn water_critical_runs_preauthorized_auto_clean() {
    let env = setup("waterauto");
    // 规则 A（配了预授权 empty）与规则 B（未授权）用不同源目录。
    let mut fa = form(&env, "预授权A", "dev\\auto-a");
    fa.auto_clean_policy = "empty".into();
    let id_a = add_rule(&env.store, &fa).unwrap();
    let dir_b = env.base.join("cache-b");
    fs::create_dir_all(&dir_b).unwrap();
    fs::write(dir_b.join("b.bin"), b"keep-b").unwrap();
    let mut fb = form(&env, "普通B", "dev\\auto-b");
    fb.source_template = dir_b.to_string_lossy().into_owned();
    let _id_b = add_rule(&env.store, &fb).unwrap();

    // 压低严重线 → 巡检时 critical → 自动清 A（未链接 → 清源目录的 a.bin）
    env.store.set_setting("water_critical_pct", "0.5").unwrap();
    let levels = water_poll(&env.store).unwrap();
    assert_eq!(levels[0].level, "critical");
    assert!(
        !env.source.join("a.bin").exists(),
        "预授权规则的文件应被自动清掉"
    );
    assert!(dir_b.join("b.bin").exists(), "未授权规则绝不动");

    // 事件：水位 error + 规则A的 clean + auto_clean 汇总
    let ev = env.store.recent_events(10).unwrap();
    assert!(ev.iter().any(|e| e.kind == "water" && e.level == "error"));
    assert!(ev
        .iter()
        .any(|e| e.kind == "auto_clean" && e.message.contains("1 条预授权规则")));
    assert!(ev
        .iter()
        .any(|e| e.kind == "clean" && e.rule_id == Some(id_a)));

    // 冷却：补一个文件再巡检 → 30 分钟冷却期内不重复触发
    fs::write(env.source.join("a2.bin"), b"again").unwrap();
    let _ = water_poll(&env.store).unwrap();
    assert!(env.source.join("a2.bin").exists(), "冷却期内不重复自动清理");
    let _ = fs::remove_dir_all(&env.base);
}

#[test]
fn add_rule_rejects_invalid_auto_clean_policy() {
    let env = setup("waterbad");
    let mut f = form(&env, "坏策略", "dev\\bad-policy");
    f.auto_clean_policy = "older:abc".into();
    let err = add_rule(&env.store, &f).unwrap_err();
    assert!(matches!(err, SlError::InvalidPath(_)), "{err}");

    let mut f2 = form(&env, "空策略OK", "dev\\ok-policy");
    f2.auto_clean_policy = String::new();
    assert!(add_rule(&env.store, &f2).is_ok(), "空串 = 未启用，应通过");
    let _ = fs::remove_dir_all(&env.base);
}
// ---------------------------------------------------------------------------
// 拖拽排序（reorder_rules）
// ---------------------------------------------------------------------------

fn ids_in_order(store: &Store, category: &str) -> Vec<i64> {
    store
        .list_rules()
        .unwrap()
        .iter()
        .filter(|r| r.category == category)
        .map(|r| r.id)
        .collect()
}

#[test]
fn reorder_rules_reorders_within_category() {
    let env = setup("reorder");
    let src = |d: &str| env.base.join(d).to_string_lossy().into_owned();
    let a = add_rule(&env.store, &form_at(&env, "A", &src("ca"), "a")).unwrap();
    let b = add_rule(&env.store, &form_at(&env, "B", &src("cb"), "b")).unwrap();
    let c = add_rule(&env.store, &form_at(&env, "C", &src("cc"), "c")).unwrap();
    assert_eq!(
        ids_in_order(&env.store, "dev"),
        vec![a, b, c],
        "初始按插入序"
    );

    let n = reorder_rules(&env.store, &[c, a, b]).unwrap();
    assert_eq!(n, 3);
    assert_eq!(ids_in_order(&env.store, "dev"), vec![c, a, b]);
    let _ = fs::remove_dir_all(&env.base);
}

#[test]
fn reorder_rules_rejects_cross_category_duplicate_and_partial() {
    let env = setup("reorderbad");
    let src = |d: &str| env.base.join(d).to_string_lossy().into_owned();
    let a = add_rule(&env.store, &form_at(&env, "A", &src("ca"), "a")).unwrap();
    let b = add_rule(&env.store, &form_at(&env, "B", &src("cb"), "b")).unwrap();
    let mut other = form_at(&env, "X", &src("cx"), "x");
    other.category = "browser".into();
    let x = add_rule(&env.store, &other).unwrap();

    // 跨分类
    let err = reorder_rules(&env.store, &[a, x]).unwrap_err();
    assert!(err.to_string().contains("同一分类"), "{err}");
    // 重复 id
    let err = reorder_rules(&env.store, &[a, a, b]).unwrap_err();
    assert!(err.to_string().contains("不止一次"), "{err}");
    // 子集（不完整覆盖）—— 筛选视图下前端只发可见子集时要被拦下
    let err = reorder_rules(&env.store, &[a]).unwrap_err();
    assert!(err.to_string().contains("全部规则"), "{err}");
    // 不存在的 id
    let err = reorder_rules(&env.store, &[a, b, 999]).unwrap_err();
    assert!(matches!(err, SlError::NotFound(_)), "{err}");
    // 空列表 = 无操作
    assert_eq!(reorder_rules(&env.store, &[]).unwrap(), 0);
    // 以上全部失败后顺序未被破坏
    assert_eq!(ids_in_order(&env.store, "dev"), vec![a, b]);
    let _ = fs::remove_dir_all(&env.base);
}
// ---------------------------------------------------------------------------
// 定时清理（schedule.rs）
// ---------------------------------------------------------------------------

#[test]
fn scheduled_clean_runs_once_per_day_at_configured_time() {
    let env = setup("sched");
    // 规则 A（预授权 empty）与规则 B（未授权）用不同源目录 —— 与水位自动清理
    // 测试同一套装置思路：未授权规则绝不能被动到。
    let mut fa = form(&env, "预授权A", "dev\\sched-a");
    fa.auto_clean_policy = "empty".into();
    let _id_a = add_rule(&env.store, &fa).unwrap();
    let dir_b = env.base.join("cache-b");
    fs::create_dir_all(&dir_b).unwrap();
    fs::write(dir_b.join("b.bin"), b"keep-b").unwrap();
    let mut fb = form(&env, "普通B", "dev\\sched-b");
    fb.source_template = dir_b.to_string_lossy().into_owned();
    let _id_b = add_rule(&env.store, &fb).unwrap();

    let now = |h: i64, m: i64| super::schedule::LocalNow {
        date: "2026-10-16".into(),
        secs: h * 3600 + m * 60,
    };

    // 默认关闭 → 到点也不跑（破坏性操作必须显式 opt-in）
    let v = super::schedule::scheduled_clean_at(&env.store, &now(3, 0)).unwrap();
    assert!(!v.ran);

    // 启用但还没到点（02:59 < 03:00）→ 不跑
    env.store
        .set_setting(super::schedule::KEY_ENABLED, "1")
        .unwrap();
    let v = super::schedule::scheduled_clean_at(&env.store, &now(2, 59)).unwrap();
    assert!(!v.ran);

    // 到点 → 跑：A 清掉、B 不动，落 last_run
    let v = super::schedule::scheduled_clean_at(&env.store, &now(3, 0)).unwrap();
    assert!(v.ran);
    assert_eq!(v.done, 1);
    assert_eq!(v.deleted_files, 1);
    assert!(
        !env.source.join("a.bin").exists(),
        "预授权规则的文件应被清掉"
    );
    assert!(dir_b.join("b.bin").exists(), "未授权规则绝不动");
    let ev = env.store.recent_events(10).unwrap();
    assert!(ev
        .iter()
        .any(|e| e.kind == "scheduled_clean" && e.level == "info"));

    // 时刻改成 05:00，但今天已跑过 → 不重复
    env.store
        .set_setting(super::schedule::KEY_TIME, "05:00")
        .unwrap();
    let v = super::schedule::scheduled_clean_at(&env.store, &now(12, 0)).unwrap();
    assert!(!v.ran, "每自然日最多一次");

    // 时刻格式坏 → 跳过并落 warn（手改库才会发生）
    env.store
        .set_setting(super::schedule::KEY_TIME, "99:xx")
        .unwrap();
    let v = super::schedule::scheduled_clean_at(&env.store, &now(23, 0)).unwrap();
    assert!(!v.ran);
    let ev = env.store.recent_events(10).unwrap();
    assert!(ev
        .iter()
        .any(|e| e.kind == "scheduled_clean" && e.level == "warn"));

    let _ = fs::remove_dir_all(&env.base);
}
