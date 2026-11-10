//! `clean` 子命令：文件夹清理。默认只预演，`--yes` 才真正删除（破坏性操作两步走）。

use std::path::Path;

use sl_core::fsops::clean::CleanPolicy;
use sl_core::i18n::{t, tf};
use sl_core::store::Store;
use sl_core::usecase::{clean_preview, clean_run, CleanTarget};
use sl_core::{SlError, SlResult};

use crate::parse::Parsed;
use crate::timefmt::{ago_text, human_duration, local_time_of_day_secs, now_utc_secs, parse_when};

/// 从参数装配五策略之一（互斥）。`water sweep` 复用同一套策略参数。
pub(crate) fn build_clean_policy(p: &Parsed, now_utc: i64, tod_secs: i64) -> SlResult<CleanPolicy> {
    let empty = p.has("empty");
    let yesterday = p.has("yesterday");
    let older = p.value("older-than");
    let from = p.value("from");
    let to = p.value("to");
    let keep = p.value("keep");
    let range = from.is_some() || to.is_some();

    let chosen = [empty, yesterday, older.is_some(), range, keep.is_some()]
        .iter()
        .filter(|b| **b)
        .count();
    if chosen == 0 {
        return Err(SlError::Other(
            "请指定清理策略：--empty / --yesterday / --older-than <时长> / --from <起> --to <止> / --keep <MB>"
                .into(),
        ));
    }
    if chosen > 1 {
        return Err(SlError::Other("清理策略五选一，不能同时指定多个".into()));
    }
    if empty {
        return Ok(CleanPolicy::Empty);
    }
    if yesterday {
        return Ok(CleanPolicy::Before {
            cutoff: now_utc - tod_secs,
        });
    }
    if let Some(v) = older {
        return Ok(CleanPolicy::Before {
            cutoff: parse_when(v, now_utc, tod_secs)?,
        });
    }
    if let Some(v) = keep {
        let mb: u64 = v
            .trim()
            .parse()
            .map_err(|_| SlError::Other("--keep 必须是正整数（单位 MB，水位下限）".into()))?;
        if mb == 0 {
            return Err(SlError::Other("--keep 不能为 0（单位 MB）".into()));
        }
        return Ok(CleanPolicy::KeepWithin {
            keep_bytes: mb.saturating_mul(1024 * 1024),
        });
    }
    let f = from.ok_or_else(|| SlError::Other("--from 与 --to 需成对给出".into()))?;
    let t = to.ok_or_else(|| SlError::Other("--from 与 --to 需成对给出".into()))?;
    Ok(CleanPolicy::Between {
        from: parse_when(f, now_utc, tod_secs)?,
        to: parse_when(t, now_utc, tod_secs)?,
    })
}

/// 把策略渲染成中文（相对时长口径，供预演展示）。`water sweep` 复用。
pub(crate) fn describe_policy(policy: &CleanPolicy, now_utc: i64) -> String {
    match policy {
        CleanPolicy::Empty => t("cli.clean.policy_empty").to_string(),
        CleanPolicy::Before { cutoff } => {
            let dur = human_duration(now_utc - *cutoff);
            tf("cli.clean.policy_before", &[("dur", dur.as_str())])
        }
        CleanPolicy::Between { from, to } => {
            let from_s = ago_text(now_utc - *from);
            let to_s = ago_text(now_utc - *to);
            tf(
                "cli.clean.policy_between",
                &[("from", from_s.as_str()), ("to", to_s.as_str())],
            )
        }
        CleanPolicy::KeepWithin { keep_bytes } => {
            let size = sl_core::util::human_bytes(*keep_bytes);
            tf("cli.clean.policy_keep", &[("size", size.as_str())])
        }
    }
}

/// 解析清理目标：数字当规则 id（清它真正落地的目录，自动带上规则排除），
/// 否则当目录路径（展开 `%VAR%`、过黑名单都在 usecase 层）。
fn parse_clean_target(target: &str) -> CleanTarget {
    match target.parse::<i64>() {
        Ok(id) => CleanTarget::Rule { id },
        Err(_) => CleanTarget::Folder {
            path: target.to_string(),
        },
    }
}

