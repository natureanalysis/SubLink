//! 只读规则库的自愈：合并「管理员属主」的残留 WAL。
//!
//! ## 问题
//!
//! 规则库在 `%PROGRAMDATA%` 下，程序既可能以管理员身份运行（建链接、写系统目录），
//! 也可能以普通用户身份运行（登录任务的 `LeastPrivilege`、直接双击快捷方式）。
//! 管理员实例被**强制结束**时，SQLite 会把 `-wal`/`-shm` 留在盘上；这两个文件的
//! 属主是 `Administrators`、普通用户只有读权限，于是下次非提权启动写不进 WAL，
//! 直接报 `attempt to write a readonly database`，界面被迫退化成内存库。
//!
//! ## 做法
//!
//! 1. 把 db + wal + shm 拷进临时目录（读权限总是有的），打开一次：SQLite 回放 WAL，
//!    关闭时自动检查点并删掉副本的 WAL —— 已提交的事务一条不丢；
//! 2. `PRAGMA integrity_check` 通过才继续；
//! 3. 原文件先备份进 `backup-repair-<时间戳>/`，把旧 WAL 挪开（否则合并后的主库
//!    会让 SQLite 把同一批事务再回放一遍），最后才把合并结果拷回主库。
//!
//! 任何一步失败都不改原文件（挪 WAL 在覆盖主库之前，且主库写权限事先确认过），
//! 最坏情况是保持原状、继续走「降级到内存库 + 顶部横幅报错」的老路。

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::store::Store;

/// 临时副本的文件名。真实库可能还叫更名前的 `tempdirector.db`，所以副本名固定在
/// 这里：拷进临时目录后与真实文件名无关。
const COPY_DB_NAME: &str = "sublink.db";
const COPY_WAL_NAME: &str = "sublink.db-wal";
const COPY_SHM_NAME: &str = "sublink.db-shm";

/// 把残留 WAL 合并回主库，让普通用户下次能正常打开。成功返回 `Ok(())`。
pub(crate) fn repair_readonly_db(db: &Path, data_dir: &Path) -> Result<(), String> {
    let db_name = db
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| "规则库路径无效".to_string())?
        .to_string();
    let wal_name = format!("{db_name}-wal");
    let shm_name = format!("{db_name}-shm");
    let wal = data_dir.join(&wal_name);
    let shm = data_dir.join(&shm_name);
    if !db.is_file() || !wal.is_file() {
        return Err("没有残留的 -wal，不属于可自愈的只读情形".into());
    }
    // 主库自己得能写，否则合并完也写不回去；这一步同时挡住"整个目录只读"的情况。
    std::fs::OpenOptions::new()
        .write(true)
        .open(db)
        .map_err(|e| format!("主库不可写：{e}"))?;

    let tmp = std::env::temp_dir().join(format!("sublink-repair-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| format!("建临时目录失败：{e}"))?;
    let copy_db = tmp.join(COPY_DB_NAME);
    std::fs::copy(db, &copy_db).map_err(|e| format!("拷贝主库失败：{e}"))?;
    for (src, name) in [(&wal, COPY_WAL_NAME), (&shm, COPY_SHM_NAME)] {
        if !src.is_file() {
            continue;
        }
        let dst = tmp.join(name);
        std::fs::copy(src, &dst).map_err(|e| format!("拷贝 {name} 失败：{e}"))?;
        // 只读属性会随文件一起被拷过来，不清掉副本照样打不开。
        #[allow(clippy::permissions_set_readonly_false)]
        if let Ok(meta) = std::fs::metadata(&dst) {
            let mut perm = meta.permissions();
            perm.set_readonly(false);
            let _ = std::fs::set_permissions(&dst, perm);
        }
    }

    // 打开副本 = 回放 WAL；离开作用域关闭时 SQLite 自动检查点并删掉副本的 WAL。
    {
        let store = Store::open(&copy_db).map_err(|e| format!("合并 WAL 失败：{e}"))?;
        let check: String = store
            .conn()
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .map_err(|e| format!("完整性检查执行失败：{e}"))?;
        if check != "ok" {
            return Err(format!("合并后完整性检查失败：{check}"));
        }
    }
    if tmp.join(COPY_WAL_NAME).is_file() {
        return Err("合并后 WAL 仍未清空，放弃替换".into());
    }

    let backup = data_dir.join(format!("backup-repair-{}", unix_secs()));
    std::fs::create_dir_all(&backup).map_err(|e| format!("建备份目录失败：{e}"))?;
    std::fs::copy(db, backup.join(&db_name)).map_err(|e| format!("备份主库失败：{e}"))?;
    std::fs::rename(&wal, backup.join(&wal_name)).map_err(|e| format!("挪开旧 WAL 失败：{e}"))?;
    if shm.is_file() {
        let _ = std::fs::rename(&shm, backup.join(&shm_name));
    }
    if let Err(e) = std::fs::copy(&copy_db, db) {
        // 写回失败：把 WAL 放回原位，保持"打开会报只读"的原状，至少不丢数据。
        let _ = std::fs::rename(backup.join(&wal_name), &wal);
        let _ = std::fs::remove_dir_all(&backup);
        return Err(format!("写回主库失败：{e}"));
    }
    let _ = std::fs::remove_dir_all(&tmp);
    Ok(())
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_base(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("td-repair-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn merges_stale_readonly_wal_and_keeps_data() {
        let base = temp_base("merge");
        let db = base.join(COPY_DB_NAME);
        {
            let s = Store::open(&db).unwrap();
            s.conn().execute("CREATE TABLE t(x INTEGER)", []).unwrap();
            s.conn().execute("INSERT INTO t VALUES (42)", []).unwrap();
        }
        // 造一个"只有读权限"的残留 WAL —— 和普通用户面对管理员属主文件时一样。
        let wal = base.join(COPY_WAL_NAME);
        std::fs::write(&wal, b"").unwrap();
        let mut perm = std::fs::metadata(&wal).unwrap().permissions();
        perm.set_readonly(true);
        std::fs::set_permissions(&wal, perm).unwrap();
        assert!(Store::open(&db).is_err(), "只读 WAL 应当让打开失败");

        repair_readonly_db(&db, &base).expect("自愈应成功");

        {
            let s = Store::open(&db).unwrap();
            let v: i64 = s
                .conn()
                .query_row("SELECT x FROM t", [], |r| r.get(0))
                .unwrap();
            assert_eq!(v, 42);
        }
        let _ = std::fs::remove_dir_all(&base);
    }
}
