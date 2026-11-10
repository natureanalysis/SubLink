//! 恢复：拆掉链接、把 RAM 盘内容搬回原位；以及只重建链接（relink）。

use std::time::Instant;

use crate::error::{SlError, SlResult};
use crate::fsops::mover::{self, Progress};
use crate::fsops::verify::TreeStats;
use crate::i18n::{t, tf};
use crate::link::create::{create_dir_link, ensure_dir, remove_dir_link};
use crate::link::detect::{detect_link, LinkState};
use crate::store::{Rule, Store};
use crate::util;

use super::apply::{outcome, ApplyOptions, Outcome};
use super::perm;
use super::resolve::{dir_has_content, resolve, rule_link_type};

/// 恢复一条规则：拆掉链接，把 RAM 盘里的内容搬回原位。
///
/// 「系统设置模式」的规则改回环境变量，由 [`super::setting::restore_setting_rule`] 处理。
pub fn restore_rule(
    store: &Store,
    rule: &Rule,
    opts: &ApplyOptions,
    sink: &mut dyn FnMut(Progress),
) -> SlResult<Outcome> {
    if super::resolve::is_setting_mode(rule) {
        return super::setting::restore_setting_rule(store, rule, opts, sink);
    }
    let started = Instant::now();
    let r = resolve(store, rule)?;
    let lt = rule_link_type(rule)?;
    crate::safety::preflight_pair(&r.source, &r.target)?;

    let state = detect_link(&r.source, Some(&r.target));

    match &state {
        LinkState::PlainDir => {
            store.set_rule_state(rule.id, "plain_dir", None)?;
            return Ok(outcome(
                rule,
                "already_plain",
                t("restore.already_plain").into(),
                None,
                started,
                false,
            ));
        }
        LinkState::SourceMissing => {
            store.set_rule_state(rule.id, "source_missing", None)?;
            return Ok(outcome(
                rule,
                "already_plain",
                t("restore.source_missing").into(),
                None,
                started,
                false,
            ));
        }
        LinkState::ForeignReparse { tag } => {
            return Err(SlError::SafetyBlocked(tf("restore.foreign", &[("tag", &format!("{tag:#010x}"))])));
        }
        LinkState::PlainFile => {
            return Err(SlError::InvalidPath(tf("restore.is_file", &[("path", &r.source.display().to_string())])));
        }
        LinkState::AccessDenied => {
            return Err(SlError::ElevationRequired(tf("apply.access_denied", &[("path", &r.source.display().to_string())])));
        }
        LinkState::IoError { message } => return Err(SlError::Other(message.clone())),
        LinkState::LinkOk { .. } | LinkState::LinkBroken { .. } => { /* 走恢复流程 */ }
        // 「系统设置模式」的状态不会走到这里（上面已经分流）。
        LinkState::SettingApplied { .. }
        | LinkState::SettingAppliedEmpty { .. }
        | LinkState::SettingMismatch { .. } => {
            return Err(SlError::Other(t("restore.unexpected_setting_state").into()));
        }
        // 同理：链接模式不会产生这个状态（它只在 setting 模式的体检里出现）。
        LinkState::ConfigInvalid { reason } => {
            return Err(SlError::Other(tf("restore.config_invalid", &[("reason", reason)])));
        }
    }

    let has_content = dir_has_content(&r.target);
    let plan = if has_content {
        t("restore.dry_move_back")
    } else {
        t("restore.dry_empty")
    };
    if opts.dry_run {
        return Ok(outcome(rule, "restored", plan.into(), None, started, true));
    }

    let op = store.op_begin(
        Some(rule.id),
        "restore",
        Some(&r.source.display().to_string()),
        Some(&r.target.display().to_string()),
    )?;

    // 1) 拆链接（只删链接条目，不碰目标内容）
    let target = match remove_dir_link(&r.source) {
        Ok(t) => t,
        Err(e) => {
            let _ = store.op_finish(op, "failed", None, None, Some(&e.to_string()));
            return Err(e);
        }
    };
    store.op_phase(op, "removing", None)?;

    // 2) 内容搬回（或建空目录）
    let stats = if has_content {
        match mover::move_tree(&target, &r.source, opts.verify, sink) {
            Ok(mr) => {
                store.op_phase(op, "linking", None)?;
                mr.stats
            }
            Err(e) => {
                // 回滚：把链接建回去，回到「已重定向」的状态
                match create_dir_link(&r.source, &target, lt) {
                    Ok(()) => {
                        perm::ensure_link_access(store, Some(rule.id), &r.source);
                        let _ = store.op_finish(
                            op,
                            "rolled_back",
                            None,
                            None,
                            Some(&tf("restore.rollback_ok", &[("e", &e.to_string())])),
                        );
                        let _ = store.set_rule_state(rule.id, "link_ok", Some(&e.to_string()));
                        let _ = store.log_event(
                            Some(rule.id),
                            "error",
                            "restore_failed",
                            &tf("restore.rollback_log", &[("name", &rule.name), ("e", &e.to_string())]),
                        );
                    }
                    Err(rb) => {
                        let _ = store.op_phase(
                            op,
                            "needs_manual",
                            Some(&tf("restore.rollback_fail", &[("e", &e.to_string()), ("rb", &rb.to_string())])),
                        );
                        let _ = store.log_event(
                            Some(rule.id),
                            "error",
                            "needs_manual",
                            &tf("restore.rollback_fail_log", &[
                                ("name", &rule.name),
                                ("e", &e.to_string()),
                                ("rb", &rb.to_string()),
                                ("tgt", &target.display().to_string()),
                                ("src", &r.source.display().to_string()),
                            ]),
                        );
                    }
                }
                return Err(e);
            }
        }
    } else {
        ensure_dir(&r.source)?;
        TreeStats::default()
    };

    store.op_finish(
        op,
        "done",
        Some(stats.files as i64),
        Some(stats.bytes as i64),
        None,
    )?;
    store.set_rule_state(rule.id, "plain_dir", None)?;
    store.log_event(
        Some(rule.id),
        "info",
        "restored",
        &tf("restore.restored_log", &[("name", &rule.name), ("plan", plan), ("summary", &stats.summary())]),
    )?;

    Ok(outcome(
        rule,
        "restored",
        if has_content {
            tf("restore.done_moved", &[("summary", &stats.summary()), ("src", &r.source.display().to_string())])
        } else {
            tf("restore.done_empty", &[("src", &r.source.display().to_string())])
        },
        Some(stats),
        started,
        false,
    ))
}

