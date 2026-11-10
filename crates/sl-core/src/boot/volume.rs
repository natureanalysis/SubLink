//! 卷就绪判定。
//!
//! RAM 盘的可用时机有两种形态：
//! * **内核驱动的盘**（ImDisk / Radeon RAMDisk 的驱动模式）：boot 早期卷就在了；
//! * **桌面程序创建的盘**：要等用户登录、那个程序起来之后卷才出现。
//!
//! 所以这里不能"探测一次就放弃"，必须带超时轮询；超时后要**告警**而不是静默失败 ——
//! 否则用户第二天开机遇到的是一堆报错的软件，却不知道是守护没跑成。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::error::{SlError, SlResult};
use crate::fsops::mover::free_space;

/// 卷是否就绪：目录存在 + 能查到容量 + 能真的写进去。
///
/// 只判断 `exists()` 是不够的：盘符被别的设备临时占用、卷处于只读挂载、
/// 或者 RAM 盘刚创建还没格式化，都会让"目录存在"但不写入。
pub fn is_volume_ready(path: &Path) -> bool {
    if !path.is_dir() {
        return false;
    }
    if free_space(path).is_err() {
        return false;
    }
    let probe = path.join(format!(".td_probe_{}", std::process::id()));
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// 卷是否"可用"：挂载了、是目录、能查到容量。**不做写探针**。
///
/// 为什么不复用 `is_volume_ready`：那个带写探针，用在**卷根**上会被权限拒绝
/// （普通用户在 `C:\` 下建文件会被拒），于是"等 C 盘就绪"会永远等不到。
/// 写能力的判定交给真正要写的那一刻（`ensure_root_dir`），错误信息也更准确。
pub fn is_volume_available(path: &Path) -> bool {
    path.is_dir() && free_space(path).is_ok()
}

/// 等待卷就绪。返回实际等待时长。
pub fn wait_for_volume(path: &Path, timeout: Duration, poll: Duration) -> SlResult<Duration> {
    let started = Instant::now();
    loop {
        if is_volume_ready(path) {
            return Ok(started.elapsed());
        }
        if started.elapsed() >= timeout {
            return Err(SlError::Other(crate::i18n::tf("volume.wait_timeout", &[
                ("secs", &timeout.as_secs().to_string()),
                ("path", &path.display().to_string()),
            ])));
        }
        std::thread::sleep(poll);
    }
}

/// 等"承载它的卷"挂载上来，**并把该目录本身建出来**。返回实际等待时长。
///
/// 为什么不能用 `wait_for_volume(path)` 替代：
/// 目标根允许是 RAM 盘下的**子目录**（例如 `V:\TempRedirect`）。
/// RAM 盘重启后连这个子目录一起消失了，如果只等它自己出现，那是永远等不到的
/// —— 整条开机重建会被静默跳过，正好毁掉这个产品最核心的承诺。
///
/// 正确顺序是：先等卷（卷根随 RAM 盘挂载必然存在）→ 再把根目录建出来 → 就算就绪。
pub fn wait_for_root(path: &Path, timeout: Duration, poll: Duration) -> SlResult<Duration> {
    let started = Instant::now();
    // 卷根。取不到（盘符都不存在）时退回等路径本身，让错误信息更直白。
    let anchor = crate::util::volume_root(path)
        .map(PathBuf::from)
        .unwrap_or_else(|| path.to_path_buf());

    wait_for_volume_available(&anchor, timeout, poll)?;
    ensure_root_dir(path)?;
    Ok(started.elapsed())
}

fn wait_for_volume_available(path: &Path, timeout: Duration, poll: Duration) -> SlResult<Duration> {
    let started = Instant::now();
    loop {
        if is_volume_available(path) {
            return Ok(started.elapsed());
        }
        if started.elapsed() >= timeout {
            return Err(SlError::Other(crate::i18n::tf("volume.wait_timeout", &[
                ("secs", &timeout.as_secs().to_string()),
                ("path", &path.display().to_string()),
            ])));
        }
        std::thread::sleep(poll);
    }
}

/// 确保目标根目录存在（RAM 盘下的一层子目录）。
pub fn ensure_root_dir(path: &Path) -> SlResult<()> {
    if path.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(path).map_err(|e| {
        SlError::Other(crate::i18n::tf("volume.create_failed", &[
            ("path", &path.display().to_string()),
            ("e", &e.to_string()),
        ]))
    })
}

/// 一次性的快速探测（不等待），用于 UI 上判断"RAM 盘在不在"。
pub fn probe_volume(path: &Path) -> (&'static str, String) {
    if !path.exists() {
        // 区分"盘都没有"和"盘在、只是根目录还没建出来（RAM 盘刚重启）"——
        // 后者是正常的、跑一次开机重建就好，不该让用户以为盘挂了。
        let volume_exists = crate::util::volume_root(path)
            .map(|v| std::path::Path::new(&v).exists())
            .unwrap_or(false);
        if volume_exists {
            return (
                "not_created",
                crate::i18n::tf("volume.not_created", &[("path", &path.display().to_string())]),
            );
        }
        return ("missing", crate::i18n::tf("volume.missing", &[("path", &path.display().to_string())]));
    }
    if !path.is_dir() {
        return ("not_dir", crate::i18n::tf("volume.not_dir", &[("path", &path.display().to_string())]));
    }
    match free_space(path) {
        Ok(free) => {
            let probe = path.join(format!(".td_probe_{}", std::process::id()));
            match std::fs::write(&probe, b"") {
                Ok(()) => {
                    let _ = std::fs::remove_file(&probe);
                    (
                        "ready",
                        crate::i18n::tf("volume.ready", &[("free", &crate::util::human_bytes(free))]),
                    )
                }
                Err(e) => ("readonly", crate::i18n::tf("volume.readonly", &[("e", &e.to_string())])),
            }
        }
        Err(e) => ("no_space_info", crate::i18n::tf("volume.no_space_info", &[("e", &e.to_string())])),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn lab(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("td_vol_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn temp_dir_is_ready() {
        let d = std::env::temp_dir();
        assert!(is_volume_ready(&d));
        let (state, _) = probe_volume(&d);
        assert_eq!(state, "ready");
    }

    #[test]
    fn missing_path_not_ready() {
        let d = std::env::temp_dir().join("td_volume_does_not_exist_xyz");
        assert!(!is_volume_ready(&d));
        let (state, _) = probe_volume(&d);
        // 卷在（temp 目录所在卷）、只是这个目录不存在 → not_created，而不是 missing
        assert_eq!(state, "not_created");
    }

    #[test]
    fn wait_times_out_with_clear_error() {
        // 用一个不存在的盘符，连卷都取不到
        let d = PathBuf::from(r"Z:\nope");
        let err = wait_for_volume(&d, Duration::from_millis(150), Duration::from_millis(50))
            .unwrap_err();
        assert!(err.to_string().contains("超时"), "{err}");
    }

    #[test]
    fn wait_returns_immediately_when_ready() {
        let d = std::env::temp_dir();
        let waited = wait_for_volume(&d, Duration::from_secs(5), Duration::from_millis(50)).unwrap();
        assert!(waited < Duration::from_secs(1));
    }

    #[test]
    fn wait_for_root_creates_missing_subdir() {
        // 模拟"RAM 盘重启后子目录没了"：卷在、目录不在
        let base = lab("rootcreate");
        let root = base.join("TempRedirect");
        assert!(!root.exists());

        let waited = wait_for_root(&root, Duration::from_secs(3), Duration::from_millis(50))
            .expect("卷在的话应该直接把根目录建出来，而不是等它自己出现");
        assert!(root.is_dir(), "根目录应被创建");
        assert!(waited < Duration::from_secs(2));

        // 建好之后探测就是 ready
        let (state, _) = probe_volume(&root);
        assert_eq!(state, "ready");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn volume_root_works_for_nonexistent_path() {
        // 这条是上面那条能成立的前提：GetVolumePathNameW 对不存在的路径会失败，
        // 必须逐级向上找到存在的祖先。否则"等卷"这一步根本无从谈起。
        let ghost = std::env::temp_dir()
            .join("td_ghost_xyz")
            .join("deep").join("deeper");
        assert!(!ghost.exists());
        let root = crate::util::volume_root(&ghost);
        assert!(root.is_some(), "不存在的路径也应该能求出卷根");
        let r = root.unwrap();
        assert!(std::path::Path::new(&r).is_dir(), "求出的卷根应真实存在：{r}");
    }

    #[test]
    fn wait_for_root_is_idempotent_on_existing_dir() {
        let base = lab("rootexists");
        let root = base.join("already");
        std::fs::create_dir_all(&root).unwrap();
        assert!(wait_for_root(&root, Duration::from_secs(2), Duration::from_millis(50)).is_ok());
        assert!(root.is_dir());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn wait_for_root_fails_clearly_when_volume_absent() {
        let root = PathBuf::from(r"Z:\nope\Redirect");
        let err = wait_for_root(&root, Duration::from_millis(150), Duration::from_millis(50))
            .unwrap_err();
        assert!(err.to_string().contains("超时"), "{err}");
    }
}
