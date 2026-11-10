//! 系统设置模式：把 `%TEMP%` / `%TMP%` 这类**由环境变量决定**的目录，
//! 改成直接改注册表里的环境变量，而不是在源路径上挂 Junction。
//!
//! ## 与链接模式的差别
//!
//! | | 链接模式 | 系统设置模式 |
//! |---|---|---|
//! | 手段 | 源路径变成 Junction 指向目标 | 改注册表 `Environment` 里的变量值 |
//! | 源路径 | 变成重解析点 | 保持真实目录（内容被搬走后删掉） |
//! | 谁生效 | 只有打开该路径的程序 | 所有读环境变量的程序 |
//!
//! ## 关键设计：源目录从哪来
//!
//! 规则里的 `source_template` 在这个模式下**不表示"要改哪个变量"**，而是
//! 表示"这个变量当前指向的目录"。例如：
//!
//! ```text
//! source_template = %LOCALAPPDATA%\Temp
//! env_vars       = TEMP,TMP
//! ```
//!
//! 含义是：把 `TEMP` / `TMP` 指向的目录（也就是 `%LOCALAPPDATA%\Temp`）
//! 搬到 RAM 盘，然后让这两个变量指向 RAM 盘上的新位置。
//!
//! 这样做的好处是：**源目录仍然有唯一键**（`source_norm`），跟链接模式
//! 共用同一套去重、导出导入、搜索逻辑；而且"这个变量原本指向哪"是可以
//! 验证的 —— 读注册表里的实际值，跟规则登记的源路径对不上就报错，
//! 而不是闷头把某个目录搬走。
//!
//! ## 执行顺序（为什么是这个顺序）
//!
//! 1. **先搬数据，再改变量**。改早了的话，新进程已经开始往新目录写，
//!    而旧目录里的数据还没搬完 —— 中间状态不一致。
//! 2. **搬数据用现成的 `mover`**（复制 → 校验 → 才删源），铁律不变。
//! 3. **改完变量广播 `WM_SETTINGCHANGE`**，让 explorer 重新读环境块。
//! 4. **删源目录**：搬完并核对通过后，把旧目录删掉（用户选择的策略：
//!    迁移到新位置并校验后删除原目录）。
//!
//! ## 回滚
//!
//! 改变量这一步失败时，必须把变量改回原值 —— 否则系统可能指向一个
//! 不存在或半成品的位置。这里先记录每个变量的原值，改失败就逐个还原。

use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::Serialize;

use crate::error::{SlError, SlResult};
use crate::fsops::mover::{self, Progress};
use crate::fsops::occupancy::{self, LockScanOptions};
use crate::i18n::{t, tf};
use crate::link::create::ensure_dir;
use crate::link::detect::LinkState;
use crate::store::{Rule, Store};
use crate::sysenv::{self, EnvScope};
use crate::util;

use super::apply::{outcome, ApplyOptions, Outcome};
use super::perm;
use super::resolve::{dir_has_content, resolve, Resolved};

/// 一条规则在系统设置模式下的"当前变量指向"。
///
/// 记录每个变量的原值与现值，改写与回滚都要靠它。
#[derive(Debug, Clone)]
struct VarSnapshot {
    name: String,
    /// 改之前的值（`None` = 原本就没有这个变量）。
    /// 仅用于状态展示与排查；回滚写回的是下面的 `original_raw`。
    #[allow(dead_code)]
    original: Option<String>,
    /// 改之前的原始文本（未展开 `%VAR%`），回滚时原样写回。
    original_raw: Option<String>,
}

/// 读出这条规则涉及的所有变量的当前状态。
fn snapshot_vars(scope: EnvScope, names: &[String]) -> SlResult<Vec<VarSnapshot>> {
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        let cur = sysenv::read_env(scope, name)?;
        out.push(VarSnapshot {
            name: name.clone(),
            original: cur.as_ref().map(|v| v.expanded()),
            original_raw: cur.map(|v| v.raw),
        });
    }
    Ok(out)
}

/// settings 键：某条规则在 apply **之前**各变量的原值（JSON）。
///
/// ## 为什么必须持久化，而不能在 restore 时现场读
///
/// restore 时变量已经被本工具改成"指向目标"了 —— 现场读到的就是目标路径，
/// 拿它当"原值"写回去，等于什么都没还原（值还是目标）。
/// 真正的原值只在 apply 之前存在那一刻，必须在 apply 时存下来。
///
/// 键名：`env_prev:<rule_id>`。值形如：
/// `{"TEMP":{"raw":"%USERPROFILE%\\AppData\\Local\\Temp","existed":true}}`。
fn env_prev_key(rule_id: i64) -> String {
    format!("env_prev:{rule_id}")
}

