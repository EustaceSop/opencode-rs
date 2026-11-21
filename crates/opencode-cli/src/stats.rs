// `opencode stats` (`opencode/src/cli/cmd/stats.ts`).
use anyhow::Result;
use chrono::{Local, TimeZone};
use indexmap::IndexMap;
use opencode_core::paths::Paths;
use opencode_session::Engine;
use opencode_storage::{Message, PartBody, Session};

const MS_IN_DAY: f64 = 24.0 * 60.0 * 60.0 * 1000.0;
const WIDTH: usize = 56;

#[derive(Default, Clone)]
pub struct ModelUsage {
    pub messages: u64,
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub cost: f64,
}

#[derive(Default)]
pub struct Stats {
    pub sessions: usize,
    pub messages: usize,
    pub cost: f64,
    pub input: f64,
    pub output: f64,
    pub reasoning: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub tools: IndexMap<String, u64>,
    pub models: IndexMap<String, ModelUsage>,
    pub days: f64,
    pub cost_per_day: f64,
    pub tokens_per_session: f64,
    pub median_tokens_per_session: f64,
}

pub struct Filter {
    pub days: Option<f64>,
    // `Some("")` is the current project.
    pub project: Option<String>,
}

fn now_ms() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_millis() as f64)
}

fn midnight_ms() -> f64 {
    let start = Local::now().date_naive().and_hms_opt(0, 0, 0).and_then(|naive| Local.from_local_datetime(&naive).earliest());
    start.map_or_else(now_ms, |date| date.timestamp_millis() as f64)
}

pub fn aggregate(engine: &Engine, filter: &Filter, current_project: &str) -> Result<Stats> {
    let store = &engine.store;
    let mut sessions: Vec<Session> = Vec::new();
    for project in store.list_projects()? {
        sessions.extend(store.list_sessions(&project.id)?);
    }
    let cutoff = match filter.days {
        None => 0.0,
        Some(0.0) => midnight_ms(),
        Some(days) => now_ms() - days * MS_IN_DAY,
    };
    let window = filter.days.map(|days| if days == 0.0 { 1.0 } else { days });
    if cutoff > 0.0 {
        sessions.retain(|session| session.time.updated as f64 >= cutoff);
    }
    if let Some(project) = &filter.project {
        let wanted = if project.is_empty() { current_project } else { project.as_str() };
        sessions.retain(|session| session.project_id == wanted);
    }

    let mut stats = Stats { sessions: sessions.len(), ..Default::default() };
    if sessions.len() > 1000 {
        println!("Large dataset detected ({} sessions). This may take a while...", sessions.len());
    }
    if sessions.is_empty() {
        stats.days = window.unwrap_or(0.0);
        return Ok(stats);
    }

    let (mut earliest, mut latest) = (now_ms(), 0.0_f64);
    let mut totals: Vec<f64> = Vec::new();
    for session in &sessions {
        let messages = store.list_messages(&session.id)?;
        stats.messages += messages.len();
        for (message, parts) in &messages {
            if let Message::Assistant(info) = message {
                let usage = stats.models.entry(format!("{}/{}", info.provider_id, info.model_id)).or_default();
                usage.messages += 1;
                usage.cost += info.cost;
                usage.input += info.tokens.input;
                usage.output += info.tokens.output + info.tokens.reasoning;
                usage.cache_read += info.tokens.cache.read;
                usage.cache_write += info.tokens.cache.write;
            }
            for part in parts {
                if let PartBody::Tool(tool) = &part.body
                    && !tool.tool.is_empty()
                {
                    *stats.tools.entry(tool.tool.clone()).or_default() += 1;
                }
            }
        }
        stats.cost += session.cost.unwrap_or(0.0);
        let mut total = 0.0;
        if let Some(tokens) = &session.tokens {
            stats.input += tokens.input;
            stats.output += tokens.output;
            stats.reasoning += tokens.reasoning;
            stats.cache_read += tokens.cache.read;
            stats.cache_write += tokens.cache.write;
            total = tokens.input + tokens.output + tokens.reasoning + tokens.cache.read + tokens.cache.write;
        }
        totals.push(total);
        earliest = earliest.min(if cutoff > 0.0 { session.time.updated } else { session.time.created } as f64);
        latest = latest.max(session.time.updated as f64);
    }

    let range = ((latest - earliest) / MS_IN_DAY).ceil().max(1.0);
    stats.days = window.unwrap_or(range);
    stats.cost_per_day = stats.cost / stats.days;
    stats.tokens_per_session = totals.iter().sum::<f64>() / sessions.len() as f64;
    totals.sort_by(f64::total_cmp);
    let mid = totals.len() / 2;
    stats.median_tokens_per_session = if totals.len().is_multiple_of(2) { (totals[mid - 1] + totals[mid]) / 2.0 } else { totals[mid] };
    Ok(stats)
}