/// 只重建链接，不搬运数据。用于「目标目录已就绪，但链接被软件更新弄丢了」，
/// 也用于修复「登记的 target 被改过、链接还指向旧目标」。
///
/// 「系统设置模式」的规则对应动作是"把变量改回目标"，由
/// [`super::setting::relink_setting_rule`] 处理。
pub fn relink_rule(store: &Store, rule: &Rule) -> SlResult<Outcome> {
    if super::resolve::is_setting_mode(rule) {
        return super::setting::relink_setting_rule(store, rule);
    }
    let started = Instant::now();
    let r = resolve(store, rule)?;
    let lt = rule_link_type(rule)?;
    let state = detect_link(&r.source, Some(&r.target));

    // 链接指向与登记目标不一致：不是"已经好了"，要拆掉旧链接重指向。
    // 复用 `retarget_rule_link` —— 它自带"建链失败就把旧链接建回去"的回滚。
    let repoint = matches!(
        &state,
        LinkState::LinkOk {
            target_mismatch: true,
            ..
        }
    );
    if repoint {
        return match retarget_rule_link(store, rule)? {
            Some(o) => Ok(o),
            None => Ok(outcome(
                rule,
                "already_ok",
                t("restore.relink_already").into(),
                None,
                started,
                false,
            )),
        };
    }

    if state.is_managed_link() {
        return Ok(outcome(
            rule,
            "already_ok",
            t("restore.relink_already").into(),
            None,
            started,
            false,
        ));
    }
    // 先做「会不会破坏数据」的判定，再谈目标目录在不在。
    // 顺序反过来会给出误导性错误（"目标不存在"，而真正的问题是"源目录里有数据"）。
    if matches!(state, LinkState::PlainDir) && dir_has_content(&r.source) {
        return Err(SlError::SafetyBlocked(tf("restore.relink_blocked", &[("path", &r.source.display().to_string())])));
    }
    if !r.target.exists() {
        return Err(SlError::InvalidPath(tf("restore.relink_no_target", &[("path", &r.target.display().to_string())])));
    }
    // 源是空壳目录：清掉再建链
    if matches!(state, LinkState::PlainDir) {
        std::fs::remove_dir(util::to_verbatim(&r.source))
            .map_err(|e| crate::error::io_ctx(e, &r.source, crate::i18n::t("io.action.clean_empty_source")))?;
    }
    create_dir_link(&r.source, &r.target, lt)?;
    perm::ensure_link_access(store, Some(rule.id), &r.source);
    store.set_rule_state(
        rule.id,
        if dir_has_content(&r.target) {
            "link_ok"
        } else {
            "link_ok_empty"
        },
        None,
    )?;
    store.log_event(
        Some(rule.id),
        "info",
        "relinked",
        &tf("restore.relink_log", &[("name", &rule.name), ("target", &r.target.display().to_string())]),
    )?;
    Ok(outcome(
        rule,
        "relinked",
        tf("restore.relink_done", &[("src", &r.source.display().to_string()), ("target", &r.target.display().to_string())]),
        None,
        started,
        false,
    ))
}

