//! engine 的集成级测试：真实文件系统 + 内存库，覆盖 apply / restore / relink 的
//! 状态机路径与回滚语义。

use super::*;
use crate::error::SlError;
use crate::fsops::mover::Progress;
use crate::fsops::verify::VerifyMode;
use crate::link::detect::{LinkState, LinkType, TargetState};
use crate::store::{NewRule, Store};
use std::fs;
use std::path::PathBuf;

struct Env {
    base: PathBuf,
    store: Store,
    rule_id: i64,
    source: PathBuf,
    target: PathBuf,
}

fn noop(_: Progress) {}

/// 构造一个「RAM 盘就是本地某个目录」的测试环境。
fn setup(tag: &str) -> Env {
    let base = std::env::temp_dir().join(format!("td_engine_{tag}_{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let source = base.join("cache");
    let ram = base.join("ram");
    fs::create_dir_all(&source).unwrap();
    fs::create_dir_all(&ram).unwrap();
    fs::write(source.join("a.txt"), b"aaa").unwrap();
    fs::create_dir_all(source.join("sub")).unwrap();
    fs::write(source.join("sub").join("b.bin"), vec![9u8; 2048]).unwrap();

    let store = Store::open_in_memory().unwrap();
    let root_id = store
        .insert_root("RAM", &ram.to_string_lossy(), true, 90000)
        .unwrap();
    let rule_id = store
        .insert_rule(&NewRule {
            name: "测试缓存".into(),
            category: "dev".into(),
            source_template: source.to_string_lossy().into_owned(),
            target_root_id: root_id,
            target_subpath: "dev\\test-cache".into(),
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
        })
        .unwrap();
    let target = ram.join("dev").join("test-cache");

    Env {
        base,
        store,
        rule_id,
        source,
        target,
    }
}

#[test]
fn apply_then_status_then_restore() {
    let env = setup("roundtrip");
    let rule = env.store.get_rule(env.rule_id).unwrap();

    let st = rule_status(&env.store, &rule).unwrap();
    assert!(matches!(st.state, LinkState::PlainDir), "{:?}", st.state);

    // dry-run 不做任何改动
    let dry = apply_rule(
        &env.store,
        &rule,
        &ApplyOptions {
            dry_run: true,
            ..Default::default()
        },
        &mut noop,
    )
    .unwrap();
    assert!(dry.dry_run);
    assert!(env.source.join("a.txt").exists(), "dry-run 不应动文件");

    // 正式应用
    let out = apply_rule(&env.store, &rule, &ApplyOptions::default(), &mut noop).unwrap();
    assert_eq!(out.action, "moved_and_linked");
    assert_eq!(out.stats.unwrap().files, 2);

    let st = rule_status(&env.store, &rule).unwrap();
    assert_eq!(st.badge, "✅");
    assert!(matches!(
        st.state,
        LinkState::LinkOk {
            target_state: TargetState::HasContent,
            ..
        }
    ));
    assert_eq!(fs::read(env.source.join("a.txt")).unwrap(), b"aaa");
    assert_eq!(
        fs::read(env.target.join("sub").join("b.bin")).unwrap().len(),
        2048
    );

    // 幂等
    let again = apply_rule(&env.store, &rule, &ApplyOptions::default(), &mut noop).unwrap();
    assert_eq!(again.action, "already_ok");

    // 恢复
    let back = restore_rule(&env.store, &rule, &ApplyOptions::default(), &mut noop).unwrap();
    assert_eq!(back.action, "restored");
    let st = rule_status(&env.store, &rule).unwrap();
    assert!(matches!(st.state, LinkState::PlainDir), "{:?}", st.state);
    assert_eq!(fs::read(env.source.join("a.txt")).unwrap(), b"aaa");
    assert!(!env.target.exists(), "恢复后目标内容应被搬走");

    // 再恢复一次：幂等
    let again = restore_rule(&env.store, &rule, &ApplyOptions::default(), &mut noop).unwrap();
    assert_eq!(again.action, "already_plain");

    let _ = fs::remove_dir_all(&env.base);
}

#[test]
fn apply_recovers_after_ram_loss() {
    let env = setup("ramloss");
    let rule = env.store.get_rule(env.rule_id).unwrap();
    apply_rule(&env.store, &rule, &ApplyOptions::default(), &mut noop).unwrap();

    // 模拟 RAM 盘重启：目标目录整个消失
    fs::remove_dir_all(&env.target).unwrap();
    let st = rule_status(&env.store, &rule).unwrap();
    assert!(matches!(st.state, LinkState::LinkBroken { .. }), "{:?}", st.state);
    assert_eq!(st.badge, "⚠️");

    let out = apply_rule(&env.store, &rule, &ApplyOptions::default(), &mut noop).unwrap();
    assert_eq!(out.action, "fixed_broken_target");
    let st = rule_status(&env.store, &rule).unwrap();
    assert!(matches!(
        st.state,
        LinkState::LinkOk {
            target_state: TargetState::Empty,
            ..
        }
    ));

    let _ = fs::remove_dir_all(&env.base);
}

/// **同卷重命名之后必须重新授权** —— 回归用例。
///
/// 同卷搬运走的是 `move_tree_ex` 的重命名快路径：源目录被整个改名到目标位置，
/// 目录**连同它的安全描述符一起被换掉**。搬运前给目标目录的授权会跟着旧目录
/// 一起消失，目标目录最终继承的是源目录的权限 —— 如果源目录不是使用者建的
/// （管理员、别的进程、或从别处继承来的），使用者就写不进去了。
///
/// 所以 `apply` 在搬运之后必须再授权一次。这条用例把"源目录对使用者没有完全控制"
/// 这个前提摆出来：一旦有人把搬运后的那次授权删掉，它立刻会红。
#[test]
fn apply_regrants_after_same_volume_rename() {
    let env = setup("renameacl");
    let rule = env.store.get_rule(env.rule_id).unwrap();
    let sid = crate::acl::current_user_sid().unwrap();

    // 造一个"使用者只读 + 可改名，但没有完全控制"的目录：
    // 这正是"这个目录是别人建的"那种形态（0x1300A9 = 读 + 执行 + 删除）。
    let restricted = format!("D:P(A;OICI;FA;;;SY)(A;OICI;0x1300A9;;;{sid})");
    crate::acl::apply_sddl(&env.source, &restricted).unwrap();
    fs::create_dir_all(&env.target).unwrap();
    crate::acl::apply_sddl(&env.target, &restricted).unwrap();
    assert!(
        !crate::acl::has_full_control(&env.source, &sid).unwrap(),
        "前置条件：源目录此刻对使用者没有完全控制"
    );

    // 正式应用：源与目标同卷 → 走重命名快路径
    let out = apply_rule(&env.store, &rule, &ApplyOptions::default(), &mut noop).unwrap();
    assert_eq!(out.action, "moved_and_linked");
    assert!(
        env.target.join("a.txt").exists(),
        "内容应落在目标目录里"
    );

    // 关键断言：搬运后目标目录必须重新拿到使用者的完全控制
    assert!(
        crate::acl::has_full_control(&env.target, &sid).unwrap(),
        "同卷重命名后目标目录必须重新授权：{}",
        crate::acl::export_sddl(&env.target).unwrap()
    );

    // 链接条目自己也要有（链接条目的 DACL 与目标目录是两回事）
    assert!(
        crate::acl::has_full_control_on_link(&env.source, &sid).unwrap(),
        "链接条目必须对使用者完全控制"
    );

    // 落到实际效果：使用者能往目标目录里写
    fs::write(env.target.join("after-apply.bin"), b"ok").unwrap();

    let _ = fs::remove_dir_all(&env.base);
}

#[test]
fn merge_into_non_empty_target() {
    let env = setup("merge");
    let rule = env.store.get_rule(env.rule_id).unwrap();
    fs::create_dir_all(&env.target).unwrap();
    fs::write(env.target.join("old.txt"), b"old").unwrap();

    // 默认拒绝
    let err = apply_rule(&env.store, &rule, &ApplyOptions::default(), &mut noop).unwrap_err();
    assert!(matches!(err, SlError::SafetyBlocked(_)), "{err}");

    // 允许合并
    let out = apply_rule(
        &env.store,
        &rule,
        &ApplyOptions {
            allow_merge: true,
            ..Default::default()
        },
        &mut noop,
    )
    .unwrap();
    assert_eq!(out.action, "merged_and_linked");
    assert!(env.target.join("old.txt").exists(), "原目标文件应保留");
    assert_eq!(fs::read(env.source.join("a.txt")).unwrap(), b"aaa");
    assert!(matches!(
        rule_status(&env.store, &rule).unwrap().state,
        LinkState::LinkOk { .. }
    ));

    let _ = fs::remove_dir_all(&env.base);
}

#[test]
fn restore_empty_target_creates_plain_dir() {
    let env = setup("restoreempty");
    let rule = env.store.get_rule(env.rule_id).unwrap();
    apply_rule(&env.store, &rule, &ApplyOptions::default(), &mut noop).unwrap();
    fs::remove_file(env.target.join("a.txt")).unwrap();
    fs::remove_file(env.target.join("sub").join("b.bin")).unwrap();
    fs::remove_dir(env.target.join("sub")).unwrap();

    let out = restore_rule(&env.store, &rule, &ApplyOptions::default(), &mut noop).unwrap();
    assert_eq!(out.action, "restored");
    assert!(env.source.is_dir());
    assert!(matches!(
        rule_status(&env.store, &rule).unwrap().state,
        LinkState::PlainDir
    ));
    let _ = fs::remove_dir_all(&env.base);
}

#[test]
fn source_missing_creates_empty_link() {
    let env = setup("missing");
    let rule = env.store.get_rule(env.rule_id).unwrap();
    fs::remove_dir_all(&env.source).unwrap();

    let out = apply_rule(&env.store, &rule, &ApplyOptions::default(), &mut noop).unwrap();
    assert_eq!(out.action, "created_and_linked");
    assert!(matches!(
        rule_status(&env.store, &rule).unwrap().state,
        LinkState::LinkOk {
            target_state: TargetState::Empty,
            ..
        }
    ));
    let _ = fs::remove_dir_all(&env.base);
}

#[test]
fn relink_refuses_contentful_plain_dir() {
    let env = setup("relink");
    let rule = env.store.get_rule(env.rule_id).unwrap();
    // 目标就绪，但源是含内容的真实目录 → 必须拒绝，绝不能把内容盖掉
    fs::create_dir_all(&env.target).unwrap();
    let err = relink_rule(&env.store, &rule).unwrap_err();
    assert!(matches!(err, SlError::SafetyBlocked(_)), "{err}");
    assert!(env.source.join("a.txt").exists());
    let _ = fs::remove_dir_all(&env.base);
}

#[test]
fn relink_requires_existing_target() {
    let env = setup("relinknotarget");
    let rule = env.store.get_rule(env.rule_id).unwrap();
    // 源是空壳目录 + 目标不存在 → 报"目标不存在"
    fs::remove_file(env.source.join("a.txt")).unwrap();
    fs::remove_file(env.source.join("sub").join("b.bin")).unwrap();
    fs::remove_dir(env.source.join("sub")).unwrap();
    let err = relink_rule(&env.store, &rule).unwrap_err();
    assert!(matches!(err, SlError::InvalidPath(_)), "{err}");
    let _ = fs::remove_dir_all(&env.base);
}

#[test]
fn relink_works_on_empty_source_shell() {
    let env = setup("relinkok");
    let rule = env.store.get_rule(env.rule_id).unwrap();
    // 源被软件更新清空成空目录，目标已就绪 → 应该能直接重链
    fs::create_dir_all(&env.target).unwrap();
    fs::remove_file(env.source.join("a.txt")).unwrap();
    fs::remove_file(env.source.join("sub").join("b.bin")).unwrap();
    fs::remove_dir(env.source.join("sub")).unwrap();

    let out = relink_rule(&env.store, &rule).unwrap();
    assert_eq!(out.action, "relinked");
    assert!(matches!(
        rule_status(&env.store, &rule).unwrap().state,
        LinkState::LinkOk { .. }
    ));
    let _ = fs::remove_dir_all(&env.base);
}

#[test]
fn relink_repairs_target_mismatch() {
    let env = setup("relinkmismatch");
    let rule = env.store.get_rule(env.rule_id).unwrap();
    apply_rule(&env.store, &rule, &ApplyOptions::default(), &mut noop).unwrap();

    // 模拟旧版本留下的状态：链接还在，但指向的不是登记的目标。
    let other = env.base.join("other");
    fs::create_dir_all(&other).unwrap();
    let old = crate::link::create::remove_dir_link(&env.source).unwrap();
    assert!(crate::util::path_eq(&old, &env.target));
    crate::link::create::create_dir_link(&env.source, &other, LinkType::Junction).unwrap();
    assert!(matches!(
        rule_status(&env.store, &rule).unwrap().state,
        LinkState::LinkOk {
            target_mismatch: true,
            ..
        }
    ));

    // 「重建链接」应该拆掉错指向的链接，并重新指回登记目标。
    let out = relink_rule(&env.store, &rule).unwrap();
    assert_eq!(out.action, "retargeted");
    assert!(matches!(
        rule_status(&env.store, &rule).unwrap().state,
        LinkState::LinkOk {
            target_mismatch: false,
            ..
        }
    ));
    assert_eq!(fs::read(env.source.join("a.txt")).unwrap(), b"aaa");
    let _ = fs::remove_dir_all(&env.base);
}

#[test]
fn retarget_rebuilds_link_and_keeps_old_target_data() {
    let env = setup("retarget");
    let rule = env.store.get_rule(env.rule_id).unwrap();
    apply_rule(&env.store, &rule, &ApplyOptions::default(), &mut noop).unwrap();
    assert!(env.target.join("a.txt").exists());

    let new_target = env.base.join("ram").join("dev").join("moved-cache");
    let mut next = rule.clone();
    next.target_subpath = "dev\\moved-cache".into();

    let out = retarget_rule_link(&env.store, &next).unwrap().expect("托管链接应被重建");
    assert_eq!(out.action, "retargeted");
    match rule_status(&env.store, &next).unwrap().state {
        LinkState::LinkOk {
            target,
            target_mismatch,
            ..
        } => {
            assert!(crate::util::path_eq(&target, &new_target));
            assert!(!target_mismatch);
        }
        other => panic!("期望链接已重指向，实际 {other:?}"),
    }
    // 旧目标数据没被删（拆链绝不递归）。
    assert!(env.target.join("a.txt").exists());
    assert_eq!(fs::read(env.target.join("a.txt")).unwrap(), b"aaa");

    // 源不是链接时不做任何事（交给 apply 处理搬运）。
    let plain = setup("retargetplain");
    let plain_rule = plain.store.get_rule(plain.rule_id).unwrap();
    assert!(retarget_rule_link(&plain.store, &plain_rule).unwrap().is_none());
    let _ = fs::remove_dir_all(&plain.base);
    let _ = fs::remove_dir_all(&env.base);
}

#[test]
fn rejected_apply_does_not_touch_anything() {
    let env = setup("reject");
    // 把目标根指向一个不存在的盘
    env.store
        .update_root(1, "RAM", "Z:\\NopeNotExist", true, 90000)
        .unwrap();
    let rule = env.store.get_rule(env.rule_id).unwrap();
    assert!(apply_rule(&env.store, &rule, &ApplyOptions::default(), &mut noop).is_err());
    assert!(env.source.join("a.txt").exists());
    let _ = fs::remove_dir_all(&env.base);
}

#[test]
fn state_keys_are_stable() {
    assert_eq!(state_key(&LinkState::PlainDir), "plain_dir");
    assert_eq!(state_key(&LinkState::SourceMissing), "source_missing");
    assert_eq!(
        state_key(&LinkState::LinkOk {
            target: PathBuf::from("R:\\x"),
            link_type: LinkType::Junction,
            target_state: TargetState::Empty,
            target_mismatch: false,
        }),
        "link_ok_empty"
    );
}

#[test]
fn clean_dir_follows_link_state() {
    let env = setup("clean_dir");
    let rule = env.store.get_rule(env.rule_id).unwrap();

    // 未应用：源是普通目录 → 清理命中源
    assert_eq!(clean_dir_for_rule(&env.store, &rule).unwrap(), env.source);

    // 应用后：源变成指向 RAM 盘目标的联接 → 清理应命中真实内容所在的目标
    let opts = ApplyOptions {
        dry_run: false,
        verify: VerifyMode::CountAndSize,
        skip_lock_check: true,
        allow_merge: false,
        best_effort: false,
    };
    apply_rule(&env.store, &rule, &opts, &mut noop).unwrap();
    assert_eq!(clean_dir_for_rule(&env.store, &rule).unwrap(), env.target);

    let _ = fs::remove_dir_all(&env.base);
}