//! 权限修复用例：把**所有**目标根 / 目标目录 / 链接条目的权限补成
//! "对应使用者完全控制"。
//!
//! ## 为什么需要单独一条用例
//!
//! [`crate::acl`] 的授权是在**创建**目录 / 建链的那一刻顺手做的。
//! 但在这条保证落地**之前**就已经建好的那些文件夹，权限不会自己变好：
//!
//! * `apply` 遇到"链接正常 + 目标有内容"会直接幂等返回，**不会**回头补权限；
//! * 开机重建只在目标目录**消失**时才会重新建目录并授权，目录还在就不管。
//!
//! 所以要有一条"回头把已有的都修一遍"的路径 —— 这就是本模块。
//!
//! ## 修什么、不修什么
//!
//! | 对象 | 处理 |
//! |---|---|
//! | 目标根（RAM 盘下的重定向目录） | 授权 |
//! | 每条规则的目标目录 | 授权（目录不存在则跳过，开机重建时会建出来并授权） |
//! | 源路径上的链接条目 | 授权（还没建链则跳过） |
//! | **卷根**（`R:\`、`C:\` 这种） | **跳过** —— 改卷根等于改整块盘的安全策略，不是本工具该做的事 |
//!
//! 对应使用者 = **当前进程令牌里的那个账号**（见 [`crate::acl::current_user_sid`]）。

use std::path::Path;

use serde::Serialize;

use crate::acl;
use crate::error::SlResult;
use crate::i18n::{t, tf};
use crate::link::detect::{detect_link, LinkState};
use crate::store::Store;

/// 一个被检查 / 修复的目录。
#[derive(Debug, Clone, Serialize)]
pub struct AclItem {
    /// `root`（目标根）/ `target`（规则的目标目录）/ `link`（链接条目）。
    pub kind: &'static str,
    pub rule_id: Option<i64>,
    pub name: String,
    pub path: String,
    /// `fixed` / `already_ok` / `pending` / `skipped` / `failed`。
    pub state: &'static str,
    pub message: String,
}

/// 一次权限体检 / 修复的完整结果。
#[derive(Debug, Clone, Serialize)]
pub struct AclReport {
    /// 对应使用者的 SID。
    pub user_sid: String,
    /// 对应使用者的账号名（`域\用户`），拿不到时为空。
    pub user_name: String,
    pub items: Vec<AclItem>,
    pub fixed: usize,
    pub already_ok: usize,
    pub pending: usize,
    pub skipped: usize,
    pub failed: usize,
}

