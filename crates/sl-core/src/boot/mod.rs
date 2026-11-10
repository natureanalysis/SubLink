//! 开机守护：让 RAM 盘重启后的"空目录"永远存在。

pub mod rebuild;
pub mod volume;

pub use rebuild::{run_boot_rebuild, run_boot_rebuild_scoped, BootReport, BootScope, RootOutcome};
pub use volume::{ensure_root_dir, is_volume_ready, probe_volume, wait_for_root, wait_for_volume};
