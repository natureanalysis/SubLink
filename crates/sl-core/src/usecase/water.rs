//! RAM 盘水位监控用例：分级、只读扫描、巡检落事件。
//!
//! 水位 = 目标根所在卷的已用百分比。RAM 盘写满的后果是软件写缓存直接失败
//! （或触发各软件自己的回退），所以越线要被**看到**：
//!
//! * [`water_levels`] 只读扫描（快照/doctor 用，无副作用）；
//! * [`water_poll`] 巡检：与 settings 里记的上次级别对比，**级别变化才落事件**，
//!   平稳期零写入、不刷事件流水。GUI 定时器调它，界面不开着也能留下水位痕迹。
//!
//! 阈值可用 settings 覆盖：`water_warn_pct` / `water_critical_pct`（数字 0~100）。
//! 本模块只**告警**不自动删文件 —— 自动清理涉及破坏性决策，须用户显式触发。
//!
//! 预授权自动清理由 [`water_poll`] 顺带跑，**一套处理、两种触发**：
//!
//! * [`run_auto_clean`]：**卷水位**到 critical 时清那些配了 `auto_clean_policy`
//!   且目标根在这块盘上的规则（"整块盘要满了"的应急处置）；
//! * [`run_folder_auto_clean`]：规则**自身目录占用**达到 `auto_clean_limit_bytes`
//!   时清它自己（"某个缓存目录撑着撑着就几 GB"的日常维护，不看卷水位）。
//!
//! 「达到文件夹容量上限」不是另一套清理，而是**水位自动清理的另一种触发**：两条
//! 路径共用 [`clean_rule_once`] 执行内核与 [`log_auto_clean_summary`] 汇总事件
//! —— 同一策略解析、同一 `clean_run` 门面（排除保护 / 事件流水照常）、同一
//! `auto_clean` 事件键，只有汇总文案里的触发词不同（「水位触线」/「目录超限」）。
//! 两条路径的差异只在**何时触发**与**冷却计时口径**（卷水位看整块盘、全局一把；
//! 容量上限看目录自身、逐规则计时）。
//!
//! 两条路径共用同一套授权模型：都要求用户显式配过 `auto_clean_policy`。

use std::collections::BTreeMap;
use std::path::Path;

use crate::engine;
use crate::error::{SlError, SlResult};
use crate::fsops::clean::CleanPolicy;
use crate::fsops::mover::volume_space;
use crate::fsops::verify::measure;
use crate::i18n::tf;
use crate::store::{Rule, Store};
use crate::util;

use super::view::WaterLevel;

/// 默认告警线（可用 settings 键 `water_warn_pct` 覆盖）。
pub const DEFAULT_WARN_PCT: f64 = 85.0;
/// 默认严重线（可用 settings 键 `water_critical_pct` 覆盖）。
pub const DEFAULT_CRIT_PCT: f64 = 95.0;

/// settings 里记「上次各根水位级别」的键（JSON：`{路径: 级别}`）。
const STATE_KEY: &str = "water_level_state";
/// settings 里记「上次自动清理时刻」（Unix 秒）—— 冷却去抖用。
const LAST_AUTO_CLEAN_KEY: &str = "water_last_auto_clean_at";
/// 自动清理冷却期：期内不重复触发（水位降不下去时不反复删）。
/// 两条触发路径共用这个时长，但**计时各自独立**（见 [`FOLDER_STATE_KEY`]）。
const AUTO_CLEAN_COOLDOWN_SECS: i64 = 30 * 60;
/// settings 里记「上次文件夹级自动清理时刻」（JSON：`{规则id: Unix秒}`）。
///
/// 为什么按规则分别计时而不是全局一把：卷水位是**一块盘**的紧急状态，全局冷却
/// 合理；文件夹级是**每个目录各自**的日常维护，某个目录刚清过不该把别的目录
/// 也推迟 30 分钟。键里只保留「配了上限」的规则，规则删掉后其残留项会被清掉。
const FOLDER_STATE_KEY: &str = "folder_auto_clean_state";

