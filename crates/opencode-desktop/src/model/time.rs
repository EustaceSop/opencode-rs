const MINUTE: u64 = 60_000;
const HOUR: u64 = 60 * MINUTE;
const DAY: u64 = 24 * HOUR;

// Compact age label for the session list.
pub fn relative(now_ms: u64, then_ms: u64) -> String {
    let age = now_ms.saturating_sub(then_ms);
    match age {
        a if a < MINUTE => "now".to_string(),
        a if a < HOUR => format!("{}m", a / MINUTE),
        a if a < DAY => format!("{}h", a / HOUR),
        a if a < 7 * DAY => format!("{}d", a / DAY),
        a if a < 52 * 7 * DAY => format!("{}w", a / (7 * DAY)),
        a => format!("{}y", a / (365 * DAY)),
    }
}

pub fn duration(ms: u64) -> String {
    match ms {
        ms if ms < 1000 => format!("{ms}ms"),
        ms if ms < MINUTE => format!("{:.1}s", ms as f64 / 1000.0),
        ms => format!("{}m {}s", ms / MINUTE, ms % MINUTE / 1000),
    }
}

// Token counts as the header shows them: 950, 4.7k, 1.2M.
pub fn compact_count(count: u64) -> String {
    match count {
        n if n < 1_000 => n.to_string(),
        n if n < 999_950 => format!("{:.1}k", n as f64 / 1_000.0),
        n => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_counts() {
        assert_eq!(compact_count(950), "950");
        assert_eq!(compact_count(4_700), "4.7k");
        assert_eq!(compact_count(999_949), "999.9k");
        assert_eq!(compact_count(999_950), "1.0M");
        assert_eq!(compact_count(1_300_000), "1.3M");
    }

    #[test]
    fn relative_buckets() {
        let now = 1_000 * DAY;
        assert_eq!(relative(now, now + 5), "now");
        assert_eq!(relative(now, now - 59_000), "now");
        assert_eq!(relative(now, now - 5 * MINUTE), "5m");
        assert_eq!(relative(now, now - 3 * HOUR), "3h");
        assert_eq!(relative(now, now - 2 * DAY), "2d");
        assert_eq!(relative(now, now - 20 * DAY), "2w");
        assert_eq!(relative(now, now - 800 * DAY), "2y");
    }

    #[test]
    fn durations() {
        assert_eq!(duration(40), "40ms");
        assert_eq!(duration(1_500), "1.5s");
        assert_eq!(duration(125_000), "2m 5s");
    }
}
