//! 应用：把源目录变成指向 RAM 盘的链接，失败必回滚。

use std::time::Instant;

use serde::Serialize;

use crate::error::{SlError, SlResult};
use crate::fsops::mover::{self, MoveReport, Progress};
use crate::fsops::occupancy::{self, LockScanOptions};
use crate::fsops::verify::{self, TreeStats, VerifyMode};
use crate::i18n::{t, tf};
use crate::link::create::{create_dir_link, ensure_dir, remove_dir_link};
use crate::link::detect::{detect_link, LinkState, TargetState};
use crate::store::{Rule, Store};

use super::perm;
use super::resolve::{dir_has_content, resolve, rule_link_type, validate_ready, Resolved};

#[derive(Debug, Clone)]
pub struct ApplyOptions {
    pub dry_run: bool,
    pub verify: VerifyMode,
    /// 跳过占用检测（危险：源可能正在被写入，副本会撕裂）。
    pub skip_lock_check: bool,
    /// 允许把源内容合并进非空的目标目录。
    pub allow_merge: bool,
    /// 尽力模式：跳过占用预检、自动允许合并、单个条目拷不动就跳过。
    /// 只应在**已提权**且用户明确接受"部分搬运"时打开。
    pub best_effort: bool,
}

impl Default for ApplyOptions {
    fn default() -> Self {
        Self {
            dry_run: false,
            verify: VerifyMode::CountAndSize,
            skip_lock_check: false,
            allow_merge: false,
            best_effort: false,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Outcome {
    pub rule_id: i64,
    pub name: String,
    /// already_ok / fixed_broken_target / created_and_linked / moved_and_linked /
    /// merged_and_linked / restored / already_plain / relinked / dry_run
    pub action: &'static str,
    pub message: String,
    pub stats: Option<TreeStats>,
    pub links_recreated: u64,
    pub elapsed_ms: u128,
    pub dry_run: bool,
}

pub(super) fn outcome(
    rule: &Rule,
    action: &'static str,
    message: String,
    stats: Option<TreeStats>,
    started: Instant,
    dry_run: bool,
) -> Outcome {
    Outcome {
        rule_id: rule.id,
        name: rule.name.clone(),
        action,
        message,
        stats,
        links_recreated: 0,
        elapsed_ms: started.elapsed().as_millis(),
        dry_run,
    }
}

/// 应用一条规则：把源目录变成指向 RAM 盘的链接。
///
/// 「系统设置模式」的规则不走这条路径 —— 它改的是注册表里的环境变量，
/// 由 [`super::setting::apply_setting_rule`] 处理。
pub fn apply_rule(
    store: &Store,
    rule: &Rule,
    opts: &ApplyOptions,
    sink: &mut dyn FnMut(Progress),
) -> SlResult<Outcome> {
    if super::resolve::is_setting_mode(rule) {
        return super::setting::apply_setting_rule(store, rule, opts, sink);
    }
    let started = Instant::now();
    let r = resolve(store, rule)?;
    validate_ready(&r)?;
    let lt = rule_link_type(rule)?;

    // 目标根可能刚被建出来（RAM 盘重启后的正常形态）→ 同样给对应使用者授权，
    // 否则落在根下面的子目录会继承到一份"没有使用者"的权限。
    // 卷根（`R:\`）跳过：改它等于改整块盘的安全策略。
    let (root_path, _) = r.root.resolved_path();
    if root_path.is_dir() && !crate::acl::is_volume_root(&root_path) {
        perm::ensure_dir_access(store, Some(rule.id), &root_path);
    }

    let state = detect_link(&r.source, Some(&r.target));

    // ---- 幂等 / 快速分支 ----
    match &state {
        LinkState::LinkOk {
            target_state: TargetState::HasContent,
            target_mismatch: false,
            ..
        } => {
            store.set_rule_state(rule.id, "link_ok", None)?;
            return Ok(outcome(
                rule,
                "already_ok",
                t("apply.already_ok").into(),
                None,
                started,
                false,
            ));
        }
        LinkState::LinkOk {
            target_state: TargetState::Empty,
            target_mismatch: false,
            ..
        } => {
            store.set_rule_state(rule.id, "link_ok_empty", None)?;
            return Ok(outcome(
                rule,
                "already_ok",
                t("apply.already_ok_empty").into(),
                None,
                started,
                false,
            ));
        }
        LinkState::LinkOk {
            target_mismatch: true,
            target,
            ..
        } => {
            return Err(SlError::Other(tf("apply.mismatch", &[
                ("actual", &target.display().to_string()),
                ("target", &r.target.display().to_string()),
            ])));
        }
        LinkState::ForeignReparse { tag } => {
            return Err(SlError::SafetyBlocked(tf("apply.foreign", &[
                ("tag", &format!("{tag:#010x}")),
                ("path", &r.source.display().to_string()),
            ])));
        }
        LinkState::PlainFile => {
            return Err(SlError::InvalidPath(tf("apply.is_file", &[("path", &r.source.display().to_string())])));
        }
        LinkState::AccessDenied => {
            return Err(SlError::ElevationRequired(tf("apply.access_denied", &[("path", &r.source.display().to_string())])));
        }
        LinkState::IoError { message } => {
            return Err(SlError::Other(message.clone()));
        }
        LinkState::LinkBroken { .. } => {
            // 链接本身完好，只是目标目录没了 —— 建回来即可。
            if opts.dry_run {
                return Ok(outcome(
                    rule,
                    "fixed_broken_target",
                    t("apply.dry_rebuild_target").into(),
                    None,
                    started,
                    true,
                ));
            }
            ensure_dir(&r.target)?;
            // 目标目录可能是提权身份建出来的 → 补一条"对应使用者完全控制"。
            // 失败不阻断（链接照样能用），但会落事件，避免权限问题变成"软件莫名报错"。
            perm::ensure_dir_access(store, Some(rule.id), &r.target);
            store.set_rule_state(rule.id, "link_ok_empty", None)?;
            store.log_event(
                Some(rule.id),
                "warn",
                "target_rebuilt",
                &tf("apply.target_rebuilt", &[("target", &r.target.display().to_string())]),
            )?;
            return Ok(outcome(
                rule,
                "fixed_broken_target",
                tf("apply.target_rebuilt_done", &[("target", &r.target.display().to_string())]),
                None,
                started,
                false,
            ));
        }
        LinkState::SourceMissing => {
            if opts.dry_run {
                return Ok(outcome(
                    rule,
                    "created_and_linked",
                    t("apply.dry_create_link").into(),
                    None,
                    started,
                    true,
                ));
            }
            let created = ensure_dir(&r.target)?;
            perm::ensure_dir_access(store, Some(rule.id), &r.target);
            create_dir_link(&r.source, &r.target, lt)?;
            perm::ensure_link_access(store, Some(rule.id), &r.source);
            store.set_rule_state(rule.id, "link_ok_empty", None)?;
            store.log_event(
                Some(rule.id),
                "info",
                "linked_without_source",
                &tf("apply.linked_no_source", &[
                    ("created", if created { t("apply.created") } else { t("apply.exists") }),
                ]),
            )?;
            return Ok(outcome(
                rule,
                "created_and_linked",
                tf("apply.linked_no_source_done", &[("target", &r.target.display().to_string())]),
                None,
                started,
                false,
            ));
        }
        // 兜底：只剩 target_state=Missing 且 mismatch=false 这一种组合。
        // 按 `detect_link` 的语义，目标不可达时应该产出 LinkBroken，
        // 所以这个分支逻辑上不可达；保守起见报错而不是猜一个动作。
        LinkState::LinkOk {
            target_state: TargetState::Missing,
            ..
        } => {
            return Err(SlError::Other(tf("apply.contradiction", &[("path", &r.source.display().to_string())])));
        }
        LinkState::PlainDir => { /* 走下面的搬运流程 */ }
        // 「系统设置模式」的状态不会走到这里（上面已经分流），
        // 但 match 必须穷尽 —— 落到这里说明分流逻辑坏了，直接报错而不是猜。
        LinkState::SettingApplied { .. }
        | LinkState::SettingAppliedEmpty { .. }
        | LinkState::SettingMismatch { .. } => {
            return Err(SlError::Other(t("apply.unexpected_setting_state").into()));
        }
        // 同理：链接模式不会产生这个状态（它只在 setting 模式的体检里出现）。
        LinkState::ConfigInvalid { reason } => {
            return Err(SlError::Other(tf("apply.config_invalid", &[("reason", reason)])));
        }
    }

    // ---- 需要搬运 ----
    let src_stats = verify::measure(&r.source)?;
    let target_non_empty = dir_has_content(&r.target);

    if target_non_empty && !opts.allow_merge && !opts.best_effort {
        return Err(SlError::SafetyBlocked(tf("apply.target_non_empty", &[("path", &r.target.display().to_string())])));
    }

    // 占用检测：证明源目录此刻没有被写入，副本才不会撕裂。
    // 尽力模式刻意跳过 —— 它的语义就是"能拿多少拿多少，删不掉就留着"。
    if !opts.skip_lock_check && !opts.best_effort {
        let report = occupancy::scan_locks(&r.source, &LockScanOptions::default())?;
        if !report.is_clear() {
            store.log_event(
                Some(rule.id),
                "warn",
                "blocked",
                &format!("{}：{}", rule.name, report.summary()),
            )?;
            return Err(SlError::Busy(tf("apply.source_busy", &[("summary", &report.summary())])));
        }
    }

    if opts.dry_run {
        let action = if target_non_empty {
            "merged_and_linked"
        } else {
            "moved_and_linked"
        };
        let what = if target_non_empty {
            t("apply.dry_merge").to_string()
        } else {
            t("apply.dry_move").to_string()
        };
        return Ok(outcome(rule, action, what.into(), Some(src_stats), started, true));
    }

    // ---- 正式执行（带流水与回滚）----
    let op = store.op_begin(
        Some(rule.id),
        "apply",
        Some(&r.source.display().to_string()),
        Some(&r.target.display().to_string()),
    )?;

    let mut action: &'static str = "moved_and_linked";
    let mut note = String::new();
    let mut move_report: Option<MoveReport> = None;

    // 目标目录先建出来并授权：搬进来的文件会**继承**目标目录的 DACL，
    // 目录本身对使用者不可写的话，搬过去的内容同样写不了。
    ensure_dir(&r.target)?;
    perm::ensure_dir_access(store, Some(rule.id), &r.target);
    let work = (|| -> SlResult<()> {
        if target_non_empty {
            store.op_phase(op, "copying", None)?;
            let mr = mover::merge_into_ex(&r.source, &r.target, opts.best_effort, sink)?;
            action = "merged_and_linked";
            note = tf("apply.merged", &[
                ("n", &(mr.copied + mr.overwritten).to_string()),
                ("over", &mr.overwritten.to_string()),
                ("skip", &mr.skipped.to_string()),
            ]);
            if !mr.failed.is_empty() {
                // 读不出来的条目没有副本，绝不能删源
                return Err(SlError::Busy(tf("move.best_effort_skipped", &[
                    ("n", &mr.failed.len().to_string()),
                    ("list", &mr.failed.iter().take(3).map(|s| s.path.clone()).collect::<Vec<_>>().join("；")),
                ])));
            }
            store.op_phase(op, "copied", None)?;
            store.op_phase(op, "removing", None)?;
            finish_delete(&r.source, mr.copied + mr.overwritten, opts.best_effort, sink)?;
        } else {
            store.op_phase(op, "copying", None)?;
            let mr = mover::move_tree_ex(&r.source, &r.target, opts.verify, opts.best_effort, sink)?;
            note = format!("{}：{}", mr.method, mr.stats.summary());
            move_report = Some(mr);
            store.op_phase(op, "copied", None)?;
        }
        store.op_phase(op, "linking", None)?;
        // 搬运结束后**再授权一次**：同卷重命名会把源目录整个搬到目标位置，
        // 目录连同它的安全描述符一起被换掉 —— 搬运前给的授权会跟着旧目录一起走。
        perm::ensure_dir_access(store, Some(rule.id), &r.target);
        sink(Progress::Linking {
            link: r.source.display().to_string(),
            target: r.target.display().to_string(),
        });
        create_dir_link(&r.source, &r.target, lt)?;
        perm::ensure_link_access(store, Some(rule.id), &r.source);
        Ok(())
    })();

    if let Err(e) = work {
        match rollback_apply(&r, target_non_empty) {
            Ok(rb) => {
                let _ = store.op_finish(
                    op,
                    "rolled_back",
                    None,
                    None,
                    Some(&tf("apply.rollback_ok", &[("e", &e.to_string()), ("rb", &rb)])),
                );
                let _ = store.set_rule_state(rule.id, "rollback", Some(&e.to_string()));
                let _ = store.log_event(
                    Some(rule.id),
                    "error",
                    "apply_failed",
                    &tf("apply.rollback_log", &[("name", &rule.name), ("e", &e.to_string()), ("rb", &rb)]),
                );
            }
            Err(rb) => {
                // 回滚也失败 → 留一条非终态流水，下次启动作为「待人工处理」
                let _ = store.op_phase(
                    op,
                    "needs_manual",
                    Some(&tf("apply.rollback_fail", &[("e", &e.to_string()), ("rb", &rb.to_string())])),
                );
                let _ = store.log_event(
                    Some(rule.id),
                    "error",
                    "needs_manual",
                    &tf("apply.rollback_fail_log", &[
                        ("name", &rule.name),
                        ("e", &e.to_string()),
                        ("rb", &rb.to_string()),
                        ("src", &r.source.display().to_string()),
                        ("tgt", &r.target.display().to_string()),
                    ]),
                );
            }
        }
        return Err(e);
    }

    let stats = move_report.as_ref().map(|m| m.stats).or(Some(src_stats));
    let links_recreated = move_report.as_ref().map(|m| m.links_recreated).unwrap_or(0);
    let files = stats.map(|s| s.files as i64).unwrap_or(0);
    let bytes = stats.map(|s| s.bytes as i64).unwrap_or(0);

    store.op_finish(op, "done", Some(files), Some(bytes), None)?;
    store.set_rule_applied(rule.id, files, bytes)?;
    store.log_event(
        Some(rule.id),
        "info",
        "applied",
        &tf("apply.applied_log", &[("name", &rule.name), ("note", &note)]),
    )?;

    let mut o = outcome(
        rule,
        action,
        tf("apply.done_note", &[
            ("note", &note),
            ("src", &r.source.display().to_string()),
            ("tgt", &r.target.display().to_string()),
        ]),
        stats,
        started,
        false,
    );
    o.links_recreated = links_recreated;
    Ok(o)
}

/// 合并后的删源。尽力模式下把"删不掉的条目"翻译成可操作的提示。
fn finish_delete(
    src: &std::path::Path,
    copied: u64,
    best_effort: bool,
    sink: &mut dyn FnMut(Progress),
) -> SlResult<()> {
    if !best_effort {
        mover::delete_tree(src, sink)?;
        return Ok(());
    }
    let del = mover::delete_tree_ex(src, sink)?;
    if !del.failures.is_empty() {
        return Err(SlError::Busy(tf("move.best_effort_leftovers", &[
            ("copied", &copied.to_string()),
            ("n", &del.failures.len().to_string()),
            ("list", &del.failures.iter().take(3).cloned().collect::<Vec<_>>().join("；")),
        ])));
    }
    Ok(())
}

/// 应用失败时的回滚。
fn rollback_apply(r: &Resolved, merged: bool) -> Result<String, String> {
    // 先尽力清掉可能半成的链接（不存在会报错，忽略）
    let _ = remove_dir_link(&r.source);

    if merged {
        // 合并路径：数据都在目标目录里。只需保证源路径存在，缓存可重造。
        if !r.source.exists() {
            ensure_dir(&r.source).map_err(|e| e.to_string())?;
            return Ok(t("apply.rb_merged").into());
        }
        return Ok(t("apply.rb_source_exists").into());
    }

    if !dir_has_content(&r.target) {
        return Ok(t("apply.rb_target_empty").into());
    }
    if r.source.exists() {
        return Ok(t("apply.rb_source_exists2").into());
    }

    let mut noop = |_: Progress| {};
    let mr = mover::move_tree(&r.target, &r.source, VerifyMode::CountAndSize, &mut noop)
        .map_err(|e| e.to_string())?;
    Ok(tf("apply.rb_moved_back", &[("summary", &mr.stats.summary())]))
}