/// apply 之前变量的原值（序列化后的 JSON 文本）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PrevVar {
    /// 原始文本（未展开 `%VAR%`）。还原时原样写回，类型也照旧。
    raw: String,
    /// 变量原本是否存在（`false` = 还原时应删掉这个变量，而不是写空串）。
    existed: bool,
}

/// 记录 apply 前的变量原值。
///
/// **只在第一次 apply 时记录**：重复 apply 时变量已经指向目标，
/// 这时再记一次就把"目标路径"当成原值存下来了 —— 还原时永远回不去。
fn save_env_prev(store: &Store, rule_id: i64, snaps: &[VarSnapshot]) -> SlResult<()> {
    if store.get_setting(&env_prev_key(rule_id))?.is_some() {
        return Ok(());
    }
    let mut map = serde_json::Map::new();
    for s in snaps {
        let prev = PrevVar {
            raw: s.original_raw.clone().unwrap_or_default(),
            existed: s.original_raw.is_some(),
        };
        map.insert(
            s.name.clone(),
            serde_json::to_value(prev).unwrap_or(serde_json::Value::Null),
        );
    }
    let json = serde_json::to_string(&map).unwrap_or_else(|_| "{}".into());
    store.set_setting(&env_prev_key(rule_id), &json)
}

/// 读回 apply 之前记录的变量原值。
fn load_env_prev(store: &Store, rule_id: i64) -> SlResult<Option<Vec<VarSnapshot>>> {
    let Some(json) = store.get_setting(&env_prev_key(rule_id))? else {
        return Ok(None);
    };
    let map: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&json).unwrap_or_default();
    let mut out = Vec::with_capacity(map.len());
    for (name, v) in map {
        let prev: PrevVar = serde_json::from_value(v).unwrap_or(PrevVar {
            raw: String::new(),
            existed: false,
        });
        out.push(VarSnapshot {
            name,
            original: if prev.existed {
                Some(prev.raw.clone())
            } else {
                None
            },
            original_raw: if prev.existed { Some(prev.raw) } else { None },
        });
    }
    Ok(Some(out))
}

/// 清除已记录的原值（restore 成功后调用，让下一次 apply 能重新记录）。
fn clear_env_prev(store: &Store, rule_id: i64) {
    let _ = store.set_setting(&env_prev_key(rule_id), "");
}

/// 把一组变量写回给定值（`None` = 删除该变量）。
///
/// 回滚用：逐个写回原值，某个失败也要继续把剩下的写完 —— 尽可能让系统回到一致状态。
fn restore_vars(scope: EnvScope, snaps: &[VarSnapshot]) -> Vec<String> {
    let mut errors = Vec::new();
    for s in snaps {
        let r = match &s.original_raw {
            Some(raw) => sysenv::write_env(scope, &s.name, raw),
            None => sysenv::delete_env(scope, &s.name).map(|_| ()),
        };
        if let Err(e) = r {
            errors.push(format!("{}: {}", s.name, e));
        }
    }
    errors
}

// ---------------------------------------------------------------------------
// 测试用的公开薄封装
//
// 上一版的缺陷是"restore 现场读注册表当原值"，那条回归护栏需要直接验证
// 「原值被持久化、读回逐字相同、重复写入不覆盖」。这些封装不碰注册表，
// 只读写规则库的 settings 表，因此可以在单元测试里安全运行。
// ---------------------------------------------------------------------------

/// 测试用的变量快照。
#[cfg(test)]
#[derive(Debug, Clone)]
pub struct VarSnapshotForTest {
    pub name: String,
    pub original_raw: Option<String>,
}

/// settings 键名（测试可见）。
#[cfg(test)]
pub fn env_prev_key_for_test(rule_id: i64) -> String {
    env_prev_key(rule_id)
}

