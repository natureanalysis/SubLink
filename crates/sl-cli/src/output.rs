//! 控制台输出辅助：状态行、规则行、操作结果的格式化。

use sl_core::engine;
use sl_core::i18n::{t, tf};
use sl_core::store::Rule;

pub(crate) fn print_status_line(i: usize, st: &engine::RuleStatus) {
    println!(
        "{} #{:<4} [{}] {}",
        st.badge,
        st.rule_id,
        sl_core::store::category_label(&st.category),
        st.name
    );
    println!("{}", tf("cli.label_source", &[("path", &st.source)]));
    println!(
        "{}",
        tf("cli.label_target", &[
            ("path", &st.target),
            ("link", &st.link_type),
        ])
    );
    println!(
        "{}",
        tf("cli.label_status", &[
            ("label", st.label.as_str()),
            ("boot", yn(st.rebuild_on_boot)),
            ("enabled", yn(st.want_enabled)),
        ])
    );
    for w in &st.warnings {
        println!("       ⚠ {w}");
    }
    if i > 0 {
        println!();
    }
}

pub(crate) fn print_rule_row(rule: &Rule) {
    println!(
        "#{:<4} [{}] {}  →  {}",
        rule.id,
        sl_core::store::category_label(&rule.category),
        rule.name,
        rule.target_subpath
    );
    println!(
        "{}",
        tf("cli.rule_detail", &[
            ("src", &rule.source_template),
            ("link", &rule.link_type),
            ("on", yn(rule.want_enabled)),
            ("boot", yn(rule.rebuild_on_boot)),
            ("elev", yn(rule.needs_elevation)),
        ])
    );
    // 「系统设置模式」额外说明改的是哪些变量、写到哪一层 —— 这决定了
    // 它影响范围有多大（用户级只影响本人，机器级影响全机）。
    if rule.redirect_mode == sl_core::store::REDIRECT_MODE_SETTING {
        let scope = sl_core::sysenv::EnvScope::parse(&rule.env_scope)
            .map(|s| s.label().to_string())
            .unwrap_or_else(|| rule.env_scope.clone());
        println!(
            "{}",
            tf("cli.rule_detail_setting", &[
                ("vars", &rule.env_vars.join("、")),
                ("scope", &scope),
            ])
        );
    }
    if let Some(n) = &rule.notes {
        println!("{}", tf("cli.label_notes", &[("notes", n)]));
    }
    if !rule.excludes.is_empty() {
        println!(
            "{}",
            tf("cli.label_excludes", &[("list", &rule.excludes.join("、"))])
        );
    }
    if !rule.auto_clean_policy.trim().is_empty() {
        println!(
            "{}",
            tf("cli.label_auto_clean", &[("policy", &rule.auto_clean_policy)])
        );
    }
    if rule.auto_clean_limit_bytes > 0 {
        println!(
            "{}",
            tf("cli.label_auto_clean_limit", &[(
                "size",
                &sl_core::util::human_bytes(rule.auto_clean_limit_bytes as u64)
            )])
        );
    }
}

pub(crate) fn yn(v: bool) -> &'static str {
    if v {
        t("cli.yes")
    } else {
        t("cli.no")
    }
}

pub(crate) fn print_outcome(o: &engine::Outcome) {
    let tag = if o.dry_run {
        t("cli.tag_dry_run")
    } else {
        match o.action {
            "already_ok" | "already_plain" => t("cli.tag_skip"),
            "moved_and_linked" | "merged_and_linked" | "created_and_linked" => t("cli.tag_done"),
            "fixed_broken_target" => t("cli.tag_fix"),
            "restored" => t("cli.tag_restore"),
            "relinked" => t("cli.tag_relink"),
            _ => t("cli.tag_done"),
        }
    };
    println!(
        "  [{tag}] #{:<4} {:<20} {}",
        o.rule_id, o.name, o.message
    );
    if let Some(s) = o.stats {
        if s.files > 0 || s.bytes > 0 {
            println!(
                "{}",
                tf("cli.label_scale", &[
                    ("stats", &s.summary()),
                    ("dur", &sl_core::util::human_dur_ms(o.elapsed_ms)),
                ])
            );
        }
    }
}