//! 体检：单条/批量规则状态扫描，以及写进规则库的状态机键。

use serde::Serialize;

use crate::error::SlResult;
use crate::link::detect::{detect_link, LinkState, TargetState};
use crate::store::{Rule, Store};

use super::resolve::resolve;

/// 一条规则的实时体检结果。
#[derive(Debug, Clone, Serialize)]
pub struct RuleStatus {
    pub rule_id: i64,
    pub name: String,
    pub category: String,
    pub source: String,
    pub target: String,
    pub root: String,
    pub link_type: String,
    /// 重定向方式：`link` / `setting`。
    pub redirect_mode: String,
    /// 「系统设置模式」下要改的环境变量名。
    pub env_vars: Vec<String>,
    /// 「系统设置模式」的写入作用域（`user` / `machine`）。
    pub env_scope: String,
    pub want_enabled: bool,
    pub rebuild_on_boot: bool,
    /// UI 徽标（✅ / ⚠️ / ❌ / 🚫 / ❔），直接来自 `LinkState::badge()`。
    pub badge: &'static str,
    pub label: String,
    pub state: LinkState,
    pub needs_attention: bool,
    pub warnings: Vec<String>,
    pub unknown_vars: Vec<String>,
}

pub fn rule_status(store: &Store, rule: &Rule) -> SlResult<RuleStatus> {
    let r = resolve(store, rule)?;
    // 「系统设置模式」的判定依据是环境变量指向哪儿，而不是源路径上有没有链接。
    let state = if super::resolve::is_setting_mode(rule) {
        // 规则配置有问题时**降级成状态**，不让错误冒泡：
        // 一条存量规则里写了后来被移出白名单的变量（比如旧版的 LOCALAPPDATA），
        // 若直接报错，整个 status / doctor 都会失败 ——
        // 一条坏规则不该拖垮全部体检。
        match super::setting::rule_scope(rule).and_then(|scope| {
            super::setting::rule_vars(rule).map(|vars| (scope, vars))
        }) {
            Ok((scope, vars)) => super::setting::setting_state(scope, &vars, &r.target),
            Err(e) => LinkState::ConfigInvalid {
                reason: e.to_string(),
            },
        }
    } else {
        detect_link(&r.source, Some(&r.target))
    };
    Ok(RuleStatus {
        rule_id: rule.id,
        name: rule.name.clone(),
        category: rule.category.clone(),
        source: r.source.display().to_string(),
        target: r.target.display().to_string(),
        root: r.root.label.clone(),
        link_type: rule.link_type.clone(),
        redirect_mode: rule.redirect_mode.clone(),
        env_vars: rule.env_vars.clone(),
        env_scope: rule.env_scope.clone(),
        want_enabled: rule.want_enabled,
        rebuild_on_boot: rule.rebuild_on_boot,
        badge: state.badge(),
        label: state.label(),
        needs_attention: state.needs_attention(),
        warnings: r.warnings.clone(),
        unknown_vars: r.unknown_vars.clone(),
        state,
    })
}

/// 批量体检。`persist` 为真时把结果写回规则库的状态缓存。
pub fn status_all(
    store: &Store,
    category: Option<&str>,
    enabled_only: bool,
    keyword: Option<&str>,
    persist: bool,
) -> SlResult<Vec<RuleStatus>> {
    let rules = store.query_rules(category, enabled_only, keyword)?;
    let mut out = Vec::with_capacity(rules.len());
    for rule in &rules {
        let st = rule_status(store, rule)?;
        if persist {
            let err = match &st.state {
                LinkState::IoError { message } => Some(message.as_str()),
                _ => None,
            };
            store.set_rule_state(rule.id, state_key(&st.state), err)?;
        }
        out.push(st);
    }
    Ok(out)
}

/// 状态机键（写进 DB 的 `last_state`）。
pub fn state_key(state: &LinkState) -> &'static str {
    match state {
        LinkState::SourceMissing => "source_missing",
        LinkState::PlainDir => "plain_dir",
        LinkState::PlainFile => "plain_file",
        LinkState::LinkOk {
            target_state: TargetState::Empty,
            ..
        } => "link_ok_empty",
        LinkState::LinkOk { .. } => "link_ok",
        LinkState::LinkBroken { .. } => "link_broken",
        LinkState::ForeignReparse { .. } => "foreign_reparse",
        LinkState::AccessDenied => "access_denied",
        LinkState::IoError { .. } => "io_error",
        LinkState::SettingApplied { .. } => "setting_ok",
        LinkState::SettingAppliedEmpty { .. } => "setting_ok_empty",
        LinkState::SettingMismatch { .. } => "setting_mismatch",
        LinkState::ConfigInvalid { .. } => "config_invalid",
    }
}