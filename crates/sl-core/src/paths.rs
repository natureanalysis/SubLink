//! 应用路径解析。
//!
//! 数据库放在 `%PROGRAMDATA%\SubLink\` 而不是 `%APPDATA%`：
//! 后台服务以 SYSTEM 身份运行，读不到（也不该读到）某个用户的 `%APPDATA%`。
//! 多用户场景下规则是共享的，路径里的 `%APPDATA%` 这类变量在**执行时按当前用户展开**。
//!
//! ## 更名兼容
//!
//! 本项目原名 TempDirector。首次以 SubLink 运行时，若旧目录
//! `%PROGRAMDATA%\TempDirector` 存在而新目录不存在，就把旧目录**整体改名**迁过来
//! （连同库文件改名），避免老用户的规则凭空消失。迁移失败（旧版仍在运行、
//! 目录被占用）时继续使用旧目录 —— 绝不复制出半套数据。

use std::path::{Path, PathBuf};

use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::UI::Shell::SHGetKnownFolderPath;
use windows_sys::core::GUID;

/// 数据目录名（`%PROGRAMDATA%\<APP_DIR_NAME>`）。
pub const APP_DIR_NAME: &str = "SubLink";

/// 更名前的数据目录名：首次运行会把它的内容整体迁到 [`APP_DIR_NAME`]。
const LEGACY_APP_DIR_NAME: &str = "TempDirector";

/// 显式覆盖数据目录（便携模式 / 自动化测试 / 沙箱环境）。
pub const DATA_DIR_ENV: &str = "SUBLINK_DATA_DIR";

/// 更名前的环境变量：仍然读取，已部署的便携脚本无需改一行即可升级。
const LEGACY_DATA_DIR_ENV: &str = "TEMPDIRECTOR_DATA_DIR";

/// 取不到 ProgramData 时，当前目录下的兜底数据目录名。
const FALLBACK_DIR_NAME: &str = ".sldata";

/// `FOLDERID_ProgramData` = {62AB5D82-FDC1-4DC3-A9DD-070D1D495D97}
const FOLDERID_PROGRAM_DATA: GUID = GUID {
    data1: 0x62AB_5D82,
    data2: 0xFDC1,
    data3: 0x4DC3,
    data4: [0xA9, 0xDD, 0x07, 0x0D, 0x1D, 0x49, 0x5D, 0x97],
};

/// 通过 `SHGetKnownFolderPath` 解析系统已知文件夹。
fn known_folder(fid: &GUID) -> Option<PathBuf> {
    unsafe {
        let mut p: *mut u16 = std::ptr::null_mut();
        let hr = SHGetKnownFolderPath(fid, 0, std::ptr::null_mut(), &mut p);
        if hr < 0 || p.is_null() {
            return None;
        }
        let mut len = 0usize;
        while *p.add(len) != 0 {
            len += 1;
        }
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, len));
        CoTaskMemFree(p as *const core::ffi::c_void);
        Some(PathBuf::from(s))
    }
}

/// 数据根目录。
///
/// 优先级：`SUBLINK_DATA_DIR`（兼容旧的 `TEMPDIRECTOR_DATA_DIR`）→ `%PROGRAMDATA%`
/// 环境变量 → `SHGetKnownFolderPath` → 当前目录下的 `.sldata`（最后兜底，保证任何环境
/// 下都能起来）。
///
/// 把环境变量放在 API 之前是有意的：这样便携模式、沙箱、CI 都能通过环境变量重定向数据根。
pub fn data_dir() -> PathBuf {
    if let Some(dir) = data_dir_env_override() {
        return dir;
    }
    if let Some(base) = program_data_dir() {
        return app_dir_under(&base);
    }
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(FALLBACK_DIR_NAME)
}

/// 环境变量指定的数据根（新变量优先，退回更名前的旧变量）。
///
/// 登录任务写进 `--data-dir` 的也必须是这个值，否则便携模式会出现
/// "注册时用 A 库、登录时跑 B 库"。
pub fn data_dir_env_override() -> Option<PathBuf> {
    for key in [DATA_DIR_ENV, LEGACY_DATA_DIR_ENV] {
        if let Ok(v) = std::env::var(key) {
            if !v.trim().is_empty() {
                return Some(PathBuf::from(v));
            }
        }
    }
    None
}

fn program_data_dir() -> Option<PathBuf> {
    if let Ok(v) = std::env::var("PROGRAMDATA") {
        if !v.trim().is_empty() {
            return Some(PathBuf::from(v));
        }
    }
    known_folder(&FOLDERID_PROGRAM_DATA)
}

