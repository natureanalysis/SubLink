//! 安全搬运。
//!
//! ## 两条路径
//!
//! * **同卷 → `rename`**：原子、瞬时，ACL / 时间戳 / ADS / 硬链接全部保留。优先走这条。
//! * **跨卷 → 复制 + 校验 + 删源**：RAM 盘场景走的就是这条。顺序不能变，
//!   **校验通过之前绝不删源**（`verify::compare_stats` 是唯一放行依据）。
//!
//! ## 明确不做的事
//!
//! * 不跟随目录树内的重解析点。遇到外来重解析点（OneDrive 占位符之类）
//!   直接失败并列出，而不是"尽力复制" —— 半吊子的搬运比失败更危险。
//! * 不用 `remove_dir_all`。删除一律走 [`delete_tree`]，它逐条判断类型，
//!   对重解析点只删链接条目本身。

use std::fs;
use std::path::{Path, PathBuf};

use filetime::FileTime;
use serde::Serialize;
use walkdir::WalkDir;
use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

use crate::error::{io_ctx, SlError, SlResult};
use crate::fsops::occupancy;
use crate::fsops::verify::{self, TreeStats, VerifyMode};
use crate::i18n::{t, tf};
use crate::link::create::{
    read_link_target, read_reparse_tag, IO_REPARSE_TAG_MOUNT_POINT, IO_REPARSE_TAG_SYMLINK,
};
use crate::util;

/// 跨卷复制时预留的余量（避免刚好卡在容量边界上复制失败）。
const SPACE_HEADROOM: u64 = 16 * 1024 * 1024;
/// 进度回调的节流粒度。
const PROGRESS_EVERY: u64 = 200;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum Progress {
    Enumerated {
        files: u64,
        bytes: u64,
    },
    Renaming {
        from: String,
        to: String,
    },
    Copying {
        files: u64,
        bytes: u64,
        current: String,
    },
    Verifying,
    Deleting {
        files: u64,
    },
    Linking {
        link: String,
        target: String,
    },
    Done {
        files: u64,
        bytes: u64,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct MoveReport {
    pub stats: TreeStats,
    /// "同卷重命名" / "跨卷复制"
    pub method: &'static str,
    /// 树内重建的链接数。
    pub links_recreated: u64,
    /// 尽力模式下拷贝阶段被跳过的条目（严格模式永远为空）。
    pub skipped: Vec<SkippedEntry>,
    pub elapsed_ms: u128,
}

/// 尽力模式下被跳过的条目。
#[derive(Debug, Clone, Serialize)]
pub struct SkippedEntry {
    pub path: String,
    pub reason: String,
}

/// 复制结果。尽力模式下 `skipped` 会有内容。
#[derive(Debug, Clone, Default, Serialize)]
pub struct CopyOutcome {
    pub files: u64,
    pub bytes: u64,
    pub links: u64,
    pub skipped: Vec<SkippedEntry>,
}

/// 删除结果。`failures` 非空表示树里还有删不掉的东西。
#[derive(Debug, Clone, Default, Serialize)]
pub struct DeleteOutcome {
    pub removed: u64,
    pub failures: Vec<String>,
}

/// 查询某个路径所在卷的可用空间。
pub fn free_space(path: &Path) -> SlResult<u64> {
    let wide = util::to_wide(&util::to_verbatim(path).to_string_lossy());
    let mut free: u64 = 0;
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io_ctx(
            std::io::Error::last_os_error(),
            path,
            t("io.action.query_free_space"),
        ));
    }
    Ok(free)
}

/// 查询某个路径所在卷的 `(总容量, 可用空间)`。
///
/// 与 [`free_space`] 的区别：这里把 `GetDiskFreeSpaceExW` 的第三个出参（总字节数）
/// 也取出来 —— 「磁盘占用统计」要画「已用 / 可用」占比，光有可用空间画不出图。
pub fn volume_space(path: &Path) -> SlResult<(u64, u64)> {
    let wide = util::to_wide(&util::to_verbatim(path).to_string_lossy());
    let mut free: u64 = 0;
    let mut total: u64 = 0;
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free,
            &mut total,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io_ctx(
            std::io::Error::last_os_error(),
            path,
            t("io.action.query_volume"),
        ));
    }
    Ok((total, free))
}

