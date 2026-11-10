//! 文件夹清理用例（清空 / 按时间删除）：解析目标、预演、执行。

use std::path::PathBuf;

use crate::engine;
use crate::error::{SlError, SlResult};
use crate::fsops::clean;
use crate::fsops::exclude::Excludes;
use crate::fsops::mover::Progress;
use crate::i18n::tf;
use crate::safety;
use crate::store::Store;
use crate::util;

use super::view::{CleanPreviewView, CleanResultView, CleanTarget};

/// 解析出真正要清理的目录，以及规则上配置的清理排除模式。
///
/// 规则：源目录若已是链接（联接/符号链接），真实内容在 RAM 盘目标上，清目标；
/// 否则（普通目录 / 未链接）清源。文件夹：展开 `%VAR%` 后过一遍系统目录黑名单。
/// 排除模式只来自规则（任意文件夹清理不带排除）。
fn resolve_clean_dir(store: &Store, target: &CleanTarget) -> SlResult<(PathBuf, String, Excludes)> {
    match target {
        CleanTarget::Rule { id } => {
            let rule = store.get_rule(*id)?;
            let dir = engine::clean_dir_for_rule(store, &rule)?;
            let excludes = Excludes::new(&rule.excludes)?;
            Ok((dir, rule.name, excludes))
        }
        CleanTarget::Folder { path } => {
            let (expanded, _unknown) = util::expand_path(path);
            let pf = safety::preflight_source(&expanded);
            if pf.is_blocked() {
                return Err(SlError::SafetyBlocked(pf.blocking.join("；")));
            }
            let label = expanded.display().to_string();
            Ok((expanded, label, Excludes::default()))
        }
    }
}

/// 预演：只算不删。目录不存在时返回空计划（而不是报错——清理一个还没内容的缓存是常见的）。
///
/// `type_excludes` 是本次清理要**保留**的文件扩展名（如 `.py`，来自清理弹窗的
/// "保留这些类型的文件"），叠加在规则自身的排除模式之上；空 = 不按类型过滤。
pub fn clean_preview(
    store: &Store,
    target: &CleanTarget,
    policy: &clean::CleanPolicy,
    type_excludes: &[String],
) -> SlResult<CleanPreviewView> {
    let (dir, label, excludes) = resolve_clean_dir(store, target)?;
    let excludes = excludes.with_exts(type_excludes)?;
    policy.validate()?;
    let plan = if dir.is_dir() {
        clean::plan(&dir, policy, &excludes)?
    } else {
        clean::CleanPlan::default()
    };
    let summary = plan.summary();
    let bytes_text = util::human_bytes(plan.bytes);
    Ok(CleanPreviewView {
        dir: dir.display().to_string(),
        label,
        files: plan.files,
        bytes: plan.bytes,
        bytes_text,
        links_skipped: plan.links_skipped,
        excluded_skipped: plan.excluded_skipped,
        sample: plan.sample,
        summary,
    })
}

/// 执行清理。`dry_run` 走预演分支不写盘。完成后落一条事件流水。
///
/// `type_excludes` 见 [`clean_preview`]；判定口径与预演共用同一 `Excludes`，
/// 保证"看到的"与"删掉的"一致。
pub fn clean_run(
    store: &Store,
    target: &CleanTarget,
    policy: &clean::CleanPolicy,
    type_excludes: &[String],
    dry_run: bool,
    on_progress: &mut dyn FnMut(&Progress),
) -> SlResult<CleanResultView> {
    let (dir, label, excludes) = resolve_clean_dir(store, target)?;
    let excludes = excludes.with_exts(type_excludes)?;
    policy.validate()?;
    let report = if dir.is_dir() {
        let mut sink = |p: Progress| on_progress(&p);
        clean::run(&dir, policy, &excludes, dry_run, &mut sink)?
    } else {
        clean::CleanReport::default()
    };
    let summary = report.summary();
    let deleted_bytes_text = util::human_bytes(report.deleted_bytes);
    if !dry_run && report.deleted_files > 0 {
        let rule_id = match target {
            CleanTarget::Rule { id } => Some(*id),
            CleanTarget::Folder { .. } => None,
        };
        store.log_event(
            rule_id,
            "info",
            "clean",
            &tf(
                "usecase_clean.log",
                &[("label", label.as_str()), ("summary", &summary)],
            ),
        )?;
        // 同时落一条操作流水：event 的 message 聚合不了，「效益统计」要从
        // operations 的结构化列（file_count / bytes）上取数。
        if let Some(id) = rule_id {
            let dir_text = dir.display().to_string();
            if let Ok(op) = store.op_begin(Some(id), "clean", Some(&dir_text), Some(&dir_text)) {
                let _ = store.op_finish(
                    op,
                    "done",
                    Some(report.deleted_files as i64),
                    Some(report.deleted_bytes as i64),
                    None,
                );
            }
        }
    }
    Ok(CleanResultView {
        dir: dir.display().to_string(),
        label,
        deleted_files: report.deleted_files,
        deleted_bytes: report.deleted_bytes,
        deleted_bytes_text,
        dirs_pruned: report.dirs_pruned,
        skipped: report.skipped,
        skip_examples: report.skip_examples,
        links_skipped: report.links_skipped,
        excluded_skipped: report.excluded_skipped,
        summary,
    })
}
