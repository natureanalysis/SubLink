//! 数据模型。

use std::borrow::Cow;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::util;

/// 重定向目标根（一块 RAM 盘，或 RAM 盘下的某个子目录）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Root {
    pub id: i64,
    pub label: String,
    /// 路径模板，例如 `R:` 或 `R:\Redirect`。
    pub path: String,
    pub is_ramdisk: bool,
    /// 开机等待该卷就绪的上限（毫秒）。
    pub wait_timeout_ms: i64,
    pub created_at: String,
}

impl Root {
    pub fn resolved_path(&self) -> (PathBuf, Vec<String>) {
        let (raw, unknown) = util::expand_path(&self.path);
        let normalized = util::normalize_root_path(&raw.to_string_lossy());
        (PathBuf::from(normalized), unknown)
    }

    pub fn as_str(&self) -> String {
        format!("{} [{}]", self.label, self.path)
    }
}

/// 一条重定向规则。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub id: i64,
    pub name: String,
    pub category: String,
    /// 源路径模板（未展开，可能含 `%APPDATA%`）。
    pub source_template: String,
    /// 归一化后的源路径，做唯一键用。
    pub source_norm: String,
    pub target_root_id: i64,
    pub target_subpath: String,
    pub link_type: String,
    /// 重定向方式：`link`（源路径上建 Junction/符号链接）/ `setting`（直接改系统设置）。
    pub redirect_mode: String,
    /// `setting` 模式下要改的环境变量名（`TEMP` / `TMP` / `LOCALAPPDATA`）。
    /// `link` 模式为空。
    pub env_vars: Vec<String>,
    /// `setting` 模式的写入作用域：`user`（HKCU，免提权）/ `machine`（HKLM，需提权）。
    pub env_scope: String,
    /// 用户期望态："我想让它开着"。
    pub want_enabled: bool,
    pub rebuild_on_boot: bool,
    pub acl_sddl: Option<String>,
    pub needs_elevation: bool,
    pub notes: Option<String>,
    pub sort_order: i64,
    /// 清理排除模式。只影响 `clean`（命中子树不删不剪），不改搬运语义。
    pub excludes: Vec<String>,
    /// 水位预授权策略：`''` 未启用 / `empty` / `older:<秒>`。
    /// 仅 water 巡检触发（卷水位 critical、或目录达到 `auto_clean_limit_bytes`）；
    /// 手动 clean 不读它。
    pub auto_clean_policy: String,
    /// 文件夹级容量上限（字节，`0` = 不启用）。
    ///
    /// 与 `auto_clean_policy` 配合：策略决定「删什么」，上限决定「什么时候删」。
    /// `> 0` 时每轮巡检测量这条规则目录**自身**的占用，达到上限即**触发一次水位
    /// 自动清理**（与卷水位路径同一套处理，只是触发条件不看卷水位 —— 单个文件夹
    /// 撑爆不必等整块盘到严重线）。
    /// `0` 时只剩「卷水位 critical」那一条触发路径（旧行为，向后兼容）。
    pub auto_clean_limit_bytes: i64,

    pub last_state: Option<String>,
    pub last_checked_at: Option<String>,
    pub last_error: Option<String>,
    pub last_files: Option<i64>,
    pub last_bytes: Option<i64>,
    pub applied_at: Option<String>,

    pub created_at: String,
    pub updated_at: String,
}

impl Rule {
    /// 按当前用户上下文展开的源路径（外加未能解析的变量名）。
    pub fn resolved_source(&self) -> (PathBuf, Vec<String>) {
        util::expand_path(&self.source_template)
    }

    /// 目标完整路径 = 目标根 + 相对子路径。
    pub fn resolved_target(&self, root: &Root) -> (PathBuf, Vec<String>) {
        let (base, unknown) = root.resolved_path();
        let sub = self.target_subpath.trim().trim_start_matches(['\\', '/']);
        if sub.is_empty() {
            (base, unknown)
        } else {
            (base.join(sub), unknown)
        }
    }

    pub fn category_label(&self) -> Cow<'_, str> {
        category_label(&self.category)
    }

    /// 给 UI 用的一行摘要。
    pub fn summary(&self) -> String {
        format!(
            "#{} [{}] {} → {}",
            self.id,
            self.category_label(),
            self.source_template,
            self.target_subpath
        )
    }
}

/// 分类展示名：内置分类走 i18n，自定义分类直接回显原名
/// （批量归类允许用户新建分类，不能把未知分类一律显示成「其它」）。
pub fn category_label(cat: &str) -> Cow<'_, str> {
    match cat {
        "browser" => Cow::Borrowed(crate::i18n::t("cat.browser")),
        "design" => Cow::Borrowed(crate::i18n::t("cat.design")),
        "dev" => Cow::Borrowed(crate::i18n::t("cat.dev")),
        "system" => Cow::Borrowed(crate::i18n::t("cat.system")),
        "media" => Cow::Borrowed(crate::i18n::t("cat.media")),
        "office" => Cow::Borrowed(crate::i18n::t("cat.office")),
        "game" => Cow::Borrowed(crate::i18n::t("cat.game")),
        "other" => Cow::Borrowed(crate::i18n::t("cat.other")),
        _ => Cow::Owned(cat.to_string()),
    }
}

