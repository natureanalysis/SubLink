//! 表单实时校验与状态展示用例（源/目标检查、状态文案、路径与字节格式化）。

use std::path::PathBuf;

use crate::boot::probe_volume;
use crate::error::SlResult;
use crate::link::detect::{detect_link, LinkState, TargetState};
use crate::safety;
use crate::store::Store;
use crate::util;

use super::view::{SourceInspection, TargetInspection};

/// 给 UI 的状态文案（把 LinkState 摊平成一行摘要）。
pub fn describe_state(state: &LinkState) -> String {
    use crate::i18n::t;
    match state {
        LinkState::LinkOk {
            target_state: TargetState::Empty,
            ..
        } => t("inspect.state.link_ok_empty").into(),
        LinkState::LinkOk { .. } => t("inspect.state.link_ok").into(),
        other => other.label(),
    }
}

/// 源路径的父目录（给"在资源管理器里打开"用）。
pub fn parent_of_display(path: &str) -> Option<PathBuf> {
    let p = PathBuf::from(path);
    if p.is_dir() {
        Some(p)
    } else {
        p.parent().filter(|q| q.is_dir()).map(|q| q.to_path_buf())
    }
}

pub fn human_bytes(n: u64) -> String {
    util::human_bytes(n)
}

/// 校验源路径，返回警告与是否需要提权（表单实时提示用）。
pub fn inspect_source(template: &str) -> SlResult<SourceInspection> {
    let (expanded, unknown) = util::expand_path(template);
    let pf = safety::preflight_source(&expanded);
    Ok(SourceInspection {
        expanded: expanded.display().to_string(),
        exists: expanded.exists(),
        blocking: pf.blocking,
        warnings: pf.warnings,
        requires_elevation: pf.requires_elevation,
        unknown_vars: unknown,
    })
}

/// 目标路径是否已被占用（表单校验用）。
pub fn inspect_target(store: &Store, root_id: i64, subpath: &str) -> SlResult<TargetInspection> {
    let root = store.get_root(root_id)?;
    let (base, unknown) = root.resolved_path();
    let sub = subpath.trim().trim_start_matches(['\\', '/']);
    let target = if sub.is_empty() {
        base.clone()
    } else {
        base.join(sub)
    };
    let target_state = detect_link(&target, None);
    let root_state = probe_volume(&base).0;
    Ok(TargetInspection {
        target: target.display().to_string(),
        // 卷在、根目录还没建（RAM 盘刚重启）也算"可用"——应用时会自动建出来
        root_ready: root_state == "ready" || root_state == "not_created",
        exists: target.exists(),
        occupied: matches!(
            target_state,
            LinkState::PlainDir | LinkState::PlainFile | LinkState::ForeignReparse { .. }
        ),
        state: describe_state(&target_state),
        unknown_vars: unknown,
    })
}