/// 校验预授权策略串：`''`（未启用）/ `empty` / `older:<正整数秒>` / `keep:<正字节数>`。
/// 空串是合法值（未启用），只有「非空但解析不了」才报错。
pub fn validate_auto_policy(s: &str) -> SlResult<()> {
    let trimmed = s.trim();
    if trimmed.is_empty() || parse_auto_policy(trimmed).is_some() {
        Ok(())
    } else {
        Err(SlError::InvalidPath(tf(
            "water.invalid_policy",
            &[("s", s)],
        )))
    }
}

/// 解析预授权策略串 → CleanPolicy（`now` 为当前 Unix 秒）。
/// `None` = 未启用或格式非法。
///
/// 形式：`''` / `empty` / `older:<正整数秒>` / `keep:<正字节数>`。
pub fn parse_auto_policy(s: &str) -> Option<CleanPolicy> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if s == "empty" {
        return Some(CleanPolicy::Empty);
    }
    if let Some(rest) = s.strip_prefix("older:") {
        let secs: i64 = rest.parse().ok()?;
        if secs <= 0 {
            return None;
        }
        return Some(CleanPolicy::Before { cutoff: now - secs });
    }
    // 空间模式：keep:<字节> —— 保留最近修改的这么多字节（水位下限）。
    if let Some(rest) = s.strip_prefix("keep:") {
        let bytes: u64 = rest.parse().ok()?;
        if bytes == 0 {
            return None;
        }
        return Some(CleanPolicy::KeepWithin { keep_bytes: bytes });
    }
    None
}

fn threshold(store: &Store, key: &str, default: f64) -> f64 {
    store
        .get_setting(key)
        .ok()
        .flatten()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|v| (0.0..=100.0).contains(v))
        .unwrap_or(default)
}

/// 纯分级（越界值视为未配置，回落默认）。
fn classify(pct: f64, warn: f64, crit: f64) -> &'static str {
    if pct >= crit {
        "critical"
    } else if pct >= warn {
        "warn"
    } else {
        "ok"
    }
}

/// 只读扫描所有目标根的当前水位（不落库、不产生事件）。
pub fn water_levels(store: &Store) -> SlResult<Vec<WaterLevel>> {
    let warn = threshold(store, "water_warn_pct", DEFAULT_WARN_PCT);
    let crit = threshold(store, "water_critical_pct", DEFAULT_CRIT_PCT);
    let mut out = Vec::new();
    for r in store.list_roots()? {
        let (path, _unknown) = r.resolved_path();
        let (level, used_pct, free, free_text, total_text, message) =
            match volume_space(Path::new(&path)) {
                Ok((total, free_b)) if total > 0 => {
                    let used = total.saturating_sub(free_b);
                    let pct = used as f64 / total as f64 * 100.0;
                    let lv = classify(pct, warn, crit);
                    let msg = crate::i18n::tf(
                        "water.message.ok",
                        &[
                            ("pct", &format!("{pct:.1}")),
                            ("free", &util::human_bytes(free_b)),
                        ],
                    );
                    (
                        lv.to_string(),
                        pct,
                        free_b,
                        util::human_bytes(free_b),
                        util::human_bytes(total),
                        msg,
                    )
                }
                Ok((total, _)) => (
                    "unknown".into(),
                    0.0,
                    0,
                    util::human_bytes(0),
                    util::human_bytes(total),
                    crate::i18n::t("water.message.zero").into(),
                ),
                Err(_) => (
                    "unknown".into(),
                    0.0,
                    0,
                    util::human_bytes(0),
                    util::human_bytes(0),
                    crate::i18n::t("water.message.unavailable").into(),
                ),
            };
        out.push(WaterLevel {
            root_id: r.id,
            label: r.label,
            path: r.path,
            used_pct,
            free,
            free_text,
            total_text,
            level,
            message,
        });
    }
    Ok(out)
}