/// 预扫描树内的重解析点。
///
/// 返回 `(可重建的链接数, 不支持的重解析点列表)`。第二项非空时必须**放弃搬运**：
/// 我们不认识的占位符（OneDrive / AppExecLink / 卷挂载点）没法忠实复制，
/// 而"复制不全却删了源"是不可接受的。
pub fn scan_links(src: &Path) -> SlResult<(u64, Vec<PathBuf>)> {
    let mut recreatable = 0u64;
    let mut foreign = Vec::new();
    for entry in WalkDir::new(util::to_verbatim(src))
        .follow_links(false)
        .into_iter()
    {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().is_symlink() {
            continue;
        }
        match read_reparse_tag(entry.path())? {
            Some(IO_REPARSE_TAG_MOUNT_POINT) | Some(IO_REPARSE_TAG_SYMLINK) => recreatable += 1,
            _ => foreign.push(util::simplify(entry.path())),
        }
    }
    Ok((recreatable, foreign))
}

/// 把 `src` 整个搬到 `dst`（`dst` 不存在或必须是空目录）。
pub fn move_tree(
    src: &Path,
    dst: &Path,
    verify_mode: VerifyMode,
    sink: &mut dyn FnMut(Progress),
) -> SlResult<MoveReport> {
    move_tree_ex(src, dst, verify_mode, false, sink)
}