/// 写入原值记录（测试可见）。
#[cfg(test)]
pub fn save_env_prev_for_test(
    store: &Store,
    rule_id: i64,
    snaps: &[VarSnapshotForTest],
) -> SlResult<()> {
    let inner: Vec<VarSnapshot> = snaps
        .iter()
        .map(|s| VarSnapshot {
            name: s.name.clone(),
            original: s.original_raw.clone(),
            original_raw: s.original_raw.clone(),
        })
        .collect();
    save_env_prev(store, rule_id, &inner)
}

/// 读回原值记录（测试可见）。
#[cfg(test)]
pub fn load_env_prev_for_test(
    store: &Store,
    rule_id: i64,
) -> SlResult<Option<Vec<VarSnapshotForTest>>> {
    Ok(load_env_prev(store, rule_id)?.map(|inner| {
        inner
            .into_iter()
            .map(|s| VarSnapshotForTest {
                name: s.name,
                original_raw: s.original_raw,
            })
            .collect()
    }))
}

/// 这条规则用到的作用域。
/// 把源目录**合并**进非空的目标目录（新者胜），合并成功后删源。
///
/// ## 为什么单独抽出来
///
/// 这条路径上有一处**铁律**：合并时若有条目读不出来（被别的程序占用），
/// 那些条目的副本根本不在目标里 —— 此时**绝不能删源**，否则就是丢数据。
///
/// 链接模式一直有这道检查（`engine/apply.rs`），系统设置模式最初漏了，
/// 结果是"合并失败被忽略 → 照样删源"。把这段抽成独立函数后：
/// * 铁律只写一遍，两条模式不会漂移；
/// * 它不碰注册表，可以被单元测试直接覆盖（`engine/setting_tests.rs`）。
pub fn merge_into_target(
    source: &Path,
    target: &Path,
    best_effort: bool,
    sink: &mut dyn FnMut(Progress),
) -> SlResult<crate::fsops::verify::TreeStats> {
    let rep = mover::merge_into_ex(source, target, best_effort, sink)?;
    if !rep.failed.is_empty() {
        // 读不出来的条目没有副本，绝不能删源。
        return Err(SlError::Busy(tf("move.best_effort_skipped", &[
            ("n", &rep.failed.len().to_string()),
            (
                "list",
                &rep.failed
                    .iter()
                    .take(3)
                    .map(|s| s.path.clone())
                    .collect::<Vec<_>>()
                    .join("；"),
            ),
        ])));
    }
    mover::delete_tree_ex(source, sink)?;
    Ok(crate::fsops::verify::TreeStats {
        files: rep.copied,
        bytes: rep.bytes,
        ..Default::default()
    })
}

/// 这条规则用到的作用域。
pub fn rule_scope(rule: &Rule) -> SlResult<EnvScope> {
    EnvScope::parse(&rule.env_scope).ok_or_else(|| {
        SlError::InvalidPath(tf("sysenv.bad_scope", &[("scope", &rule.env_scope)]))
    })
}

/// 这条规则要改的环境变量名（已规范化校验）。
pub fn rule_vars(rule: &Rule) -> SlResult<Vec<String>> {
    sysenv::normalize_vars(&rule.env_vars)
}

/// 「系统设置模式」的一种可行组合。
///
/// 前端用它决定要不要给出「系统设置」这个重定向方式：组合为空 = 这条
/// 规则的源路径用不了这个模式，下拉里就不该出现这个选项。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SettingOption {
    /// 写入作用域：`user` / `machine`。
    pub scope: String,
    /// 该作用域下正好指向这个目录的变量名（通常是 `TEMP` + `TMP`）。
    pub vars: Vec<String>,
}

/// 列出可以用「系统设置模式」落地的组合（空 = 这条路径用不了这个模式）。
///
/// ## 为什么必须反查注册表
///
/// setting 模式改的是**注册表里环境变量的值**，所以它只对
/// "某个白名单变量当前正好指向的目录"成立 —— 变量指向别处，
/// apply 会被 [`check_source_matches`] 以 `source_mismatch` 拒掉。
///
/// 因此判定不能靠"源路径长得像临时目录"，只能读注册表实际值反查：
/// 某个作用域的 `TEMP` / `TMP` 展开后等于源路径，就算一个可行组合。
/// 读不到（变量不存在，比如 HKLM 里没写 `TEMP`）就跳过那个组合。
///
/// ## 为什么不看源路径是不是链接
///
/// 源路径已经是 Junction（链接模式接管过）时，setting 模式确实做不了
/// —— 但那是**操作顺序**问题（先恢复成真实目录再切模式），不是
/// "这条路走不通"。这里只回答"变量指不指向这里"，剩下的交给
/// apply 时的安全检查报错，不在 UI 上替用户把路堵死。
pub fn setting_options_for_source(source: &Path) -> Vec<SettingOption> {
    let mut out: Vec<SettingOption> = Vec::new();
    for scope in [EnvScope::User, EnvScope::Machine] {
        let mut vars: Vec<String> = Vec::new();
        for name in sysenv::SUPPORTED_VARS {
            let Ok(Some(cur)) = sysenv::read_env_expanded(scope, name) else {
                continue;
            };
            if var_points_at(&cur, source) {
                vars.push(name.to_string());
            }
        }
        if !vars.is_empty() {
            out.push(SettingOption {
                scope: scope.as_str().to_string(),
                vars,
            });
        }
    }
    out
}