/// 体检 + 修复（`dry_run = true` 时只体检不改）。
///
/// `sid` 指定要授权给谁；不给就用**当前进程令牌里的那个账号**。
/// 提权运行时必须显式传：UAC 里如果换用另一个管理员账号登录，
/// 令牌里的 SID 就变成那个人了 —— 那时候要授的仍然是原来那个使用者。
///
/// 返回逐项结果，调用方（CLI / GUI）自己决定怎么呈现。
pub fn acl_fix(store: &Store, dry_run: bool, sid: Option<&str>) -> SlResult<AclReport> {
    let sid = match sid.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => s.to_string(),
        None => acl::current_user_sid()?,
    };
    let mut items: Vec<AclItem> = Vec::new();

    // ---- 1. 目标根 ----
    for root in store.list_roots()? {
        let (path, unknown) = root.resolved_path();
        let path_str = path.display().to_string();
        if !unknown.is_empty() {
            items.push(AclItem {
                kind: "root",
                rule_id: None,
                name: root.label.clone(),
                path: path_str,
                state: "skipped",
                message: tf("acl.fix.skip_unresolved", &[("vars", &unknown.join("、"))]),
            });
            continue;
        }
        // 卷根不自动改：那是整块盘的安全策略，不该由"修缓存目录权限"顺手改掉。
        if acl::is_volume_root(&path) {
            items.push(AclItem {
                kind: "root",
                rule_id: None,
                name: root.label.clone(),
                path: path_str,
                state: "skipped",
                message: t("acl.fix.skip_volume_root").into(),
            });
            continue;
        }
        let (state, message) = ensure_dir_permission(&path, &sid, dry_run);
        items.push(AclItem {
            kind: "root",
            rule_id: None,
            name: root.label.clone(),
            path: path_str,
            state,
            message,
        });
    }

    // ---- 2. 规则：目标目录 + 链接条目 ----
    for rule in store.list_rules()? {
        let resolved = match crate::engine::resolve(store, &rule) {
            Ok(r) => r,
            Err(e) => {
                items.push(AclItem {
                    kind: "target",
                    rule_id: Some(rule.id),
                    name: rule.name.clone(),
                    path: String::new(),
                    state: "failed",
                    message: e.to_string(),
                });
                continue;
            }
        };

        if !resolved.unknown_vars.is_empty() {
            let msg = tf("acl.fix.skip_unresolved", &[("vars", &resolved.unknown_vars.join("、"))]);
            items.push(AclItem {
                kind: "target",
                rule_id: Some(rule.id),
                name: rule.name.clone(),
                path: resolved.target.display().to_string(),
                state: "skipped",
                message: msg.clone(),
            });
            items.push(AclItem {
                kind: "link",
                rule_id: Some(rule.id),
                name: rule.name.clone(),
                path: resolved.source.display().to_string(),
                state: "skipped",
                message: msg,
            });
            continue;
        }

        // 目标目录：不存在就跳过（开机重建会把它建出来并顺手授权）
        let target_item = if resolved.target.is_dir() {
            let (state, message) = ensure_dir_permission(&resolved.target, &sid, dry_run);
            AclItem {
                kind: "target",
                rule_id: Some(rule.id),
                name: rule.name.clone(),
                path: resolved.target.display().to_string(),
                state,
                message,
            }
        } else {
            AclItem {
                kind: "target",
                rule_id: Some(rule.id),
                name: rule.name.clone(),
                path: resolved.target.display().to_string(),
                state: "skipped",
                message: t("acl.fix.skip_missing").into(),
            }
        };
        items.push(target_item);

        // 链接条目：只有已经建链的才需要授权
        let link_item = match detect_link(&resolved.source, None) {
            LinkState::LinkOk { .. } | LinkState::LinkBroken { .. } => {
                let (state, message) = ensure_link_permission(&resolved.source, &sid, dry_run);
                AclItem {
                    kind: "link",
                    rule_id: Some(rule.id),
                    name: rule.name.clone(),
                    path: resolved.source.display().to_string(),
                    state,
                    message,
                }
            }
            _ => AclItem {
                kind: "link",
                rule_id: Some(rule.id),
                name: rule.name.clone(),
                path: resolved.source.display().to_string(),
                state: "skipped",
                message: t("acl.fix.skip_not_link").into(),
            },
        };
        items.push(link_item);
    }

    let mut report = AclReport {
        user_sid: sid,
        user_name: acl::current_user_name().unwrap_or_default(),
        items,
        fixed: 0,
        already_ok: 0,
        pending: 0,
        skipped: 0,
        failed: 0,
    };
    for it in &report.items {
        match it.state {
            "fixed" => report.fixed += 1,
            "already_ok" => report.already_ok += 1,
            "pending" => report.pending += 1,
            "skipped" => report.skipped += 1,
            _ => report.failed += 1,
        }
    }
    Ok(report)
}

/// 目录：已有完全控制就什么都不做，否则补一条 ACE。
fn ensure_dir_permission(path: &Path, sid: &str, dry_run: bool) -> (&'static str, String) {
    match acl::has_full_control(path, sid) {
        Ok(true) => return ("already_ok", t("acl.fix.already").into()),
        Ok(false) => {}
        Err(e) => return ("failed", e.to_string()),
    }
    if dry_run {
        return ("pending", t("acl.fix.pending").into());
    }
    match acl::grant_full_control(path) {
        Ok(()) => ("fixed", t("acl.fix.fixed").into()),
        Err(e) => ("failed", e.to_string()),
    }
}

