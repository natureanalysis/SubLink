//! 规则库的导入导出（JSON）。
//!
//! ## 为什么导出要带"目标引用"而不是直接写 id
//!
//! 规则里存的是 `target_root_id`，跨机器没有意义。导出时把它换成 `root_ref`
//! （`r0`、`r1`…），导入时按路径重新绑定到本机的目标根。
//!
//! ## `{RAM_ROOT}` 占位符
//!
//! 分享配置给别人时对方不一定用 `R:`，所以导出后手动把目标根路径写成
//! `{RAM_ROOT}`，导入时用 `--ram-root` 指定实际盘符即可。
//!
//! ## 隐藏态也跟着走
//!
//! "哪些规则被隐藏"是展示偏好（存在 settings，不是规则属性），但选中它导出就是
//! "我这套配置希望你先看到的规则"，所以逐条挂在 `BundleRule::hidden` 上。导入时按
//! **来源路径**重新绑定到本机规则（id 跨机器没有意义），双向同步：导出方隐藏的 →
//! 导入后隐藏，导出方可见的 → 导入后取消隐藏。结果就是"导入后的隐藏态与导出时一致"。
//! 旧配置文件（无该字段）一律按不隐藏处理；本次没带过来的本机规则，其隐藏态不受影响。

use serde::{Deserialize, Serialize};

use crate::error::{SlError, SlResult};
use crate::i18n::{t, tf};
use crate::store::{NewRule, Store};

pub const FORMAT_VERSION: i32 = 1;
pub const APP_TAG: &str = "SubLink";
/// 更名前（TempDirector）导出的配置 tag：导入时按兼容处理，不提示"来源不明"。
pub const LEGACY_APP_TAG: &str = "TempDirector";
pub const RAM_ROOT_PLACEHOLDER: &str = "{RAM_ROOT}";

