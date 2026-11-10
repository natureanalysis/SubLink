//! 用例门面层。
//!
//! GUI（Tauri 命令层）/ CLI / 无界面子模式**只依赖这一层**，不直接编排
//! `engine` / `fsops` / `link` 等内部模块 —— 内部重构只影响本层。
//! 本层函数返回 serde 可序列化结构（前端 JSON 契约），故可脱离窗口单测。

pub mod acl;
pub mod apply;
pub mod autostart;
pub mod clean;
pub mod disk;
pub mod elevated;
pub mod inspect;
pub mod lifecycle;
pub mod overview;
pub mod presets;
pub mod rules;
pub mod schedule;
pub mod stats;
pub mod view;
pub mod water;

pub use acl::{acl_fix, AclItem, AclReport};
pub use apply::{apply_rules, check_locks, relink, restore_rules};
pub use autostart::{
    autostart_disable, autostart_elevated_disable, autostart_elevated_enable, autostart_enable,
    autostart_run_now, autostart_startup_disable, autostart_startup_enable, autostart_status,
};
pub use clean::{clean_preview, clean_run};
pub use disk::disk_usage;
pub use elevated::run_elevated_job;
pub use inspect::{describe_state, human_bytes, inspect_source, inspect_target, parent_of_display};
pub use lifecycle::{boot_rebuild, boot_rebuild_scoped, export_config, import_config};
pub use overview::{
    last_boot_problem, recent_events, recent_ops, root_views, rule_views, snapshot,
};
pub use presets::{preset_rules, user_presets_path};
pub use rules::{
    add_root, add_rule, capture_acl, clear_acl, delete_root, delete_rules, hidden_rule_ids,
    reorder_rules, set_category, set_enabled, set_hidden, undo_last, undo_state, update_rule,
};
pub use schedule::scheduled_clean_poll;
pub use stats::savings;
pub use view::*;
pub use water::{water_levels, water_poll};

#[cfg(test)]
mod tests;
