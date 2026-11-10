//! 编排层：把「规则 + 目标根」翻译成一次可审计、可回滚的操作。
//!
//! ## 应用一条规则的决策树（`apply_rule`）
//!
//! | 当前状态 | 动作 |
//! |---|---|
//! | 链接正常 + 目标有内容 | 幂等返回，什么都不做 |
//! | 链接正常 + 目标为空 | 幂等返回（RAM 盘刚重启的正常形态） |
//! | 链接正常但指向与登记不符 | 拒绝，要求先恢复 |
//! | 链接断裂（目标丢失） | 重建目标目录，链接即恢复 |
//! | 源不存在 | 建目标目录 + 直接建链（软件首次运行就会落到 RAM 盘） |
//! | 源是普通目录 + 目标空 | 搬运 → 校验 → 建链 |
//! | 源是普通目录 + 目标有内容 | 合并（新者胜）→ 删源 → 建链 |
//! | 外来重解析点 | 拒绝，绝不触碰 |
//!
//! ## 失败一定回滚
//!
//! 建链失败会把已搬过去的内容**搬回来**；搬不回来则留下一条非终态的操作流水，
//! 下次启动时作为「待修复」提示给用户，而不是静默留下一个半成品。
//!
//! ## 子模块
//!
//! - `resolve`：规则 → 源/目标路径（展开环境变量、安全预检、清理目标判定）
//! - `status`：体检（`RuleStatus`、批量扫描、写库的状态机键）
//! - `apply`：应用（搬运/合并 → 建链，失败回滚）
//! - `restore`：恢复（拆链搬回）、只重建链接（relink）、改目标后重指向（retarget）
//! - `perm`：权限兜底（建完目录/链接后给对应使用者完全控制）

mod apply;
mod perm;
mod resolve;
mod restore;
mod setting;
mod status;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod setting_tests;

#[cfg(test)]
mod setting_options_tests;

pub use apply::{apply_rule, ApplyOptions, Outcome};
pub use perm::{ensure_dir_access, ensure_link_access};
pub use resolve::{
    boot_requires_elevation, clean_dir_for_rule, is_setting_mode, resolve, rule_link_type,
    rule_requires_elevation, source_requires_elevation, Resolved,
};
pub use restore::{relink_rule, restore_rule, retarget_rule_link};
pub use setting::{
    apply_setting_rule, merge_into_target, rebuild_setting_dir, relink_setting_rule,
    restore_setting_rule, rule_scope, rule_vars, setting_options_for_source, setting_state,
    var_points_at, SettingOption,
};

#[cfg(test)]
pub use setting::{
    env_prev_key_for_test, load_env_prev_for_test, save_env_prev_for_test, VarSnapshotForTest,
};
pub use status::{rule_status, state_key, status_all, RuleStatus};