use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const CHARS: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
const MASK: u128 = 0xFFFF_FFFF_FFFF;
const RANDOM_LENGTH: usize = 14;

static STATE: Mutex<(u64, u64)> = Mutex::new((0, 0));

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("ID {id} does not start with {prefix}")]
pub struct IdError {
    pub id: String,
    pub prefix: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Prefix {
    Session,
    Message,
    Part,
    Event,
    Permission,
    PermissionSaved,
    Question,
    Pty,
    Workspace,
    Credential,
    Connection,
    Job,
    Tool,
}

impl Prefix {
    pub const fn as_str(self) -> &'static str {
        match self {
            Prefix::Session => "ses",
            Prefix::Message => "msg",
            Prefix::Part => "prt",
            Prefix::Event => "evt",
            Prefix::Permission => "per",
            Prefix::PermissionSaved => "psv",
            Prefix::Question => "que",
            Prefix::Pty => "pty",
            Prefix::Workspace => "wrk",
            Prefix::Credential => "cred",
            Prefix::Connection => "con",
            Prefix::Job => "job",
            Prefix::Tool => "tool",
        }
    }

    pub const fn is_descending(self) -> bool {
        matches!(self, Prefix::Session)
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

pub fn time_hex(timestamp: u64, counter: u64, descending: bool) -> String {
    let current = u128::from(timestamp) * 0x1000 + u128::from(counter);
    let value = if descending { !current } else { current } & MASK;
    format!("{value:012x}")
}

fn random_suffix() -> String {
    let mut bytes = [0u8; RANDOM_LENGTH];
    getrandom::fill(&mut bytes).expect("system random source unavailable");
    bytes
        .iter()
        .map(|byte| CHARS[usize::from(byte % 62)] as char)
        .collect()
}

pub fn create(prefix: &str, descending: bool, timestamp: Option<u64>) -> String {
    let time = {
        let mut state = STATE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let timestamp = timestamp.unwrap_or_else(now_ms);
        if timestamp != state.0 {
            *state = (timestamp, 0);
        }
        state.1 += 1;
        time_hex(timestamp, state.1, descending)
    };
    format!("{prefix}_{time}{}", random_suffix())
}

pub fn ascending(prefix: Prefix) -> String {
    create(prefix.as_str(), false, None)
}

pub fn descending(prefix: Prefix) -> String {
    create(prefix.as_str(), true, None)
}

pub fn generate(prefix: Prefix) -> String {
    create(prefix.as_str(), prefix.is_descending(), None)
}

pub fn check(prefix: Prefix, id: &str) -> Result<(), IdError> {
    if id.starts_with(prefix.as_str()) {
        return Ok(());
    }
    Err(IdError {
        id: id.to_string(),
        prefix: prefix.as_str(),
    })
}

pub fn generate_or(prefix: Prefix, given: Option<String>) -> Result<String, IdError> {
    match given {
        Some(id) => check(prefix, &id).map(|()| id),
        None => Ok(generate(prefix)),
    }
}

pub fn session() -> String {
    generate(Prefix::Session)
}

pub fn message() -> String {
    generate(Prefix::Message)
}

pub fn part() -> String {
    generate(Prefix::Part)
}

// Ascending IDs only; yields the timestamp modulo 2^36 ms, as the original does.
pub fn timestamp(id: &str) -> Option<u64> {
    let (_, body) = id.split_once('_')?;
    let encoded = u64::from_str_radix(body.get(..12)?, 16).ok()?;
    Some(encoded / 0x1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    static CLOCK: Mutex<()> = Mutex::new(());

    fn clock() -> std::sync::MutexGuard<'static, ()> {
        CLOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn time_field_matches_reference_implementation() {
        assert_eq!(time_hex(1_700_000_000_000, 1, false), "bcfe56800001");
        assert_eq!(time_hex(1_700_000_000_000, 1, true), "4301a97ffffe");
        assert_eq!(time_hex(1_700_000_000_000, 2, false), "bcfe56800002");
        assert_eq!(time_hex(1_700_000_000_001, 1, false), "bcfe56801001");
        assert_eq!(time_hex(1_778_000_000_000, 1, false), "df90ef400001");
        assert_eq!(time_hex(1_778_000_000_000, 1, true), "206f10bffffe");
    }

    #[test]
    fn shape_is_prefix_hex12_base62x14() {
        for prefix in [
            Prefix::Session,
            Prefix::Message,
            Prefix::Part,
            Prefix::Credential,
            Prefix::Tool,
        ] {
            let id = generate(prefix);
            let (head, body) = id.split_once('_').unwrap();
            assert_eq!(head, prefix.as_str());
            assert_eq!(body.len(), 26);
            assert!(
                body[..12]
                    .bytes()
                    .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
            );
            assert!(body[12..].bytes().all(|byte| byte.is_ascii_alphanumeric()));
        }
    }

    #[test]
    fn prefixes_match_table() {
        let table = [
            (Prefix::Session, "ses"),
            (Prefix::Message, "msg"),
            (Prefix::Part, "prt"),
            (Prefix::Event, "evt"),
            (Prefix::Permission, "per"),
            (Prefix::PermissionSaved, "psv"),
            (Prefix::Question, "que"),
            (Prefix::Pty, "pty"),
            (Prefix::Workspace, "wrk"),
            (Prefix::Credential, "cred"),
            (Prefix::Connection, "con"),
            (Prefix::Job, "job"),
            (Prefix::Tool, "tool"),
        ];
        for (prefix, expected) in table {
            assert_eq!(prefix.as_str(), expected);
            assert_eq!(prefix.is_descending(), prefix == Prefix::Session);
        }
    }

    #[test]
    fn ascending_ids_sort_in_creation_order() {
        let _guard = clock();
        let ids: Vec<String> = (0..1000).map(|_| message()).collect();
        assert!(ids.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn descending_ids_sort_newest_first() {
        let _guard = clock();
        let ids: Vec<String> = (0..1000).map(|_| session()).collect();
        assert!(ids.windows(2).all(|pair| pair[0] > pair[1]));
    }

    #[test]
    fn explicit_timestamps_order_across_milliseconds() {
        let _guard = clock();
        let base = 1_690_000_000_000;
        let older = create("msg", false, Some(base));
        let newer = create("msg", false, Some(base + 1));
        assert!(older < newer);
        assert_eq!(timestamp(&older), Some(base % (1 << 36)));
        let old_session = create("ses", true, Some(base + 2));
        let new_session = create("ses", true, Some(base + 3));
        assert!(new_session < old_session);
    }

    #[test]
    fn given_ids_must_carry_the_prefix() {
        assert_eq!(
            generate_or(Prefix::Message, Some("msg_custom".into())).unwrap(),
            "msg_custom"
        );
        let error = generate_or(Prefix::Message, Some("ses_custom".into())).unwrap_err();
        assert_eq!(error.to_string(), "ID ses_custom does not start with msg");
        assert!(generate_or(Prefix::Part, None).unwrap().starts_with("prt_"));
    }
}
