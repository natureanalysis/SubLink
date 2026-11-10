//! 概览用例：界面快照、目标根/规则视图、事件与操作流水、上次开机报告。

use crate::boot::{probe_volume, BootReport};
use crate::engine;
use crate::error::SlResult;
use crate::i18n::{t, tf};
use crate::paths;
use crate::store::{EventRow, Operation, Rule, Store, CATEGORIES};

use super::view::{Banner, RootView, RuleView, Stats, UiSnapshot, UndoView, VERSION};

pub fn snapshot(store: &Store) -> SlResult<UiSnapshot> {
    let roots = root_views(store)?;
    let rules = rule_views(store)?;

    let mut stats = Stats {
        total: rules.len(),
        ..Default::default()
    };
    for r in &rules {
        if r.want_enabled {
            stats.enabled += 1;
        }
        if r.needs_attention {
            stats.needs_attention += 1;
        }
        match r.badge.as_str() {
            "✅" => stats.ok += 1,
            "⚠️" => stats.warn += 1,
            "❌" => stats.plain += 1,
            _ => stats.other += 1,
        }
    }

    let open_ops = store.list_open_ops()?;
    let mut banners: Vec<Banner> = Vec::new();

    // 登录任务是静默跑的：它出问题必须在这里被看到，而不是躺在日志里
    if let Some((at, summary)) = last_boot_problem(store) {
        banners.push(Banner {
            level: "warn",
            text: tf("banner.boot_problem", &[("at", &at), ("summary", &summary)]),
            action: None,
        });
    }

    let roots_ready = roots
        .iter()
        .filter(|r| r.state == "ready" || r.state == "not_created")
        .count();
    if roots.is_empty() {
        banners.push(Banner {
            level: "warn",
            text: t("banner.no_roots").into(),
            action: None,
        });
    } else if roots_ready == 0 {
        banners.push(Banner {
            level: "error",
            text: t("banner.all_roots_down").into(),
            action: None,
        });
    }
    if stats.needs_attention > 0 {
        banners.push(Banner {
            level: "warn",
            text: tf(
                "banner.needs_attention",
                &[("n", &stats.needs_attention.to_string())],
            ),
            action: None,
        });
    }
    if !open_ops.is_empty() {
        banners.push(Banner {
            level: "error",
            text: tf("banner.open_ops", &[("n", &open_ops.len().to_string())]),
            action: None,
        });
    }
    for r in roots.iter().filter(|r| r.state != "ready") {
        banners.push(Banner {
            level: "warn",
            text: tf(
                "banner.root_error",
                &[
                    ("id", &r.id.to_string()),
                    ("path", &r.path),
                    ("message", &r.message),
                ],
            ),
            action: None,
        });
    }

    // RAM 盘水位：写满会导致软件写缓存直接失败，必须顶在概览上（只读，事件归 water_poll）。
    for w in super::water::water_levels(store)? {
        match w.level.as_str() {
            "critical" => banners.push(Banner {
                level: "error",
                text: tf(
                    "banner.disk_critical",
                    &[
                        ("label", &w.label),
                        ("path", &w.path),
                        ("pct", &format!("{:.1}", w.used_pct)),
                        ("free", &w.free_text),
                    ],
                ),
                action: Some("sweep".into()),
            }),
            "warn" => banners.push(Banner {
                level: "warn",
                text: tf(
                    "banner.disk_warn",
                    &[
                        ("label", &w.label),
                        ("path", &w.path),
                        ("pct", &format!("{:.1}", w.used_pct)),
                        ("free", &w.free_text),
                    ],
                ),
                action: Some("sweep".into()),
            }),
            _ => {}
        }
    }

    Ok(UiSnapshot {
        version: VERSION.to_string(),
        schema_version: store.user_version()?,
        data_dir: paths::data_dir().display().to_string(),
        db_path: paths::db_path().display().to_string(),
        categories: {
            let mut cats: Vec<String> = CATEGORIES.iter().map(|s| s.to_string()).collect();
            // 批量归类允许新建分类：把库里实际用到的自定义分类追加到内置清单之后，
            // 侧栏、筛选下拉和行内编辑才能继续列出来。
            for r in &rules {
                if !cats.iter().any(|c| c == &r.category) {
                    cats.push(r.category.clone());
                }
            }
            cats
        },
        roots,
        rules,
        stats,
        banners,
        open_ops,
        undo: {
            let st = super::rules::undo_state(store)?;
            UndoView {
                pending: st.pending,
                kind: st.op.unwrap_or_default(),
                names: st.names,
            }
        },
    })
}