/// 新版目录优先；新版还不存在而旧版存在时，把旧目录整体改名迁移过来。
/// 迁移失败时返回旧目录，保证程序照常可用。
fn app_dir_under(base: &Path) -> PathBuf {
    let new_dir = base.join(APP_DIR_NAME);
    let old_dir = base.join(LEGACY_APP_DIR_NAME);
    if new_dir.is_dir() || !old_dir.is_dir() {
        return new_dir;
    }
    match std::fs::rename(&old_dir, &new_dir) {
        Ok(()) => {
            // 目录已经迁到新位置；库文件改名失败也不影响打开 —— `db_path` 会认旧文件名。
            if let Err(e) = rename_db_files(&new_dir) {
                eprintln!("库文件改名未完成（将按旧文件名打开）：{e}");
            }
            new_dir
        }
        Err(e) => {
            eprintln!(
                "数据目录迁移未完成，继续使用 {}：{e}",
                old_dir.display()
            );
            old_dir
        }
    }
}

/// 目录整体改名后，把库文件也从旧名换成新名（含 `-wal` / `-shm`）。
fn rename_db_files(dir: &Path) -> std::io::Result<()> {
    for suffix in ["", "-wal", "-shm"] {
        let old = dir.join(format!("tempdirector.db{suffix}"));
        let new = dir.join(format!("sublink.db{suffix}"));
        if old.is_file() && !new.exists() {
            std::fs::rename(&old, &new)?;
        }
    }
    Ok(())
}

pub fn db_path() -> PathBuf {
    db_file_in(&data_dir())
}

/// 新名优先；目录里只有更名前的 `tempdirector.db` 时继续用它
/// （数据目录整体迁移未完成时的兜底，避免新建一个空库让规则"消失"）。
fn db_file_in(dir: &Path) -> PathBuf {
    let new = dir.join("sublink.db");
    if new.exists() {
        return new;
    }
    let old = dir.join("tempdirector.db");
    if old.is_file() {
        return old;
    }
    new
}

/// 提权任务投放目录。提权 worker 只接受这个目录里的 job 文件，
/// 且只认里面的 rule_id（防"提权后变成任意路径删除器"）。
pub fn jobs_dir() -> PathBuf {
    data_dir().join("jobs")
}

pub fn log_dir() -> PathBuf {
    data_dir().join("logs")
}

/// 确保数据目录存在。
pub fn ensure_dirs() -> std::io::Result<()> {
    std::fs::create_dir_all(data_dir())?;
    std::fs::create_dir_all(jobs_dir())?;
    std::fs::create_dir_all(log_dir())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn scratch(tag: &str) -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("sublink-paths-{tag}-{n}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn legacy_data_dir_is_migrated_with_db_files() {
        let base = scratch("migrate");
        let old = base.join(LEGACY_APP_DIR_NAME);
        std::fs::create_dir_all(old.join("logs")).unwrap();
        for suffix in ["", "-wal", "-shm"] {
            std::fs::write(old.join(format!("tempdirector.db{suffix}")), b"x").unwrap();
        }
        std::fs::write(old.join("presets.json"), b"{}").unwrap();

        let got = app_dir_under(&base);
        let new = base.join(APP_DIR_NAME);
        assert_eq!(got, new, "旧目录存在、新目录不存在时应迁到新目录");
        assert!(new.join("presets.json").is_file(), "目录内容应整体带过来");
        for suffix in ["", "-wal", "-shm"] {
            assert!(
                new.join(format!("sublink.db{suffix}")).is_file(),
                "库文件应改成新名"
            );
            assert!(!new.join(format!("tempdirector.db{suffix}")).exists());
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn existing_new_dir_wins_over_legacy() {
        let base = scratch("both");
        std::fs::create_dir_all(base.join(APP_DIR_NAME)).unwrap();
        std::fs::create_dir_all(base.join(LEGACY_APP_DIR_NAME)).unwrap();

        assert_eq!(app_dir_under(&base), base.join(APP_DIR_NAME));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn fresh_install_uses_new_dir() {
        let base = scratch("fresh");
        assert_eq!(app_dir_under(&base), base.join(APP_DIR_NAME));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn db_file_falls_back_to_legacy_name() {
        let base = scratch("dbname");
        // 空目录：用新名（首次启动会新建）
        assert_eq!(db_file_in(&base), base.join("sublink.db"));

        // 迁移未完成、只有旧名库：必须继续打开旧库，否则会新建空库让规则消失
        std::fs::write(base.join("tempdirector.db"), b"x").unwrap();
        assert_eq!(db_file_in(&base), base.join("tempdirector.db"));

        // 新名存在则新名优先
        std::fs::write(base.join("sublink.db"), b"x").unwrap();
        assert_eq!(db_file_in(&base), base.join("sublink.db"));
        let _ = std::fs::remove_dir_all(&base);
    }
}