/// 纯比较：一个环境变量的当前值是不是指向 `source`。
///
/// 抽成独立函数是为了能单测 —— 判定逻辑很浅，但它一旦写错，
/// 界面就会给一条**根本落不了地**的规则开出「系统设置」选项，
/// 而这类错误在真机上表现为 apply 时的 `source_mismatch`，很难回溯。
pub fn var_points_at(current: &str, source: &Path) -> bool {
    let cur = current.trim();
    if cur.is_empty() {
        return false;
    }
    util::path_eq(&PathBuf::from(cur), source)
}

/// 规则登记的源目录，与变量当前实际指向的目录是否一致。
///
/// 不一致意味着"有人改过这个变量"或"规则登记错了路径" —— 两种情况都不能
/// 闷头搬数据，必须报出来让用户确认。
fn check_source_matches(scope: EnvScope, vars: &[String], source: &Path) -> SlResult<()> {
    for name in vars {
        let cur = sysenv::read_env_expanded(scope, name)?;
        let Some(cur) = cur else { continue };
        if cur.trim().is_empty() {
            continue;
        }
        let cur_path = PathBuf::from(cur.trim());
        if !util::path_eq(&cur_path, source) {
            return Err(SlError::SafetyBlocked(tf("sysenv.source_mismatch", &[
                ("var", name),
                ("current", &cur_path.display().to_string()),
                ("expected", &source.display().to_string()),
            ])));
        }
    }
    Ok(())
}

/// 检测系统设置模式下的规则状态。
///
/// 判定只看**变量指向哪儿**，不看源路径上有没有链接（这个模式压根不建链接）：
///
/// * 全部变量指向目标 → 已生效（再看目标有没有内容）；
/// * 变量还指向原位置 → 未应用（❌）；
/// * 变量指向别处 → 被人手工改过（❌，apply 时会拒绝并报出当前值）。
pub fn setting_state(scope: EnvScope, vars: &[String], target: &Path) -> LinkState {
    // 全部变量都指向目标 → 已生效；有一个不是 → 状态不匹配。
    let mut all_match = true;
    let mut first_current: Option<String> = None;
    for name in vars {
        match sysenv::read_env_expanded(scope, name) {
            Ok(Some(cur)) => {
                let trimmed = cur.trim().to_string();
                if first_current.is_none() {
                    first_current = Some(trimmed.clone());
                }
                if !util::path_eq(&PathBuf::from(&trimmed), target) {
                    all_match = false;
                }
            }
            Ok(None) => {
                all_match = false;
                if first_current.is_none() {
                    first_current = None;
                }
            }
            Err(_) => {
                all_match = false;
            }
        }
    }    if !all_match {
        return LinkState::SettingMismatch {
            current: first_current.filter(|s| !s.is_empty()),
        };
    }
    // 生效了：再看目标目录有没有内容（空 = RAM 盘刚重启，目录刚建出来）。
    if dir_has_content(target) {
        LinkState::SettingApplied {
            target: target.to_path_buf(),
            has_content: true,
        }
    } else {
        LinkState::SettingAppliedEmpty {
            target: target.to_path_buf(),
        }
    }
}

