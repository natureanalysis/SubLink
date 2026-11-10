//! 撤销栈：删除与修改都能撤销。
//!
//! ## 一条栈，不分两种表
//!
//! 界面上的「撤销」按钮只有一个，所以栈也只能有一条：**一次用户操作 = 一个 `seq`**，
//! 撤销时整体弹掉最近那一个 `seq`。删除与修改的区别只落在 `op` 列与载荷上：
//!
//! * `delete`：规则整行被搬走了（回收站语义）。载荷是整行快照 + 事件行 + 隐藏态。
//! * `update`：规则还在，只是字段被改了。载荷是「字段级前后值」。
//!
//! ## 为什么删除是"搬走"而不是"打个标记"
//!
//! 规则会被开机重建、水位巡检、定时清理读到 —— 任何一处漏掉"跳过已删除规则"的
//! 判断，就等于删掉的规则开机又被建回来。因此这里**真的把行从 `rules` 表拿走**，
//! 整行快照搬进撤销栈；所有既有查询（`list_rules` / `rules_for_boot` / …）
//! 不用改一行，就自动看不见已删除的规则。
//!
//! ## 快照里存了什么
//!
//! * 规则整行（含 `last_state` / `applied_at` 等状态缓存）—— 删除要回到"删除那一刻"的样子；
//! * 该规则的事件日志行 —— 规则删掉时 `events` 外键是 `ON DELETE CASCADE`，
//!   不快照就永久丢了运维历史；
//! * 该规则的操作流水 id —— `operations` 外键是 `ON DELETE SET NULL`，
//!   行还在但关联断了，快照下来才能在撤销时接回去；
//! * 隐藏态 —— 隐藏名单在 `settings` 里按 id 存，规则被删时会被顺手剔除，
//!   不记下来撤销后这条规则就变成可见了。
//!
//! ## 为什么修改用「字段级前后值」而不是整行快照
//!
//! 整行快照会把状态缓存（`last_state` / `applied_at` / `last_bytes` …）一起回滚 ——
//! 那是**磁盘现状**的缓存，回滚它等于把上次检测的结果谎报成"改之前的样子"。
//! 字段级快照只记用户改过的**配置字段**，回退时也只动这些字段。
//!
//! ## 回退的判定：只回退"还没被再改过"的字段
//!
//! 用户改了 A 规则 → 又改了 A 规则的另一个字段 → 撤销（弹掉的是最后一次修改）。
//! 若先撤销的是较早那次修改，直接写回旧值会把较新的修改一并抹掉。
//! 因此**逐字段比对**：当前值仍等于"改后值"才回退，否则跳过并在结果里说明原因。
//!
//! ## 撤销不碰磁盘（除了把链接指回旧目标）
//!
//! 删除本来只删规则记录、不动目录与链接，所以撤销同样只是搬记录。
//! 唯一的例外是「目标被改回去了」：正向修改会把托管链接重建到新目标，
//! 撤销后要把链接**指回**旧目标（`engine::retarget_rule_link`），
//! 否则记录说旧目标、链接却指着新目标 —— 那是"配置已改、链接还指着别处"的中间态。
//! 这一步在用例层做（引擎在存储层之上），失败只提示、不回滚已恢复的记录。

use rusqlite::{params, params_from_iter, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{SlError, SlResult};
use crate::i18n::tf;
use crate::store::model::{rule_from_row, NewRule, Rule, RULE_COLUMNS};
use crate::store::Store;
use crate::util;

/// 撤销操作类型：删除（整行搬走）。
pub const OP_DELETE: &str = "delete";
/// 撤销操作类型：修改（字段被改）。
pub const OP_UPDATE: &str = "update";

/// 可撤销的**配置字段**白名单。
///
/// 不含 `id` / `created_at` / `updated_at`（身份与时间戳不是用户改的），
/// 也不含 `last_state` / `last_checked_at` / `last_error` / `last_files` /
/// `last_bytes` / `applied_at`（那是磁盘现状的缓存，回滚它等于造假），
/// 更不含 `acl_sddl` / `needs_elevation`（前者是单独采集的，后者由源路径推导）。
pub const CONFIG_FIELDS: &[&str] = &[
    "name",
    "category",
    "source_template",
    "target_root_id",
    "target_subpath",
    "link_type",
    "redirect_mode",
    "env_vars",
    "env_scope",
    "want_enabled",
    "rebuild_on_boot",
    "notes",
    "excludes",
    "auto_clean_policy",
    "auto_clean_limit_bytes",
    "sort_order",
];

/// 目标相关字段：改了这几个，磁盘上的链接可能还指着旧目标。
pub const TARGET_FIELDS: &[&str] = &["target_root_id", "target_subpath", "link_type"];

/// 被删除规则的事件日志行（去掉 `rule_id`，恢复时按规则 id 回填）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrashedEvent {
    pub id: i64,
    pub level: String,
    pub kind: String,
    pub message: String,
    pub at: String,
}

/// 一条被删除规则的完整快照。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrashedRule {
    /// 规则整行。`id` 一并保留：撤销后规则身份不变（事件 / 隐藏态都还认它）。
    pub rule: Rule,
    /// 该规则的事件日志（随规则一起被外键级联删掉，所以要自己留一份）。
    pub events: Vec<TrashedEvent>,
    /// 该规则的操作流水 id（行还在，但 `rule_id` 被置空了）。
    pub op_ids: Vec<i64>,
    /// 删除前是否处于隐藏态。
    pub hidden: bool,
}

