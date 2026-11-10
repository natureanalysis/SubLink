//! 生命周期用例：开机重建、配置导出/导入。

use std::path::Path;

use crate::boot::{run_boot_rebuild, run_boot_rebuild_scoped, BootScope};
use crate::error::{SlError, SlResult};
use crate::i18n::tf;
use crate::portable;
use crate::store::Store;

use super::view::{BootReport, ImportReport};

pub fn boot_rebuild(store: &Store) -> SlResult<BootReport> {
    run_boot_rebuild(store)
}

/// 按权限分工的开机重建（登录任务用）。
pub fn boot_rebuild_scoped(store: &Store, scope: BootScope) -> SlResult<BootReport> {
    run_boot_rebuild_scoped(store, scope)
}

pub fn export_config(store: &Store, path: &Path) -> SlResult<(usize, usize)> {
    let bundle = portable::export_bundle(store)?;
    let json = serde_json::to_string_pretty(&bundle)?;
    std::fs::write(path, json.as_bytes()).map_err(|e| {
        SlError::Other(tf(
            "lifecycle.write_failed",
            &[
                ("path", &path.display().to_string()),
                ("err", &e.to_string()),
            ],
        ))
    })?;
    Ok((bundle.roots.len(), bundle.rules.len()))
}

pub fn import_config(
    store: &Store,
    path: &Path,
    ram_root: Option<&str>,
    dry_run: bool,
) -> SlResult<ImportReport> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        SlError::Other(tf(
            "lifecycle.read_failed",
            &[
                ("path", &path.display().to_string()),
                ("err", &e.to_string()),
            ],
        ))
    })?;
    let bundle: portable::Bundle = serde_json::from_str(&text).map_err(|e| {
        SlError::Other(tf(
            "lifecycle.parse_failed",
            &[
                ("path", &path.display().to_string()),
                ("err", &e.to_string()),
            ],
        ))
    })?;
    portable::import_bundle(store, &bundle, ram_root, dry_run)
}
