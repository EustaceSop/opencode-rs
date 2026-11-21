// Deterministic Rust ids for imported rows, so a re-import finds the rows it wrote before.
use sha1::{Digest, Sha1};

const CHARS: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
const MASK: u64 = 0xFFFF_FFFF_FFFF;

fn digest(parts: &[&str]) -> [u8; 20] {
    let mut hasher = Sha1::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    hasher.finalize().into()
}

fn suffix(bytes: &[u8]) -> String {
    bytes.iter().take(14).map(|byte| CHARS[usize::from(byte % 62)] as char).collect()
}

// `ses_<12 hex><14 base62>`, all derived from the source and its session id.
pub fn session(source: &str, key: &str) -> String {
    let hash = digest(&["session", source, key]);
    let hex: String = hash[..6].iter().map(|b| format!("{b:02x}")).collect();
    format!("ses_{hex}{}", suffix(&hash[6..]))
}

// Ascending ids: the 12 hex digits carry `value` (`ms * 0x1000 + counter`, as `opencode_storage::id` encodes time).
pub fn ascending(prefix: &str, value: u64, seed: &[&str]) -> String {
    format!("{prefix}_{:012x}{}", value & MASK, suffix(&digest(seed)))
}

// Strictly increasing id values that follow the timestamps where they allow it.
#[derive(Debug, Default)]
pub struct Clock {
    last: u64,
}

impl Clock {
    pub fn next(&mut self, ms: u64) -> u64 {
        let value = (ms.saturating_mul(0x1000)).max(self.last + 1);
        self.last = value;
        value
    }
}

pub fn part_value(message_ms: u64, index: usize) -> u64 {
    message_ms.saturating_mul(0x1000) + index as u64 + 1
}

// RFC 3339 / ISO 8601 text to epoch milliseconds.
pub fn iso_ms(text: &str) -> Option<u64> {
    let parsed = chrono::DateTime::parse_from_rfc3339(text.trim()).ok()?;
    u64::try_from(parsed.timestamp_millis()).ok()
}

pub fn file_mtime_ms(path: &std::path::Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_stable_and_shaped() {
        let a = session("claude-code", "abc");
        assert_eq!(a, session("claude-code", "abc"));
        assert_ne!(a, session("codex", "abc"));
        let (head, body) = a.split_once('_').unwrap();
        assert_eq!((head, body.len()), ("ses", 26));
        let mut clock = Clock::default();
        let first = clock.next(1_700_000_000_000);
        let same = clock.next(1_700_000_000_000);
        let earlier = clock.next(1_600_000_000_000);
        assert!(first < same && same < earlier);
        let m1 = ascending("msg", first, &["x", "1"]);
        let m2 = ascending("msg", same, &["x", "2"]);
        assert!(m1 < m2 && m1.starts_with("msg_bcfe56800000"));
        assert_eq!(opencode_storage::id::timestamp(&m1), Some(1_700_000_000_000 % (1 << 36)));
        assert_eq!(iso_ms("2024-05-06T07:08:09.123Z"), Some(1_714_979_289_123));
        assert_eq!(iso_ms("2024-05-06T09:08:09+02:00"), Some(1_714_979_289_000));
        assert_eq!(iso_ms("nope"), None);
    }
}