/// 一个字段的「改前 / 改后」值。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldChange {
    /// 字段名（见 [`CONFIG_FIELDS`]）。
    pub field: String,
    /// 改之前的值（JSON 编码：文本=字符串、开关=布尔、数值=整数、数组=字符串数组）。
    pub before: Value,
    /// 改之后的值。
    pub after: Value,
}

/// 撤销载荷：按 `op` 解释。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UndoPayload {
    /// 删除：整行快照。
    Delete { snapshot: TrashedRule },
    /// 修改：字段级前后值。
    Update { changes: Vec<FieldChange> },
}

/// 撤销栈里的一行（一个 `seq` 可能有多行：批量操作每条规则一行）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UndoEntry {
    pub id: i64,
    pub seq: i64,
    pub op: String,
    pub rule_id: i64,
    pub name: String,
    pub payload: UndoPayload,
    pub created_at: String,
}

/// 一次撤销的结果。
#[derive(Debug, Clone, Default)]
pub struct UndoOutcome {
    /// 成功撤销/恢复的规则名。
    pub restored: Vec<String>,
    /// 没能撤销的条目及原因（一行一条）。
    pub skipped: Vec<String>,
    /// 本次撤销的操作类型（`delete` / `update`）；栈为空时为 `None`。
    pub op: Option<String>,
    /// 需要把链接指回旧目标的规则 id（目标被改回去了，链接还指着新目标）。
    pub need_relink: Vec<i64>,
}

impl UndoOutcome {
    /// 本次撤销是否什么都没做（栈本来就是空的）。
    pub fn is_empty(&self) -> bool {
        self.restored.is_empty() && self.skipped.is_empty()
    }
}

/// 撤销栈现状（给界面决定「撤销」按钮要不要亮、亮起来会撤什么）。
#[derive(Debug, Clone, Default)]
pub struct UndoState {
    /// 栈里待撤销的规则条数（一次操作可能涉及多条）。
    pub pending: usize,
    /// 栈顶操作类型（`delete` / `update`）。
    pub op: Option<String>,
    /// 栈顶操作涉及的规则名。
    pub names: Vec<String>,
}

// ---------------------------------------------------------------------------
// 字段值的 JSON 编解码
// ---------------------------------------------------------------------------

fn str_array(items: &[String]) -> Value {
    Value::Array(items.iter().map(|s| Value::String(s.clone())).collect())
}

fn opt_str(v: &Option<String>) -> Value {
    match v {
        Some(s) => Value::String(s.clone()),
        None => Value::Null,
    }
}

/// 取规则某个配置字段的当前值（`None` = 该字段不在白名单里）。
fn field_value(rule: &Rule, field: &str) -> Option<Value> {
    Some(match field {
        "name" => Value::String(rule.name.clone()),
        "category" => Value::String(rule.category.clone()),
        "source_template" => Value::String(rule.source_template.clone()),
        "target_root_id" => Value::from(rule.target_root_id),
        "target_subpath" => Value::String(rule.target_subpath.clone()),
        "link_type" => Value::String(rule.link_type.clone()),
        "redirect_mode" => Value::String(rule.redirect_mode.clone()),
        "env_vars" => str_array(&rule.env_vars),
        "env_scope" => Value::String(rule.env_scope.clone()),
        "want_enabled" => Value::Bool(rule.want_enabled),
        "rebuild_on_boot" => Value::Bool(rule.rebuild_on_boot),
        "notes" => opt_str(&rule.notes),
        "excludes" => str_array(&rule.excludes),
        "auto_clean_policy" => Value::String(rule.auto_clean_policy.clone()),
        "auto_clean_limit_bytes" => Value::from(rule.auto_clean_limit_bytes),
        "sort_order" => Value::from(rule.sort_order),
        _ => return None,
    })
}

/// 字段名 → 数据库列名。**只认白名单**，返回的常量直接拼进 SQL，不接受外部输入。
fn column_of(field: &str) -> Option<&'static str> {
    Some(match field {
        "name" => "name",
        "category" => "category",
        "source_template" => "source_template",
        "target_root_id" => "target_root_id",
        "target_subpath" => "target_subpath",
        "link_type" => "link_type",
        "redirect_mode" => "redirect_mode",
        "env_vars" => "env_vars",
        "env_scope" => "env_scope",
        "want_enabled" => "want_enabled",
        "rebuild_on_boot" => "rebuild_on_boot",
        "notes" => "notes",
        "excludes" => "excludes",
        "auto_clean_policy" => "auto_clean_policy",
        "auto_clean_limit_bytes" => "auto_clean_limit_bytes",
        "sort_order" => "sort_order",
        _ => return None,
    })
}

/// JSON 值 → SQLite 值（按字段类型解释）。
fn to_sql_value(field: &str, v: &Value) -> rusqlite::types::Value {
    match field {
        "want_enabled" | "rebuild_on_boot" => rusqlite::types::Value::Integer(
            i64::from(v.as_bool().unwrap_or(false)),
        ),
        "target_root_id" | "auto_clean_limit_bytes" | "sort_order" => {
            rusqlite::types::Value::Integer(v.as_i64().unwrap_or(0))
        }
        "env_vars" | "excludes" => rusqlite::types::Value::Text(
            serde_json::to_string(v).unwrap_or_else(|_| "[]".into()),
        ),
        "notes" => match v.as_str() {
            Some(s) => rusqlite::types::Value::Text(s.to_string()),
            None => rusqlite::types::Value::Null,
        },
        _ => rusqlite::types::Value::Text(v.as_str().unwrap_or_default().to_string()),
    }
}

