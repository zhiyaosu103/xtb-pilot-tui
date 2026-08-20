//! 时间工具（设计文档 §4.3：时钟策略）。
//!
//! - 存储用 Unix 秒（跨语言友好，无 chrono 依赖）；
//! - 时限/停滞判定一律用单调时钟（`std::time::Instant`），容忍
//!   Windows 睡眠导致的 WSL 时钟跳变。

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// 当前 Unix 秒（墙钟，仅用于记录与展示）。
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 单调时钟此刻。
pub fn monotonic_now() -> Instant {
    Instant::now()
}

/// 单调时钟距 `since` 的秒数。
pub fn monotonic_secs(since: Instant) -> u64 {
    since.elapsed().as_secs()
}

/// Unix 秒 → UTC "YYYY-MM-DD HH:MM:SS"（纯算术，civil-from-days 算法）。
pub fn format_unix(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let (hour, min, sec) = (
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60,
    );
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{min:02}:{sec:02}")
}

/// days（自 1970-01-01 起）→ (年, 月, 日)。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 人类可读时长（秒 → "1h02m03s"）。
pub fn format_duration(secs: u64) -> String {
    let (h, rem) = (secs / 3600, secs % 3600);
    let (m, s) = (rem / 60, rem % 60);
    if h > 0 {
        format!("{h}h{m:02}m{s:02}s")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

/// 单调时钟下的剩余时长（`deadline - now`）。
pub fn monotonic_remaining(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_known_epoch() {
        // 1970-01-01 00:00:00
        assert_eq!(format_unix(0), "1970-01-01 00:00:00");
        // 2026-08-20 02:45:00 UTC = 1786824000 + ... 用逆运算验证
        let secs = 1_785_000_000;
        let s = format_unix(secs);
        assert_eq!(s.len(), 19);
        assert!(s.starts_with("2026"));
    }

    #[test]
    fn civil_from_days_known_dates() {
        // 1970-01-01 = day 0
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 2000-01-01 = day 10957
        assert_eq!(civil_from_days(10_957), (2000, 1, 1));
        // 2026-08-20 = day 20685
        assert_eq!(civil_from_days(20_685), (2026, 8, 20));
    }

    #[test]
    fn format_duration_shapes() {
        assert_eq!(format_duration(0), "0s");
        assert_eq!(format_duration(63), "1m03s");
        assert_eq!(format_duration(3723), "1h02m03s");
    }

    #[test]
    fn monotonic_now_advances() {
        let t0 = monotonic_now();
        std::thread::sleep(Duration::from_millis(5));
        assert!(t0.elapsed() >= Duration::from_millis(5));
    }
}
