//! 视图结构：前端 JSON 契约 + 用例层常用核心类型的再导出。
//!
//! 这些结构的字段名就是线上契约（前端 `app.js` 依赖），改动需同步前端与命令层。
//! 之所以放在 core 的用例层：GUI 的 `api` 模块只是本层的薄再导出，
//! 逻辑脱离窗口即可单测，也便于将来接到服务端或别的 UI。

use serde::{Deserialize, Serialize};

use crate::engine::ApplyOptions;
use crate::fsops::verify::VerifyMode;
use crate::store::NewRule;

// ---- 核心类型再导出：让前端适配层只依赖 `usecase`，不直接触达领域模块。----
pub use crate::boot::BootReport;
pub use crate::engine::SettingOption;
pub use crate::error::{SlError, SlResult};
pub use crate::fsops::clean::{CleanPolicy, CleanReport, CleanSampleEntry};
pub use crate::fsops::mover::Progress;
pub use crate::fsops::occupancy::LockReport;
pub use crate::portable::ImportReport;
pub use crate::store::{EventRow, Operation, Store};

#[derive(Debug, Clone, Serialize)]
pub struct RootView {
    pub id: i64,
    pub label: String,
    pub path: String,
    pub is_ramdisk: bool,
    pub wait_timeout_ms: i64,
    /// `ready` / `missing` / `not_dir` / `readonly` / `no_space_info`
    pub state: &'static str,
    pub message: String,
    pub unknown_vars: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RuleView {
    pub id: i64,
    pub name: String,
    pub category: String,
    /// 未展开的模板（编辑表单用）。
    pub source_template: String,
    /// 展开后的真实源路径（展示用）。
    pub source: String,
    pub target: String,
    pub target_root_id: i64,
    pub target_subpath: String,
    pub link_type: String,
    /// 重定向方式：`link` / `setting`。
    pub redirect_mode: String,
    /// 「系统设置模式」下要改的环境变量名。
    pub env_vars: Vec<String>,
    /// 「系统设置模式」的写入作用域（`user` / `machine`）。
    pub env_scope: String,
    /// 这条规则用「系统设置模式」的可行组合（作用域 + 变量名）。
    ///
    /// 空 = 源路径不是任何 `TEMP` / `TMP` 当前指向的目录，
    /// 这个模式对它是不可实现的 —— 前端据此不显示「系统设置」选项。
    pub setting_options: Vec<SettingOption>,
    pub want_enabled: bool,
    pub rebuild_on_boot: bool,
    /// 是否已存有 DACL 模板。
    pub has_acl: bool,
    pub needs_elevation: bool,
    pub notes: Option<String>,
    /// 清理排除模式（只影响 clean，不改搬运）。
    pub excludes: Vec<String>,
    /// 水位预授权策略：`''` / `empty` / `older:<秒>`（详情展示用）。
    pub auto_clean_policy: String,
    /// 文件夹级容量上限（字节，`0` = 不启用）：本规则目录自身占用到量即自动清理。
    pub auto_clean_limit_bytes: i64,
    /// 是否被用户隐藏（纯展示偏好，见 `usecase::rules::KEY_HIDDEN_RULES`）。
    /// 隐藏只影响列表显示，规则照样参与开机重建、巡检与所有批量操作。
    pub hidden: bool,
    pub badge: String,
    pub label: String,
    pub needs_attention: bool,
    pub warnings: Vec<String>,
    pub unknown_vars: Vec<String>,
    pub last_files: Option<i64>,
    pub last_bytes: Option<i64>,
    pub applied_at: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Stats {
    pub total: usize,
    pub enabled: usize,
    pub ok: usize,
    pub warn: usize,
    pub plain: usize,
    pub other: usize,
    pub needs_attention: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct UiSnapshot {
    pub version: String,
    pub schema_version: i32,
    pub data_dir: String,
    pub db_path: String,
    pub categories: Vec<String>,
    pub roots: Vec<RootView>,
    pub rules: Vec<RuleView>,
    pub stats: Stats,
    /// 顶部横幅（就绪状态 / 待处理项 / 上次中断的操作）。
    pub banners: Vec<Banner>,
    pub open_ops: Vec<Operation>,
    /// 撤销栈：还有什么可撤销（工具栏「撤销」按钮据此亮灭与提示）。
    pub undo: UndoView,
}

/// 撤销栈状态（前端 JSON 契约）。
#[derive(Debug, Clone, Default, Serialize)]
pub struct UndoView {
    /// 待撤销的规则条数。0 = 没什么可撤销的。
    pub pending: usize,
    /// 栈顶操作类型：`delete`（删掉的规则）/ `update`（被改过的规则）/ 空 = 没有。
    pub kind: String,
    /// 栈顶操作涉及的规则名。
    pub names: Vec<String>,
}

/// 一次撤销的结果（前端 JSON 契约）。
#[derive(Debug, Clone, Default, Serialize)]
pub struct UndoResult {
    /// 成功撤销/恢复的规则名。
    pub restored: Vec<String>,
    /// 没能撤销的条目及原因（一行一条）。
    pub skipped: Vec<String>,
    /// 本次撤销的操作类型：`delete` / `update`；空 = 撤销栈本来就是空的。
    pub kind: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Banner {
    /// `info` / `warn` / `error`
    pub level: &'static str,
    pub text: String,
    /// 可选动作标识（前端据此渲按钮）：`sweep` = 一键清理。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ItemOutcome {
    pub rule_id: i64,
    pub name: String,
    pub ok: bool,
    pub action: String,
    pub message: String,
    pub elapsed_ms: u128,
}

/// 新增/编辑规则的表单。
///
/// ⚠️ **线格式统一用 snake_case，两个方向一致**（读视图 `RuleView` 也是 snake_case）。
/// 这里不要加 `rename_all = "camelCase"`：Tauri 只把**命令参数名**从 camelCase
/// 转成 snake_case（`rootId` → `root_id`），**嵌套结构体的字段不转**。
/// 一旦前端发 `sourceTemplate` 而这里要 `source_template`，界面保存就会报
/// `missing field source_template` —— 而这种错在 mock 后端里看不出来
/// （mock 不校验形状）。`rule_form_wire_contract` 测试钉住这个约定。
#[derive(Debug, Clone, Deserialize)]
pub struct RuleForm {
    pub name: String,
    pub category: String,
    pub source_template: String,
    pub target_root_id: i64,
    pub target_subpath: String,
    pub link_type: String,
    /// 重定向方式：`link`（建 Junction）/ `setting`（改系统设置）。
    #[serde(default = "default_redirect_mode")]
    pub redirect_mode: String,
    /// 「系统设置模式」下要改的环境变量名（如 `["TEMP","TMP"]`）。
    #[serde(default)]
    pub env_vars: Vec<String>,
    /// 「系统设置模式」的写入作用域：`user` / `machine`。
    #[serde(default = "default_env_scope")]
    pub env_scope: String,
    #[serde(default)]
    pub want_enabled: bool,
    #[serde(default = "yes")]
    pub rebuild_on_boot: bool,
    #[serde(default)]
    pub notes: Option<String>,
    /// 清理排除模式，如 `["sessions", "logs/old"]`。空 = 不排除。
    #[serde(default)]
    pub excludes: Vec<String>,
    /// 水位预授权策略：`''` 未启用 / `empty` / `older:<秒>`。
    /// 仅 water 巡检在 critical 时触发；手动 clean 不读它。
    #[serde(default)]
    pub auto_clean_policy: String,
    /// 文件夹级容量上限（字节，`0` = 不启用）。`> 0` 时必须配非空策略 ——
    /// 策略决定删什么，没有策略就无从下手（`validate_form` 把关）。
    #[serde(default)]
    pub auto_clean_limit_bytes: i64,
}

fn yes() -> bool {
    true
}

/// 缺省的重定向方式 = 链接模式（与 v4 及更早的规则库行为一致）。
fn default_redirect_mode() -> String {
    crate::store::REDIRECT_MODE_LINK.into()
}

/// 缺省的写入作用域 = 用户级（HKCU，免提权）。
fn default_env_scope() -> String {
    "user".into()
}

impl RuleForm {
    pub(crate) fn to_new_rule(&self) -> NewRule {
        NewRule {
            name: self.name.clone(),
            category: self.category.clone(),
            source_template: self.source_template.clone(),
            target_root_id: self.target_root_id,
            target_subpath: self.target_subpath.clone(),
            link_type: self.link_type.clone(),
            redirect_mode: self.redirect_mode.clone(),
            env_vars: self.env_vars.clone(),
            env_scope: self.env_scope.clone(),
            want_enabled: self.want_enabled,
            rebuild_on_boot: self.rebuild_on_boot,
            notes: self.notes.clone(),
            excludes: self.excludes.clone(),
            auto_clean_policy: self.auto_clean_policy.clone(),
            auto_clean_limit_bytes: self.auto_clean_limit_bytes,
        }
    }
}

/// 应用/恢复的选项。字段同样是 snake_case（约定见 [`RuleForm`]）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ApplyOpts {
    #[serde(default)]
    pub dry_run: bool,
    /// 目标目录非空时允许合并（同路径取较新者）。
    #[serde(default)]
    pub allow_merge: bool,
    /// 跳过占用检测（危险）。
    #[serde(default)]
    pub skip_lock_check: bool,
    /// 尽力模式：单条目失败不中断，删除不了的源条目留在原处并报错。
    /// 只在提权执行时对 `C:\Windows\*` 这类源由上层自动打开。
    #[serde(default)]
    pub best_effort: bool,
    /// 逐文件 SHA-256（慢）。
    #[serde(default)]
    pub sha256: bool,
}

impl ApplyOpts {
    pub(crate) fn to_engine(&self) -> ApplyOptions {
        ApplyOptions {
            dry_run: self.dry_run,
            verify: if self.sha256 {
                VerifyMode::Sha256
            } else {
                VerifyMode::CountAndSize
            },
            skip_lock_check: self.skip_lock_check,
            allow_merge: self.allow_merge,
            best_effort: self.best_effort,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceInspection {
    pub expanded: String,
    pub exists: bool,
    pub blocking: Vec<String>,
    pub warnings: Vec<String>,
    pub requires_elevation: bool,
    pub unknown_vars: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TargetInspection {
    pub target: String,
    pub root_ready: bool,
    pub exists: bool,
    /// 目标位置已有真实文件/目录 —— apply 需要 `allow_merge` 才能继续。
    pub occupied: bool,
    pub state: String,
    pub unknown_vars: Vec<String>,
}

/// 一条预设。与 `presets.json` 的字段一一对应（serde 反序列化目标）。
///
/// `note` 可空：值得让用户看到的坑（版本号目录、占用频繁、索引重建等）。
#[derive(Debug, Clone, Deserialize)]
pub struct PresetDef {
    pub name: String,
    pub category: String,
    pub source_template: String,
    /// RAM 盘下的默认子路径（可被用户改写）。
    pub target_subpath: String,
    pub note: Option<String>,
    /// 重定向方式：`link`（默认，建 Junction）/ `setting`（直接改环境变量）。
    #[serde(default = "default_redirect_mode")]
    pub redirect_mode: String,
    /// `setting` 模式要改的环境变量名。
    #[serde(default)]
    pub env_vars: Vec<String>,
    /// `setting` 模式的写入作用域（`user` / `machine`）。
    #[serde(default = "default_env_scope")]
    pub env_scope: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PresetView {
    pub name: String,
    pub category: String,
    pub source_template: String,
    pub target_subpath: String,
    pub note: Option<String>,
    /// 展开后的源目录（展示用）。
    pub source: String,
    /// 本机是否真的存在这个目录 —— 不存在的说明软件没装，直接不返回给前端。
    pub exists: bool,
    /// 重定向方式：`link` / `setting`。
    pub redirect_mode: String,
    /// `setting` 模式要改的环境变量名。
    pub env_vars: Vec<String>,
    /// `setting` 模式的写入作用域（`user` / `machine`）。
    pub env_scope: String,
    /// 这个预设用「系统设置模式」的可行组合（作用域 + 变量名）。
    /// 空 = 该模板的源路径不是任何 `TEMP` / `TMP` 当前指向的目录。
    pub setting_options: Vec<SettingOption>,
}

/// 清理对象：要么是一条规则（清它真正落地的目录），要么是一个任意文件夹。
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CleanTarget {
    Rule { id: i64 },
    Folder { path: String },
}

/// 预演视图（执行前给用户确认用）。
#[derive(Debug, Clone, Serialize)]
pub struct CleanPreviewView {
    pub dir: String,
    pub label: String,
    pub files: u64,
    pub bytes: u64,
    pub bytes_text: String,
    pub links_skipped: u64,
    pub excluded_skipped: u64,
    pub sample: Vec<CleanSampleEntry>,
    pub summary: String,
}

/// 执行结果视图。
#[derive(Debug, Clone, Serialize)]
pub struct CleanResultView {
    pub dir: String,
    pub label: String,
    pub deleted_files: u64,
    pub deleted_bytes: u64,
    pub deleted_bytes_text: String,
    pub dirs_pruned: u64,
    pub skipped: u64,
    pub skip_examples: Vec<String>,
    pub links_skipped: u64,
    pub excluded_skipped: u64,
    pub summary: String,
}

/// 效益统计：把「重定向到 RAM 盘」的收益量化（都是可解释的数字，不做估算）。
#[derive(Debug, Clone, Serialize)]
pub struct SavingsView {
    /// 当前确实重定向到 RAM 盘的规则数（期望开启 且 上次检测链接正常）。
    pub redirected_rules: usize,
    /// 这些规则当前的缓存总量 —— 它们不写 SSD。
    pub redirected_files: u64,
    pub redirected_bytes: u64,
    pub redirected_text: String,
    /// 历次搬运累计（apply 成功终态）—— 从 SSD 搬到 RAM 盘的累计量。
    pub moved_files: u64,
    pub moved_bytes: u64,
    pub moved_text: String,
    /// 历次清理累计（clean 成功终态）—— 腾出来的空间。
    pub cleaned_files: u64,
    pub cleaned_bytes: u64,
    pub cleaned_text: String,
}

/// 单条规则的缓存目录占用（实时扫描所得）。
#[derive(Debug, Clone, Serialize)]
pub struct DiskUsageEntry {
    pub id: i64,
    pub name: String,
    pub dir: String,
    pub files: u64,
    pub bytes: u64,
    pub bytes_text: String,
    /// 目录当前是否存在（不存在时占用为 0）。
    pub exists: bool,
}

/// 一个卷的容量占用。
#[derive(Debug, Clone, Serialize)]
pub struct VolumeUsage {
    /// 卷根，如 `C:\` 或 `R:\`。
    pub path: String,
    pub total: u64,
    pub free: u64,
    pub used: u64,
    pub total_text: String,
    pub free_text: String,
    pub used_text: String,
    /// 已用百分比（0～100）。
    pub used_pct: f64,
    pub is_ramdisk: bool,
    /// 该卷在本应用里扮演的角色：`RAM 盘` / `目标盘` / `源盘`。
    pub roles: Vec<String>,
}

/// 「磁盘空间占用统计」的完整视图。
#[derive(Debug, Clone, Serialize)]
pub struct DiskUsageView {
    pub rules: Vec<DiskUsageEntry>,
    pub volumes: Vec<VolumeUsage>,
    pub total_bytes: u64,
    pub total_bytes_text: String,
    pub rule_count: usize,
    pub elapsed_ms: u128,
}

/// 一个目标根（RAM 盘）的水位。
#[derive(Debug, Clone, Serialize)]
pub struct WaterLevel {
    pub root_id: i64,
    pub label: String,
    /// 根路径模板原文（如 `R:`）。
    pub path: String,
    /// 已用百分比（0～100）。卷不可用时为 0。
    pub used_pct: f64,
    /// 剩余字节数。
    pub free: u64,
    pub free_text: String,
    pub total_text: String,
    /// `ok` / `warn` / `critical` / `unknown`（卷不可用）。
    pub level: String,
    pub message: String,
}

/// 定时清理一次巡检的结果（「未到点 / 未启用」时是全零 + `ran=false`）。
#[derive(Debug, Clone, Default, Serialize)]
pub struct ScheduledCleanView {
    /// 本次是否真的执行了清理（到点且当天首次）。
    pub ran: bool,
    /// 成功执行的规则数。
    pub done: usize,
    pub failed: usize,
    /// 配了策略但策略不合法、被跳过的规则数。
    pub skipped: usize,
    pub deleted_files: u64,
    pub deleted_bytes: u64,
    pub deleted_text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AutostartView {
    pub registered: bool,
    pub task_name: String,
    pub command: String,
    pub message: String,
    /// 高权限登录任务（只处理 C:\Windows 等受保护源）。
    pub elevated_registered: bool,
    pub elevated_task_name: String,
    pub elevated_command: String,
    pub elevated_message: String,
    /// 系统启动时任务（SYSTEM 身份，登录前重建系统设置模式规则的目标目录）。
    pub startup_registered: bool,
    pub startup_task_name: String,
    pub startup_command: String,
    pub startup_message: String,
    /// 已注册任务的参数是旧版（没有 --skip-elevated / --elevated-only），建议重新启用。
    pub needs_update: bool,
    /// 当前进程是否已提权（界面据此解释"为什么这次不用弹 UAC"）。
    pub elevated: bool,
    pub data_dir: String,
    /// 便携模式下会写进任务的参数尾巴。
    pub extra_args: String,
    pub recommended_exe: String,
}

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