/// 应用一条系统设置模式的规则。
///
/// 流程见模块文档：搬数据 → 改变量 → 广播 → 删源目录。
pub fn apply_setting_rule(
    store: &Store,
    rule: &Rule,
    opts: &ApplyOptions,
    sink: &mut dyn FnMut(Progress),
) -> SlResult<Outcome> {
    let started = Instant::now();
    let r = resolve(store, rule)?;
    let scope = rule_scope(rule)?;
    let vars = rule_vars(rule)?;

    // 源目录与目标不能构成自环（同一个目录 / 互为祖先）。
    crate::safety::preflight_pair(&r.source, &r.target)?;

    // 变量当前指向必须与规则登记的源一致，否则不搬。
    check_source_matches(scope, &vars, &r.source)?;

    // 目标根可能是 RAM 盘下的子目录，RAM 盘重启后会消失 → 先建出来。
    let (root_path, _) = r.root.resolved_path();
    if !root_path.is_dir() {
        crate::boot::ensure_root_dir(&root_path)?;
    }

    let state = setting_state(scope, &vars, &r.target);
    match &state {
        LinkState::SettingApplied { .. } => {
            store.set_rule_state(rule.id, "setting_ok", None)?;
            return Ok(outcome(
                rule,
                "already_ok",
                t("sysenv.already_ok").into(),
                None,
                started,
                false,
            ));
        }
        LinkState::SettingAppliedEmpty { .. } => {
            store.set_rule_state(rule.id, "setting_ok_empty", None)?;
            return Ok(outcome(
                rule,
                "already_ok",
                t("sysenv.already_ok_empty").into(),
                None,
                started,
                false,
            ));
        }
        LinkState::SettingMismatch { current } => {
            // 变量没指向目标 —— 正是我们要修的状态，继续往下走。
            let _ = current;
        }
        _ => {
            return Err(SlError::Other(tf("sysenv.unexpected_state", &[])));
        }
    }

    // 占用检测：源目录此刻正被写入的话，搬出来的副本会撕裂。
    if !opts.skip_lock_check && !opts.best_effort && r.source.is_dir() {
        let report = occupancy::scan_locks(&r.source, &LockScanOptions::default())?;
        if !report.is_clear() {
            store.log_event(
                Some(rule.id),
                "warn",
                "blocked",
                &format!("{}：{}", rule.name, report.summary()),
            )?;
            return Err(SlError::Busy(tf("apply.source_busy", &[("summary", &report.summary())])));
        }
    }

    if opts.dry_run {
        return Ok(outcome(
            rule,
            "setting_dry_run",
            tf("sysenv.dry_run", &[("target", &r.target.display().to_string())]).into(),
            None,
            started,
            true,
        ));
    }

    let op = store.op_begin(
        Some(rule.id),
        "apply",
        Some(&r.source.display().to_string()),
        Some(&r.target.display().to_string()),
    )?;

    // 记录变量原值 —— 回滚要用，也要留给 restore 用。
    let snaps = snapshot_vars(scope, &vars)?;
    // 原值必须在**改之前**存进规则库：restore 时变量已指向目标，
    // 现场读不到真正的原值（那样还原等于没还原）。
    save_env_prev(store, rule.id, &snaps)?;

    // ---- 1. 搬数据 ----
    let source_has_content = r.source.is_dir() && dir_has_content(&r.source);
    let mut stats = None;
    if source_has_content {
        store.op_phase(op, "copying", None)?;
        ensure_dir(&r.target)?;
        perm::ensure_dir_access(store, Some(rule.id), &r.target);
        let mr = if dir_has_content(&r.target) {
            // 目标非空 → 合并（新者胜）
            merge_into_target(&r.source, &r.target, opts.best_effort, sink)?
        } else {
            let mr = mover::move_tree_ex(&r.source, &r.target, opts.verify, opts.best_effort, sink)?;
            mr.stats
        };
        stats = Some(mr);
    } else {
        ensure_dir(&r.target)?;
        perm::ensure_dir_access(store, Some(rule.id), &r.target);
    }

    // ---- 2. 改变量 ----
    store.op_phase(op, "linking", None)?;
    let mut written: Vec<VarSnapshot> = Vec::new();
    for s in &snaps {
        if let Err(e) = sysenv::set_env_to_path(scope, &s.name, &r.target) {
            // 回滚：把已经改过的变量改回原值。
            let errs = restore_vars(scope, &written);
            let _ = store.op_finish(op, "rolled_back", None, None, Some(&e.to_string()));
            let _ = store.set_rule_state(rule.id, "setting_rollback", Some(&e.to_string()));
            let _ = store.log_event(
                Some(rule.id),
                "error",
                "apply_failed",
                &tf("sysenv.rollback_log", &[
                    ("name", &rule.name),
                    ("e", &e.to_string()),
                    ("rb", &if errs.is_empty() { "ok".to_string() } else { errs.join("；") }),
                ]),
            );
            return Err(e);
        }
        written.push(s.clone());
    }

    // ---- 3. 广播环境变量变更 ----
    // 广播失败不阻断：注册表已经写对了，只是新进程可能晚一点才拿到新值。
    let broadcast_ok = sysenv::broadcast_environment_change().unwrap_or(false);
    if !broadcast_ok {
        store.log_event(
            Some(rule.id),
            "warn",
            "broadcast_failed",
            &tf("sysenv.broadcast_failed_log", &[("name", &rule.name)]),
        )?;
    }

    // ---- 4. 删源目录 ----
    // 搬运并核对通过后才删（用户选择的策略：迁移到新位置并校验后删除原目录）。
    // 删的是**普通真实目录**，不是重解析点 —— 但仍走 delete_tree，
    // 它对树内的重解析点只删链接条目本身，绝不跟随。
    let mut noop = |_: Progress| {};
    if r.source.is_dir() {
        let del = mover::delete_tree_ex(&r.source, &mut noop)?;
        if !del.failures.is_empty() {
            // 删不干净不算失败（目录里可能还有别的东西），但必须留痕。
            store.log_event(
                Some(rule.id),
                "warn",
                "source_leftovers",
                &tf("sysenv.leftovers_log", &[
                    ("name", &rule.name),
                    ("n", &del.failures.len().to_string()),
                ]),
            )?;
        }
    }

    let files = stats.as_ref().map(|s| s.files as i64).unwrap_or(0);
    let bytes = stats.as_ref().map(|s| s.bytes as i64).unwrap_or(0);
    store.op_finish(op, "done", Some(files), Some(bytes), None)?;
    store.set_rule_applied(rule.id, files, bytes)?;
    store.set_rule_state(rule.id, "setting_ok", None)?;
    store.log_event(
        Some(rule.id),
        "info",
        "applied",
        &tf("sysenv.applied_log", &[
            ("name", &rule.name),
            ("vars", &vars.join("/")),
            ("target", &r.target.display().to_string()),
        ]),
    )?;

    let mut o = outcome(
        rule,
        "setting_applied",
        tf("sysenv.applied", &[
            ("vars", &vars.join("/")),
            ("target", &r.target.display().to_string()),
        ]),
        stats,
        started,
        false,
    );
    o.links_recreated = 0;
    Ok(o)
}

