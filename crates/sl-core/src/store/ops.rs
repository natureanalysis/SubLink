//! 操作流水与事件日志。

use rusqlite::{params, OptionalExtension};

use crate::error::SlResult;
use crate::store::model::{EventRow, Operation};
use crate::store::{Store, OP_TERMINAL_PHASES};

const OP_COLUMNS: &str = "id, rule_id, kind, phase, source_path, target_path, \
     file_count, bytes, error, started_at, finished_at";

fn op_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Operation> {
    Ok(Operation {
        id: row.get(0)?,
        rule_id: row.get(1)?,
        kind: row.get(2)?,
        phase: row.get(3)?,
        source_path: row.get(4)?,
        target_path: row.get(5)?,
        file_count: row.get(6)?,
        bytes: row.get(7)?,
        error: row.get(8)?,
        started_at: row.get(9)?,
        finished_at: row.get(10)?,
    })
}

impl Store {
    /// 开始一次操作，返回流水 id。
    pub fn op_begin(
        &self,
        rule_id: Option<i64>,
        kind: &str,
        source: Option<&str>,
        target: Option<&str>,
    ) -> SlResult<i64> {
        self.conn().execute(
            "INSERT INTO operations(rule_id, kind, phase, source_path, target_path)
             VALUES(?1, ?2, 'planned', ?3, ?4)",
            params![rule_id, kind, source, target],
        )?;
        Ok(self.conn().last_insert_rowid())
    }

    /// 推进阶段。非终态阶段会一直留在库里，成为"上次中断待修复"的线索。
    pub fn op_phase(&self, id: i64, phase: &str, error: Option<&str>) -> SlResult<()> {
        self.conn().execute(
            "UPDATE operations SET phase=?2, error=COALESCE(?3, error) WHERE id=?1",
            params![id, phase, error],
        )?;
        Ok(())
    }

    pub fn op_finish(
        &self,
        id: i64,
        phase: &str,
        files: Option<i64>,
        bytes: Option<i64>,
        error: Option<&str>,
    ) -> SlResult<()> {
        self.conn().execute(
            "UPDATE operations SET phase=?2, file_count=COALESCE(?3, file_count),
                    bytes=COALESCE(?4, bytes), error=COALESCE(?5, error),
                    finished_at=datetime('now')
             WHERE id=?1",
            params![id, phase, files, bytes, error],
        )?;
        Ok(())
    }

    /// 累计某类操作（仅成功终态 `done`）的文件数与字节数 —— 「效益统计」的数据源。
    ///
    /// 为什么不从 `events` 统计：那里的 `message` 是给人看的文本
    /// （"清理「X」：N 个文件 / Y"），聚合不了；所以 clean 与 apply 一样落操作流水，
    /// 统计只认结构化列。
    pub fn sum_ops_done(&self, kind: &str) -> SlResult<(u64, u64)> {
        let (files, bytes): (i64, i64) = self.conn().query_row(
            "SELECT COALESCE(SUM(file_count), 0), COALESCE(SUM(bytes), 0)
             FROM operations WHERE kind = ?1 AND phase = 'done'",
            [kind],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok((files.max(0) as u64, bytes.max(0) as u64))
    }

    /// 非终态的操作 —— 即"上次运行崩在中途"的待修复清单。
    pub fn list_open_ops(&self) -> SlResult<Vec<Operation>> {
        let sql = format!(
            "SELECT {OP_COLUMNS} FROM operations
             WHERE phase NOT IN ('done', 'failed', 'rolled_back')
             ORDER BY id"
        );
        let mut stmt = self.conn().prepare(&sql)?;
        let rows = stmt.query_map([], op_from_row)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn recent_ops(&self, limit: i64) -> SlResult<Vec<Operation>> {
        let sql = format!("SELECT {OP_COLUMNS} FROM operations ORDER BY id DESC LIMIT ?1");
        let mut stmt = self.conn().prepare(&sql)?;
        let rows = stmt.query_map([limit], op_from_row)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// 启动时把上次崩溃留下的"进行中"标记为失败，避免它永远挂在待修复列表里。
    /// 返回被标记的条数。
    pub fn mark_stale_ops(&self) -> SlResult<usize> {
        let n = self.conn().execute(
            "UPDATE operations
                SET phase='failed',
                    error=COALESCE(error, '进程在操作完成前退出（可能是崩溃或强制结束）'),
                    finished_at=COALESCE(finished_at, datetime('now'))
              WHERE phase NOT IN ('done', 'failed', 'rolled_back')",
            [],
        )?;
        Ok(n)
    }

    pub fn log_event(
        &self,
        rule_id: Option<i64>,
        level: &str,
        kind: &str,
        message: &str,
    ) -> SlResult<()> {
        self.conn().execute(
            "INSERT INTO events(rule_id, level, kind, message) VALUES(?1, ?2, ?3, ?4)",
            params![rule_id, level, kind, message],
        )?;
        Ok(())
    }

    pub fn recent_events(&self, limit: i64) -> SlResult<Vec<EventRow>> {
        let mut stmt = self.conn().prepare(
            "SELECT id, rule_id, level, kind, message, at FROM events ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit], |r| {
            Ok(EventRow {
                id: r.get(0)?,
                rule_id: r.get(1)?,
                level: r.get(2)?,
                kind: r.get(3)?,
                message: r.get(4)?,
                at: r.get(5)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn count_events(&self) -> SlResult<i64> {
        Ok(self
            .conn()
            .query_row("SELECT count(*) FROM events", [], |r| r.get(0))
            .optional()?
            .unwrap_or(0))
    }

    /// 判断某个阶段是否终态。
    pub fn is_terminal_phase(phase: &str) -> bool {
        OP_TERMINAL_PHASES.contains(&phase)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn op_lifecycle() {
        let s = Store::open_in_memory().unwrap();
        let id = s
            .op_begin(None, "apply", Some("C:\\a"), Some("R:\\a"))
            .unwrap();
        assert_eq!(s.list_open_ops().unwrap().len(), 1);

        s.op_phase(id, "copying", None).unwrap();
        assert_eq!(s.list_open_ops().unwrap()[0].phase, "copying");

        s.op_finish(id, "done", Some(10), Some(1024), None).unwrap();
        assert!(s.list_open_ops().unwrap().is_empty());

        let recent = s.recent_ops(10).unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].file_count, Some(10));
        assert!(recent[0].finished_at.is_some());
    }

    #[test]
    fn stale_ops_are_marked_failed() {
        let s = Store::open_in_memory().unwrap();
        let live = s.op_begin(None, "apply", None, None).unwrap();
        let done = s.op_begin(None, "apply", None, None).unwrap();
        s.op_finish(done, "done", None, None, None).unwrap();
        assert_eq!(s.list_open_ops().unwrap().len(), 1);

        let n = s.mark_stale_ops().unwrap();
        assert_eq!(n, 1);
        assert!(s.list_open_ops().unwrap().is_empty());
        let ops = s.recent_ops(10).unwrap();
        let marked = ops.iter().find(|o| o.id == live).unwrap();
        assert_eq!(marked.phase, "failed");
        assert!(marked.error.is_some());
        assert!(Store::is_terminal_phase(&marked.phase));
    }

    #[test]
    fn events_are_recorded() {
        let s = Store::open_in_memory().unwrap();
        s.log_event(None, "info", "boot_ok", "重建完成").unwrap();
        s.log_event(None, "warn", "link_lost", "链接丢失").unwrap();
        let evs = s.recent_events(10).unwrap();
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].kind, "link_lost"); // 倒序
        assert_eq!(s.count_events().unwrap(), 2);
    }
}