/// 比较旧行与新表单，算出**配置字段**级别的差异。
///
/// 只记真正变了的字段：GUI 的行内编辑提交的是**完整表单**，没动过的字段
/// 若也记进去，撤销栈会被无关字段灌满。
pub(crate) fn diff_new_rule(old: &Rule, nr: &NewRule) -> Vec<FieldChange> {
    let mut changes: Vec<FieldChange> = Vec::new();
    let mut push = |field: &str, before: Value, after: Value| {
        if before != after {
            changes.push(FieldChange {
                field: field.to_string(),
                before,
                after,
            });
        }
    };
    push("name", old.name.clone().into(), nr.name.clone().into());
    push("category", old.category.clone().into(), nr.category.clone().into());
    push(
        "source_template",
        old.source_template.clone().into(),
        nr.source_template.clone().into(),
    );
    push(
        "target_root_id",
        old.target_root_id.into(),
        nr.target_root_id.into(),
    );
    push(
        "target_subpath",
        old.target_subpath.clone().into(),
        nr.target_subpath.clone().into(),
    );
    push("link_type", old.link_type.clone().into(), nr.link_type.clone().into());
    push(
        "redirect_mode",
        old.redirect_mode.clone().into(),
        nr.redirect_mode.clone().into(),
    );
    push("env_vars", str_array(&old.env_vars), str_array(&nr.env_vars));
    push("env_scope", old.env_scope.clone().into(), nr.env_scope.clone().into());
    push(
        "want_enabled",
        old.want_enabled.into(),
        nr.want_enabled.into(),
    );
    push(
        "rebuild_on_boot",
        old.rebuild_on_boot.into(),
        nr.rebuild_on_boot.into(),
    );
    push("notes", opt_str(&old.notes), opt_str(&nr.notes));
    push("excludes", str_array(&old.excludes), str_array(&nr.excludes));
    push(
        "auto_clean_policy",
        old.auto_clean_policy.clone().into(),
        nr.auto_clean_policy.clone().into(),
    );
    push(
        "auto_clean_limit_bytes",
        old.auto_clean_limit_bytes.into(),
        nr.auto_clean_limit_bytes.into(),
    );
    changes
}

/// 单个字段的一次改动（批量归类 / 排序这类操作直接用）。
pub(crate) fn field_change(field: &str, before: Value, after: Value) -> Option<FieldChange> {
    if before == after {
        None
    } else {
        Some(FieldChange {
            field: field.to_string(),
            before,
            after,
        })
    }
}

/// 一次字段回退的结果。
#[derive(Debug, Clone, Default)]
pub struct RevertResult {
    /// 实际回退的字段名。
    pub reverted: Vec<String>,
    /// 没能回退的字段及原因（一行一条）。
    pub skipped: Vec<String>,
    /// 目标相关字段被改回去了 → 链接还指着新目标，需要重建。
    pub need_relink: bool,
}

impl Store {
    /// 把一批规则移入回收站（软删除）。返回（实际移入条数，批次号）。
    ///
    /// 同一批共用一个批次号 —— 界面上一次「删除 N 条」就是一个批次，
    /// 「撤销」把这一批整体搬回来，而不是只搬一条。
    pub fn trash_rules(&self, ids: &[i64]) -> SlResult<(usize, i64)> {
        if ids.is_empty() {
            return Ok((0, 0));
        }
        let conn = self.conn();
        let tx = conn.unchecked_transaction()?;
        let seq = next_undo_seq(&tx)?;
        let hidden: std::collections::HashSet<i64> =
            self.hidden_rule_ids()?.into_iter().collect();
        let mut moved = 0usize;
        for id in ids {
            let sql = format!("SELECT {RULE_COLUMNS} FROM rules WHERE id = ?1");
            let rule = tx
                .query_row(&sql, [id], rule_from_row)
                .optional()?
                .ok_or_else(|| {
                    SlError::NotFound(tf(
                        "db.rule_not_found",
                        &[("id", &id.to_string())],
                    ))
                })?;

            let events = {
                let mut stmt = tx.prepare(
                    "SELECT id, level, kind, message, at FROM events
                     WHERE rule_id = ?1 ORDER BY id",
                )?;
                let rows = stmt.query_map([id], |r| {
                    Ok(TrashedEvent {
                        id: r.get(0)?,
                        level: r.get(1)?,
                        kind: r.get(2)?,
                        message: r.get(3)?,
                        at: r.get(4)?,
                    })
                })?;
                let mut out = Vec::new();
                for r in rows {
                    out.push(r?);
                }
                out
            };

            let op_ids = {
                let mut stmt = tx.prepare("SELECT id FROM operations WHERE rule_id = ?1 ORDER BY id")?;
                let rows = stmt.query_map([id], |r| r.get::<_, i64>(0))?;
                let mut out = Vec::new();
                for r in rows {
                    out.push(r?);
                }
                out
            };

            let snapshot = TrashedRule {
                rule: rule.clone(),
                events,
                op_ids,
                hidden: hidden.contains(id),
            };
            push_undo(
                &tx,
                seq,
                OP_DELETE,
                *id,
                &rule.name,
                &UndoPayload::Delete { snapshot },
            )?;

            // 真正从 rules 表拿走：外键会把该规则的事件级联删掉（已快照在 payload 里）。
            let n = tx.execute("DELETE FROM rules WHERE id = ?1", [id])?;
            if n == 0 {
                return Err(SlError::NotFound(tf(
                    "db.rule_not_found",
                    &[("id", &id.to_string())],
                )));
            }
            moved += 1;
        }
        tx.commit()?;
        Ok((moved, seq))
    }