/// 恢复一条系统设置模式的规则：变量改回原值，目录搬回去。
pub fn restore_setting_rule(
    store: &Store,
    rule: &Rule,
    opts: &ApplyOptions,
    sink: &mut dyn FnMut(Progress),
) -> SlResult<Outcome> {
    let started = Instant::now();
    let r = resolve(store, rule)?;
    let scope = rule_scope(rule)?;
    let vars = rule_vars(rule)?;
    crate::safety::preflight_pair(&r.source, &r.target)?;

    // 变量本来就没指向目标 → 无需恢复（幂等）。
    let points_to_target = vars.iter().all(|name| {
        matches!(sysenv::read_env_expanded(scope, name), Ok(Some(cur)) if util::path_eq(&PathBuf::from(cur.trim()), &r.target))
    });
    if !points_to_target {
        store.set_rule_state(rule.id, "setting_mismatch", None)?;
        return Ok(outcome(
            rule,
            "already_plain",
            t("sysenv.restore_noop").into(),
            None,
            started,
            false,
        ));
    }

    // 原值从规则库里读（apply 时存的）。读不到才退回现场快照 ——
    // 现场快照此时已是"指向目标"的值，只能当兜底，不能当准。
    let snaps = match load_env_prev(store, rule.id)? {
        Some(saved) if !saved.is_empty() => saved,
        _ => snapshot_vars(scope, &vars)?,
    };

    if opts.dry_run {
        return Ok(outcome(
            rule,
            "setting_dry_run",
            tf("sysenv.restore_dry_run", &[("src", &r.source.display().to_string())]).into(),
            None,
            started,
            true,
        ));
    }

    let op = store.op_begin(
        Some(rule.id),
        "restore",
        Some(&r.source.display().to_string()),
        Some(&r.target.display().to_string()),
    )?;

    // ---- 1. 变量改回原值 ----
    // 原值可能是"变量本来不存在" → 删掉它，而不是写空串。
    store.op_phase(op, "linking", None)?;
    let errs = restore_vars(scope, &snaps);
    if !errs.is_empty() {
        let msg = errs.join("；");
        let _ = store.op_finish(op, "failed", None, None, Some(&msg));
        return Err(SlError::Other(tf("sysenv.restore_failed", &[("err", &msg)])));
    }
    let _ = sysenv::broadcast_environment_change();

    // ---- 2. 数据搬回源位置 ----
    store.op_phase(op, "copying", None)?;
    let has_content = dir_has_content(&r.target);
    let stats = if has_content {
        ensure_dir(&r.source)?;
        perm::ensure_dir_access(store, Some(rule.id), &r.source);
        let mr = mover::move_tree(&r.target, &r.source, opts.verify, sink)?;
        mr.stats
    } else {
        ensure_dir(&r.source)?;
        crate::fsops::verify::TreeStats::default()
    };

    store.op_finish(
        op,
        "done",
        Some(stats.files as i64),
        Some(stats.bytes as i64),
        None,
    )?;
    store.set_rule_state(rule.id, "plain_dir", None)?;
    // 还原完成 → 原值记录作废，下次 apply 重新记录（那时才是真正的"原值"）。
    clear_env_prev(store, rule.id);
    store.log_event(
        Some(rule.id),
        "info",
        "restored",
        &tf("sysenv.restored_log", &[
            ("name", &rule.name),
            ("vars", &vars.join("/")),
        ]),
    )?;

    Ok(outcome(
        rule,
        "setting_restored",
        tf("sysenv.restored", &[
            ("vars", &vars.join("/")),
            ("src", &r.source.display().to_string()),
        ]),
        Some(stats),
        started,
        false,
    ))
}