/// 这个 tag 是否出自本程序（含更名前的版本）。
pub fn is_known_app(app: &str) -> bool {
    app == APP_TAG || app == LEGACY_APP_TAG
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleRoot {
    #[serde(rename = "ref")]
    pub root_ref: String,
    pub label: String,
    pub path: String,
    #[serde(default = "yes")]
    pub is_ramdisk: bool,
    #[serde(default = "default_timeout")]
    pub wait_timeout_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleRule {
    pub name: String,
    #[serde(default = "other_category")]
    pub category: String,
    pub source_template: String,
    pub root_ref: String,
    pub target_subpath: String,
    #[serde(default = "junction_link")]
    pub link_type: String,
    /// 重定向方式：`link` / `setting`。旧配置文件无此字段 → `link`。
    #[serde(default = "default_redirect_mode")]
    pub redirect_mode: String,
    /// 「系统设置模式」下要改的环境变量名。旧配置文件无此字段 → 空。
    #[serde(default)]
    pub env_vars: Vec<String>,
    /// 「系统设置模式」的写入作用域。旧配置文件无此字段 → `user`。
    #[serde(default = "default_env_scope")]
    pub env_scope: String,
    #[serde(default)]
    pub want_enabled: bool,
    #[serde(default = "yes")]
    pub rebuild_on_boot: bool,
    #[serde(default)]
    pub acl_sddl: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    /// 清理排除模式（v2 起支持；旧配置文件无此字段 → 空）。
    #[serde(default)]
    pub excludes: Vec<String>,
    /// 水位预授权策略（v3 起支持；旧配置文件无此字段 → 未启用）。
    #[serde(default)]
    pub auto_clean_policy: String,
    /// 文件夹级容量上限（字节，v4 起支持；旧配置文件无此字段 → 0 = 不启用）。
    #[serde(default)]
    pub auto_clean_limit_bytes: i64,
    /// 界面上的隐藏态（纯展示偏好；旧配置文件无此字段 → 不隐藏）。
    #[serde(default)]
    pub hidden: bool,
}

impl BundleRule {
    /// 这条规则在本机的"来源路径指纹"，口径与 `Store::insert_rule` 一致
    /// （展开环境变量 → 规范化）。算不出来（含未知变量 / 非绝对路径）时返回 `None`。
    fn source_norm(&self) -> Option<String> {
        let (source, unknown) = crate::util::expand_path(&self.source_template);
        if !unknown.is_empty() || !source.is_absolute() {
            return None;
        }
        Some(crate::util::normalize_key(&source.to_string_lossy()))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bundle {
    pub schema: i32,
    pub app: String,
    pub exported_at: String,
    pub roots: Vec<BundleRoot>,
    pub rules: Vec<BundleRule>,
}

fn yes() -> bool {
    true
}
fn default_timeout() -> i64 {
    90000
}
fn other_category() -> String {
    "other".into()
}
fn junction_link() -> String {
    "junction".into()
}

/// 旧配置文件没有 `redirect_mode` → 按链接模式处理（与 v4 行为一致）。
fn default_redirect_mode() -> String {
    crate::store::REDIRECT_MODE_LINK.into()
}

/// 旧配置文件没有 `env_scope` → 按用户级处理。
fn default_env_scope() -> String {
    "user".into()
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ImportReport {
    pub roots_reused: usize,
    pub roots_created: usize,
    pub rules_created: usize,
    pub rules_skipped: usize,
    /// 本次导入中与导出方同步为"隐藏"的规则条数（见模块文档）。
    pub rules_hidden: usize,
    pub errors: Vec<String>,
    pub dry_run: bool,
}

impl ImportReport {
    pub fn one_line(&self) -> String {
        tf(
            "portable.summary",
            &[
                (
                    "prefix",
                    if self.dry_run {
                        t("portable.dry_prefix")
                    } else {
                        ""
                    },
                ),
                ("roots_new", &self.roots_created.to_string()),
                ("roots_reuse", &self.roots_reused.to_string()),
                ("rules_new", &self.rules_created.to_string()),
                ("rules_skip", &self.rules_skipped.to_string()),
                ("hidden", &self.rules_hidden.to_string()),
                ("errors", &self.errors.len().to_string()),
            ],
        )
    }
}

/// 导出当前规则库。
pub fn export_bundle(store: &Store) -> SlResult<Bundle> {
    let exported_at: String = store
        .conn()
        .query_row("SELECT datetime('now')", [], |r| r.get(0))?;

    let mut roots = Vec::new();
    let mut ref_of: std::collections::HashMap<i64, String> = std::collections::HashMap::new();
    for (i, root) in store.list_roots()?.into_iter().enumerate() {
        let root_ref = format!("r{i}");
        ref_of.insert(root.id, root_ref.clone());
        roots.push(BundleRoot {
            root_ref,
            label: root.label,
            path: root.path,
            is_ramdisk: root.is_ramdisk,
            wait_timeout_ms: root.wait_timeout_ms,
        });
    }

    let hidden: std::collections::HashSet<i64> =
        store.hidden_rule_ids()?.into_iter().collect();

    let mut rules = Vec::new();
    for rule in store.list_rules()? {
        let root_ref = match ref_of.get(&rule.target_root_id) {
            Some(r) => r.clone(),
            None => {
                // 目标根缺失（理论上被 RESTRICT 外键挡住）——跳过而不是导出一个坏配置
                continue;
            }
        };
        rules.push(BundleRule {
            name: rule.name,
            category: rule.category,
            source_template: rule.source_template,
            root_ref,
            target_subpath: rule.target_subpath,
            link_type: rule.link_type,
            redirect_mode: rule.redirect_mode,
            env_vars: rule.env_vars,
            env_scope: rule.env_scope,
            want_enabled: rule.want_enabled,
            rebuild_on_boot: rule.rebuild_on_boot,
            acl_sddl: rule.acl_sddl,
            notes: rule.notes,
            excludes: rule.excludes,
            auto_clean_policy: rule.auto_clean_policy,
            auto_clean_limit_bytes: rule.auto_clean_limit_bytes,
            hidden: hidden.contains(&rule.id),
        });
    }

    Ok(Bundle {
        schema: FORMAT_VERSION,
        app: APP_TAG.into(),
        exported_at,
        roots,
        rules,
    })
}

/// 导入一份配置。单条规则出错不会中断整批（收集到 `errors` 里）。
pub fn import_bundle(
    store: &Store,
    bundle: &Bundle,
    ram_root_override: Option<&str>,
    dry_run: bool,
) -> SlResult<ImportReport> {
    if bundle.schema > FORMAT_VERSION {
        return Err(SlError::Other(tf(
            "portable.version_too_high",
            &[
                ("ver", &bundle.schema.to_string()),
                ("max", &FORMAT_VERSION.to_string()),
            ],
        )));
    }

    let mut report = ImportReport {
        dry_run,
        ..Default::default()
    };

    // 1) 目标根：同路径复用，否则新建
    let mut id_of: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    for br in &bundle.roots {
        let path = match ram_root_override {
            Some(v) if br.path.trim() == RAM_ROOT_PLACEHOLDER => v.to_string(),
            _ => br.path.clone(),
        };
        if path.trim().is_empty() {
            report
                .errors
                .push(tf("portable.root_path_empty", &[("ref", &br.root_ref)]));
            continue;
        }
        let existing = store.list_roots()?.into_iter().find(|r| {
            crate::util::normalize_root_path(&r.path) == crate::util::normalize_root_path(&path)
        });
        match existing {
            Some(r) => {
                id_of.insert(br.root_ref.clone(), r.id);
                report.roots_reused += 1;
            }
            None => {
                if dry_run {
                    id_of.insert(br.root_ref.clone(), -1);
                    report.roots_created += 1;
                } else {
                    let id =
                        store.insert_root(&br.label, &path, br.is_ramdisk, br.wait_timeout_ms)?;
                    id_of.insert(br.root_ref.clone(), id);
                    report.roots_created += 1;
                }
            }
        }
    }

    // 2) 规则
    //
    // 隐藏态收集：新建成功的规则用新 id；判重跳过的规则用"来源路径"找回本机那条
    // （否则跨机器导入时同一个源的隐藏态会丢）。规则都落库后再一次性写回。
    let mut hide_ids: Vec<i64> = Vec::new();
    let mut show_ids: Vec<i64> = Vec::new();
    for (idx, br) in bundle.rules.iter().enumerate() {
        let root_id = match id_of.get(&br.root_ref) {
            Some(id) => *id,
            None => {
                report.errors.push(tf(
                    "portable.unknown_root",
                    &[("name", &br.name), ("ref", &br.root_ref)],
                ));
                continue;
            }
        };
        let nr = NewRule {
            name: br.name.clone(),
            category: br.category.clone(),
            source_template: br.source_template.clone(),
            target_root_id: root_id.max(1),
            target_subpath: br.target_subpath.clone(),
            link_type: br.link_type.clone(),
            redirect_mode: br.redirect_mode.clone(),
            env_vars: br.env_vars.clone(),
            env_scope: br.env_scope.clone(),
            want_enabled: br.want_enabled,
            rebuild_on_boot: br.rebuild_on_boot,
            notes: br.notes.clone(),
            excludes: br.excludes.clone(),
            auto_clean_policy: br.auto_clean_policy.clone(),
            auto_clean_limit_bytes: br.auto_clean_limit_bytes,
        };

        if dry_run {
            report.rules_created += 1;
            continue;
        }
        match store.insert_rule(&nr) {
            Ok(id) => {
                if let Some(sddl) = br.acl_sddl.as_deref() {
                    let _ = store.set_rule_flags(id, br.rebuild_on_boot, &br.link_type, Some(sddl));
                }
                report.rules_created += 1;
                if br.hidden {
                    hide_ids.push(id);
                } else {
                    show_ids.push(id);
                }
            }
            Err(e) => {
                // 已存在 / 安全检查拒绝都算"跳过"而不是"错误中断"
                report.rules_skipped += 1;
                report.errors.push(tf(
                    "portable.rule_import_failed",
                    &[
                        ("idx", &(idx + 1).to_string()),
                        ("name", &br.name),
                        ("err", &e.to_string()),
                    ],
                ));
                // 判重跳过时，隐藏态仍要对齐到本机已有的那条（按来源路径找回）。
                if let Some(existing) = br
                    .source_norm()
                    .and_then(|n| store.find_rule_by_source(&n).ok().flatten())
                {
                    if br.hidden {
                        hide_ids.push(existing.id);
                    } else {
                        show_ids.push(existing.id);
                    }
                }
            }
        }
    }

    // 3) 隐藏态落库：先"取消隐藏"再"隐藏"。两个列表正常不会重叠，但固定顺序能
    //    保证语义稳定。dry_run 不写库（此时连规则 id 都还没有）。
    if !dry_run {
        if !show_ids.is_empty() {
            store.set_rules_hidden(&show_ids, false)?;
        }
        if !hide_ids.is_empty() {
            store.set_rules_hidden(&hide_ids, true)?;
            report.rules_hidden = hide_ids.len();
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    fn rule_at(name: &str, source: &str, root_id: i64) -> NewRule {
        NewRule {
            name: name.into(),
            category: "other".into(),
            source_template: source.into(),
            target_root_id: root_id,
            target_subpath: format!("t\\{}", name),
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
        }
    }

    #[test]
    fn export_import_roundtrip() {
        let src = Store::open_in_memory().unwrap();
        let root_id = src.insert_root("RAM", "R:", true, 90000).unwrap();
        src.insert_rule(&NewRule {
            name: "Chrome".into(),
            category: "browser".into(),
            source_template: r"C:\Users\a\AppData\Local\Chrome\Cache".into(),
            target_root_id: root_id,
            target_subpath: "browsers\\chrome".into(),
            link_type: "junction".into(),
            redirect_mode: "link".into(),
            env_vars: Vec::new(),
            env_scope: "user".into(),
            want_enabled: true,
            rebuild_on_boot: true,
            notes: Some("先关 Chrome".into()),
            excludes: vec!["sessions".into()],
            auto_clean_policy: "older:3600".into(),
            auto_clean_limit_bytes: 4 * 1024 * 1024 * 1024,
        })
        .unwrap();

        let bundle = export_bundle(&src).unwrap();
        assert_eq!(bundle.schema, FORMAT_VERSION);
        assert_eq!(bundle.roots.len(), 1);
        assert_eq!(bundle.rules.len(), 1);
        assert_eq!(bundle.rules[0].root_ref, "r0");
        assert_eq!(bundle.rules[0].auto_clean_policy, "older:3600");
        assert_eq!(
            bundle.rules[0].auto_clean_limit_bytes,
            4 * 1024 * 1024 * 1024,
            "文件夹级容量上限必须随导出带出"
        );
        assert!(!bundle.rules[0].hidden, "未隐藏的规则导出时 hidden 应为 false");

        let json = serde_json::to_string_pretty(&bundle).unwrap();
        assert!(!json.contains("{RAM_ROOT}")); // 导出是实路径，占位符由使用者手改
        let parsed: Bundle = serde_json::from_str(&json).unwrap();

        let dst = Store::open_in_memory().unwrap();
        let rep = import_bundle(&dst, &parsed, None, false).unwrap();
        assert_eq!(rep.roots_created, 1);
        assert_eq!(rep.rules_created, 1);
        assert!(rep.errors.is_empty(), "{:?}", rep.errors);

        let rules = dst.list_rules().unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].name, "Chrome");
        assert_eq!(rules[0].target_subpath, "browsers\\chrome");
        assert_eq!(
            rules[0].auto_clean_limit_bytes,
            4 * 1024 * 1024 * 1024,
            "导入必须还原文件夹级容量上限"
        );
    }

    #[test]
    fn import_is_idempotent_by_source_path() {
        let src = Store::open_in_memory().unwrap();
        let root_id = src.insert_root("RAM", "R:", true, 90000).unwrap();
        src.insert_rule(&NewRule {
            name: "A".into(),
            category: "dev".into(),
            source_template: r"C:\Temp\A".into(),
            target_root_id: root_id,
            target_subpath: "a".into(),
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
        let bundle = export_bundle(&src).unwrap();

        let dst = Store::open_in_memory().unwrap();
        let r1 = import_bundle(&dst, &bundle, None, false).unwrap();
        assert_eq!(r1.rules_created, 1);
        let r2 = import_bundle(&dst, &bundle, None, false).unwrap();
        assert_eq!(r2.rules_created, 0);
        assert_eq!(r2.rules_skipped, 1);
        assert_eq!(dst.list_rules().unwrap().len(), 1);
    }

    #[test]
    fn ram_root_placeholder_is_rebound() {
        let mut bundle = Bundle {
            schema: FORMAT_VERSION,
            app: APP_TAG.into(),
            exported_at: "2026-01-01".into(),
            roots: vec![BundleRoot {
                root_ref: "r0".into(),
                label: "RAM".into(),
                path: RAM_ROOT_PLACEHOLDER.into(),
                is_ramdisk: true,
                wait_timeout_ms: 90000,
            }],
            rules: vec![BundleRule {
                name: "X".into(),
                category: "dev".into(),
                source_template: r"C:\Temp\X".into(),
                root_ref: "r0".into(),
                target_subpath: "x".into(),
                link_type: "junction".into(),
                redirect_mode: "link".into(),
                env_vars: Vec::new(),
                env_scope: "user".into(),
                want_enabled: true,
                rebuild_on_boot: true,
                acl_sddl: None,
                notes: None,
                excludes: Vec::new(),
                auto_clean_policy: String::new(),
                auto_clean_limit_bytes: 0,
                hidden: false,
            }],
        };
        let dst = Store::open_in_memory().unwrap();
        let rep = import_bundle(&dst, &bundle, Some("T:\\Redir"), false).unwrap();
        assert_eq!(rep.roots_created, 1);
        assert_eq!(dst.list_roots().unwrap()[0].path, "T:\\Redir");

        // 去掉占位符后应能正常导入到新库；裸盘符会被规范化成 `R:\`
        bundle.roots[0].path = "R:".into();
        let dst2 = Store::open_in_memory().unwrap();
        let rep2 = import_bundle(&dst2, &bundle, None, false).unwrap();
        assert_eq!(rep2.roots_created, 1);
        assert_eq!(dst2.list_roots().unwrap()[0].path, "R:\\");
    }

    #[test]
    fn rejects_newer_schema() {
        let bundle = Bundle {
            schema: FORMAT_VERSION + 1,
            app: APP_TAG.into(),
            exported_at: "x".into(),
            roots: vec![],
            rules: vec![],
        };
        let dst = Store::open_in_memory().unwrap();
        assert!(import_bundle(&dst, &bundle, None, false).is_err());
    }

    #[test]
    fn dry_run_writes_nothing() {
        let src = Store::open_in_memory().unwrap();
        src.insert_root("RAM", "R:", true, 90000).unwrap();
        let bundle = export_bundle(&src).unwrap();

        let dst = Store::open_in_memory().unwrap();
        let rep = import_bundle(&dst, &bundle, None, true).unwrap();
        assert!(rep.dry_run);
        assert_eq!(rep.roots_created, 1);
        assert!(dst.list_roots().unwrap().is_empty());
        assert!(dst.list_rules().unwrap().is_empty());
    }

    #[test]
    fn hidden_state_travels_with_config() {
        let src = Store::open_in_memory().unwrap();
        let root_id = src.insert_root("RAM", "R:", true, 90000).unwrap();
        let a = src
            .insert_rule(&rule_at("A", r"C:\Temp\HiddenA", root_id))
            .unwrap();
        src.insert_rule(&rule_at("B", r"C:\Temp\HiddenB", root_id))
            .unwrap();
        src.set_rules_hidden(&[a], true).unwrap();

        let bundle = export_bundle(&src).unwrap();
        let flags: Vec<bool> = bundle.rules.iter().map(|r| r.hidden).collect();
        assert_eq!(flags, vec![true, false], "隐藏态必须随导出带出");

        let dst = Store::open_in_memory().unwrap();
        let rep = import_bundle(&dst, &bundle, None, false).unwrap();
        assert_eq!(rep.rules_created, 2);
        assert_eq!(rep.rules_hidden, 1, "报告里应体现本次导入的隐藏条数");

        let hidden = dst.hidden_rule_ids().unwrap();
        assert_eq!(hidden.len(), 1);
        let rules = dst.list_rules().unwrap();
        let hidden_rule = rules.iter().find(|r| r.id == hidden[0]).unwrap();
        assert_eq!(hidden_rule.name, "A", "导入后隐藏的应与导出时是同一条");
        assert!(!hidden.contains(&rules.iter().find(|r| r.name == "B").unwrap().id));
    }

    #[test]
    fn config_without_hidden_field_imports_as_visible() {
        // 模拟旧版导出文件：没有 hidden 字段。
        let json = r#"{
            "schema": 1,
            "app": "SubLink",
            "exported_at": "2026-01-01",
            "roots": [{"ref": "r0", "label": "RAM", "path": "R:", "is_ramdisk": true, "wait_timeout_ms": 90000}],
            "rules": [{"name": "A", "category": "dev", "source_template": "C:\\Temp\\OldA", "root_ref": "r0", "target_subpath": "a", "link_type": "junction", "want_enabled": true, "rebuild_on_boot": true}]
        }"#;
        let bundle: Bundle = serde_json::from_str(json).unwrap();
        assert!(!bundle.rules[0].hidden, "缺字段必须按不隐藏处理");

        // 本机已有同源规则且被隐藏：导入后应对齐导出方（可见）。
        let dst = Store::open_in_memory().unwrap();
        let root_id = dst.insert_root("RAM", "R:", true, 90000).unwrap();
        let id = dst
            .insert_rule(&rule_at("A", r"C:\Temp\OldA", root_id))
            .unwrap();
        dst.set_rules_hidden(&[id], true).unwrap();

        let rep = import_bundle(&dst, &bundle, None, false).unwrap();
        assert_eq!(rep.rules_skipped, 1, "同源规则应判重跳过");
        assert_eq!(rep.rules_hidden, 0);
        assert!(
            dst.hidden_rule_ids().unwrap().is_empty(),
            "旧配置无隐藏信息 → 导入后按不隐藏处理"
        );
    }

    #[test]
    fn import_only_touches_rules_it_carries() {
        let src = Store::open_in_memory().unwrap();
        let src_root = src.insert_root("RAM", "R:", true, 90000).unwrap();
        let a = src
            .insert_rule(&rule_at("A", r"C:\Temp\OnlyA", src_root))
            .unwrap();
        src.set_rules_hidden(&[a], true).unwrap();
        let bundle = export_bundle(&src).unwrap();

        let dst = Store::open_in_memory().unwrap();
        let root_id = dst.insert_root("RAM", "R:", true, 90000).unwrap();
        let local = dst
            .insert_rule(&rule_at("Local", r"C:\Temp\NotInBundle", root_id))
            .unwrap();
        dst.set_rules_hidden(&[local], true).unwrap();

        let rep = import_bundle(&dst, &bundle, None, false).unwrap();
        assert_eq!(rep.rules_created, 1);
        let hidden = dst.hidden_rule_ids().unwrap();
        assert!(
            hidden.contains(&local),
            "本机已有的、与本次导入无关的隐藏不能被清掉"
        );
        assert_eq!(hidden.len(), 2);
        assert!(hidden.iter().any(|id| *id != local), "必须新增一条隐藏");
    }

    #[test]
    fn dry_run_does_not_sync_hidden() {
        let src = Store::open_in_memory().unwrap();
        let src_root = src.insert_root("RAM", "R:", true, 90000).unwrap();
        let a = src
            .insert_rule(&rule_at("A", r"C:\Temp\DryA", src_root))
            .unwrap();
        src.set_rules_hidden(&[a], true).unwrap();
        let bundle = export_bundle(&src).unwrap();

        let dst = Store::open_in_memory().unwrap();
        let rep = import_bundle(&dst, &bundle, None, true).unwrap();
        assert!(rep.dry_run);
        assert_eq!(rep.rules_created, 1);
        assert_eq!(rep.rules_hidden, 0);
        assert!(dst.hidden_rule_ids().unwrap().is_empty());
    }

    #[test]
    fn one_line_reports_hidden_count() {
        let rep = ImportReport {
            rules_created: 3,
            rules_skipped: 1,
            rules_hidden: 2,
            ..Default::default()
        };
        let s = rep.one_line();
        assert!(s.contains('2'), "摘要里应带上隐藏条数：{s}");
        assert!(!s.contains('{'), "占位符必须都被替换掉：{s}");
    }
}