    /// 撤销栈现状：待撤销条数 + 栈顶操作类型 + 涉及的规则名。
    pub fn undo_state(&self) -> SlResult<UndoState> {
        let mut state = UndoState::default();
        let conn = self.conn();
        let seq: Option<i64> =
            conn.query_row("SELECT MAX(seq) FROM rule_undo", [], |r| r.get::<_, Option<i64>>(0))?;
        let Some(seq) = seq else {
            return Ok(state);
        };
        let mut stmt = conn.prepare(
            "SELECT op, name FROM rule_undo WHERE seq = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map([seq], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for r in rows {
            let (op, name) = r?;
            if state.op.is_none() {
                state.op = Some(op);
            }
            state.names.push(name);
        }
        state.pending = state.names.len();
        Ok(state)
    }

    /// 撤销最近一次操作（删除或修改）。
    ///
    /// 恢复不碰磁盘上的目录与链接 —— 删除本来就只删规则记录，
    /// 所以撤销同样只是把记录搬回来，不需要提权、不搬数据。
    /// （唯一的例外是目标被改回去时需要把链接指回旧目标，由用例层补这一步。）
    ///
    /// 单条失败（源路径已被新规则占用、目标根已删、字段已被再次修改）不影响同批其它条目：
    /// 失败项记进 `skipped` 说明原因，成功的照常恢复。
    pub fn undo_last(&self) -> SlResult<UndoOutcome> {
        let mut out = UndoOutcome::default();
        let conn = self.conn();
        let tx = conn.unchecked_transaction()?;

        // 栈为空时 MAX() 返回 NULL —— 用 Option 取，别让类型错误冒出来。
        let seq: Option<i64> = tx.query_row("SELECT MAX(seq) FROM rule_undo", [], |r| {
            r.get::<_, Option<i64>>(0)
        })?;
        let Some(seq) = seq else {
            return Ok(out);
        };

        let (entries, broken) = load_undo_entries(&tx, seq)?;
        if entries.is_empty() && broken.is_empty() {
            return Ok(out);
        }
        out.op = entries.first().map(|e| e.op.clone());
        out.skipped.extend(broken);

        for entry in entries {
            match &entry.payload {
                UndoPayload::Delete { snapshot } => {
                    undo_deleted_rule(&tx, snapshot, &mut out)?;
                }
                UndoPayload::Update { changes } => {
                    let res = revert_rule_fields(&tx, entry.rule_id, &entry.name, changes)?;
                    if !res.reverted.is_empty() {
                        out.restored.push(entry.name.clone());
                    }
                    out.skipped.extend(res.skipped);
                    if res.need_relink {
                        out.need_relink.push(entry.rule_id);
                    }
                }
            }
        }

        // 这一批已经处理完（成功或跳过都算处理完），从栈里弹掉，
        // 否则下一次撤销会永远卡在同一批上。
        tx.execute("DELETE FROM rule_undo WHERE seq = ?1", [seq])?;
        tx.commit()?;
        Ok(out)
    }
}

/// 分配一个新的操作序号（一次用户操作 = 一个序号）。
pub(crate) fn next_undo_seq(tx: &Transaction<'_>) -> SlResult<i64> {
    Ok(tx.query_row(
        "SELECT COALESCE(MAX(seq), 0) + 1 FROM rule_undo",
        [],
        |r| r.get(0),
    )?)
}

/// 往栈里写一条记录。
pub(crate) fn push_undo(
    tx: &Transaction<'_>,
    seq: i64,
    op: &str,
    rule_id: i64,
    name: &str,
    payload: &UndoPayload,
) -> SlResult<()> {
    let payload = serde_json::to_string(payload)?;
    tx.execute(
        "INSERT INTO rule_undo(seq, op, rule_id, name, payload) VALUES(?1, ?2, ?3, ?4, ?5)",
        params![seq, op, rule_id, name, payload],
    )?;
    Ok(())
}

/// 读出某个序号的全部撤销记录。
///
/// 载荷坏掉的行单独返回（不混进正常条目）—— 快照坏了不能整批回滚，
/// 那等于什么都撤不回来；跳这一条并把原因报给用户。
fn load_undo_entries(tx: &Transaction<'_>, seq: i64) -> SlResult<(Vec<UndoEntry>, Vec<String>)> {
    let mut stmt = tx.prepare(
        "SELECT id, seq, op, rule_id, name, payload, created_at
         FROM rule_undo WHERE seq = ?1 ORDER BY id",
    )?;
    let rows = stmt.query_map([seq], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, String>(5)?,
            r.get::<_, String>(6)?,
        ))
    })?;
    let mut out = Vec::new();
    let mut broken = Vec::new();
    for row in rows {
        let (id, seq, op, rule_id, name, payload, created_at) = row?;
        match serde_json::from_str::<UndoPayload>(&payload) {
            Ok(p) => out.push(UndoEntry {
                id,
                seq,
                op,
                rule_id,
                name,
                payload: p,
                created_at,
            }),
            Err(e) => broken.push(tf(
                "trash.snapshot_broken",
                &[("err", &e.to_string())],
            )),
        }
    }
    Ok((out, broken))
}