pub fn root_views(store: &Store) -> SlResult<Vec<RootView>> {
    let mut out = Vec::new();
    for r in store.list_roots()? {
        let (path, unknown) = r.resolved_path();
        let (state, message) = probe_volume(&path);
        out.push(RootView {
            id: r.id,
            label: r.label,
            path: r.path,
            is_ramdisk: r.is_ramdisk,
            wait_timeout_ms: r.wait_timeout_ms,
            state,
            message,
            unknown_vars: unknown,
        });
    }
    Ok(out)
}

pub fn rule_views(store: &Store) -> SlResult<Vec<RuleView>> {
    // 隐藏态存在 settings 里（不是 rules 表的列）：一次读出来，逐条比对，
    // 不在这里反复查库。
    let hidden: std::collections::HashSet<i64> =
        super::rules::hidden_rule_ids(store)?.into_iter().collect();
    let mut out = Vec::new();
    for rule in store.list_rules()? {
        let resolved = engine::resolve(store, &rule)?;
        let status = engine::rule_status(store, &rule)?;
        let is_hidden = hidden.contains(&rule.id);
        out.push(merge_view(rule, resolved, status, is_hidden));
    }
    Ok(out)
}

fn merge_view(
    rule: Rule,
    resolved: engine::Resolved,
    status: engine::RuleStatus,
    hidden: bool,
) -> RuleView {
    // 「系统设置模式」能不能用，取决于源路径是不是某个 TEMP/TMP 当前指向的目录。
    // 已经是 setting 模式的规则保留它自己的组合（哪怕此刻对不上），
    // 否则用户一编辑就会把规则悄悄降级成链接模式。
    // （必须在 move 掉 `rule` 的字段之前算完。）
    let setting_options = if rule.redirect_mode == crate::store::REDIRECT_MODE_SETTING {
        if rule.env_vars.is_empty() {
            Vec::new()
        } else {
            vec![engine::SettingOption {
                scope: rule.env_scope.clone(),
                vars: rule.env_vars.clone(),
            }]
        }
    } else {
        engine::setting_options_for_source(&resolved.source)
    };
    RuleView {
        id: rule.id,
        name: rule.name,
        category: rule.category,
        source_template: rule.source_template,
        source: resolved.source.display().to_string(),
        target: resolved.target.display().to_string(),
        target_root_id: rule.target_root_id,
        target_subpath: rule.target_subpath,
        link_type: rule.link_type,
        redirect_mode: rule.redirect_mode,
        env_vars: rule.env_vars,
        env_scope: rule.env_scope,
        setting_options,
        want_enabled: rule.want_enabled,
        rebuild_on_boot: rule.rebuild_on_boot,
        has_acl: rule.acl_sddl.is_some(),
        needs_elevation: rule.needs_elevation,
        notes: rule.notes,
        excludes: rule.excludes,
        auto_clean_policy: rule.auto_clean_policy,
        auto_clean_limit_bytes: rule.auto_clean_limit_bytes,
        hidden,
        badge: status.badge.to_string(),
        label: status.label,
        needs_attention: status.needs_attention,
        warnings: status.warnings,
        unknown_vars: status.unknown_vars,
        last_files: rule.last_files,
        last_bytes: rule.last_bytes,
        applied_at: rule.applied_at,
    }
}

pub fn recent_events(store: &Store, limit: i64) -> SlResult<Vec<EventRow>> {
    store.recent_events(limit)
}

pub fn recent_ops(store: &Store, limit: i64) -> SlResult<Vec<Operation>> {
    store.recent_ops(limit)
}

