// Token and cost accounting (`Session.getUsage`) and the context-overflow math (`session/overflow.ts`).
use opencode_core::config::Info;
use opencode_llm::provider::Model;
use opencode_llm::Usage;
use opencode_storage::{TokenCache, Tokens};

// Exact decimal value of a finite f64 as (mantissa, scale), from its shortest round-trip form.
fn decimal(value: f64) -> Option<(i128, u32)> {
    let text = format!("{value}");
    let (whole, fraction) = text.split_once('.').unwrap_or((&text, ""));
    let digits = format!("{whole}{fraction}");
    Some((digits.parse().ok()?, u32::try_from(fraction.len()).ok()?))
}

// sum(tokens * rate) / 1e6 in decimal arithmetic, as the original does with Decimal.js.
fn priced(terms: &[(f64, f64)]) -> f64 {
    let exact = || -> Option<f64> {
        let mut parts = Vec::new();
        for (tokens, rate) in terms {
            let (a, sa) = decimal(*tokens)?;
            let (b, sb) = decimal(*rate)?;
            parts.push((a.checked_mul(b)?, sa + sb + 6));
        }
        let scale = parts.iter().map(|(_, s)| *s).max()?;
        let mut sum = 0i128;
        for (mantissa, s) in parts {
            sum = sum.checked_add(mantissa.checked_mul(10i128.checked_pow(scale - s)?)?)?;
        }
        let digits = sum.unsigned_abs().to_string();
        let width = scale as usize;
        let padded = format!("{digits:0>width$}", width = width + 1);
        let (whole, fraction) = padded.split_at(padded.len() - width);
        let sign = if sum < 0 { "-" } else { "" };
        format!("{sign}{whole}.{fraction}").parse().ok()
    };
    exact().unwrap_or_else(|| terms.iter().map(|(tokens, rate)| tokens * rate / 1e6).sum())
}

pub fn tokens(usage: &Usage) -> Tokens {
    Tokens {
        total: Some(usage.total as f64),
        input: usage.input.saturating_sub(usage.cache_read + usage.cache_write) as f64,
        output: usage.output.saturating_sub(usage.reasoning) as f64,
        reasoning: usage.reasoning as f64,
        cache: TokenCache { read: usage.cache_read as f64, write: usage.cache_write as f64 },
    }
}

// Reasoning tokens are charged at the output rate; the price row is chosen by raw input tokens.
pub fn cost(model: &Model, usage: &Usage) -> f64 {
    let tokens = tokens(usage);
    let rates = model.cost.rates(usage.input as f64);
    let finite = |v: f64| if v.is_finite() { v } else { 0.0 };
    let value = priced(&[
        (tokens.input, finite(rates.input)),
        (tokens.output, finite(rates.output)),
        (tokens.cache.read, finite(rates.cache_read)),
        (tokens.cache.write, finite(rates.cache_write)),
        (tokens.reasoning, finite(rates.output)),
    ]);
    if value.is_finite() { value.max(0.0) } else { 0.0 }
}

// Rust build: the room kept free before automatic compaction follows opencode 2.x (`calculateCeiling` in
// `packages/core/src/session/compaction.ts`): 15% of the input window, at least 16k when the window has room for it.
// `compaction.reserved` (2.x `compaction.buffer`) overrides it when it leaves some room.
const RESERVE_MIN: u64 = 16_000;
const RESERVE_SHARE: f64 = 0.15;

// v2 `inputWindow`: the largest prompt the model takes, or 0 when unknown.
pub fn input_window(model: &Model) -> u64 {
    match (model.limit_input, model.limit_context) {
        (0, context) => context,
        (input, 0) => input,
        (input, context) => input.min(context),
    }
}

// v2 `calculateCeiling` for a known window.
pub fn ceiling(window: u64, reserved: Option<u64>) -> u64 {
    if let Some(tokens) = reserved.filter(|tokens| *tokens < window) {
        return window - tokens;
    }
    let share = (window as f64 * RESERVE_SHARE).floor() as u64;
    window - share.max(if window >= 2 * RESERVE_MIN { RESERVE_MIN } else { 0 })
}

// The largest request before automatic compaction; 0 when the window is unknown.
pub fn usable(cfg: &Info, model: &Model) -> u64 {
    match input_window(model) {
        0 => 0,
        window => ceiling(window, cfg.compaction.reserved),
    }
}

pub fn is_overflow(cfg: &Info, tokens: &Tokens, model: &Model) -> bool {
    if cfg.compaction.auto == Some(false) || input_window(model) == 0 {
        return false;
    }
    let count = match tokens.total {
        Some(total) if total != 0.0 => total,
        _ => tokens.input + tokens.output + tokens.cache.read + tokens.cache.write,
    };
    count >= usable(cfg, model) as f64
}

// `Token.estimate`: UTF-16 length / 4, rounded half up.
pub fn estimate(text: &str) -> u64 {
    let units = text.encode_utf16().count() as f64;
    (units / 4.0 + 0.5).floor().max(0.0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_sum_is_exact() {
        assert_eq!(priced(&[(1.0, 0.1), (1.0, 0.2)]), 0.0000003);
        assert_eq!(priced(&[(1_000_000.0, 3.0), (500_000.0, 15.0)]), 10.5);
        assert_eq!(priced(&[(0.0, 3.0)]), 0.0);
        assert_eq!(priced(&[(7.0, 0.075)]), 0.000000525);
    }

    #[test]
    fn headroom_is_a_share_of_the_window_with_a_floor() {
        // 15% of 200k is 30k; 15% of 64k is 9.6k, below the 16k floor.
        assert_eq!(ceiling(200_000, None), 170_000);
        assert_eq!(ceiling(64_000, None), 48_000);
        // A window under 32k keeps only the share.
        assert_eq!(ceiling(1_000, None), 850);
        // An explicit reserve wins unless it leaves no room.
        assert_eq!(ceiling(200_000, Some(20_000)), 180_000);
        assert_eq!(ceiling(10_000, Some(20_000)), 8_500);
    }

    #[test]
    fn estimate_rounds_half_up_on_utf16_units() {
        assert_eq!(estimate(""), 0);
        assert_eq!(estimate("ab"), 1);
        assert_eq!(estimate("a"), 0);
        assert_eq!(estimate("abcdef"), 2);
        assert_eq!(estimate("\u{1F600}\u{1F600}\u{1F600}"), 2);
    }
}