/// [`move_tree`] 的尽力模式版本。
///
/// `best_effort = true` 时：读不了 / 拷不动 / 重命名失败的**单个条目**会被跳过并记录，
/// 而不是让整棵树失败；同卷重命名失败会退化为"复制 → 校验 → 删源"。
///
/// 唯一不妥协的是**删源**：只要源里还有删不掉的文件（被占用），就不会建立链接，
/// 而是返回错误并把已复制的内容留在目标里 —— 下次重试时合并路径会接着收敛。
pub fn move_tree_ex(
    src: &Path,
    dst: &Path,
    verify_mode: VerifyMode,
    best_effort: bool,
    sink: &mut dyn FnMut(Progress),
) -> SlResult<MoveReport> {
    let started = std::time::Instant::now();

    let md = fs::symlink_metadata(src).map_err(|e| io_ctx(e, src, t("io.action.read_source")))?;
    if md.file_type().is_symlink() {
        return Err(SlError::SafetyBlocked(tf("move.src_is_link", &[
            ("path", &src.display().to_string()),
        ])));
    }
    if !md.is_dir() {
        return Err(SlError::InvalidPath(tf("move.src_not_dir", &[
            ("path", &src.display().to_string()),
        ])));
    }
    if util::path_eq(src, dst) {
        return Err(SlError::SafetyBlocked(t("move.same_path").into()));
    }

    if dst.exists() {
        if !dst.is_dir() {
            return Err(SlError::InvalidPath(tf("move.dst_not_dir", &[
                ("path", &dst.display().to_string()),
            ])));
        }
        if !occupancy::dir_is_empty(dst)? {
            return Err(SlError::SafetyBlocked(tf("move.dst_not_empty", &[
                ("path", &dst.display().to_string()),
            ])));
        }
    } else if let Some(parent) = dst.parent() {
        fs::create_dir_all(util::to_verbatim(parent))
            .map_err(|e| io_ctx(e, parent, t("io.action.create_parent")))?;
    }

    // 预扫描：外来重解析点 → 立刻失败，此时什么都还没动。
    let (_recreatable, foreign) = scan_links(src)?;
    if !foreign.is_empty() {
        let list = foreign
            .iter()
            .take(5)
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join("；");
        return Err(SlError::SafetyBlocked(tf("move.foreign_reparse", &[
            ("n", &foreign.len().to_string()),
            ("list", &list),
        ])));
    }

    let stats = verify::measure(src)?;

    // ---- 同卷：原子重命名 ----
    if util::same_volume(src, dst) {
        if dst.exists() {
            // 只删空目录，不用 remove_dir_all
            fs::remove_dir(util::to_verbatim(dst))
                .map_err(|e| io_ctx(e, dst, t("io.action.clean_empty_target")))?;
        }
        sink(Progress::Renaming {
            from: src.display().to_string(),
            to: dst.display().to_string(),
        });
        match fs::rename(util::to_verbatim(src), util::to_verbatim(dst)) {
            Ok(()) => {
                sink(Progress::Done {
                    files: stats.files,
                    bytes: stats.bytes,
                });
                return Ok(MoveReport {
                    stats,
                    method: t("move.method.rename"),
                    links_recreated: 0,
                    skipped: Vec::new(),
                    elapsed_ms: started.elapsed().as_millis(),
                });
            }
            Err(e) if !best_effort => {
                return Err(io_ctx(e, src, t("io.action.rename")));
            }
            Err(_) => {
                // 树里有被打开的文件时目录改不了名 —— 尽力模式下退化为复制，
                // 能拿多少拿多少，删不掉的部分由下面的删源阶段如实报出。
            }
        }
    }

    // ---- 跨卷：复制 → 校验 → 删源 ----
    sink(Progress::Enumerated {
        files: stats.files,
        bytes: stats.bytes,
    });

    let probe = dst.parent().unwrap_or(dst);
    let free = free_space(probe)?;
    if free < stats.bytes.saturating_add(SPACE_HEADROOM) {
        return Err(SlError::NoSpace {
            need: util::human_bytes(stats.bytes.saturating_add(SPACE_HEADROOM)),
            free: util::human_bytes(free),
        });
    }

    let copied = copy_tree_ex(src, dst, best_effort, sink)?;

    sink(Progress::Verifying);
    let actual = verify::measure(dst)?;
    if best_effort && !copied.skipped.is_empty() {
        // 有跳过项时源的目标态不再是原始快照，只能按"实际拷出去多少"核对。
        if copied.files != actual.files
            || copied.bytes != actual.bytes
            || copied.links != actual.links
        {
            return Err(SlError::VerifyFailed(tf("move.count_mismatch", &[
                ("copied", &copied.files.to_string()),
                ("actual", &actual.files.to_string()),
            ])));
        }
    } else {
        if copied.files != actual.files {
            return Err(SlError::VerifyFailed(tf("move.count_mismatch", &[
                ("copied", &copied.files.to_string()),
                ("actual", &actual.files.to_string()),
            ])));
        }
        verify::compare_stats(&stats, &actual, t("move.verify_ctx"))?;

        if verify_mode == VerifyMode::Sha256 {
            let h_src = verify::hash_tree(src)?;
            let h_dst = verify::hash_tree(dst)?;
            if h_src != h_dst {
                return Err(SlError::VerifyFailed(tf("move.sha_mismatch", &[
                    ("src", &h_src),
                    ("dst", &h_dst),
                ])));
            }
        }
    }

    // 有读不出来的条目时绝不删源：那些文件的副本根本不在目标里。
    if !copied.skipped.is_empty() {
        return Err(SlError::Busy(tf("move.best_effort_skipped", &[
            ("n", &copied.skipped.len().to_string()),
            ("list", &copied
                .skipped
                .iter()
                .take(3)
                .map(|s| s.path.clone())
                .collect::<Vec<_>>()
                .join("；")),
        ])));
    }

    // 到这里才允许删源。
    let del = delete_tree_ex(src, sink)?;
    if !del.failures.is_empty() {
        return Err(SlError::Busy(tf("move.best_effort_leftovers", &[
            ("copied", &copied.files.to_string()),
            ("n", &del.failures.len().to_string()),
            ("list", &del.failures.iter().take(3).cloned().collect::<Vec<_>>().join("；")),
        ])));
    }

    sink(Progress::Done {
        files: stats.files,
        bytes: stats.bytes,
    });
    Ok(MoveReport {
        stats,
        method: t("move.method.copy"),
        links_recreated: copied.links,
        skipped: copied.skipped,
        elapsed_ms: started.elapsed().as_millis(),
    })
}

/// 复制整棵树（含重建链接）。返回 `(文件数, 重建的链接数)`。
pub fn copy_tree(src: &Path, dst: &Path, sink: &mut dyn FnMut(Progress)) -> SlResult<(u64, u64)> {
    let out = copy_tree_ex(src, dst, false, sink)?;
    Ok((out.files, out.links))
}