pub const CATEGORIES: &[&str] = &[
    "browser", "design", "dev", "system", "media", "office", "game", "other",
];

/// 重定向方式：链接（源路径上建 Junction / 符号链接）。
pub const REDIRECT_MODE_LINK: &str = "link";
/// 重定向方式：系统设置（直接改注册表里的环境变量）。
pub const REDIRECT_MODE_SETTING: &str = "setting";

fn default_redirect_mode() -> String {
    REDIRECT_MODE_LINK.into()
}

fn default_env_scope() -> String {
    "user".into()
}

/// 新建规则的入参。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewRule {
    pub name: String,
    pub category: String,
    pub source_template: String,
    pub target_root_id: i64,
    pub target_subpath: String,
    pub link_type: String,
    /// 重定向方式：`link` / `setting`。缺省（老配置）= `link`。
    #[serde(default = "default_redirect_mode")]
    pub redirect_mode: String,
    /// `setting` 模式要改的环境变量名。缺省 = 空（= link 模式）。
    #[serde(default)]
    pub env_vars: Vec<String>,
    /// `setting` 模式的写入作用域。缺省 = `user`。
    #[serde(default = "default_env_scope")]
    pub env_scope: String,
    pub want_enabled: bool,
    pub rebuild_on_boot: bool,
    pub notes: Option<String>,
    #[serde(default)]
    pub excludes: Vec<String>,
    /// 水位预授权策略：`''` / `empty` / `older:<秒>`。缺省（老配置文件）= 未启用。
    #[serde(default)]
    pub auto_clean_policy: String,
    /// 文件夹级容量上限（字节，`0` = 不启用）。缺省（老配置文件）= 不启用。
    #[serde(default)]
    pub auto_clean_limit_bytes: i64,
}

/// 操作流水的一行。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Operation {
    pub id: i64,
    pub rule_id: Option<i64>,
    pub kind: String,
    pub phase: String,
    pub source_path: Option<String>,
    pub target_path: Option<String>,
    pub file_count: Option<i64>,
    pub bytes: Option<i64>,
    pub error: Option<String>,
    pub started_at: String,
    pub finished_at: Option<String>,
}

/// 事件日志的一行。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventRow {
    pub id: i64,
    pub rule_id: Option<i64>,
    pub level: String,
    pub kind: String,
    pub message: String,
    pub at: String,
}

pub const RULE_COLUMNS: &str = "id, name, category, source_template, source_norm, \
     target_root_id, target_subpath, link_type, redirect_mode, env_vars, env_scope, \
     want_enabled, rebuild_on_boot, \
     acl_sddl, needs_elevation, notes, sort_order, excludes, auto_clean_policy, \
     auto_clean_limit_bytes, \
     last_state, last_checked_at, last_error, last_files, last_bytes, applied_at, \
     created_at, updated_at";

/// DB 里的 `excludes` 列（JSON 数组文本）→ `Vec<String>`。
/// 手工改库写坏 JSON 时按"无排除"处理，不让一条脏数据拖垮整个列表。
fn excludes_from_db(text: String) -> Vec<String> {
    serde_json::from_str(&text).unwrap_or_default()
}

/// DB 里的 `env_vars` 列（JSON 数组文本）→ `Vec<String>`。
/// 写坏 JSON 时按"无变量"处理（等价于 link 模式），不让脏数据拖垮列表。
fn env_vars_from_db(text: String) -> Vec<String> {
    serde_json::from_str(&text).unwrap_or_default()
}

pub fn rule_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Rule> {
    Ok(Rule {
        id: row.get(0)?,
        name: row.get(1)?,
        category: row.get(2)?,
        source_template: row.get(3)?,
        source_norm: row.get(4)?,
        target_root_id: row.get(5)?,
        target_subpath: row.get(6)?,
        link_type: row.get(7)?,
        redirect_mode: row.get(8)?,
        env_vars: env_vars_from_db(row.get(9)?),
        env_scope: row.get(10)?,
        want_enabled: row.get(11)?,
        rebuild_on_boot: row.get(12)?,
        acl_sddl: row.get(13)?,
        needs_elevation: row.get(14)?,
        notes: row.get(15)?,
        sort_order: row.get(16)?,
        excludes: excludes_from_db(row.get(17)?),
        auto_clean_policy: row.get(18)?,
        auto_clean_limit_bytes: row.get(19)?,
        last_state: row.get(20)?,
        last_checked_at: row.get(21)?,
        last_error: row.get(22)?,
        last_files: row.get(23)?,
        last_bytes: row.get(24)?,
        applied_at: row.get(25)?,
        created_at: row.get(26)?,
        updated_at: row.get(27)?,
    })
}

pub const ROOT_COLUMNS: &str = "id, label, path, is_ramdisk, wait_timeout_ms, created_at";

pub fn root_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Root> {
    Ok(Root {
        id: row.get(0)?,
        label: row.get(1)?,
        path: row.get(2)?,
        is_ramdisk: row.get(3)?,
        wait_timeout_ms: row.get(4)?,
        created_at: row.get(5)?,
    })
}