/// 链接条目：同上，但读 / 写的是**链接自身**的 DACL。
fn ensure_link_permission(path: &Path, sid: &str, dry_run: bool) -> (&'static str, String) {
    match acl::has_full_control_on_link(path, sid) {
        Ok(true) => return ("already_ok", t("acl.fix.already").into()),
        Ok(false) => {}
        Err(e) => return ("failed", e.to_string()),
    }
    if dry_run {
        return ("pending", t("acl.fix.pending").into());
    }
    match acl::grant_full_control_on_link(path) {
        Ok(()) => ("fixed", t("acl.fix.fixed").into()),
        Err(e) => ("failed", e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{NewRule, Store};
    use std::fs;
    use std::path::PathBuf;

    fn setup(tag: &str) -> (Store, PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!("td_aclfix_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let source = base.join("cache");
        let ram = base.join("ram");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&ram).unwrap();
        fs::write(source.join("a.txt"), b"aaa").unwrap();

        let store = Store::open_in_memory().unwrap();
        let root_id = store
            .insert_root("RAM", &ram.to_string_lossy(), true, 90000)
            .unwrap();
        store
            .insert_rule(&NewRule {
                name: "测试缓存".into(),
                category: "dev".into(),
                source_template: source.to_string_lossy().into_owned(),
                target_root_id: root_id,
                target_subpath: "dev\\test-cache".into(),
                link_type: "junction".into(),
                redirect_mode: "link".into(),
                env_vars: Vec::new(),
                env_scope: "user".into(),
                want_enabled: true,
                rebuild_on_boot: true,
                notes: None,
                excludes: Vec::new(),
                auto_clean_policy: String::new(),
                auto_clean_limit_bytes: 0,
            })
            .unwrap();
        let target = ram.join("dev").join("test-cache");
        (store, base, target)
    }

    /// 缺权限的目标目录与目标根会被补上；已经有的一律不动（幂等）。
    #[test]
    fn acl_fix_grants_missing_permissions_and_is_idempotent() {
        let (store, base, target) = setup("fix");
        let sid = acl::current_user_sid().unwrap();
        let ram = target.parent().unwrap().parent().unwrap().to_path_buf();

        // 把目标根与目标目录的权限收窄成"使用者只读 + 可改名，没有完全控制"。
        // 用 protected（D:P）切断继承，模拟"目录是别人建的"。
        fs::create_dir_all(&target).unwrap();
        let restricted = format!("D:P(A;OICI;FA;;;SY)(A;OICI;0x1300A9;;;{sid})");
        acl::apply_sddl(&ram, &restricted).unwrap();
        acl::apply_sddl(&target, &restricted).unwrap();
        assert!(!acl::has_full_control(&target, &sid).unwrap());

        // 第一遍：目标根与目标目录都缺权限 → 都该被修好
        let report = acl_fix(&store, false, None).unwrap();
        let root_item = report.items.iter().find(|i| i.kind == "root").unwrap();
        let target_item = report.items.iter().find(|i| i.kind == "target").unwrap();
        let link_item = report.items.iter().find(|i| i.kind == "link").unwrap();
        assert_eq!(root_item.state, "fixed", "{root_item:?}");
        assert_eq!(target_item.state, "fixed", "{target_item:?}");
        assert!(
            acl::has_full_control(&target, &sid).unwrap(),
            "目标目录应拿到完全控制"
        );

        // 源路径还没建链 → 链接条目跳过（没有可授权的东西）
        assert_eq!(link_item.state, "skipped", "{link_item:?}");
        assert_eq!(report.failed, 0, "{:?}", report.items);

        // 第二遍：全都已经是"本来就有"，不再改
        let again = acl_fix(&store, false, None).unwrap();
        assert_eq!(again.fixed, 0, "重复跑不该再改：{:?}", again.items);
        assert_eq!(again.already_ok, 2, "{:?}", again.items);
        assert_eq!(again.skipped, 1, "{:?}", again.items);
        assert_eq!(again.failed, 0, "{:?}", again.items);

        let _ = fs::remove_dir_all(&base);
    }

    /// 预演模式只看不动。
    #[test]
    fn acl_check_does_not_touch_permissions() {
        let (store, base, target) = setup("check");
        let sid = acl::current_user_sid().unwrap();
        let ram = target.parent().unwrap().parent().unwrap().to_path_buf();

        fs::create_dir_all(&target).unwrap();
        let restricted = format!("D:P(A;OICI;FA;;;SY)(A;OICI;0x1300A9;;;{sid})");
        acl::apply_sddl(&ram, &restricted).unwrap();
        acl::apply_sddl(&target, &restricted).unwrap();

        let report = acl_fix(&store, true, None).unwrap();
        assert_eq!(report.fixed, 0);
        assert_eq!(report.pending, 2, "{:?}", report.items);
        assert!(
            !acl::has_full_control(&target, &sid).unwrap(),
            "预演不应真的授权"
        );

        let _ = fs::remove_dir_all(&base);
    }
}
