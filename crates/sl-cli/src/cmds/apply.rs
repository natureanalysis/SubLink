//! `apply` / `restore` / `relink`：把规则真正落地或撤销。
//!
//! 这里直接编排 `engine::apply_rule` / `engine::restore_rule`（而不是走
//! `sl_core::usecase` 的批量门面）：CLI 需要 `engine::Outcome` 里的搬运规模
//! 与耗时明细（`print_outcome` 依赖 `stats` 字段），那是 GUI 契约
//! `usecase::ItemOutcome` 没有的信息。

use sl_core::engine::{self, apply_rule, restore_rule, ApplyOptions};
use sl_core::fsops::verify::VerifyMode;
use sl_core::i18n::{t, tf};
use sl_core::store::Store;
use sl_core::{SlError, SlResult};

use crate::output::print_outcome;
use crate::parse::Parsed;

pub(crate) fn cmd_apply(store: &Store, p: &Parsed) -> SlResult<()> {
    let opts = ApplyOptions {
        dry_run: p.has("dry-run"),
        verify: if p.has("sha256") {
            VerifyMode::Sha256
        } else {
            VerifyMode::CountAndSize
        },
        skip_lock_check: p.has("skip-lock-check"),
        allow_merge: p.has("merge"),
        best_effort: p.has("best-effort"),
    };

    let ids = collect_target_ids(store, p, "apply")?;
    let mut noop = |_p: sl_core::fsops::mover::Progress| {};

    let mut ok = 0usize;
    let mut failed = 0usize;
    let multiple = ids.len() > 1;
    for id in ids {
        let rule = match store.get_rule(id) {
            Ok(r) => r,
            Err(e) => {
                eprintln!(
                    "{}",
                    tf("cli.apply.fail_get", &[("id", &id.to_string()), ("e", &e.to_string())])
                );
                failed += 1;
                continue;
            }
        };
        if multiple {
            println!(
                "{}",
                tf("cli.apply.section", &[("id", &id.to_string()), ("name", rule.name.as_str())])
            );
        }
        match apply_rule(store, &rule, &opts, &mut noop) {
            Ok(o) => {
                print_outcome(&o);
                ok += 1;
            }
            Err(e) => {
                let name_col = format!("{:<20}", rule.name);
                println!(
                    "{}",
                    tf(
                        "cli.apply.fail_apply",
                        &[("id", &id.to_string()), ("name", name_col.as_str()), ("e", &e.to_string())]
                    )
                );
                failed += 1;
            }
        }
    }
    println!();
    let action = if opts.dry_run {
        t("cli.apply.action_dry_run")
    } else {
        t("cli.apply.action_done")
    };
    println!(
        "{}",
        tf(
            "cli.apply.summary",
            &[("action", action), ("ok", &ok.to_string()), ("failed", &failed.to_string())]
        )
    );
    if failed > 0 {
        return Err(SlError::Other(format!("有 {failed} 条未成功")));
    }
    Ok(())
}

pub(crate) fn cmd_restore(store: &Store, p: &Parsed) -> SlResult<()> {
    let opts = ApplyOptions {
        dry_run: p.has("dry-run"),
        verify: if p.has("sha256") {
            VerifyMode::Sha256
        } else {
            VerifyMode::CountAndSize
        },
        skip_lock_check: true,
        allow_merge: true,
        best_effort: p.has("best-effort"),
    };
    let ids = collect_target_ids(store, p, "restore")?;
    let mut noop = |_p: sl_core::fsops::mover::Progress| {};
    let mut ok = 0usize;
    let mut failed = 0usize;
    for id in ids {
        let rule = match store.get_rule(id) {
            Ok(r) => r,
            Err(e) => {
                println!(
                    "{}",
                    tf("cli.apply.fail_get", &[("id", &id.to_string()), ("e", &e.to_string())])
                );
                failed += 1;
                continue;
            }
        };
        match restore_rule(store, &rule, &opts, &mut noop) {
            Ok(o) => {
                print_outcome(&o);
                ok += 1;
            }
            Err(e) => {
                let name_col = format!("{:<20}", rule.name);
                println!(
                    "{}",
                    tf(
                        "cli.apply.fail_apply",
                        &[("id", &id.to_string()), ("name", name_col.as_str()), ("e", &e.to_string())]
                    )
                );
                failed += 1;
            }
        }
    }
    println!();
    let action = if opts.dry_run {
        t("cli.apply.action_dry_run")
    } else {
        t("cli.apply.action_done")
    };
    println!(
        "{}",
        tf(
            "cli.apply.summary",
            &[("action", action), ("ok", &ok.to_string()), ("failed", &failed.to_string())]
        )
    );
    if failed > 0 {
        return Err(SlError::Other(format!("有 {failed} 条未成功")));
    }
    Ok(())
}

pub(crate) fn cmd_relink(store: &Store, p: &Parsed) -> SlResult<()> {
    let id = p
        .required(0, "<规则id>")?
        .parse::<i64>()
        .map_err(|_| SlError::Other("规则 id 必须是整数".into()))?;
    let rule = store.get_rule(id)?;
    let o = engine::relink_rule(store, &rule)?;
    print_outcome(&o);
    Ok(())
}

/// `--all` 时取所有规则，否则取显式列出的 id。`apply` 只处理"期望开启"的规则。
fn collect_target_ids(store: &Store, p: &Parsed, kind: &str) -> SlResult<Vec<i64>> {
    if p.has("all") {
        let rules = store.list_rules()?;
        let ids: Vec<i64> = rules
            .iter()
            .filter(|r| kind == "restore" || r.want_enabled || p.has("include-disabled"))
            .map(|r| r.id)
            .collect();
        if ids.is_empty() {
            println!("{}", t("cli.apply.no_rules"));
        }
        return Ok(ids);
    }
    let ids = p.ids(0)?;
    if ids.is_empty() {
        return Err(SlError::Other(format!(
            "请指定规则 id，或用 `{kind} --all` 处理全部"
        )));
    }
    Ok(ids)
}