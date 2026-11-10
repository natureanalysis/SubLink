//! 规则库（SQLite）。
//!
//! ## 为什么放 `%PROGRAMDATA%` 而不是 `%APPDATA%`
//!
//! 后台服务以 SYSTEM 身份跑，读不到某个用户的 `%APPDATA%`。规则是全局共享的，
//! 而路径里的 `%APPDATA%` 这类变量在**执行时按当前用户展开**，所以一份库够用。
//!
//! ## 并发
//!
//! GUI 与服务可能同时打开这个库，所以固定开 WAL + `busy_timeout`，
//! 并且**不持有长事务**：所有写操作都是短事务。

pub mod model;
pub mod ops;
mod repair;
pub mod rules;
pub mod undo;

pub use model::{
    category_label, EventRow, NewRule, Operation, Root, Rule, CATEGORIES, REDIRECT_MODE_LINK,
    REDIRECT_MODE_SETTING, ROOT_COLUMNS, RULE_COLUMNS,
};
pub use undo::{UndoOutcome, UndoState};

use std::path::Path;
use std::time::Duration;

use rusqlite::Connection;

use crate::error::SlResult;

/// 当前 schema 版本。加迁移时递增，并在 `migrate` 里补一段 `if current < N`。
pub const SCHEMA_VERSION: i32 = 7;

/// 视为"已结束"的操作流水阶段。其余阶段在启动时都算待修复项。
pub const OP_TERMINAL_PHASES: [&str; 3] = ["done", "failed", "rolled_back"];

