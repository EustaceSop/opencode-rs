// Rust build: `/stats` after opencode 2.x (`packages/tui/src/feature-plugins/system/stats.tsx`, `stats-data.ts`):
// tokens, best streak, active days and sessions since January 1st, local time. The 2.x poster's activity calendar
// and logo are left out; a day counts as active when an answer was written on it.
use chrono::{Datelike, Local, NaiveDate, TimeZone};
use opencode_storage::{Message, Store};
use std::collections::BTreeSet;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Stats {
    pub tokens: f64,
    pub streak: usize,
    pub active_days: usize,
    pub sessions: usize,
    pub from: Option<NaiveDate>,
    pub to: Option<NaiveDate>,
}

fn day(ms: u64) -> Option<NaiveDate> {
    Local.timestamp_millis_opt(ms as i64).single().map(|time| time.date_naive())
}

// The longest run of consecutive days.
fn streak(days: &BTreeSet<NaiveDate>) -> usize {
    let (mut best, mut run, mut previous) = (0, 0, None::<NaiveDate>);
    for day in days {
        run = if previous.is_some_and(|p| p.succ_opt() == Some(*day)) { run + 1 } else { 1 };
        best = best.max(run);
        previous = Some(*day);
    }
    best
}

pub fn collect(store: &Store, now_ms: u64) -> anyhow::Result<Stats> {
    let today = day(now_ms).unwrap_or_default();
    let from = NaiveDate::from_ymd_opt(today.year(), 1, 1).unwrap_or(today);
    let start = Local.from_local_datetime(&from.and_hms_opt(0, 0, 0).unwrap_or_default()).earliest().map_or(0, |t| t.timestamp_millis().max(0) as u64);
    let mut stats = Stats { from: Some(from), to: Some(today), ..Default::default() };
    let mut days = BTreeSet::new();
    for project in store.list_projects()? {
        for session in store.list_sessions(&project.id)? {
            if session.time.created >= start && session.parent_id.is_none() {
                stats.sessions += 1;
            }
            for (message, _) in store.list_messages(&session.id)? {
                let Message::Assistant(answer) = message else { continue };
                if answer.time.created < start {
                    continue;
                }
                let t = &answer.tokens;
                stats.tokens += t.input + t.output + t.reasoning + t.cache.read + t.cache.write;
                days.extend(day(answer.time.created));
            }
        }
    }
    stats.active_days = days.len();
    stats.streak = streak(&days);
    Ok(stats)
}

// `Intl.NumberFormat("en-US", { notation: "compact", maximumFractionDigits: 1 })`.
pub fn compact(value: f64) -> String {
    let units = [(1e12, "T"), (1e9, "B"), (1e6, "M"), (1e3, "K")];
    for (size, suffix) in units {
        if value.abs() >= size {
            let scaled = (value / size * 10.0).round() / 10.0;
            return format!("{}{suffix}", trim(scaled));
        }
    }
    trim((value * 10.0).round() / 10.0)
}

fn trim(value: f64) -> String {
    let text = format!("{value:.1}");
    text.strip_suffix(".0").map_or(text.clone(), str::to_string)
}

pub fn text(stats: &Stats) -> String {
    let range = match (stats.from, stats.to) {
        (Some(from), Some(to)) if from.year() == to.year() && from.month() == to.month() => from.format("%b %Y").to_string(),
        (Some(from), Some(to)) if from.year() == to.year() => format!("{} – {}", from.format("%b"), to.format("%b %Y")),
        (Some(from), Some(to)) => format!("{} – {}", from.format("%b %Y"), to.format("%b %Y")),
        _ => String::new(),
    };
    [
        range,
        String::new(),
        format!("{} TOKENS", compact(stats.tokens)),
        String::new(),
        format!("{} days  best streak", compact(stats.streak as f64)),
        format!("{}  active days", compact(stats.active_days as f64)),
        format!("{}  sessions", compact(stats.sessions as f64)),
    ]
    .join("\n")
}

pub fn render(store: &Store) -> anyhow::Result<String> {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
    Ok(text(&collect(store, now)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_and_streaks() {
        assert_eq!((compact(0.0), compact(999.0), compact(1_234.0), compact(1_250_000.0), compact(3e9)), ("0".into(), "999".into(), "1.2K".into(), "1.3M".into(), "3B".into()));
        let d = |m, day| NaiveDate::from_ymd_opt(2026, m, day).unwrap();
        let days: BTreeSet<NaiveDate> = [d(1, 30), d(1, 31), d(2, 1), d(2, 5), d(2, 6)].into_iter().collect();
        assert_eq!(streak(&days), 3);
        assert_eq!(streak(&BTreeSet::new()), 0);
        let shown = text(&Stats { tokens: 1_500.0, streak: 3, active_days: 5, sessions: 2, from: Some(d(1, 1)), to: Some(d(11, 11)) });
        assert_eq!(shown, "Jan – Nov 2026\n\n1.5K TOKENS\n\n3 days  best streak\n5  active days\n2  sessions");
    }

    #[test]
    fn counts_this_year_from_the_store() {
        let store = Store::open_in_memory().unwrap();
        let stats = collect(&store, 1_800_000_000_000).unwrap();
        assert_eq!((stats.tokens, stats.sessions, stats.active_days, stats.streak), (0.0, 0, 0, 0));
    }
}
