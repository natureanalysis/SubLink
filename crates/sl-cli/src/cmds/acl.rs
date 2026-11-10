//! `acl` 子命令：权限体检 / 修复。
//!
//! 对应使用者 = **当前进程令牌里的那个账号**（提权不改变用户 SID）。
//! 逐个检查并修复：目标根、每条规则的目标目录、源路径上的链接条目。

use sl_core::i18n::{t, tf};
use sl_core::store::Store;
use sl_core::usecase::{acl_fix, AclItem};
use sl_core::SlResult;

use crate::parse::Parsed;

pub(crate) fn cmd_acl(store: &Store, p: &Parsed) -> SlResult<()> {
    match p.pos(0).unwrap_or("check") {
        "check" => run(store, p, true),
        "fix" => run(store, p, false),
        other => Err(sl_core::SlError::Other(tf(
            "cli.acl.unknown_sub",
            &[("sub", other)],
        ))),
    }
}

fn run(store: &Store, p: &Parsed, dry_run: bool) -> SlResult<()> {
    // --sid：提权运行时锁定要授权的账号。UAC 里换了管理员账号登录的话，
    // 令牌里的 SID 就变了 —— 那时候要授的仍然是原来那个使用者。
    let sid = p.value("sid").map(|s| s.trim()).filter(|s| !s.is_empty());
    let report = acl_fix(store, dry_run, sid)?;

    if p.has("json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&report)
                .unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
        );
        return Ok(());
    }

    println!("{}", t("cli.acl.title"));
    let who = if report.user_name.trim().is_empty() {
        report.user_sid.clone()
    } else {
        format!("{}（{}）", report.user_name, report.user_sid)
    };
    println!("{}", tf("cli.acl.identity", &[("who", who.as_str())]));
    println!();

    for it in &report.items {
        println!("{}", format_item(it));
    }

    println!();
    println!(
        "{}",
        tf(
            "cli.acl.summary",
            &[
                ("fixed", &report.fixed.to_string()),
                ("already", &report.already_ok.to_string()),
                ("pending", &report.pending.to_string()),
                ("skipped", &report.skipped.to_string()),
                ("failed", &report.failed.to_string()),
            ]
        )
    );

    if report.failed > 0 {
        return Err(sl_core::SlError::Other(tf(
            "cli.acl.failed_count",
            &[("n", &report.failed.to_string())],
        )));
    }
    Ok(())
}

fn format_item(it: &AclItem) -> String {
    let kind = match it.kind {
        "root" => t("cli.acl.kind_root"),
        "link" => t("cli.acl.kind_link"),
        _ => t("cli.acl.kind_target"),
    };
    let icon = match it.state {
        "fixed" => "🔧",
        "already_ok" => "✅",
        "pending" => "⏳",
        "failed" => "❌",
        _ => "⏭️",
    };
    let name = match it.rule_id {
        Some(id) => format!("#{id} {}", it.name),
        None => it.name.clone(),
    };
    format!(
        "{icon} [{kind}] {name} → {}\n       {}",
        it.path, it.message
    )
}