/// [`copy_tree`] 的尽力模式版本：单个条目失败只记录、不中断。
pub fn copy_tree_ex(
    src: &Path,
    dst: &Path,
    best_effort: bool,
    sink: &mut dyn FnMut(Progress),
) -> SlResult<CopyOutcome> {
    let src_v = util::to_verbatim(src);
    let dst_v = util::to_verbatim(dst);
    fs::create_dir_all(&dst_v).map_err(|e| io_ctx(e, dst, t("io.action.create_target")))?;

    let mut out = CopyOutcome::default();

    for entry in WalkDir::new(&src_v)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
    {
        let entry = match entry {
            Ok(e) => e,
            Err(e) if best_effort => {
                out.skipped.push(SkippedEntry {
                    path: util::simplify(src).display().to_string(),
                    reason: e.to_string(),
                });
                continue;
            }
            Err(e) => {
                return Err(SlError::Other(tf("move.walk_failed", &[
                    ("err", &e.to_string()),
                    ("path", &src.display().to_string()),
                ])))
            }
        };
        let rel = match entry.path().strip_prefix(&src_v) {
            Ok(r) => r.to_path_buf(),
            Err(_) => continue,
        };
        if rel.as_os_str().is_empty() {
            continue; // 根目录本身
        }
        let to = dst_v.join(&rel);
        let ft = entry.file_type();

        if ft.is_symlink() {
            let result = (|| -> SlResult<()> {
                let tag = read_reparse_tag(entry.path())?;
                let target = read_link_target(entry.path())?;
                let target_v = util::to_verbatim(&target);
                match tag {
                    Some(IO_REPARSE_TAG_MOUNT_POINT) => {
                        // junction crate 的参数顺序是 (target, junction)
                        junction::create(&target_v, &to).map_err(|e| {
                            SlError::Other(tf("move.recreate_junction_failed", &[
                                ("err", &e.to_string()),
                                ("from", &entry.path().display().to_string()),
                                ("to", &target.display().to_string()),
                            ]))
                        })?;
                    }
                    Some(IO_REPARSE_TAG_SYMLINK) => {
                        std::os::windows::fs::symlink_dir(&target_v, &to).map_err(|e| {
                            SlError::Other(tf("move.recreate_symlink_failed", &[
                                ("err", &e.to_string()),
                            ]))
                        })?;
                    }
                    other => {
                        return Err(SlError::SafetyBlocked(tf("move.unsupported_reparse", &[
                            ("tag", &format!("{other:?}")),
                            ("path", &entry.path().display().to_string()),
                        ])));
                    }
                }
                Ok(())
            })();

            match result {
                Ok(()) => out.links += 1,
                Err(e) if best_effort => out.skipped.push(SkippedEntry {
                    path: util::simplify(entry.path()).display().to_string(),
                    reason: e.to_string(),
                }),
                Err(e) => return Err(e),
            }
            continue;
        }

        if ft.is_dir() {
            match fs::create_dir_all(&to) {
                Ok(()) => {}
                Err(e) if best_effort => out.skipped.push(SkippedEntry {
                    path: util::simplify(entry.path()).display().to_string(),
                    reason: e.to_string(),
                }),
                Err(e) => return Err(io_ctx(e, entry.path(), t("io.action.create_subdir"))),
            }
            continue;
        }

        if ft.is_file() {
            let n = match fs::copy(entry.path(), &to) {
                Ok(n) => n,
                Err(e) if best_effort => {
                    out.skipped.push(SkippedEntry {
                        path: util::simplify(entry.path()).display().to_string(),
                        reason: e.to_string(),
                    });
                    continue;
                }
                Err(e) => return Err(io_ctx(e, entry.path(), t("io.action.copy_file"))),
            };
            out.files += 1;
            out.bytes += n;

            // 保留时间戳与只读属性
            if let Ok(md) = entry.metadata() {
                let _ = filetime::set_file_mtime(&to, FileTime::from_last_modification_time(&md));
                let _ = filetime::set_file_atime(&to, FileTime::from_last_access_time(&md));
                if md.permissions().readonly() {
                    if let Ok(mut perm) = fs::metadata(&to).map(|m| m.permissions()) {
                        perm.set_readonly(true);
                        let _ = fs::set_permissions(&to, perm);
                    }
                }
            }

            if out.files % PROGRESS_EVERY == 0 {
                sink(Progress::Copying {
                    files: out.files,
                    bytes: out.bytes,
                    current: util::simplify(entry.path()).display().to_string(),
                });
            }
        }
    }

    Ok(out)
}

/// 删除整棵目录树。返回删除的文件数。
///
/// * 根是重解析点 → 直接拒绝（那是链接，应该用 `link::remove_dir_link`）。
/// * 子项是重解析点 → 只删链接条目本身，**不跟随**。
/// * 只读文件先清掉只读属性再删（Windows 上只读文件无法直接删除）。
pub fn delete_tree(dir: &Path, sink: &mut dyn FnMut(Progress)) -> SlResult<u64> {
    let out = delete_tree_ex(dir, sink)?;
    if !out.failures.is_empty() {
        return Err(SlError::Busy(tf("move.delete_failures", &[
            ("n", &out.failures.len().to_string()),
            ("list", &out.failures.iter().take(3).cloned().collect::<Vec<_>>().join("；")),
        ])));
    }
    Ok(out.removed)
}