/// 上一次开机重建是不是有问题（界面顶部提示用）。返回 `(时间, 摘要)`。
///
/// 普通任务与高权限任务各写一份报告，这里把两份合起来看：
/// 任何一份有问题都要提示，不能因为另一份正常就把它盖掉。
pub fn last_boot_problem(store: &Store) -> Option<(String, String)> {
    let mut at_parts: Vec<String> = Vec::new();
    let mut summary_parts: Vec<String> = Vec::new();
    for (report_key, at_key) in [
        ("last_boot_report", "last_boot_at"),
        ("last_boot_report_elevated", "last_boot_at_elevated"),
    ] {
        let Some(json) = store.get_setting(report_key).ok().flatten() else {
            continue;
        };
        let Ok(report) = serde_json::from_str::<BootReport>(&json) else {
            continue;
        };
        if !report.has_problem() {
            continue;
        }
        if let Some(at) = store.get_setting(at_key).ok().flatten() {
            if !at.is_empty() {
                at_parts.push(at);
            }
        }
        summary_parts.push(report.one_line());
    }
    if summary_parts.is_empty() {
        return None;
    }
    Some((at_parts.join(" / "), summary_parts.join("；")))
}

#[cfg(test)]
mod tests {
    //! 视图里的 `setting_options` —— 前端靠它决定要不要给出「系统设置」选项。

    use crate::store::{NewRule, Store, REDIRECT_MODE_LINK, REDIRECT_MODE_SETTING};

    fn rule(name: &str, src: &str, mode: &str, vars: &[&str]) -> NewRule {
        NewRule {
            name: name.into(),
            category: "system".into(),
            source_template: src.into(),
            target_root_id: 1,
            target_subpath: "system\\probe".into(),
            link_type: "junction".into(),
            redirect_mode: mode.into(),
            env_vars: vars.iter().map(|s| s.to_string()).collect(),
            env_scope: "user".into(),
            want_enabled: true,
            rebuild_on_boot: true,
            notes: None,
            excludes: Vec::new(),
            auto_clean_policy: String::new(),
            auto_clean_limit_bytes: 0,
        }
    }

    fn store_with_root() -> Store {
        let store = Store::open_in_memory().unwrap();
        store
            .ensure_root("R", "R:\\", true, 1000)
            .expect("加目标根");
        store
    }

    /// 视图必须带上这个字段 —— 前端按它过滤下拉里的「系统设置」选项。
    #[test]
    fn rule_view_carries_setting_options() {
        let store = store_with_root();
        store
            .insert_rule(&rule("探针", r"Z:\nope-8f2c1a", REDIRECT_MODE_LINK, &[]))
            .unwrap();
        let views = super::rule_views(&store).unwrap();
        let json = serde_json::to_string(&views[0]).unwrap();
        assert!(
            json.contains("setting_options"),
            "规则视图必须带 setting_options 字段，前端靠它过滤选项"
        );
    }

    /// 已经「系统设置」模式的规则，即便此刻变量指向对不上，
    /// 也要保留它自己的组合 —— 否则用户一编辑就把它降级成链接模式了。
    #[test]
    fn setting_rule_keeps_its_own_combination() {
        let store = store_with_root();
        store
            .insert_rule(&rule(
                "已设置",
                r"Z:\nope-8f2c1a",
                REDIRECT_MODE_SETTING,
                &["TEMP", "TMP"],
            ))
            .unwrap();
        let views = super::rule_views(&store).unwrap();
        let opts = &views[0].setting_options;
        assert_eq!(opts.len(), 1, "存量 setting 规则应保留一个组合");
        assert_eq!(opts[0].scope, "user");
        assert_eq!(opts[0].vars, vec!["TEMP".to_string(), "TMP".to_string()]);
    }

    /// 链接模式的规则，源路径没人指向时就没有可行组合。
    #[test]
    fn link_rule_without_match_has_no_options() {
        let store = store_with_root();
        store
            .insert_rule(&rule("普通", r"Z:\nope-8f2c1a", REDIRECT_MODE_LINK, &[]))
            .unwrap();
        let views = super::rule_views(&store).unwrap();
        assert!(
            views[0].setting_options.is_empty(),
            "源路径没人指向时不应有可行组合"
        );
    }
}