pub(crate) fn cmd_clean(store: &Store, p: &Parsed) -> SlResult<()> {
    let target_str = p.required(0, "<规则id | 目录路径>")?;
    let now_utc = now_utc_secs();
    let tod = local_time_of_day_secs() as i64;
    let policy = build_clean_policy(p, now_utc, tod)?;
    let target = parse_clean_target(&target_str);

    // 默认预演；只有显式 --yes 才真正删除（破坏性操作，两步走）。
    if !p.has("yes") {
        let pv = clean_preview(store, &target, &policy, &[])?;
        if p.json() {
            println!(
                "{}",
                serde_json::json!({ "policy": policy, "dry_run": true, "preview": pv })
            );
            return Ok(());
        }
        println!("{}", tf("cli.clean.target", &[("label", pv.label.as_str())]));
        println!("{}", tf("cli.clean.dir", &[("dir", pv.dir.as_str())]));
        let policy_desc = describe_policy(&policy, now_utc);
        println!("{}", tf("cli.clean.policy", &[("policy", policy_desc.as_str())]));
        println!();
        if !Path::new(&pv.dir).is_dir() {
            println!("{}", t("cli.clean.no_dir"));
            return Ok(());
        }
        if pv.files == 0 {
            println!("{}", t("cli.clean.no_files"));
            return Ok(());
        }
        println!("{}", tf("cli.clean.will_delete", &[("summary", pv.summary.as_str())]));
        if pv.links_skipped > 0 {
            println!(
                "{}",
                tf("cli.clean.links_skipped", &[("n", &pv.links_skipped.to_string())])
            );
        }
        if pv.excluded_skipped > 0 {
            println!(
                "{}",
                tf("cli.clean.excluded_skipped", &[("n", &pv.excluded_skipped.to_string())])
            );
        }
        for s in &pv.sample {
            let size = sl_core::util::human_bytes(s.bytes);
            let ago = ago_text(now_utc - s.mtime);
            println!(
                "{}",
                tf(
                    "cli.clean.sample_row",
                    &[("path", s.path.as_str()), ("size", size.as_str()), ("ago", ago.as_str())]
                )
            );
        }
        if pv.files > pv.sample.len() as u64 {
            println!(
                "{}",
                tf(
                    "cli.clean.sample_more",
                    &[("shown", &pv.sample.len().to_string()), ("total", &pv.files.to_string())]
                )
            );
        }
        println!();
        println!("{}", t("cli.clean.dry_run_hint"));
        return Ok(());
    }

    let quiet = p.json();
    let mut sink = |prog: &sl_core::fsops::mover::Progress| {
        if quiet {
            return;
        }
        if let sl_core::fsops::mover::Progress::Deleting { files } = prog {
            print!(
                "{}",
                tf("cli.clean.deleting_progress", &[("files", &files.to_string())])
            );
            use std::io::Write;
            let _ = std::io::stdout().flush();
        }
    };
    // 事件流水由 usecase 层统一落库（原 CLI 自行记一条会重复）。
    let report = clean_run(store, &target, &policy, &[], false, &mut sink)?;
    if !quiet {
        println!();
    }
    if p.json() {
        println!(
            "{}",
            serde_json::json!({ "policy": policy, "dry_run": false, "result": report })
        );
    } else {
        println!("✅ {}", report.summary);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_700_000_000;
    const TOD: i64 = 3600 * 10 + 200; // 当天已过 10:03:20

    fn parsed(args: &[&str]) -> Parsed {
        Parsed::parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn build_policy_five_modes() {
        assert_eq!(
            build_clean_policy(&parsed(&["--empty"]), NOW, TOD).unwrap(),
            CleanPolicy::Empty
        );
        assert_eq!(
            build_clean_policy(&parsed(&["--yesterday"]), NOW, TOD).unwrap(),
            CleanPolicy::Before { cutoff: NOW - TOD }
        );
        assert_eq!(
            build_clean_policy(&parsed(&["--older-than", "1h"]), NOW, TOD).unwrap(),
            CleanPolicy::Before { cutoff: NOW - 3600 }
        );
        assert_eq!(
            build_clean_policy(&parsed(&["--from", "2d", "--to", "1h"]), NOW, TOD).unwrap(),
            CleanPolicy::Between {
                from: NOW - 2 * 86400,
                to: NOW - 3600
            }
        );
        assert_eq!(
            build_clean_policy(&parsed(&["--keep", "100"]), NOW, TOD).unwrap(),
            CleanPolicy::KeepWithin {
                keep_bytes: 100 * 1024 * 1024
            }
        );
    }

    #[test]
    fn build_policy_rejects_bad_combos() {
        assert!(build_clean_policy(&parsed(&[]), NOW, TOD).is_err(), "无策略应报错");
        assert!(
            build_clean_policy(&parsed(&["--empty", "--yesterday"]), NOW, TOD).is_err(),
            "多策略应报错"
        );
        assert!(
            build_clean_policy(&parsed(&["--from", "2d"]), NOW, TOD).is_err(),
            "--from 缺 --to 应报错"
        );
        assert!(
            build_clean_policy(&parsed(&["--keep", "0"]), NOW, TOD).is_err(),
            "--keep 0 应报错"
        );
        assert!(
            build_clean_policy(&parsed(&["--keep", "x"]), NOW, TOD).is_err(),
            "--keep 非整数应报错"
        );
        assert!(
            build_clean_policy(&parsed(&["--keep", "100", "--empty"]), NOW, TOD).is_err(),
            "--keep 与其他策略同用应报错"
        );
    }
}