/// [`delete_tree`] 的明细版本：删不掉的条目放在 `failures` 里而不是直接报错。
pub fn delete_tree_ex(dir: &Path, sink: &mut dyn FnMut(Progress)) -> SlResult<DeleteOutcome> {
    let md = fs::symlink_metadata(dir).map_err(|e| io_ctx(e, dir, t("io.action.read_delete_dir")))?;
    if md.file_type().is_symlink() {
        return Err(SlError::SafetyBlocked(tf("move.refuse_delete_reparse", &[
            ("path", &dir.display().to_string()),
        ])));
    }
    if !md.is_dir() {
        return Err(SlError::InvalidPath(tf("move.delete_not_dir", &[
            ("path", &dir.display().to_string()),
        ])));
    }

    let mut out = DeleteOutcome::default();

    // contents_first：先删子项再删父项，这样父目录删的时候已经是空的。
    for entry in WalkDir::new(util::to_verbatim(dir))
        .contents_first(true)
        .follow_links(false)
        .into_iter()
    {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                out.failures.push(e.to_string());
                continue;
            }
        };
        let path = entry.path();
        let ft = entry.file_type();

        let outcome = if ft.is_symlink() {
            // 链接条目：先按目录链接删，不行再按文件链接删。绝不进目标。
            fs::remove_dir(path).or_else(|_| fs::remove_file(path))
        } else if ft.is_dir() {
            fs::remove_dir(path)
        } else {
            match fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(e) if e.raw_os_error() == Some(5) => {
                    clear_readonly(path);
                    fs::remove_file(path)
                }
                Err(e) => Err(e),
            }
        };

        match outcome {
            Ok(()) => {
                if ft.is_file() {
                    out.removed += 1;
                    if out.removed % PROGRESS_EVERY == 0 {
                        sink(Progress::Deleting { files: out.removed });
                    }
                }
            }
            Err(e) => out.failures.push(tf("move.delete_failure_entry", &[
                ("path", &util::simplify(path).display().to_string()),
                ("err", &e.to_string()),
            ])),
        }
    }

    Ok(out)
}

// 仅 Windows：此处在 Unix 上会有 world-writable 顾虑，本工具不支持 Unix。
#[allow(clippy::permissions_set_readonly_false)]
fn clear_readonly(path: &Path) {
    if let Ok(md) = fs::symlink_metadata(path) {
        let mut perm = md.permissions();
        if perm.readonly() {
            perm.set_readonly(false);
            let _ = fs::set_permissions(path, perm);
        }
    }
}

/// 合并结果。
#[derive(Debug, Clone, Serialize)]
pub struct MergeReport {
    /// 目标里没有、从源复制过去的文件数。
    pub copied: u64,
    /// 目标里已有、且不比源旧，被跳过的文件数。
    pub skipped: u64,
    /// 目标里已有、但比源旧，被覆盖的文件数。
    pub overwritten: u64,
    pub bytes: u64,
    /// 尽力模式下读不了 / 拷不动的条目（严格模式永远为空）。
    pub failed: Vec<SkippedEntry>,
}

/// 把 `src` 的内容合并进 `dst`（`dst` 可以非空，必须已存在）。
///
/// 冲突策略：**同相对路径取修改时间较新的一方**。已存在且不比源新的文件跳过，
/// 不删除目标里多出来的文件。
///
/// 用于"软件更新把缓存目录重建成了真实目录，而 RAM 盘上还留着旧缓存"的场景。
/// 注意这里的核对不变式比 `move_tree` 弱：只保证**源里的每个条目都在目标里存在过**，
/// 不做字节级校验（合并本来就是有取舍的）。
pub fn merge_into(src: &Path, dst: &Path, sink: &mut dyn FnMut(Progress)) -> SlResult<MergeReport> {
    merge_into_ex(src, dst, false, sink)
}