/// 目标 / 链接类型被修改后，把源路径上已有的托管链接重建到新目标。
///
/// 只处理源路径**已经是本工具建立的链接**的情况（Junction / 目录符号链接，
/// 含目标暂时不可达的死链）。源不是链接时返回 `None` —— 那种情况要走
/// `apply_rule`（可能涉及搬运数据），不能在保存表单时静默做。
///
/// 旧目标里的内容不会被删除：`remove_dir_link` 只删链接条目，绝不递归。
/// 建链失败会自动把旧链接恢复回去，避免源路径留下一个空洞。
pub fn retarget_rule_link(store: &Store, rule: &Rule) -> SlResult<Option<Outcome>> {
    let started = Instant::now();
    let r = resolve(store, rule)?;
    let lt = rule_link_type(rule)?;
    let state = detect_link(&r.source, None);
    if !state.is_managed_link() {
        return Ok(None);
    }

    // 目标根可能随 RAM 盘重启消失，和应用路径一样先把它建出来。
    crate::boot::ensure_root_dir(&r.root.resolved_path().0)?;
    if !crate::acl::is_volume_root(&r.root.resolved_path().0) {
        perm::ensure_dir_access(store, Some(rule.id), &r.root.resolved_path().0);
    }
    crate::safety::preflight_pair(&r.source, &r.target)?;
    ensure_dir(&r.target)?;
    perm::ensure_dir_access(store, Some(rule.id), &r.target);

    let old_type = state.link_type().unwrap_or(lt);
    let old_target = remove_dir_link(&r.source)?;

    if let Err(e) = create_dir_link(&r.source, &r.target, lt) {
        // 回滚：把旧链接建回去，绝不让源路径变成一个空洞。
        match create_dir_link(&r.source, &old_target, old_type) {
            Ok(()) => {
                perm::ensure_link_access(store, Some(rule.id), &r.source);
                let _ = store.set_rule_state(rule.id, "link_ok", Some(&e.to_string()));
                let _ = store.log_event(
                    Some(rule.id),
                    "error",
                    "retarget_failed",
                    &tf("restore.retarget_rollback_ok", &[("e", &e.to_string())]),
                );
            }
            Err(rb) => {
                let _ = store.log_event(
                    Some(rule.id),
                    "error",
                    "needs_manual",
                    &tf("restore.retarget_rollback_fail", &[
                        ("e", &e.to_string()),
                        ("rb", &rb.to_string()),
                        ("src", &r.source.display().to_string()),
                        ("target", &old_target.display().to_string()),
                    ]),
                );
            }
        }
        return Err(e);
    }
    perm::ensure_link_access(store, Some(rule.id), &r.source);

    let has_content = dir_has_content(&r.target);
    store.set_rule_state(
        rule.id,
        if has_content { "link_ok" } else { "link_ok_empty" },
        None,
    )?;
    store.log_event(
        Some(rule.id),
        "info",
        "retargeted",
        &tf("restore.retarget_log", &[
            ("name", &rule.name),
            ("target", &r.target.display().to_string()),
        ]),
    )?;
    Ok(Some(outcome(
        rule,
        "retargeted",
        tf("restore.retarget_done", &[
            ("src", &r.source.display().to_string()),
            ("target", &r.target.display().to_string()),
        ]),
        None,
        started,
        false,
    )))
}