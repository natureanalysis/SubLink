//! 磁盘空间占用统计用例：逐规则实时扫描 + 相关卷容量汇总。

use std::collections::BTreeMap;
use std::path::Path;

use crate::engine;
use crate::error::SlResult;
use crate::fsops::mover::volume_space;
use crate::fsops::verify::{measure, TreeStats};
use crate::store::Store;
use crate::util;

use super::view::{DiskUsageEntry, DiskUsageView, VolumeUsage};

fn volume_usage_of(root: &str, is_ramdisk: bool, role: &str) -> Option<VolumeUsage> {
    let (total, free) = volume_space(Path::new(root)).ok()?;
    let used = total.saturating_sub(free);
    let used_pct = if total > 0 {
        (used as f64 / total as f64) * 100.0
    } else {
        0.0
    };
    Some(VolumeUsage {
        path: root.to_string(),
        total,
        free,
        used,
        total_text: util::human_bytes(total),
        free_text: util::human_bytes(free),
        used_text: util::human_bytes(used),
        used_pct,
        is_ramdisk,
        roles: vec![role.to_string()],
    })
}

fn merge_volume(map: &mut BTreeMap<String, VolumeUsage>, incoming: Option<VolumeUsage>) {
    let Some(v) = incoming else { return };
    match map.get_mut(&v.path) {
        Some(existing) => {
            existing.is_ramdisk |= v.is_ramdisk;
            for r in v.roles {
                if !existing.roles.contains(&r) {
                    existing.roles.push(r);
                }
            }
        }
        None => {
            map.insert(v.path.clone(), v);
        }
    }
}

/// 实时统计每条规则缓存目录的占用，并汇总相关卷的容量。
///
/// 为什么实时扫描而不是复用 `last_bytes`：后者只在"应用过"的规则上才有值，
/// 反映的还是搬运那一刻的大小、早已过时。占用统计的价值恰恰在"现在到底占了多少"。
/// 代价是慢，所以通过 `on_progress` 回吐进度，UI 侧有反馈。
pub fn disk_usage(
    store: &Store,
    on_progress: &mut dyn FnMut(usize, usize, &str),
) -> SlResult<DiskUsageView> {
    let started = std::time::Instant::now();
    let rules = store.list_rules()?;
    let total = rules.len();
    let mut entries: Vec<DiskUsageEntry> = Vec::new();
    let mut total_bytes: u64 = 0;

    for (i, rule) in rules.iter().enumerate() {
        on_progress(i, total, &rule.name);
        // 变量没解析出来等情况下跳过这条，不拖垮整张统计图。
        let dir = match engine::clean_dir_for_rule(store, rule) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let stats: TreeStats = if dir.is_dir() {
            measure(&dir).unwrap_or_default()
        } else {
            TreeStats::default()
        };
        total_bytes += stats.bytes;
        entries.push(DiskUsageEntry {
            id: rule.id,
            name: rule.name.clone(),
            dir: dir.display().to_string(),
            files: stats.files,
            bytes: stats.bytes,
            bytes_text: util::human_bytes(stats.bytes),
            exists: dir.is_dir(),
        });
    }
    on_progress(total, total, "");
    entries.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.name.cmp(&b.name)));

    // 卷：目标根（RAM 盘）+ 各源目录所在盘，按盘符去重、角色合并。
    let mut vol_map: BTreeMap<String, VolumeUsage> = BTreeMap::new();
    for r in store.list_roots()? {
        let (path, _unknown) = r.resolved_path();
        if let Some(root) = util::volume_root(&path) {
            let role = if r.is_ramdisk {
                crate::i18n::t("disk.role.ram")
            } else {
                crate::i18n::t("disk.role.target")
            };
            merge_volume(&mut vol_map, volume_usage_of(&root, r.is_ramdisk, role));
        }
    }
    for rule in &rules {
        if let Ok(resolved) = engine::resolve(store, rule) {
            if let Some(root) = util::volume_root(&resolved.source) {
                merge_volume(
                    &mut vol_map,
                    volume_usage_of(&root, false, crate::i18n::t("disk.role.source")),
                );
            }
        }
    }
    let mut volumes: Vec<VolumeUsage> = vol_map.into_values().collect();
    // RAM 盘在前，其余按盘符排序。
    volumes.sort_by(|a, b| {
        b.is_ramdisk
            .cmp(&a.is_ramdisk)
            .then_with(|| a.path.cmp(&b.path))
    });

    Ok(DiskUsageView {
        rules: entries,
        volumes,
        total_bytes,
        total_bytes_text: util::human_bytes(total_bytes),
        rule_count: total,
        elapsed_ms: started.elapsed().as_millis(),
    })
}
