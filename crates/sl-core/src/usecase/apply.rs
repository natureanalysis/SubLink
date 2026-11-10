//! 应用/恢复编排：批量执行、失败隔离、单条重链、占用检测。

use crate::engine;
use crate::error::{SlError, SlResult};
use crate::fsops::mover::Progress;
use crate::fsops::occupancy::{self, LockReport, LockScanOptions};
use crate::i18n::tf;
use crate::store::Store;

use super::view::{ApplyOpts, ItemOutcome};

/// 批量应用。**单条失败不影响其它条目**，逐条收集结果。
/// `on_progress` 会在每条开始前与过程中的关键节点被调用。
pub fn apply_rules(
    store: &Store,
    ids: &[i64],
    opts: &ApplyOpts,
    on_progress: &mut dyn FnMut(usize, usize, &str, &Progress),
) -> Vec<ItemOutcome> {
    let total = ids.len();
    let mut out = Vec::with_capacity(total);
    for (i, id) in ids.iter().enumerate() {
        let rule = match store.get_rule(*id) {
            Ok(r) => r,
            Err(e) => {
                out.push(ItemOutcome {
                    rule_id: *id,
                    name: format!("#{id}"),
                    ok: false,
                    action: "not_found".into(),
                    message: e.to_string(),
                    elapsed_ms: 0,
                });
                continue;
            }
        };
        on_progress(
            i,
            total,
            &rule.name,
            &Progress::Enumerated { files: 0, bytes: 0 },
        );
        let mut sink = |p: Progress| on_progress(i, total, &rule.name, &p);
        match engine::apply_rule(store, &rule, &opts.to_engine(), &mut sink) {
            Ok(o) => out.push(ItemOutcome {
                rule_id: o.rule_id,
                name: o.name,
                ok: true,
                action: o.action.to_string(),
                message: o.message,
                elapsed_ms: o.elapsed_ms,
            }),
            Err(e) => out.push(ItemOutcome {
                rule_id: rule.id,
                name: rule.name,
                ok: false,
                action: "failed".into(),
                message: e.to_string(),
                elapsed_ms: 0,
            }),
        }
    }
    out
}

pub fn restore_rules(
    store: &Store,
    ids: &[i64],
    opts: &ApplyOpts,
    on_progress: &mut dyn FnMut(usize, usize, &str, &Progress),
) -> Vec<ItemOutcome> {
    let total = ids.len();
    let mut out = Vec::with_capacity(total);
    for (i, id) in ids.iter().enumerate() {
        let rule = match store.get_rule(*id) {
            Ok(r) => r,
            Err(e) => {
                out.push(ItemOutcome {
                    rule_id: *id,
                    name: format!("#{id}"),
                    ok: false,
                    action: "not_found".into(),
                    message: e.to_string(),
                    elapsed_ms: 0,
                });
                continue;
            }
        };
        on_progress(i, total, &rule.name, &Progress::Deleting { files: 0 });
        let mut sink = |p: Progress| on_progress(i, total, &rule.name, &p);
        match engine::restore_rule(store, &rule, &opts.to_engine(), &mut sink) {
            Ok(o) => out.push(ItemOutcome {
                rule_id: o.rule_id,
                name: o.name,
                ok: true,
                action: o.action.to_string(),
                message: o.message,
                elapsed_ms: o.elapsed_ms,
            }),
            Err(e) => out.push(ItemOutcome {
                rule_id: rule.id,
                name: rule.name,
                ok: false,
                action: "failed".into(),
                message: e.to_string(),
                elapsed_ms: 0,
            }),
        }
    }
    out
}

pub fn relink(store: &Store, id: i64) -> SlResult<ItemOutcome> {
    let rule = store.get_rule(id)?;
    let o = engine::relink_rule(store, &rule)?;
    Ok(ItemOutcome {
        rule_id: o.rule_id,
        name: o.name,
        ok: true,
        action: o.action.to_string(),
        message: o.message,
        elapsed_ms: o.elapsed_ms,
    })
}

pub fn check_locks(store: &Store, id: i64) -> SlResult<LockReport> {
    let rule = store.get_rule(id)?;
    let resolved = engine::resolve(store, &rule)?;
    if !resolved.source.is_dir() {
        return Err(SlError::InvalidPath(tf(
            "apply.no_source_dir",
            &[("path", &resolved.source.display().to_string())],
        )));
    }
    let report = occupancy::scan_locks(&resolved.source, &LockScanOptions::default())?;
    if !report.is_clear() {
        store.log_event(
            Some(id),
            "warn",
            "blocked",
            &tf(
                "apply.blocked_log",
                &[("name", &rule.name), ("summary", &report.summary())],
            ),
        )?;
    }
    Ok(report)
}
