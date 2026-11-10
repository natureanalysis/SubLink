//! 时间参数与相对时长的解析 / 展示。
//!
//! 缓存清理关心的是新旧程度，不是绝对时刻，用相对时长既直观、
//! 又彻底绕开日历/时区数学（不引 chrono）。

use std::time::{SystemTime, UNIX_EPOCH};

use sl_core::i18n::{t, tf};
use sl_core::{SlError, SlResult};

/// 本地"当天已过多少秒"（时:分:秒）。用于把 UTC now 折算本地零点：
/// 本地零点 = utc_now − 当天已过秒数。绕开时区偏移与 DST 边界（OS 直接给本地墙上时间）。
#[cfg(windows)]
pub(crate) fn local_time_of_day_secs() -> u32 {
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    let mut st: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut st) };
    st.wHour as u32 * 3600 + st.wMinute as u32 * 60 + st.wSecond as u32
}

#[cfg(not(windows))]
pub(crate) fn local_time_of_day_secs() -> u32 {
    0
}

pub(crate) fn now_utc_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 把秒数转成"3 天 / 1 小时 / 2 分 / 45 秒"这样的大单位人话。
/// 不复用 `util::human_dur_ms`（它只到秒级，86400s 会显示成"86400.0s"）。
pub(crate) fn human_duration(secs: i64) -> String {
    if secs <= 0 {
        return t("timefmt.zero").into();
    }
    let d = secs / 86400;
    let h = (secs % 86400) / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    let mut parts: Vec<String> = Vec::new();
    if d > 0 {
        parts.push(tf("timefmt.days", &[("n", &d.to_string())]));
        if h > 0 && d < 30 {
            parts.push(tf("timefmt.hours", &[("n", &h.to_string())]));
        }
    } else if h > 0 {
        parts.push(tf("timefmt.hours", &[("n", &h.to_string())]));
        if m > 0 {
            parts.push(tf("timefmt.minutes", &[("n", &m.to_string())]));
        }
    } else if m > 0 {
        parts.push(tf("timefmt.minutes", &[("n", &m.to_string())]));
    } else {
        parts.push(tf("timefmt.seconds", &[("n", &s.to_string())]));
    }
    parts.join(" ")
}

/// 距今多久以前（带"前"）。
pub(crate) fn ago_text(secs_ago: i64) -> String {
    if secs_ago <= 0 {
        return t("timefmt.now").into();
    }
    tf("timefmt.ago", &[("dur", &human_duration(secs_ago))])
}

/// 解析时间点参数为 Unix 秒。接受：
/// - `today` / `yesterday`：本地零点 / 昨天零点；
/// - 纯数字：绝对 Unix 秒；
/// - 相对时长 `90s` / `30m` / `2h` / `7d`：距今多久以前。
pub(crate) fn parse_when(s: &str, now_utc: i64, tod_secs: i64) -> SlResult<i64> {
    let t = s.trim().to_ascii_lowercase();
    if t == "today" {
        return Ok(now_utc - tod_secs);
    }
    if t == "yesterday" {
        return Ok(now_utc - tod_secs - 86400);
    }
    if let Ok(secs) = t.parse::<i64>() {
        return Ok(secs);
    }
    // 数字前缀 + 字母单位。用 find+切片把字母**保留**在 unit 里
    // （split_once 会丢掉分隔符本身，"1h" 会被切成 ("1","")）。
    let idx = t.find(|c: char| c.is_alphabetic()).ok_or_else(|| {
        SlError::Other(tf("timefmt.bad_time", &[("s", s)]))
    })?;
    let (num, unit) = (&t[..idx], &t[idx..]);
    let n: i64 = num
        .parse()
        .map_err(|_| SlError::Other(tf("timefmt.bad_number", &[("s", s)])))?;
    let mult = match unit {
        "s" | "sec" | "secs" => 1,
        "m" | "min" | "mins" => 60,
        "h" | "hr" | "hrs" => 3600,
        "d" | "day" | "days" => 86400,
        _ => {
            return Err(SlError::Other(tf("timefmt.bad_unit", &[("unit", unit)])))
        }
    };
    Ok(now_utc - n * mult)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sl_core::i18n::{set_locale, Locale};

    fn zh() {
        set_locale(Locale::Zh);
    }

    const NOW: i64 = 1_700_000_000;
    const TOD: i64 = 3600 * 10 + 200; // 当天已过 10:03:20

    #[test]
    fn parse_when_relative_and_keywords() {
        zh();
        assert_eq!(parse_when("1h", NOW, TOD).unwrap(), NOW - 3600);
        assert_eq!(parse_when("30m", NOW, TOD).unwrap(), NOW - 1800);
        assert_eq!(parse_when("7d", NOW, TOD).unwrap(), NOW - 7 * 86400);
        assert_eq!(parse_when("90s", NOW, TOD).unwrap(), NOW - 90);
        assert_eq!(parse_when("today", NOW, TOD).unwrap(), NOW - TOD);
        assert_eq!(parse_when("yesterday", NOW, TOD).unwrap(), NOW - TOD - 86400);
        assert_eq!(parse_when("1234567890", NOW, TOD).unwrap(), 1234567890);
        assert!(parse_when("abc", NOW, TOD).is_err());
        assert!(parse_when("5x", NOW, TOD).is_err(), "未知单位应报错");
    }

    #[test]
    fn ago_text_is_relative() {
        zh();
        assert_eq!(ago_text(0), "现在");
        assert!(ago_text(3600).ends_with('前'));
        assert!(ago_text(-5).ends_with("在"), "未来时刻归一为「现在」");
    }

    #[test]
    fn human_duration_uses_big_units() {
        zh();
        assert_eq!(human_duration(45), "45 秒");
        assert_eq!(human_duration(120), "2 分");
        assert_eq!(human_duration(3600), "1 小时");
        assert_eq!(human_duration(86400), "1 天");
        assert_eq!(human_duration(259200), "3 天");
        assert_eq!(human_duration(90000), "1 天 1 小时");
    }
}