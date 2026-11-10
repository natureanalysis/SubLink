//! 定时（低峰期）清理：与水位预授权自动清理互补的**例行维护**。
//!
//! * 水位预授权（`water.rs` 的 `run_auto_clean`）：**应急** —— RAM 盘快满了才动，
//!   只处理 critical 卷上的规则；
//! * 定时清理：**例行** —— 每天到点（默认凌晨 03:00，可改可关）把**所有**
//!   配了 `auto_clean_policy` 的规则按各自策略清一遍，让缓存别攒到触发水位。
//!
//! 两者共用同一套授权模型：规则级 `auto_clean_policy`（用户显式授权过
//! "这种文件可以自动删"），这里**不发明**新的删除策略 —— 没配策略的规则绝不动。
//!
//! 触发方式：GUI 前端定时器与水位巡检同一节拍地调 [`scheduled_clean_poll`]；
//! 到点且当天没跑过才执行，**每自然日最多一次**（`scheduled_clean_last_run`
//! 记本地日期）。默认关闭（破坏性操作必须显式 opt-in）。

use crate::i18n::tf;
use crate::store::Store;
use crate::util;

use super::view::ScheduledCleanView;
use super::water::parse_auto_policy;

/// settings 键：是否启用（"1" = 启用；缺省 = 关）。
pub const KEY_ENABLED: &str = "scheduled_clean_enabled";
/// settings 键：每天执行的时刻 "HH:MM"（缺省 03:00 —— 夜里没人用机器）。
pub const KEY_TIME: &str = "scheduled_clean_time";
/// settings 键：上次执行日期 "YYYY-MM-DD"（内部状态，每天最多一次的判据）。
pub const KEY_LAST_RUN: &str = "scheduled_clean_last_run";

pub const DEFAULT_TIME: &str = "03:00";

/// 巡检：没到点 / 未启用 / 今天跑过了 → 全零 `ran=false`；到点则执行清理。
pub fn scheduled_clean_poll(store: &Store) -> crate::SlResult<ScheduledCleanView> {
    scheduled_clean_at(store, &local_now())
}

/// 本地"现在"（测试通过注入 [`LocalNow`] 覆盖，不真等到凌晨三点）。
pub(crate) struct LocalNow {
    /// 本地日期 "YYYY-MM-DD"（"每天最多一次"的判定粒度）。
    pub(crate) date: String,
    /// 本地当天已过秒数（与 `scheduled_clean_time` 同一坐标系）。
    pub(crate) secs: i64,
}

/// 直接问 OS 本地墙上时间（SYSTEMTIME 自带年月日时分），绕开时区/DST 数学。
#[cfg(windows)]
fn local_now() -> LocalNow {
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    let mut st: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut st) };
    LocalNow {
        date: format!("{:04}-{:02}-{:02}", st.wYear, st.wMonth, st.wDay),
        secs: st.wHour as i64 * 3600 + st.wMinute as i64 * 60 + st.wSecond as i64,
    }
}

/// 非 Windows 无窗口场景（CI 编译）：给个 UTC 近似即可，真机路径不走这里。
#[cfg(not(windows))]
fn local_now() -> LocalNow {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    LocalNow {
        date: format!("d{}", secs / 86400),
        secs: secs % 86400,
    }
}

/// 注入时间的执行体（[`scheduled_clean_poll`] 的可测内核）。
pub(crate) fn scheduled_clean_at(
    store: &Store,
    now: &LocalNow,
) -> crate::SlResult<ScheduledCleanView> {
    let mut v = ScheduledCleanView::default();
    if store.get_setting(KEY_ENABLED)?.as_deref().map(str::trim) != Some("1") {
        return Ok(v);
    }
    let time = store
        .get_setting(KEY_TIME)?
        .unwrap_or_else(|| DEFAULT_TIME.to_string());
    let target = match parse_hhmm(&time) {
        Some(t) => t,
        None => {
            // 手工改库才会出现（设置界面只给合法输入）：跳过但要被看到。
            store.log_event(
                None,
                "warn",
                "scheduled_clean",
                &tf("schedule.bad_time", &[("time", &time)]),
            )?;
            return Ok(v);
        }
    };
    if now.secs < target {
        return Ok(v);
    }
    if store.get_setting(KEY_LAST_RUN)?.as_deref() == Some(now.date.as_str()) {
        return Ok(v);
    }

    v.ran = true;
    for rule in store.list_rules()? {
        if rule.auto_clean_policy.trim().is_empty() {
            continue;
        }
        let Some(policy) = parse_auto_policy(&rule.auto_clean_policy) else {
            v.skipped += 1;
            store.log_event(
                Some(rule.id),
                "warn",
                "scheduled_clean",
                &tf(
                    "schedule.bad_rule_policy",
                    &[("name", &rule.name), ("s", &rule.auto_clean_policy)],
                ),
            )?;
            continue;
        };
        match super::clean::clean_run(
            store,
            &super::view::CleanTarget::Rule { id: rule.id },
            &policy,
            &[],
            false,
            &mut |_| {},
        ) {
            Ok(rep) => {
                v.done += 1;
                v.deleted_files += rep.deleted_files;
                v.deleted_bytes += rep.deleted_bytes;
            }
            Err(_) => v.failed += 1,
        }
    }
    v.deleted_text = util::human_bytes(v.deleted_bytes);
    store.set_setting(KEY_LAST_RUN, &now.date)?;
    if v.done + v.failed + v.skipped > 0 {
        let level = if v.failed > 0 { "warn" } else { "info" };
        store.log_event(
            None,
            level,
            "scheduled_clean",
            &tf(
                "schedule.log",
                &[
                    ("n", &(v.done + v.failed).to_string()),
                    ("files", &v.deleted_files.to_string()),
                    ("size", &v.deleted_text),
                    (
                        "failed",
                        &if v.failed > 0 {
                            tf("schedule.failed_suffix", &[("n", &v.failed.to_string())])
                        } else {
                            String::new()
                        },
                    ),
                ],
            ),
        )?;
    }
    Ok(v)
}

/// "HH:MM" → 当天秒数（23:59 合法、25:00 / 12:60 不合法）。
/// 公开给 GUI 设置命令做入参校验 —— 时刻格式只有一个权威定义。
pub fn parse_hhmm(s: &str) -> Option<i64> {
    let (h, m) = s.trim().split_once(':')?;
    let h: i64 = h.parse().ok()?;
    let m: i64 = m.parse().ok()?;
    if !(0..=23).contains(&h) || !(0..=59).contains(&m) {
        return None;
    }
    Some(h * 3600 + m * 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hhmm_boundaries() {
        assert_eq!(parse_hhmm("03:00"), Some(3 * 3600));
        assert_eq!(parse_hhmm(" 23:59 "), Some(23 * 3600 + 59 * 60));
        assert_eq!(parse_hhmm("00:00"), Some(0));
        assert_eq!(parse_hhmm("24:00"), None);
        assert_eq!(parse_hhmm("12:60"), None);
        assert_eq!(parse_hhmm("12"), None);
        assert_eq!(parse_hhmm("ab:cd"), None);
        assert_eq!(parse_hhmm(""), None);
    }
}