/// [`merge_into`] 的尽力模式版本：单个条目失败只记录，不中断。
pub fn merge_into_ex(
    src: &Path,
    dst: &Path,
    best_effort: bool,
    sink: &mut dyn FnMut(Progress),
) -> SlResult<MergeReport> {
    let src_v = util::to_verbatim(src);
    let dst_v = util::to_verbatim(dst);

    if !dst.is_dir() {
        return Err(SlError::InvalidPath(tf("move.merge_dst_missing", &[
            ("path", &dst.display().to_string()),
        ])));
    }

    let mut report = MergeReport {
        copied: 0,
        skipped: 0,
        overwritten: 0,
        bytes: 0,
        failed: Vec::new(),
    };
    // 记录失败条目对应的源绝对路径，核对不变式时要排除它们的整棵子树。
    let mut failed_src: Vec<PathBuf> = Vec::new();

    for entry in WalkDir::new(&src_v)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
    {
        let entry = match entry {
            Ok(e) => e,
            Err(e) if best_effort => {
                failed_src.push(src_v.clone());
                report.failed.push(SkippedEntry {
                    path: util::simplify(src).display().to_string(),
                    reason: e.to_string(),
                });
                continue;
            }
            Err(e) => {
                return Err(SlError::Other(tf("move.walk_failed", &[
                    ("err", &e.to_string()),
                    ("path", &src.display().to_string()),
                ])))
            }
        };
        let rel = match entry.path().strip_prefix(&src_v) {
            Ok(r) => r.to_path_buf(),
            Err(_) => continue,
        };
        if rel.as_os_str().is_empty() {
            continue;
        }
        let to = dst_v.join(&rel);
        let ft = entry.file_type();

        if ft.is_symlink() {
            if fs::symlink_metadata(&to).is_ok() {
                report.skipped += 1;
                continue;
            }
            let result = (|| -> SlResult<()> {
                let tag = read_reparse_tag(entry.path())?;
                let target = read_link_target(entry.path())?;
                let target_v = util::to_verbatim(&target);
                match tag {
                    Some(IO_REPARSE_TAG_MOUNT_POINT) => {
                        // junction crate 的参数顺序是 (target, junction)
                        junction::create(&target_v, &to).map_err(|e| {
                            SlError::Other(tf("move.recreate_junction_failed_merge", &[
                                ("err", &e.to_string()),
                                ("to", &to.display().to_string()),
                            ]))
                        })?;
                    }
                    Some(IO_REPARSE_TAG_SYMLINK) => {
                        std::os::windows::fs::symlink_dir(&target_v, &to).map_err(|e| {
                            SlError::Other(tf("move.recreate_symlink_failed", &[
                                ("err", &e.to_string()),
                            ]))
                        })?;
                    }
                    other => {
                        return Err(SlError::SafetyBlocked(tf("move.unsupported_reparse", &[
                            ("tag", &format!("{other:?}")),
                            ("path", &entry.path().display().to_string()),
                        ])));
                    }
                }
                Ok(())
            })();

            match result {
                Ok(()) => report.copied += 1,
                Err(e) if best_effort => {
                    failed_src.push(entry.path().to_path_buf());
                    report.failed.push(SkippedEntry {
                        path: util::simplify(entry.path()).display().to_string(),
                        reason: e.to_string(),
                    });
                }
                Err(e) => return Err(e),
            }
            continue;
        }

        if ft.is_dir() {
            match fs::create_dir_all(&to) {
                Ok(()) => {}
                Err(e) if best_effort => {
                    failed_src.push(entry.path().to_path_buf());
                    report.failed.push(SkippedEntry {
                        path: util::simplify(entry.path()).display().to_string(),
                        reason: e.to_string(),
                    });
                }
                Err(e) => return Err(io_ctx(e, entry.path(), t("io.action.create_subdir"))),
            }
            continue;
        }

        if ft.is_file() {
            let src_md = entry.metadata().ok();
            let dst_md = fs::symlink_metadata(&to).ok();

            let was_overwrite = if let Some(dm) = dst_md {
                let src_time = src_md
                    .as_ref()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let dst_time = dm
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                if src_time <= dst_time {
                    report.skipped += 1;
                    continue;
                }
                true
            } else {
                false
            };

            let n = match fs::copy(entry.path(), &to) {
                Ok(n) => n,
                Err(e) if best_effort => {
                    failed_src.push(entry.path().to_path_buf());
                    report.failed.push(SkippedEntry {
                        path: util::simplify(entry.path()).display().to_string(),
                        reason: e.to_string(),
                    });
                    continue;
                }
                Err(e) => return Err(io_ctx(e, entry.path(), t("io.action.merge_file"))),
            };
            if was_overwrite {
                report.overwritten += 1;
            } else {
                report.copied += 1;
            }
            report.bytes += n;

            if let Some(md) = src_md {
                let _ = filetime::set_file_mtime(&to, FileTime::from_last_modification_time(&md));
                let _ = filetime::set_file_atime(&to, FileTime::from_last_access_time(&md));
            }

            let done = report.copied + report.overwritten;
            if done % PROGRESS_EVERY == 0 {
                sink(Progress::Copying {
                    files: done,
                    bytes: report.bytes,
                    current: util::simplify(entry.path()).display().to_string(),
                });
            }
        }
    }

    // 不变式：源里的每个文件/链接都必须已经在目标里存在
    let mut missing: Vec<String> = Vec::new();
    for entry in WalkDir::new(&src_v)
        .follow_links(false)
        .into_iter()
        .flatten()
    {
        if entry.path() == src_v {
            continue;
        }
        if entry.file_type().is_dir() {
            continue;
        }
        if failed_src.iter().any(|f| entry.path() == f || entry.path().starts_with(f)) {
            continue; // 已经作为失败条目上报过，不再算"丢文件"
        }
        let rel = entry.path().strip_prefix(&src_v).unwrap_or(entry.path());
        if fs::symlink_metadata(dst_v.join(rel)).is_err() {
            missing.push(util::simplify(entry.path()).display().to_string());
            if missing.len() >= 5 {
                break;
            }
        }
    }
    if !missing.is_empty() {
        return Err(SlError::VerifyFailed(tf("move.merge_missing", &[
            ("n", &missing.len().to_string()),
            ("list", &missing.join("；")),
        ])));
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("td_move_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        base
    }

    fn noop(_: Progress) {}

    #[test]
    fn cross_volume_style_copy_then_delete() {
        // 用同卷但强制走复制路径来验证 copy+verify+delete 逻辑
        let base = fixture("copy");
        let src = base.join("src");
        let dst = base.join("dst");
        fs::create_dir_all(src.join("deep").join("deeper")).unwrap();
        fs::write(src.join("a.txt"), b"aaa").unwrap();
        fs::write(src.join("deep").join("b.bin"), vec![7u8; 4096]).unwrap();
        fs::write(src.join("deep").join("deeper").join("c.txt"), b"c").unwrap();

        let stats_before = verify::measure(&src).unwrap();
        let (_files, links) = copy_tree(&src, &dst, &mut noop).unwrap();
        assert_eq!(links, 0);

        let stats_after = verify::measure(&dst).unwrap();
        verify::compare_stats(&stats_before, &stats_after, "测试核对").unwrap();

        let removed = delete_tree(&src, &mut noop).unwrap();
        assert_eq!(removed, 3);
        assert!(!src.exists());
        assert!(dst.join("deep").join("b.bin").exists());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn delete_tree_refuses_reparse_root() {
        let base = fixture("refuselink");
        let target = base.join("target");
        let link = base.join("link");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("keep.txt"), b"KEEP").unwrap();
        crate::link::create::create_dir_link(&link, &target, crate::link::LinkType::Junction)
            .unwrap();

        // 对链接本身递归删除必须被拒绝
        let err = delete_tree(&link, &mut noop).unwrap_err();
        assert!(matches!(err, SlError::SafetyBlocked(_)), "{err}");
        // 目标内容必须完好
        assert_eq!(fs::read(target.join("keep.txt")).unwrap(), b"KEEP");

        // 但作为"树内的链接子项"是可以删链接条目的
        let removed = delete_tree(&base, &mut noop).unwrap();
        assert_eq!(removed, 1); // keep.txt
        assert!(!link.exists(), "链接条目应被删除");
        assert!(!base.exists());
    }

    #[test]
    fn merge_keeps_newer_and_skips_existing() {
        let base = fixture("merge");
        let src = base.join("src");
        let dst = base.join("dst");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();

        // 源里三件东西：目标没有的、目标有但更旧的、目标有且更新的
        fs::write(src.join("only_src.txt"), b"from-src").unwrap();
        fs::write(src.join("stale.txt"), b"src-newer-content").unwrap();
        fs::write(dst.join("stale.txt"), b"old").unwrap();
        fs::write(src.join("fresh.txt"), b"src-older").unwrap();
        fs::write(dst.join("fresh.txt"), b"dst-newer").unwrap();
        fs::write(dst.join("keep.txt"), b"target-only").unwrap();

        // 把 dst 的两个文件时间分别调成"更旧"和"更新"
        let now = filetime::FileTime::now();
        filetime::set_file_mtime(dst.join("stale.txt"), filetime::FileTime::from_unix_time(1_000_000, 0)).unwrap();
        filetime::set_file_mtime(src.join("stale.txt"), filetime::FileTime::from_unix_time(2_000_000, 0)).unwrap();
        filetime::set_file_mtime(src.join("fresh.txt"), filetime::FileTime::from_unix_time(1_000_000, 0)).unwrap();
        filetime::set_file_mtime(dst.join("fresh.txt"), filetime::FileTime::from_unix_time(2_000_000, 0)).unwrap();
        let _ = now;

        let rep = merge_into(&src, &dst, &mut noop).unwrap();
        assert_eq!(rep.copied, 1, "只有 only_src.txt 是新复制");
        assert_eq!(rep.overwritten, 1, "stale.txt 应被覆盖");
        assert_eq!(rep.skipped, 1, "fresh.txt 应被跳过");

        assert_eq!(fs::read(dst.join("only_src.txt")).unwrap(), b"from-src");
        assert_eq!(fs::read(dst.join("stale.txt")).unwrap(), b"src-newer-content");
        assert_eq!(fs::read(dst.join("fresh.txt")).unwrap(), b"dst-newer");
        assert_eq!(fs::read(dst.join("keep.txt")).unwrap(), b"target-only");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn merge_requires_existing_target() {
        let base = fixture("mergemissing");
        let src = base.join("src");
        fs::create_dir_all(&src).unwrap();
        let err = merge_into(&src, &base.join("nope"), &mut noop).unwrap_err();
        assert!(matches!(err, SlError::InvalidPath(_)), "{err}");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn rejects_same_path() {        let base = fixture("samepath");
        let err = move_tree(&base, &base, VerifyMode::CountAndSize, &mut noop).unwrap_err();
        assert!(matches!(err, SlError::SafetyBlocked(_)), "{err}");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn rejects_non_empty_target() {
        let base = fixture("nonempty");
        let src = base.join("src");
        let dst = base.join("dst");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        fs::write(dst.join("occupied"), b"x").unwrap();
        let err = move_tree(&src, &dst, VerifyMode::CountAndSize, &mut noop).unwrap_err();
        assert!(matches!(err, SlError::SafetyBlocked(_)), "{err}");
        let _ = fs::remove_dir_all(&base);
    }

    /// 用不允许任何共享的句柄锁住文件，模拟"被别的程序打开"。
    fn lock_no_share(path: &Path) -> windows_sys::Win32::Foundation::HANDLE {
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::Storage::FileSystem::{CreateFileW, OPEN_EXISTING};
        let wide = util::to_wide(&util::to_verbatim(path).to_string_lossy());
        let h = unsafe {
            CreateFileW(
                wide.as_ptr(),
                0x8000_0000, // GENERIC_READ
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(h, INVALID_HANDLE_VALUE, "锁文件失败");
        h
    }

    fn unlock(h: windows_sys::Win32::Foundation::HANDLE) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(h);
        }
    }

    #[test]
    fn best_effort_skips_locked_file_and_reports_leftovers() {
        let base = fixture("best_effort");
        let src = base.join("src");
        let dst = base.join("dst");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("free.txt"), b"free").unwrap();
        fs::write(src.join("busy.txt"), b"busy").unwrap();

        let h = lock_no_share(&src.join("busy.txt"));

        let out = copy_tree_ex(&src, &dst, true, &mut noop).unwrap();
        assert_eq!(out.files, 1);
        assert_eq!(out.skipped.len(), 1, "被锁文件应被跳过：{:?}", out.skipped);
        assert!(dst.join("free.txt").exists());
        assert!(!dst.join("busy.txt").exists());

        // 严格模式不许带病复制
        assert!(copy_tree(&src, &base.join("dst2"), &mut noop).is_err());

        // 删源：删得掉的就删，删不掉的留在原处
        let del = delete_tree_ex(&src, &mut noop).unwrap();
        assert_eq!(del.removed, 1);
        assert!(!del.failures.is_empty());
        assert!(src.join("busy.txt").exists());
        assert!(!src.join("free.txt").exists());

        unlock(h);
        let _ = fs::remove_dir_all(&base);
    }
}