/// settings 键：界面上被隐藏的规则 id（JSON 数组）。
///
/// 隐藏是**展示偏好**，不是规则属性：它不进 `rules` 表，所以不参与开机重建 /
/// 水位巡检 / 批量操作。但它**随配置导出导入**（见 `crate::portable`）：导出时
/// 逐条挂在规则上（`BundleRule::hidden`），导入时按来源路径重新绑定到本机规则 ——
/// 规则 id 跨机器没有意义，只有来源路径能对上。
pub const KEY_HIDDEN_RULES: &str = "hidden_rule_ids";

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> SlResult<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        Self::new(Connection::open(path)?)
    }

    /// 打开默认位置（`%PROGRAMDATA%\SubLink\sublink.db`）。
    ///
    /// 只读错误先尝试一次自愈：管理员实例被强杀会留下普通用户写不进的
    /// `-wal`/`-shm`，合并回主库后就能正常打开（见 [`repair`]）。
    pub fn open_default() -> SlResult<Self> {
        crate::paths::ensure_dirs()?;
        let db = crate::paths::db_path();
        match Self::open(&db) {
            Ok(s) => Ok(s),
            Err(e) if e.to_string().contains("readonly") => {
                match repair::repair_readonly_db(&db, &crate::paths::data_dir()) {
                    Ok(()) => Self::open(&db),
                    Err(reason) => {
                        eprintln!("只读规则库自愈未生效：{reason}");
                        Err(e)
                    }
                }
            }
            Err(e) => Err(e),
        }
    }

    /// 内存库，供单元测试使用。
    pub fn open_in_memory() -> SlResult<Self> {
        Self::new(Connection::open_in_memory()?)
    }

    fn new(conn: Connection) -> SlResult<Self> {
        // 内存库不支持 WAL，因此这三条都允许失败。
        let _ = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get::<_, String>(0));
        let _ = conn.busy_timeout(Duration::from_millis(5000));
        let _ = conn.execute("PRAGMA foreign_keys=ON", []);
        let _ = conn.execute("PRAGMA synchronous=NORMAL", []);

        let mut store = Store { conn };
        store.migrate()?;
        Ok(store)
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn user_version(&self) -> SlResult<i32> {
        Ok(self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))?)
    }

    /// 当前本地时间（`YYYY-MM-DD HH:MM:SS`）。用于给人看的"上次做了某事"。
    pub fn now(&self) -> SlResult<String> {
        Ok(self
            .conn
            .query_row("SELECT datetime('now', 'localtime')", [], |r| r.get(0))?)
    }

    fn migrate(&mut self) -> SlResult<()> {
        let current = self.user_version()?;
        if current < 1 {
            self.conn.execute_batch(include_str!("schema.sql"))?;
        } else {
            // 增量迁移只对「缺该列的老库」执行；新库走上面的建表（schema.sql 已含全部列）。
            if current < 2 {
                // v2：规则加清理排除模式（JSON 数组）。
                self.conn.execute_batch(
                    "ALTER TABLE rules ADD COLUMN excludes TEXT NOT NULL DEFAULT '[]';",
                )?;
            }
            if current < 3 {
                // v3：规则加水位预授权策略（'' / empty / older:<秒>）。
                self.conn.execute_batch(
                    "ALTER TABLE rules ADD COLUMN auto_clean_policy TEXT NOT NULL DEFAULT '';",
                )?;
            }
            if current < 4 {
                // v4：规则加文件夹级容量上限（字节，0 = 不启用）。
                // 老库默认 0 = 只有「卷水位 critical」一条触发路径，行为与 v3 一致。
                self.conn.execute_batch(
                    "ALTER TABLE rules ADD COLUMN auto_clean_limit_bytes INTEGER NOT NULL DEFAULT 0;",
                )?;
            }
            if current < 5 {
                // v5：规则加重定向方式（'link' / 'setting'）。
                // 老库默认 'link' + 空 env_vars + 'user' 作用域 = 行为与 v4 完全一致。
                self.conn.execute_batch(
                    "ALTER TABLE rules ADD COLUMN redirect_mode TEXT NOT NULL DEFAULT 'link';
                     ALTER TABLE rules ADD COLUMN env_vars TEXT NOT NULL DEFAULT '[]';
                     ALTER TABLE rules ADD COLUMN env_scope TEXT NOT NULL DEFAULT 'user';",
                )?;
            }
            if current < 6 {
                // v6：回收站表（删除规则改为软删除，可撤销）。
                // 用 IF NOT EXISTS：新建库走 schema.sql 时表已存在，这里再执行也安全。
                self.conn.execute_batch(
                    "CREATE TABLE IF NOT EXISTS rule_trash (
                         id         INTEGER PRIMARY KEY AUTOINCREMENT,
                         rule_id    INTEGER NOT NULL,
                         batch_no   INTEGER NOT NULL,
                         name       TEXT    NOT NULL,
                         payload    TEXT    NOT NULL,
                         deleted_at TEXT    NOT NULL DEFAULT (datetime('now'))
                     );
                     CREATE INDEX IF NOT EXISTS idx_trash_batch ON rule_trash(batch_no);",
                )?;
            }
            if current < 7 {
                // v7：撤销栈合并成一张表（删除与修改共用一条栈）。
                // 老库（v6）的回收站内容按 'delete' 搬进 rule_undo，序号沿用原批次号，
                // 搬完把 rule_trash 删掉 —— 撤销栈只有一张表，免得两边各记一半。
                self.conn.execute_batch(
                    "CREATE TABLE IF NOT EXISTS rule_undo (
                         id         INTEGER PRIMARY KEY AUTOINCREMENT,
                         seq        INTEGER NOT NULL,
                         op         TEXT    NOT NULL,
                         rule_id    INTEGER NOT NULL,
                         name       TEXT    NOT NULL,
                         payload    TEXT    NOT NULL,
                         created_at TEXT    NOT NULL DEFAULT (datetime('now'))
                     );
                     CREATE INDEX IF NOT EXISTS idx_undo_seq ON rule_undo(seq);
                     INSERT INTO rule_undo(seq, op, rule_id, name, payload, created_at)
                         SELECT batch_no, 'delete', rule_id, name, payload, deleted_at FROM rule_trash;
                     DROP TABLE IF EXISTS rule_trash;",
                )?;
            }
        }
        self.conn
            .pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(())
    }

    // ---------------- settings ----------------

    pub fn get_setting(&self, key: &str) -> SlResult<Option<String>> {
        let mut stmt = self.conn.prepare("SELECT v FROM settings WHERE k = ?1")?;
        let mut rows = stmt.query([key])?;
        match rows.next()? {
            Some(row) => Ok(Some(row.get(0)?)),
            None => Ok(None),
        }
    }

    pub fn set_setting(&self, key: &str, value: &str) -> SlResult<()> {
        self.conn.execute(
            "INSERT INTO settings(k, v) VALUES(?1, ?2)
             ON CONFLICT(k) DO UPDATE SET v = excluded.v",
            rusqlite::params![key, value],
        )?;
        Ok(())
    }

    // ---------------- 隐藏态（存在 settings，不是规则属性） ----------------

    /// 当前被隐藏的规则 id（升序）。手工改库写坏 JSON 时按"没有隐藏"处理。
    ///
    /// 已删除规则的残留 id 会被顺手剔除（只读不写；落库由 [`Store::set_rules_hidden`]
    /// 负责），否则残留 id 会一直躺在 settings 里，将来新建出同号规则会被莫名隐藏。
    pub fn hidden_rule_ids(&self) -> SlResult<Vec<i64>> {
        let ids = self
            .get_setting(KEY_HIDDEN_RULES)?
            .and_then(|s| serde_json::from_str::<Vec<i64>>(&s).ok())
            .unwrap_or_default();
        let mut set: std::collections::BTreeSet<i64> = ids.into_iter().collect();
        let alive: std::collections::HashSet<i64> =
            self.list_rules()?.iter().map(|r| r.id).collect();
        set.retain(|id| alive.contains(id));
        Ok(set.into_iter().collect())
    }

    /// 整批设置隐藏态（`hidden=true` 隐藏 / `false` 恢复显示），返回写入后的隐藏总数。
    ///
    /// 为什么整批重写而不是逐条改：状态就是一个 JSON 数组，重写最省事、也最不容易
    /// 出现"同一 id 出现两次"这类脏数据。不存在的 id 静默忽略（可能刚被别处删掉）。
    pub fn set_rules_hidden(&self, ids: &[i64], hidden: bool) -> SlResult<usize> {
        let alive: std::collections::HashSet<i64> =
            self.list_rules()?.iter().map(|r| r.id).collect();
        let mut set: std::collections::BTreeSet<i64> =
            self.hidden_rule_ids()?.into_iter().collect();
        for id in ids {
            if !alive.contains(id) {
                continue;
            }
            if hidden {
                set.insert(*id);
            } else {
                set.remove(id);
            }
        }
        let list: Vec<i64> = set.into_iter().collect();
        self.set_setting(
            KEY_HIDDEN_RULES,
            &serde_json::to_string(&list).unwrap_or_else(|_| "[]".into()),
        )?;
        Ok(list.len())
    }

    pub fn all_settings(&self) -> SlResult<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare("SELECT k, v FROM settings ORDER BY k")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_migrates_to_current_version() {
        let mut store = Store::open_in_memory().unwrap();
        assert_eq!(store.user_version().unwrap(), SCHEMA_VERSION);
        // 再跑一次不应报错（幂等）
        store.migrate().unwrap();
        assert_eq!(store.user_version().unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn tables_exist() {
        let store = Store::open_in_memory().unwrap();
        for t in ["roots", "rules", "operations", "events", "settings"] {
            let n: i64 = store
                .conn()
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [t],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "缺少表 {t}");
        }
    }

    #[test]
    fn settings_roundtrip() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.get_setting("k").unwrap().is_none());
        store.set_setting("k", "v1").unwrap();
        assert_eq!(store.get_setting("k").unwrap().as_deref(), Some("v1"));
        store.set_setting("k", "v2").unwrap();
        assert_eq!(store.get_setting("k").unwrap().as_deref(), Some("v2"));
    }
}