/// 巡检：扫描水位，与上次记录对比，**级别变化**才落一条事件并写回状态。
///
/// 事件级别映射：进入 `warn` → warn；进入 `critical` → error；降级 → info（恢复）。
/// 首次巡检只建立基线，`ok` 不落事件（避免装完就有一条废话）。
pub fn water_poll(store: &Store) -> SlResult<Vec<WaterLevel>> {
    let levels = water_levels(store)?;
    let old: BTreeMap<String, String> = store
        .get_setting(STATE_KEY)?
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let mut new_state: BTreeMap<String, String> = BTreeMap::new();
    for lv in &levels {
        if lv.level == "unknown" {
            continue;
        }
        new_state.insert(lv.path.clone(), lv.level.clone());
        // 首次巡检视为从 ok 出发：直接 warn/critical（装完就写满）也要落事件，只有 ok 静默建基线。
        let from = old.get(&lv.path).map(String::as_str).unwrap_or("ok");
        if from == lv.level {
            continue;
        }
        let (ev_level, msg) = match lv.level.as_str() {
            "critical" => (
                "error",
                tf(
                    "water.critical",
                    &[
                        ("label", &lv.label),
                        ("path", &lv.path),
                        ("pct", &format!("{:.1}", lv.used_pct)),
                        ("free", &lv.free_text),
                    ],
                ),
            ),
            "warn" => (
                "warn",
                tf(
                    "water.warn",
                    &[
                        ("label", &lv.label),
                        ("path", &lv.path),
                        ("pct", &format!("{:.1}", lv.used_pct)),
                        ("free", &lv.free_text),
                    ],
                ),
            ),
            _ => (
                "info",
                tf(
                    "water.recovered",
                    &[
                        ("label", &lv.label),
                        ("path", &lv.path),
                        ("pct", &format!("{:.1}", lv.used_pct)),
                        ("free", &lv.free_text),
                    ],
                ),
            ),
        };
        store.log_event(None, ev_level, "water", &msg)?;
    }

    if old != new_state {
        store.set_setting(STATE_KEY, &serde_json::to_string(&new_state)?)?;
    }

    run_auto_clean(store, &levels)?;
    // 文件夹级触发与卷水位无关（不看 levels），所以放在这里独立跑：
    // 卷还宽裕时，某个缓存目录自己撑到上限也该被清。
    run_folder_auto_clean(store)?;
    Ok(levels)
}

/// 一次预授权自动清理的累计结果（供两条触发路径共用与汇总落事件）。
#[derive(Default)]
struct AutoCleanTally {
    /// 成功执行清理的规则条数。
    done: usize,
    /// 清理失败的规则条数。
    failed: usize,
    deleted_files: u64,
    deleted_bytes: u64,
}

/// 统一的预授权清理执行内核：走 `clean_run` 门面并把结果记进 `tally`。
///
/// [`run_auto_clean`]（**卷水位**触线）与 [`run_folder_auto_clean`]（目录自身达到
/// **文件夹容量上限**）共用这一段 —— 即「达到文件夹容量上限」执行的就是**水位自动
/// 清理**本身：同一套策略解析、同一 `clean_run` 门面（预演语义、排除保护、事件流水
/// 照常），两条路径的清理动作完全一致。
///
/// 策略由调用方先解析（坏策略各路径自行落 `water.bad_rule_policy` warn 并决定计时），
/// 这里只负责「清一遍 + 记账」。
fn clean_rule_once(
    store: &Store,
    rule: &Rule,
    policy: &CleanPolicy,
    tally: &mut AutoCleanTally,
) {
    match super::clean::clean_run(
        store,
        &super::view::CleanTarget::Rule { id: rule.id },
        policy,
        &[],
        false,
        &mut |_| {},
    ) {
        Ok(rep) => {
            tally.done += 1;
            tally.deleted_files += rep.deleted_files;
            tally.deleted_bytes += rep.deleted_bytes;
        }
        Err(_) => tally.failed += 1,
    }
}

