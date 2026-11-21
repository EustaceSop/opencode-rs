// File logging as `core/src/observability/logging.ts`: one `key=value` line per event appended to `<data>/log/opencode.log`
// (no rotation in v1.19.5), mirrored to stderr under `OPENCODE_PRINT_LOGS=1`, filtered by `OPENCODE_LOG_LEVEL` (default INFO).
use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::Path;
use std::sync::Mutex;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{Layer, filter::LevelFilter};

pub const FILE: &str = "opencode.log";

struct Sink {
    file: Mutex<Option<File>>,
    stderr: bool,
    run: String,
}

// Bare when it has no whitespace, `=`, `"` or `\`; JSON-quoted otherwise.
fn format(value: &str) -> String {
    if !value.is_empty() && !value.chars().any(|c| c.is_whitespace() || matches!(c, '=' | '"' | '\\')) {
        return value.to_string();
    }
    serde_json::Value::String(value.to_string()).to_string()
}

struct Fields(String);

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        let _ = write!(self.0, " {}={}", field.name(), format(value));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let _ = write!(self.0, " {}={}", field.name(), format(&format!("{value:?}")));
    }
}

pub fn line(run: &str, level: &Level, fields: &str) -> String {
    let stamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    format!("timestamp={stamp} level={} run={run}{fields}", level.as_str())
}

impl<S: Subscriber> Layer<S> for Sink {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut fields = Fields(String::new());
        event.record(&mut fields);
        let line = line(&self.run, event.metadata().level(), &fields.0);
        if let Some(file) = self.file.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            let _ = writeln!(file, "{line}");
        }
        if self.stderr {
            eprintln!("{line}");
        }
    }
}

pub fn level() -> LevelFilter {
    match std::env::var("OPENCODE_LOG_LEVEL").unwrap_or_default().to_uppercase().as_str() {
        "DEBUG" => LevelFilter::DEBUG,
        "WARN" => LevelFilter::WARN,
        "ERROR" => LevelFilter::ERROR,
        _ => LevelFilter::INFO,
    }
}

// Eight hex digits, as the original's `crypto.randomUUID().slice(0, 8)`.
fn run_id() -> String {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let mixed = (nanos as u64 ^ ((std::process::id() as u64) << 32)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    format!("{:08x}", (mixed >> 32) as u32)
}

pub fn init(directory: &Path) {
    let file = std::fs::create_dir_all(directory).ok().and_then(|()| OpenOptions::new().create(true).append(true).open(directory.join(FILE)).ok());
    let sink = Sink { file: Mutex::new(file), stderr: std::env::var("OPENCODE_PRINT_LOGS").as_deref() == Ok("1"), run: run_id() };
    let _ = tracing_subscriber::registry().with(sink.with_filter(level())).try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_are_key_value_pairs() {
        assert_eq!((format("plain"), format("two words"), format("a=b"), format("")), ("plain".into(), "\"two words\"".into(), "\"a=b\"".into(), "\"\"".into()));
        assert_eq!(format("say \"hi\""), r#""say \"hi\"""#);
        let line = line("ab12cd34", &Level::WARN, " message=\"it broke\" code=7");
        let (stamp, rest) = line.split_once(' ').unwrap();
        assert_eq!(rest, "level=WARN run=ab12cd34 message=\"it broke\" code=7");
        let stamp = stamp.strip_prefix("timestamp=").unwrap();
        assert!(stamp.ends_with('Z') && chrono::DateTime::parse_from_rfc3339(stamp).is_ok(), "{stamp}");
        let id = run_id();
        assert!(id.len() == 8 && id.chars().all(|c| c.is_ascii_hexdigit()), "{id}");
    }
}