/// 把一条被删除的规则搬回 `rules` 表。
fn undo_deleted_rule(
    tx: &Transaction<'_>,
    snap: &TrashedRule,
    out: &mut UndoOutcome,
) -> SlResult<()> {
    let rule = snap.rule.clone();

    // 目标根还在吗？不在就没法恢复（外键也会挡）。
    let root_exists = tx
        .query_row(
            "SELECT 1 FROM roots WHERE id = ?1",
            [rule.target_root_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !root_exists {
        out.skipped.push(tf(
            "trash.root_gone",
            &[("name", &rule.name), ("id", &rule.id.to_string())],
        ));
        return Ok(());
    }

    // 源路径被新规则占用了？那是另一条规则，不能覆盖。
    let clash: Option<i64> = tx
        .query_row(
            "SELECT id FROM rules WHERE source_norm = ?1 AND id <> ?2",
            params![rule.source_norm, rule.id],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(other) = clash {
        out.skipped.push(tf(
            "trash.source_taken",
            &[("name", &rule.name), ("other", &other.to_string())],
        ));
        return Ok(());
    }

    insert_rule_snapshot(tx, &rule)?;

    // 事件日志：规则已回到表里，把行插回去。
    for ev in &snap.events {
        tx.execute(
            "INSERT INTO events(id, rule_id, level, kind, message, at)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
            params![ev.id, rule.id, ev.level, ev.kind, ev.message, ev.at],
        )?;
    }
    // 操作流水：行一直在，只是 `rule_id` 被置空了，这里接回去。
    for op_id in &snap.op_ids {
        tx.execute(
            "UPDATE operations SET rule_id = ?1 WHERE id = ?2",
            params![rule.id, op_id],
        )?;
    }
    // 隐藏态：删除时被剔出名单了，恢复后要放回去。
    // 这里直接合并名单而不是调 set_rules_hidden —— 那个函数会重写整份名单，
    // 而这里正在事务里，要保住同批其它条目已经恢复的隐藏态。
    if snap.hidden {
        let mut list: Vec<i64> = tx
            .query_row(
                "SELECT v FROM settings WHERE k = ?1",
                [crate::store::KEY_HIDDEN_RULES],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .and_then(|s| serde_json::from_str::<Vec<i64>>(&s).ok())
            .unwrap_or_default();
        if !list.contains(&rule.id) {
            list.push(rule.id);
            list.sort_unstable();
        }
        tx.execute(
            "INSERT INTO settings(k, v) VALUES(?1, ?2)
             ON CONFLICT(k) DO UPDATE SET v = excluded.v",
            params![
                crate::store::KEY_HIDDEN_RULES,
                serde_json::to_string(&list)?
            ],
        )?;
    }

    out.restored.push(rule.name);
    Ok(())
}

/// 把字段级前后值写回 `rules` 表。
///
/// 只回退"当前值仍等于改后值"的字段 —— 已经被再次改过的字段跳过并说明原因，
/// 免得一次撤销把用户后来的修改一并抹掉。
fn revert_rule_fields(
    tx: &Transaction<'_>,
    rule_id: i64,
    name: &str,
    changes: &[FieldChange],
) -> SlResult<RevertResult> {
    let mut res = RevertResult::default();
    let sql = format!("SELECT {RULE_COLUMNS} FROM rules WHERE id = ?1");
    let Some(current) = tx.query_row(&sql, [rule_id], rule_from_row).optional()? else {
        res.skipped.push(tf(
            "undo.rule_gone",
            &[("name", name), ("id", &rule_id.to_string())],
        ));
        return Ok(res);
    };

    let mut sets: Vec<String> = Vec::new();
    // ?1 永远是规则 id，后面的列从 ?2 起。
    let mut vals: Vec<rusqlite::types::Value> = vec![rusqlite::types::Value::Integer(rule_id)];
    let mut idx = 2usize;
    // 源路径变了要重算 `source_norm`（唯一键），单独处理。
    let mut new_source_norm: Option<String> = None;

    for change in changes {
        let Some(col) = column_of(&change.field) else {
            continue;
        };
        let Some(cur) = field_value(&current, &change.field) else {
            continue;
        };
        if cur != change.after {
            res.skipped.push(tf(
                "undo.field_changed",
                &[("name", name), ("field", &change.field)],
            ));
            continue;
        }

        // 目标根被删了 → 恢复成它会让外键失败，跳过这个字段。
        if change.field == "target_root_id" {
            let root_id = change.before.as_i64().unwrap_or(0);
            let exists = tx
                .query_row("SELECT 1 FROM roots WHERE id = ?1", [root_id], |_| Ok(()))
                .optional()?
                .is_some();
            if !exists {
                res.skipped.push(tf(
                    "undo.root_gone",
                    &[("name", name), ("id", &root_id.to_string())],
                ));
                continue;
            }
        }

        // 源路径变了：`source_norm` 是唯一键，重算后还得确认没被别的规则占着。
        if change.field == "source_template" {
            let template = change.before.as_str().unwrap_or_default().to_string();
            let (source, unknown) = util::expand_path(&template);
            if !unknown.is_empty() {
                res.skipped.push(tf(
                    "undo.source_unresolved",
                    &[("name", name), ("vars", &unknown.join("、"))],
                ));
                continue;
            }
            let norm = util::normalize_key(&source.to_string_lossy());
            let clash: Option<i64> = tx
                .query_row(
                    "SELECT id FROM rules WHERE source_norm = ?1 AND id <> ?2",
                    params![norm, rule_id],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(other) = clash {
                res.skipped.push(tf(
                    "undo.source_taken",
                    &[("name", name), ("other", &other.to_string())],
                ));
                continue;
            }
            new_source_norm = Some(norm);
        }

        sets.push(format!("{col}=?{idx}"));
        vals.push(to_sql_value(&change.field, &change.before));
        idx += 1;
        res.reverted.push(change.field.clone());
        if TARGET_FIELDS.contains(&change.field.as_str()) {
            res.need_relink = true;
        }
    }

    if sets.is_empty() {
        return Ok(res);
    }
    if let Some(norm) = new_source_norm {
        sets.push(format!("source_norm=?{idx}"));
        vals.push(rusqlite::types::Value::Text(norm));
    }
    let sql = format!(
        "UPDATE rules SET {}, updated_at=datetime('now') WHERE id=?1",
        sets.join(", ")
    );
    tx.execute(&sql, params_from_iter(vals.iter()))?;
    Ok(res)
}

/// 把快照里的规则整行写回 `rules` 表（带原 id 与全部状态缓存）。
fn insert_rule_snapshot(tx: &Transaction<'_>, r: &Rule) -> SlResult<()> {
    tx.execute(
        "INSERT INTO rules(id, name, category, source_template, source_norm, target_root_id,
                           target_subpath, link_type, redirect_mode, env_vars, env_scope,
                           want_enabled, rebuild_on_boot, acl_sddl, needs_elevation, notes,
                           sort_order, excludes, auto_clean_policy, auto_clean_limit_bytes,
                           last_state, last_checked_at, last_error, last_files, last_bytes,
                           applied_at, created_at, updated_at)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,
                 ?21,?22,?23,?24,?25,?26,?27,?28)",
        params![
            r.id,
            r.name,
            r.category,
            r.source_template,
            r.source_norm,
            r.target_root_id,
            r.target_subpath,
            r.link_type,
            r.redirect_mode,
            serde_json::to_string(&r.env_vars)?,
            r.env_scope,
            r.want_enabled,
            r.rebuild_on_boot,
            r.acl_sddl,
            r.needs_elevation,
            r.notes,
            r.sort_order,
            serde_json::to_string(&r.excludes)?,
            r.auto_clean_policy,
            r.auto_clean_limit_bytes,
            r.last_state,
            r.last_checked_at,
            r.last_error,
            r.last_files,
            r.last_bytes,
            r.applied_at,
            r.created_at,
            r.updated_at,
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_store() -> Store {
        let s = Store::open_in_memory().unwrap();
        s.insert_root("RAMDisk", "R:", true, 90000).unwrap();
        s
    }

    fn new_rule(name: &str, src: &str, sub: &str) -> NewRule {
        NewRule {
            name: name.into(),
            category: "browser".into(),
            source_template: src.into(),
            target_root_id: 1,
            target_subpath: sub.into(),
            link_type: "junction".into(),
            redirect_mode: "link".into(),
            env_vars: Vec::new(),
            env_scope: "user".into(),
            want_enabled: true,
            rebuild_on_boot: true,
            notes: None,
            excludes: Vec::new(),
            auto_clean_policy: String::new(),
            auto_clean_limit_bytes: 0,
        }
    }

    #[test]
    fn trashed_rule_leaves_the_active_set() {
        let s = mk_store();
        let id = s.insert_rule(&new_rule("Chrome", r"C:\Temp\Chrome", "c")).unwrap();
        s.set_rule_enabled(id, true).unwrap();

        let (moved, seq) = s.trash_rules(&[id]).unwrap();
        assert_eq!(moved, 1);
        assert_eq!(seq, 1);

        // 规则表里已经看不见它了 —— 开机重建、水位巡检也自然跳过。
        assert!(s.list_rules().unwrap().is_empty());
        assert!(s.rules_for_boot().unwrap().is_empty());
        assert!(s.get_rule(id).is_err());

        let st = s.undo_state().unwrap();
        assert_eq!(st.pending, 1);
        assert_eq!(st.op.as_deref(), Some(OP_DELETE));
        assert_eq!(st.names, vec!["Chrome".to_string()]);
    }

    #[test]
    fn undo_restores_rule_with_same_id_and_fields() {
        let s = mk_store();
        let id = s.insert_rule(&new_rule("Chrome", r"C:\Temp\Chrome", "c")).unwrap();
        s.log_event(Some(id), "info", "applied", "已应用").unwrap();
        let mut nr = new_rule("Chrome", r"C:\Temp\Chrome", "c");
        nr.notes = Some("备注".into());
        nr.excludes = vec!["logs".into()];
        nr.auto_clean_limit_bytes = 1024;
        s.update_rule(id, &nr).unwrap();

        s.trash_rules(&[id]).unwrap();
        let out = s.undo_last().unwrap();
        assert_eq!(out.restored, vec!["Chrome".to_string()]);
        assert!(out.skipped.is_empty());
        assert_eq!(out.op.as_deref(), Some(OP_DELETE));

        let back = s.get_rule(id).unwrap();
        assert_eq!(back.id, id, "撤销后必须还是同一个 id");
        assert_eq!(back.notes.as_deref(), Some("备注"));
        assert_eq!(back.excludes, vec!["logs".to_string()]);
        assert_eq!(back.auto_clean_limit_bytes, 1024);
        assert!(back.want_enabled);

        // 事件日志也回来了。
        let evs = s.recent_events(10).unwrap();
        assert!(evs.iter().any(|e| e.rule_id == Some(id)));

        // 删除那条撤销记录已经弹掉；更早那次「改备注」还留在栈里（后进先出）。
        let st = s.undo_state().unwrap();
        assert_eq!(st.op.as_deref(), Some(OP_UPDATE));
        assert_eq!(st.pending, 1);
    }

    #[test]
    fn undo_restores_hidden_state() {
        let s = mk_store();
        let id = s.insert_rule(&new_rule("A", r"C:\Temp\A", "a")).unwrap();
        s.set_rules_hidden(&[id], true).unwrap();
        s.trash_rules(&[id]).unwrap();
        assert!(s.hidden_rule_ids().unwrap().is_empty());

        s.undo_last().unwrap();
        assert_eq!(s.hidden_rule_ids().unwrap(), vec![id]);
    }

    #[test]
    fn undo_skips_rule_whose_source_was_taken() {
        let s = mk_store();
        let id = s.insert_rule(&new_rule("A", r"C:\Temp\A", "a")).unwrap();
        s.trash_rules(&[id]).unwrap();
        // 用户又建了同源的新规则。
        let other = s.insert_rule(&new_rule("A2", r"C:\Temp\A", "a2")).unwrap();

        let out = s.undo_last().unwrap();
        assert!(out.restored.is_empty());
        assert_eq!(out.skipped.len(), 1);
        // 新规则不能被覆盖。
        assert_eq!(s.get_rule(other).unwrap().name, "A2");
        assert_eq!(s.list_rules().unwrap().len(), 1);
    }

    #[test]
    fn undo_is_noop_when_stack_is_empty() {
        let s = mk_store();
        let out = s.undo_last().unwrap();
        assert!(out.is_empty());
        assert!(out.op.is_none());
    }

    #[test]
    fn batch_undo_takes_the_latest_batch_first() {
        let s = mk_store();
        let a = s.insert_rule(&new_rule("A", r"C:\Temp\A", "a")).unwrap();
        let b = s.insert_rule(&new_rule("B", r"C:\Temp\B", "b")).unwrap();
        s.trash_rules(&[a]).unwrap();
        s.trash_rules(&[b]).unwrap();

        let out = s.undo_last().unwrap();
        assert_eq!(out.restored, vec!["B".to_string()]);
        let out = s.undo_last().unwrap();
        assert_eq!(out.restored, vec!["A".to_string()]);
        assert_eq!(s.list_rules().unwrap().len(), 2);
    }

    // ---- 修改的撤销 ----

    #[test]
    fn undo_reverts_a_field_edit() {
        let s = mk_store();
        let id = s.insert_rule(&new_rule("Chrome", r"C:\Temp\Chrome", "c")).unwrap();
        let mut nr = new_rule("Edge", r"C:\Temp\Chrome", "c");
        nr.notes = Some("新备注".into());
        s.update_rule(id, &nr).unwrap();

        assert_eq!(s.get_rule(id).unwrap().name, "Edge");
        let st = s.undo_state().unwrap();
        assert_eq!(st.op.as_deref(), Some(OP_UPDATE));
        // 记的是**改完之后**的名字：用户此刻在列表里看到的就是它。
        assert_eq!(st.names, vec!["Edge".to_string()]);

        let out = s.undo_last().unwrap();
        assert_eq!(out.restored, vec!["Edge".to_string()]);
        assert!(out.skipped.is_empty());
        let back = s.get_rule(id).unwrap();
        assert_eq!(back.name, "Chrome");
        assert_eq!(back.notes, None);
    }

    #[test]
    fn undo_keeps_fields_that_were_changed_again() {
        let s = mk_store();
        let id = s.insert_rule(&new_rule("Chrome", r"C:\Temp\Chrome", "c")).unwrap();
        let nr = new_rule("Edge", r"C:\Temp\Chrome", "c");
        s.update_rule(id, &nr).unwrap();
        // 之后又改了另一个字段 —— 这一批是栈顶，先撤它。
        let mut nr2 = new_rule("Edge", r"C:\Temp\Chrome", "c");
        nr2.notes = Some("后来的备注".into());
        s.update_rule(id, &nr2).unwrap();

        // 撤掉第二次修改：备注回到没有，名字仍是 Edge。
        let out = s.undo_last().unwrap();
        assert_eq!(out.restored, vec!["Edge".to_string()]);
        let back = s.get_rule(id).unwrap();
        assert_eq!(back.notes, None);
        assert_eq!(back.name, "Edge");

        // 再撤一次：名字回到 Chrome。
        let out = s.undo_last().unwrap();
        assert_eq!(out.restored, vec!["Edge".to_string()]);
        assert_eq!(s.get_rule(id).unwrap().name, "Chrome");
    }

    #[test]
    fn undo_skips_a_field_that_was_changed_again() {
        let s = mk_store();
        let id = s.insert_rule(&new_rule("Chrome", r"C:\Temp\Chrome", "c")).unwrap();
        let nr = new_rule("Edge", r"C:\Temp\Chrome", "c");
        s.update_rule(id, &nr).unwrap();
        // 绕过撤销栈直接改回同一个字段 —— 撤销时不能把这次改动抹掉。
        s.conn()
            .execute("UPDATE rules SET name = ?1 WHERE id = ?2", params!["Opera", id])
            .unwrap();

        let out = s.undo_last().unwrap();
        assert!(out.restored.is_empty());
        assert_eq!(out.skipped.len(), 1);
        assert_eq!(s.get_rule(id).unwrap().name, "Opera");
    }

    #[test]
    fn undo_reverts_enable_toggle() {
        let s = mk_store();
        let id = s.insert_rule(&new_rule("Chrome", r"C:\Temp\Chrome", "c")).unwrap();
        s.set_rules_enabled(&[id], false).unwrap();
        assert!(!s.get_rule(id).unwrap().want_enabled);

        let out = s.undo_last().unwrap();
        assert_eq!(out.restored, vec!["Chrome".to_string()]);
        assert!(s.get_rule(id).unwrap().want_enabled);
    }

    #[test]
    fn undo_reverts_category_change() {
        let s = mk_store();
        let id = s.insert_rule(&new_rule("Chrome", r"C:\Temp\Chrome", "c")).unwrap();
        s.set_rules_category(&[id], "dev").unwrap();
        assert_eq!(s.get_rule(id).unwrap().category, "dev");

        let out = s.undo_last().unwrap();
        assert_eq!(out.restored, vec!["Chrome".to_string()]);
        assert_eq!(s.get_rule(id).unwrap().category, "browser");
    }

    #[test]
    fn undo_reverts_reorder() {
        let s = mk_store();
        let a = s.insert_rule(&new_rule("A", r"C:\Temp\A", "a")).unwrap();
        let b = s.insert_rule(&new_rule("B", r"C:\Temp\B", "b")).unwrap();
        let before_a = s.get_rule(a).unwrap().sort_order;
        let before_b = s.get_rule(b).unwrap().sort_order;

        s.set_rules_order(&[b, a]).unwrap();
        assert_eq!(s.get_rule(a).unwrap().sort_order, 2);

        let out = s.undo_last().unwrap();
        assert_eq!(out.restored.len(), 2);
        assert_eq!(s.get_rule(a).unwrap().sort_order, before_a);
        assert_eq!(s.get_rule(b).unwrap().sort_order, before_b);
    }

    #[test]
    fn undo_stack_keeps_delete_and_update_in_one_order() {
        let s = mk_store();
        let id = s.insert_rule(&new_rule("Chrome", r"C:\Temp\Chrome", "c")).unwrap();
        let nr = new_rule("Edge", r"C:\Temp\Chrome", "c");
        s.update_rule(id, &nr).unwrap();
        s.trash_rules(&[id]).unwrap();

        // 栈顶是删除。
        assert_eq!(s.undo_state().unwrap().op.as_deref(), Some(OP_DELETE));
        let out = s.undo_last().unwrap();
        assert_eq!(out.op.as_deref(), Some(OP_DELETE));
        assert_eq!(out.restored, vec!["Edge".to_string()]);
        assert_eq!(s.get_rule(id).unwrap().name, "Edge");

        // 栈顶变成那次修改。
        assert_eq!(s.undo_state().unwrap().op.as_deref(), Some(OP_UPDATE));
        let out = s.undo_last().unwrap();
        assert_eq!(out.op.as_deref(), Some(OP_UPDATE));
        assert_eq!(s.get_rule(id).unwrap().name, "Chrome");
    }

    #[test]
    fn undo_reverts_source_template_and_recomputes_norm() {
        let s = mk_store();
        let id = s.insert_rule(&new_rule("Chrome", r"C:\Temp\Chrome", "c")).unwrap();
        let nr = new_rule("Chrome", r"C:\Temp\Other", "c");
        s.update_rule(id, &nr).unwrap();
        assert_eq!(s.get_rule(id).unwrap().source_norm, r"c:\temp\other");

        s.undo_last().unwrap();
        let back = s.get_rule(id).unwrap();
        assert_eq!(back.source_template, r"C:\Temp\Chrome");
        assert_eq!(back.source_norm, r"c:\temp\chrome");
    }

    #[test]
    fn undo_marks_target_change_for_relink() {
        let s = mk_store();
        s.insert_root("Second", r"R:\Second", true, 90000).unwrap();
        let id = s.insert_rule(&new_rule("Chrome", r"C:\Temp\Chrome", "c")).unwrap();
        let mut nr = new_rule("Chrome", r"C:\Temp\Chrome", "c2");
        nr.target_root_id = 2;
        s.update_rule(id, &nr).unwrap();

        let out = s.undo_last().unwrap();
        assert_eq!(out.need_relink, vec![id]);
        assert_eq!(s.get_rule(id).unwrap().target_root_id, 1);
    }
}