/// 落一条 `auto_clean` 汇总事件。两条触发路径共用同一事件键与文案，只有触发词
/// 不同（`water.trigger.volume` / `water.trigger.folder`），让人一眼看出这次清理是
/// 「水位触线」还是「目录超限」触发的。
fn log_auto_clean_summary(store: &Store, trigger: &str, tally: &AutoCleanTally) -> SlResult<()> {
    let level = if tally.failed > 0 { "warn" } else { "info" };
    store.log_event(
        None,
        level,
        "auto_clean",
        &tf(
            "water.auto_clean_log",
            &[
                ("trigger", trigger),
                ("done", &tally.done.to_string()),
                ("files", &tally.deleted_files.to_string()),
                ("size", &util::human_bytes(tally.deleted_bytes)),
                (
                    "failed",
                    &if tally.failed > 0 {
                        tf("water.failed_suffix", &[("n", &tally.failed.to_string())])
                    } else {
                        String::new()
                    },
                ),
            ],
        ),
    )
}

/// 水位 critical 时的预授权自动清理。
///
/// * 只处理「目标根在 critical 卷上 **且** 配了 `auto_clean_policy`」的规则；
/// * 冷却期内（30 分钟）不重复触发 —— 水位降不下去（比如文件都被占用）时不反复删；
/// * 每条规则走 [`clean_rule_once`]（与文件夹容量上限路径同一套处理），
///   另落一条 `auto_clean` 汇总事件，触发词为「水位触线」。
fn run_auto_clean(store: &Store, levels: &[WaterLevel]) -> SlResult<()> {
    let critical_roots: Vec<i64> = levels
        .iter()
        .filter(|w| w.level == "critical")
        .map(|w| w.root_id)
        .collect();
    if critical_roots.is_empty() {
        return Ok(());
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    if let Some(last) = store
        .get_setting(LAST_AUTO_CLEAN_KEY)?
        .and_then(|s| s.trim().parse::<i64>().ok())
    {
        if now - last < AUTO_CLEAN_COOLDOWN_SECS {
            return Ok(());
        }
    }

    let mut tally = AutoCleanTally::default();
    for rule in store.list_rules()? {
        if rule.auto_clean_policy.trim().is_empty()
            || !critical_roots.contains(&rule.target_root_id)
        {
            continue;
        }
        let Some(policy) = parse_auto_policy(&rule.auto_clean_policy) else {
            // 配置坏了（手工改库等）：不拖垮巡检，但要被看到。
            store.log_event(
                Some(rule.id),
                "warn",
                "auto_clean",
                &tf(
                    "water.bad_rule_policy",
                    &[("name", &rule.name), ("s", &rule.auto_clean_policy)],
                ),
            )?;
            continue;
        };
        clean_rule_once(store, &rule, &policy, &mut tally);
    }

    if tally.done + tally.failed > 0 {
        log_auto_clean_summary(store, &tf("water.trigger.volume", &[]), &tally)?;
        store.set_setting(LAST_AUTO_CLEAN_KEY, &now.to_string())?;
    }
    Ok(())
}

/// 文件夹级预授权自动清理：规则**自身目录占用**达到 `auto_clean_limit_bytes`（文件夹
/// 容量上限）时清它自己 —— 这是**水位自动清理的另一种触发**，不是另一套清理。
///
/// 与 [`run_auto_clean`] 的差异只在**何时触发**，处理内核与事件口径完全共用：
/// * **不看卷水位** —— 单个缓存目录撑到几 GB 时，整块盘可能还很宽裕，
///   等卷到 critical 才清就太晚了；这条路径只管"这个目录自己是不是该瘦身了"。
/// * **逐规则计时**（[`FOLDER_STATE_KEY`]）—— 某个目录刚清过不该推迟别的目录。
/// * 只处理 `auto_clean_limit_bytes > 0` 且配了合法 `auto_clean_policy` 的规则
///   （策略决定"删什么"，上限决定"什么时候删"，缺一不可）；
/// * 清理走 [`clean_rule_once`]（与卷水位路径同一套），另落一条汇总事件，触发词为
///   「目录超限」。
///
/// 状态语义：**所有非清理分支都写回旧时间戳**（而不是删掉这一项）——占用在
/// 上限附近震荡时，若每轮都重置计时就会绕过冷却反复删；只有真清过才刷新为 `now`。
fn run_folder_auto_clean(store: &Store) -> SlResult<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let prev: BTreeMap<i64, i64> = store
        .get_setting(FOLDER_STATE_KEY)?
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let mut next: BTreeMap<i64, i64> = BTreeMap::new();
    let mut tally = AutoCleanTally::default();

    for rule in store.list_rules()? {
        // 没配上限的规则完全不受这条路径影响（旧行为）。
        if rule.auto_clean_limit_bytes <= 0 {
            continue;
        }
        let last = prev.get(&rule.id).copied().unwrap_or(0);
        if last > 0 && now - last < AUTO_CLEAN_COOLDOWN_SECS {
            // 冷却期内：保留计时，不重复删。
            next.insert(rule.id, last);
            continue;
        }
        let Some(policy) = parse_auto_policy(&rule.auto_clean_policy) else {
            // 配了上限却没给（或写坏）策略：无从下手，但不拖垮巡检，要能被看到。
            next.insert(rule.id, last);
            store.log_event(
                Some(rule.id),
                "warn",
                "auto_clean",
                &tf(
                    "water.bad_rule_policy",
                    &[("name", &rule.name), ("s", &rule.auto_clean_policy)],
                ),
            )?;
            continue;
        };

        // 量这条规则目录**自身**的占用（不是整块卷）。
        let used = match engine::clean_dir_for_rule(store, &rule) {
            Ok(dir) if dir.is_dir() => measure(&dir).map(|s| s.bytes).unwrap_or(0),
            _ => 0,
        };
        if used < rule.auto_clean_limit_bytes as u64 {
            next.insert(rule.id, last);
            continue;
        }

        // 达到容量上限 = 触发一次水位自动清理（与卷水位路径同一套处理）。
        clean_rule_once(store, &rule, &policy, &mut tally);
        // 成功或失败都记时间戳，免得冷却期内反复重试同一个坏目录。
        next.insert(rule.id, now);
    }

    if tally.done + tally.failed > 0 {
        log_auto_clean_summary(store, &tf("water.trigger.folder", &[]), &tally)?;
    }

    // 只有状态真变了才写库（平稳期零写入，与水位路径同一取舍）。
    if prev != next {
        store.set_setting(FOLDER_STATE_KEY, &serde_json::to_string(&next)?)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_boundaries() {
        assert_eq!(classify(0.0, 85.0, 95.0), "ok");
        assert_eq!(classify(84.9, 85.0, 95.0), "ok");
        assert_eq!(classify(85.0, 85.0, 95.0), "warn", "告警线含等于");
        assert_eq!(classify(94.9, 85.0, 95.0), "warn");
        assert_eq!(classify(95.0, 85.0, 95.0), "critical", "严重线含等于");
        assert_eq!(classify(100.0, 85.0, 95.0), "critical");
        // 阈值可自定义（warn 高于默认 crit 也不管：配置者优先）
        assert_eq!(classify(70.0, 70.0, 99.0), "warn");
    }

    #[test]
    fn parse_auto_policy_forms() {
        assert!(parse_auto_policy("").is_none(), "空 = 未启用");
        assert!(parse_auto_policy("  ").is_none());
        assert!(matches!(
            parse_auto_policy("empty"),
            Some(CleanPolicy::Empty)
        ));
        assert!(matches!(
            parse_auto_policy(" older:3600 "),
            Some(CleanPolicy::Before { .. })
        ));
        assert_eq!(
            parse_auto_policy("keep:104857600"),
            Some(CleanPolicy::KeepWithin { keep_bytes: 104_857_600 }),
            "空间模式：keep:<字节>"
        );
        assert!(matches!(
            parse_auto_policy(" keep:1 "),
            Some(CleanPolicy::KeepWithin { .. })
        ));
        // 非法：前缀不对 / 非数字 / 零 / 负数
        assert!(parse_auto_policy("Empty").is_none(), "大小写敏感");
        assert!(parse_auto_policy("older:abc").is_none());
        assert!(parse_auto_policy("older:0").is_none());
        assert!(parse_auto_policy("older:-5").is_none());
        assert!(parse_auto_policy("older:").is_none());
        assert!(parse_auto_policy("keep:abc").is_none());
        assert!(parse_auto_policy("keep:0").is_none(), "下限必须为正");
        assert!(parse_auto_policy("keep:").is_none());
        assert!(parse_auto_policy("keep:-1").is_none());
        // 校验函数与解析一致
        assert!(validate_auto_policy("").is_ok());
        assert!(validate_auto_policy("empty").is_ok());
        assert!(validate_auto_policy("older:60").is_ok());
        assert!(validate_auto_policy("keep:104857600").is_ok());
        assert!(validate_auto_policy("older:x").is_err());
        assert!(validate_auto_policy("keep:0").is_err());
        assert!(validate_auto_policy("keep:x").is_err());
    }

    // ------------------------------------------------------------------
    // 文件夹级自动清理
    // ------------------------------------------------------------------

    /// 造一个「源目录含 3 个 10 字节文件」的库与源目录。
    fn folder_env(tag: &str) -> (Store, std::path::PathBuf, std::path::PathBuf) {
        let base = std::env::temp_dir().join(format!("td_water_folder_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let src = base.join("cache");
        let ram = base.join("ram");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&ram).unwrap();
        for i in 0..3 {
            std::fs::write(src.join(format!("f{i}.bin")), vec![1u8; 10]).unwrap();
        }
        let store = Store::open_in_memory().unwrap();
        store
            .insert_root("RAM", &ram.to_string_lossy(), true, 5000)
            .unwrap();
        (store, src, base)
    }

    fn add_folder_rule(
        store: &Store,
        src: &std::path::Path,
        sub: &str,
        policy: &str,
        limit: i64,
    ) -> i64 {
        store
            .insert_rule(&crate::store::NewRule {
                name: format!("R-{sub}"),
                category: "other".into(),
                source_template: src.to_string_lossy().into_owned(),
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
                auto_clean_policy: policy.into(),
                auto_clean_limit_bytes: limit,
            })
            .unwrap()
    }

    fn dir_files(dir: &std::path::Path) -> usize {
        std::fs::read_dir(dir).unwrap().count()
    }

    fn saved_state(store: &Store) -> BTreeMap<i64, i64> {
        store
            .get_setting(FOLDER_STATE_KEY)
            .unwrap()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    #[test]
    fn folder_auto_clean_skips_under_limit_and_when_disabled() {
        let (store, src, base) = folder_env("under");
        // 上限 100 KB，实际只有 30 字节 → 不动
        let id = add_folder_rule(&store, &src, "x", "empty", 100 * 1024);
        run_folder_auto_clean(&store).unwrap();
        assert_eq!(dir_files(&src), 3, "未到上限不该动文件");
        assert_eq!(saved_state(&store).get(&id), Some(&0), "未清理只记 0，不是清理时刻");

        // 上限 0 = 不启用：即使目录远超任何阈值也不清。
        // 换一个源目录（同一源路径只能有一条规则，撞 UNIQUE）。
        let src0 = base.join("cache0");
        std::fs::create_dir_all(&src0).unwrap();
        for i in 0..3 {
            std::fs::write(src0.join(format!("h{i}.bin")), vec![1u8; 10]).unwrap();
        }
        let id0 = add_folder_rule(&store, &src0, "y", "empty", 0);
        run_folder_auto_clean(&store).unwrap();
        assert_eq!(dir_files(&src0), 3, "上限 0 的规则完全不受这条路径影响");
        assert!(
            !saved_state(&store).contains_key(&id0),
            "上限 0 的规则不该进状态表"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 「达到文件夹容量上限」与「水位触线」是同一套处理：同一执行内核、同一事件键，
    /// 只有汇总文案里的触发词不同。
    #[test]
    fn folder_limit_reuses_water_auto_clean_log() {
        let (store, src, base) = folder_env("reuse");
        // 30 字节 > 25 字节上限 → 触发一次水位自动清理
        add_folder_rule(&store, &src, "x", "empty", 25);
        run_folder_auto_clean(&store).unwrap();
        assert_eq!(dir_files(&src), 0, "达到上限应按策略清一次");

        let ev = store.recent_events(10).unwrap();
        let summary = ev
            .iter()
            .find(|e| e.rule_id.is_none() && e.kind == "auto_clean")
            .expect("应落一条统一汇总事件");
        assert_eq!(summary.level, "info");
        assert_eq!(
            summary.message,
            tf(
                "water.auto_clean_log",
                &[
                    ("trigger", &tf("water.trigger.folder", &[])),
                    ("done", "1"),
                    ("files", "3"),
                    ("size", &util::human_bytes(30)),
                    ("failed", ""),
                ],
            ),
            "汇总文案应与水位路径共用同一键、只有触发词不同"
        );
        // 两种触发词必须区分开，且文件夹专属旧键已并掉。
        assert_ne!(
            tf("water.trigger.volume", &[]),
            tf("water.trigger.folder", &[])
        );
        assert!(
            crate::i18n::t("water.folder_auto_clean_log").is_empty(),
            "文件夹专属汇总键应已并入 water.auto_clean_log"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn folder_auto_clean_triggers_over_limit_then_cools_down() {
        let (store, src, base) = folder_env("over");
        // 30 字节 > 25 字节上限 → 立刻清
        let id = add_folder_rule(&store, &src, "x", "empty", 25);
        run_folder_auto_clean(&store).unwrap();
        assert_eq!(dir_files(&src), 0, "超过上限应清空目录（根目录保留）");

        let last = *saved_state(&store).get(&id).expect("清过必须记时间戳");
        assert!(last > 0, "清理时刻必须是真实的 Unix 秒");

        // 会话级汇总事件（rule_id = None 的就是文件夹级那条）
        let ev = store.recent_events(20).unwrap();
        assert!(
            ev.iter()
                .any(|e| e.rule_id.is_none() && e.kind == "auto_clean" && e.message.contains('3')),
            "应落一条文件夹级汇总事件：{ev:?}"
        );

        // 再灌一批文件：冷却期内不该再删（否则超限线附近会反复删）
        for i in 0..3 {
            std::fs::write(src.join(format!("g{i}.bin")), vec![1u8; 10]).unwrap();
        }
        run_folder_auto_clean(&store).unwrap();
        assert_eq!(dir_files(&src), 3, "冷却期内不该重复删");
        assert_eq!(
            saved_state(&store).get(&id),
            Some(&last),
            "冷却期内必须保留原时间戳"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn folder_auto_clean_warns_on_broken_policy_without_crashing() {
        let (store, src, base) = folder_env("broken");
        // 配了上限却没给合法策略：记 warn、不动文件、不拖垮巡检
        add_folder_rule(&store, &src, "x", "older:abc", 1);
        run_folder_auto_clean(&store).unwrap();
        assert_eq!(dir_files(&src), 3, "策略坏了就不该动手");
        let ev = store.recent_events(10).unwrap();
        assert!(
            ev.iter()
                .any(|e| e.kind == "auto_clean" && e.level == "warn" && e.rule_id.is_some()),
            "坏策略应落一条带规则 id 的 warn：{ev:?}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn folder_auto_clean_state_entries_drop_when_rule_loses_limit() {
        let (store, src, base) = folder_env("dropstate");
        let id = add_folder_rule(&store, &src, "x", "empty", 25);
        run_folder_auto_clean(&store).unwrap();
        assert!(saved_state(&store).contains_key(&id));

        // 取消上限后，残留状态项应在下一轮被清掉（表里只留"配了上限"的规则）
        let nr = crate::store::NewRule {
            name: "R-x".into(),
            category: "other".into(),
            source_template: src.to_string_lossy().into_owned(),
            target_root_id: 1,
            target_subpath: "x".into(),
            link_type: "junction".into(),
            redirect_mode: "link".into(),
            env_vars: Vec::new(),
            env_scope: "user".into(),
            want_enabled: true,
            rebuild_on_boot: true,
            notes: None,
            excludes: Vec::new(),
            auto_clean_policy: "empty".into(),
            auto_clean_limit_bytes: 0,
        };
        store.update_rule(id, &nr).unwrap();
        run_folder_auto_clean(&store).unwrap();
        assert!(
            !saved_state(&store).contains_key(&id),
            "取消上限后状态项应被清掉"
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