/// 系统设置模式下的"重建"：把变量重新指回目标，并确保目标目录存在。
///
/// 与链接模式的 `relink` 对应 —— 链接丢了就重建，变量被改歪了就改回来。
pub fn relink_setting_rule(store: &Store, rule: &Rule) -> SlResult<Outcome> {
    let started = Instant::now();
    let r = resolve(store, rule)?;
    let scope = rule_scope(rule)?;
    let vars = rule_vars(rule)?;

    // 目标根可能刚被建出来（RAM 盘重启后的正常形态）。
    crate::boot::ensure_root_dir(&r.root.resolved_path().0)?;
    ensure_dir(&r.target)?;
    perm::ensure_dir_access(store, Some(rule.id), &r.target);

    let state = setting_state(scope, &vars, &r.target);
    if matches!(state, LinkState::SettingApplied { .. }) {
        store.set_rule_state(rule.id, "setting_ok", None)?;
        return Ok(outcome(
            rule,
            "already_ok",
            t("sysenv.already_ok").into(),
            None,
            started,
            false,
        ));
    }

    for name in &vars {
        sysenv::set_env_to_path(scope, name, &r.target)?;
    }
    let _ = sysenv::broadcast_environment_change();

    let has_content = dir_has_content(&r.target);
    store.set_rule_state(
        rule.id,
        if has_content { "setting_ok" } else { "setting_ok_empty" },
        None,
    )?;
    store.log_event(
        Some(rule.id),
        "info",
        "relinked",
        &tf("sysenv.relinked_log", &[
            ("name", &rule.name),
            ("target", &r.target.display().to_string()),
        ]),
    )?;
    Ok(outcome(
        rule,
        "setting_relinked",
        tf("sysenv.relinked", &[
            ("vars", &vars.join("/")),
            ("target", &r.target.display().to_string()),
        ]),
        None,
        started,
        false,
    ))
}

/// 供 `boot::rebuild` 使用：系统设置模式下只需确保目标目录存在。
///
/// 环境变量是**注册表里的持久值**，重启后不会丢 —— 所以开机重建要做的
/// 只有一件事：把 RAM 盘上的目标目录建出来（否则变量指向一个不存在的目录）。
/// 这与链接模式"重建目录 → 链接自动恢复"是同一件事的两面。
///
/// 授权在 `boot::rebuild` 里就地做（那里能把告警带进开机报告），
/// 这里只负责"目录存在"这一件事。
pub fn rebuild_setting_dir(store: &Store, rule: &Rule, r: &Resolved) -> SlResult<bool> {
    let _ = (store, rule);
    ensure_dir(&r.target)
}