// `Number.prototype.toLocaleString()` for en-US integers.
fn grouped(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

// `Number.prototype.toFixed` for non-negative values: an exact tie rounds up, where Rust's formatter rounds to even.
pub fn to_fixed(value: f64, digits: usize) -> String {
    if !value.is_finite() || value < 0.0 {
        return format!("{value:.digits$}");
    }
    // 120 fractional digits hold the exact expansion of any double this code formats.
    let exact = format!("{value:.120}");
    let (whole, fraction) = exact.split_once('.').unwrap_or((&exact, ""));
    let mut kept: Vec<u8> = whole.bytes().chain(fraction.bytes().take(digits)).map(|b| b - b'0').collect();
    if fraction.as_bytes().get(digits).is_some_and(|next| *next >= b'5') {
        let mut at = kept.len();
        loop {
            if at == 0 {
                kept.insert(0, 1);
                break;
            }
            at -= 1;
            if kept[at] == 9 {
                kept[at] = 0;
            } else {
                kept[at] += 1;
                break;
            }
        }
    }
    let text: String = kept.iter().map(|d| (b'0' + d) as char).collect();
    let split = text.len() - digits;
    if digits == 0 { text } else { format!("{}.{}", &text[..split], &text[split..]) }
}

fn number(value: f64) -> String {
    if value >= 1_000_000.0 {
        format!("{}M", to_fixed(value / 1_000_000.0, 1))
    } else if value >= 1000.0 {
        format!("{}K", to_fixed(value / 1000.0, 1))
    } else {
        number_plain(value)
    }
}

fn row(label: &str, value: &str) -> String {
    let padding = (WIDTH - 1).saturating_sub(label.chars().count() + value.chars().count());
    format!("│{label}{}{value} │", " ".repeat(padding))
}

fn clean(value: f64) -> f64 {
    if value.is_nan() { 0.0 } else { value }
}

const TOP: &str = "┌────────────────────────────────────────────────────────┐";
const MIDDLE: &str = "├────────────────────────────────────────────────────────┤";
const BOTTOM: &str = "└────────────────────────────────────────────────────────┘";

// `models`: `None` hides the section, `Some(usize::MAX)` shows every model.
pub fn render(stats: &Stats, tool_limit: Option<usize>, model_limit: Option<usize>) -> String {
    let mut out: Vec<String> = Vec::new();
    out.extend([TOP.into(), "│                       OVERVIEW                         │".into(), MIDDLE.into()]);
    out.push(row("Sessions", &grouped(stats.sessions as u64)));
    out.push(row("Messages", &grouped(stats.messages as u64)));
    out.push(row("Days", &number_plain(stats.days)));
    out.extend([BOTTOM.into(), String::new()]);

    out.extend([TOP.into(), "│                    COST & TOKENS                       │".into(), MIDDLE.into()]);
    out.push(row("Total Cost", &format!("${}", to_fixed(clean(stats.cost), 2))));
    out.push(row("Avg Cost/Day", &format!("${}", to_fixed(clean(stats.cost_per_day), 2))));
    out.push(row("Avg Tokens/Session", &number(clean(stats.tokens_per_session).round())));
    out.push(row("Median Tokens/Session", &number(clean(stats.median_tokens_per_session).round())));
    out.push(row("Input", &number(stats.input)));
    out.push(row("Output", &number(stats.output)));
    out.push(row("Cache Read", &number(stats.cache_read)));
    out.push(row("Cache Write", &number(stats.cache_write)));
    out.extend([BOTTOM.into(), String::new()]);

    if let Some(limit) = model_limit
        && !stats.models.is_empty()
    {
        let mut models: Vec<(&String, &ModelUsage)> = stats.models.iter().collect();
        models.sort_by_key(|(_, usage)| std::cmp::Reverse(usage.messages));
        out.extend([TOP.into(), "│                      MODEL USAGE                       │".into(), MIDDLE.into()]);
        for (model, usage) in models.into_iter().take(limit) {
            out.push(format!("│ {model}{} │", " ".repeat(54usize.saturating_sub(model.chars().count()))));
            out.push(row("  Messages", &grouped(usage.messages)));
            out.push(row("  Input Tokens", &number(usage.input)));
            out.push(row("  Output Tokens", &number(usage.output)));
            out.push(row("  Cache Read", &number(usage.cache_read)));
            out.push(row("  Cache Write", &number(usage.cache_write)));
            out.push(row("  Cost", &format!("${}", to_fixed(usage.cost, 4))));
            out.push(MIDDLE.into());
        }
        // The original moves the cursor up over the last separator; here it is replaced outright.
        out.pop();
        out.push(BOTTOM.into());
    }
    out.push(String::new());

    if !stats.tools.is_empty() {
        let mut tools: Vec<(&String, &u64)> = stats.tools.iter().collect();
        tools.sort_by_key(|(_, count)| std::cmp::Reverse(**count));
        let total: u64 = stats.tools.values().sum();
        let shown: Vec<_> = match tool_limit {
            Some(limit) if limit > 0 => tools.into_iter().take(limit).collect(),
            _ => tools,
        };
        let max = shown.iter().map(|(_, count)| **count).max().unwrap_or(1);
        out.extend([TOP.into(), "│                      TOOL USAGE                        │".into(), MIDDLE.into()]);
        for (tool, count) in shown {
            let bar = "█".repeat((((*count as f64 / max as f64) * 20.0).floor() as usize).max(1));
            let percentage = to_fixed((*count as f64 / total as f64) * 100.0, 1);
            let name: String = if tool.chars().count() > 18 { format!("{}..", tool.chars().take(16).collect::<String>()) } else { tool.clone() };
            let content = format!(" {name:<18} {bar:<20} {count:>3} ({percentage:>4}%)");
            let padding = WIDTH.saturating_sub(content.chars().count() + 1);
            out.push(format!("│{content}{} │", " ".repeat(padding)));
        }
        out.push(BOTTOM.into());
    }
    out.push(String::new());
    out.join("\n")
}

fn number_plain(value: f64) -> String {
    if value.fract() == 0.0 { format!("{}", value as i64) } else { value.to_string() }
}

pub fn run(paths: &Paths, days: Option<f64>, tools: Option<usize>, models: Option<usize>, project: Option<String>) -> Result<i32> {
    let engine = Engine::new(paths.clone())?;
    let current = opencode_storage::identity::resolve(&std::env::current_dir()?).id;
    let stats = aggregate(&engine, &Filter { days, project }, &current)?;
    println!("{}", render(&stats, tools, models));
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_and_rows() {
        assert_eq!((number(999.0), number(1000.0), number(1_250_000.0), number(12_345.0)), ("999".into(), "1.0K".into(), "1.3M".into(), "12.3K".into()));
        assert_eq!((grouped(0), grouped(999), grouped(1000), grouped(1_234_567)), ("0".into(), "999".into(), "1,000".into(), "1,234,567".into()));
        assert_eq!((to_fixed(1.25, 1), to_fixed(0.125, 2), to_fixed(2.5, 0), to_fixed(1.005, 2), to_fixed(9.96, 1), to_fixed(0.0, 2)), ("1.3".into(), "0.13".into(), "3".into(), "1.00".into(), "10.0".into(), "0.00".into()));
        assert_eq!(row("Sessions", "12"), format!("│Sessions{}12 │", " ".repeat(45)));
        assert_eq!(row("Sessions", "12").chars().count(), 58);
    }

    #[test]
    fn tables_follow_the_limits() {
        let mut stats = Stats { sessions: 1234, messages: 5, cost: 1.005, days: 2.0, cost_per_day: 0.5025, tokens_per_session: 1500.4, input: 2_000_000.0, ..Default::default() };
        for (name, messages) in [("a/one", 1), ("b/two", 9)] {
            stats.models.insert(name.into(), ModelUsage { messages, cost: 0.12345, ..Default::default() });
        }
        for (name, count) in [("read", 3), ("a_tool_with_a_very_long_name", 12), ("bash", 1)] {
            stats.tools.insert(name.into(), count);
        }
        let all = render(&stats, None, Some(usize::MAX));
        let lines: Vec<&str> = all.lines().collect();
        assert!(lines.contains(&row("Sessions", "1,234").as_str()) && lines.contains(&row("Avg Tokens/Session", "1.5K").as_str()) && lines.contains(&row("Input", "2.0M").as_str()));
        assert!(lines.contains(&row("Avg Cost/Day", "$0.50").as_str()) && lines.contains(&row("  Cost", "$0.1235").as_str()), "{all}");
        // Most used first; the last model block closes the table instead of a separator.
        let (first, second) = (all.find("│ b/two").unwrap(), all.find("│ a/one").unwrap());
        assert!(first < second && all[second..].find(MIDDLE).is_none_or(|next| next > all[second..].find(BOTTOM).unwrap()));
        let tools: Vec<&str> = lines.iter().copied().filter(|line| line.contains('█')).collect();
        assert_eq!(tools.len(), 3);
        assert_eq!(tools[0], format!("│ a_tool_with_a_ve.. {}  12 (75.0%)    │", "█".repeat(20)));
        assert_eq!(tools[1], format!("│ read{} {}{}   3 (18.8%)    │", " ".repeat(14), "█".repeat(5), " ".repeat(15)));
        assert_eq!(tools[2], format!("│ bash{} █{}   1 ( 6.3%)    │", " ".repeat(14), " ".repeat(19)));
        assert!(lines.iter().filter(|line| !line.is_empty()).all(|line| line.chars().count() == 58), "{all}");

        let limited = render(&stats, Some(1), Some(1));
        assert!(limited.contains("b/two") && !limited.contains("a/one") && limited.matches('█').count() == 20, "{limited}");
        let hidden = render(&stats, None, None);
        assert!(!hidden.contains("MODEL USAGE") && hidden.contains("TOOL USAGE") && hidden.ends_with("┘\n"));
        assert!(!render(&Stats::default(), None, Some(3)).contains("USAGE"));
    }
}
