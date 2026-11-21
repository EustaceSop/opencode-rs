// Error classification, context-overflow detection and the session-level retry policy.
use crate::Error;
use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;
use std::time::SystemTime;

pub const INITIAL_DELAY: u64 = 2000;
pub const BACKOFF_FACTOR: f64 = 2.0;
pub const JITTER_FACTOR: f64 = 0.25;
pub const MAX_DELAY_NO_HEADERS: u64 = 30_000;
pub const MAX_DELAY: u64 = 2_147_483_647;
pub const MAX_RETRIES: u32 = 5;
pub const GO_UPSELL_MESSAGE: &str = "Free usage exceeded, subscribe to Go";
pub const GO_UPSELL_URL: &str = "https://opencode.ai/go";

fn compile(patterns: &[&str]) -> Vec<Regex> {
    patterns.iter().map(|p| Regex::new(&format!("(?i){p}")).unwrap()).collect()
}

static OVERFLOW: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    compile(&[
        r"prompt is too long",
        r"request_too_large",
        r"input is too long for requested model",
        r"exceeds the context window",
        r"exceeds (?:the )?(?:model'?s )?maximum context length(?: of [\d,]+ tokens?|\s*\([\d,]+\))",
        r"input token count.*exceeds the maximum",
        r"tokens in request more than max tokens allowed",
        r"maximum prompt length is \d+",
        r"reduce the length of the messages",
        r"maximum context length is \d+ tokens",
        r"exceeds (?:the )?maximum allowed input length of [\d,]+ tokens?",
        r"input \(\d+ tokens\) is longer than the model'?s context length \(\d+ tokens\)",
        r"exceeds the limit of \d+",
        r"exceeds the available context size",
        r"greater than the context length",
        r"context window exceeds limit",
        r"exceeded model token limit",
        r"context[_ ]length[_ ]exceeded",
        r"request entity too large",
        r"context length is only \d+ tokens",
        r"input length.*exceeds.*context length",
        r"prompt too long; exceeded (?:max )?context length",
        r"too large for model with \d+ maximum context length",
        r"prompt has [\d,]+ tokens?, but the configured context size is [\d,]+ tokens?",
        r"model_context_window_exceeded",
        r"too many tokens",
        r"token limit exceeded",
    ])
});
static OVERFLOW_NO_BODY: LazyLock<Vec<Regex>> = LazyLock::new(|| compile(&[r"^4(00|13)\s*(status code)?\s*\(no body\)"]));
static OVERFLOW_EXCLUSIONS: LazyLock<Vec<Regex>> =
    LazyLock::new(|| compile(&[r"^(throttling error|service unavailable):", r"rate limit", r"too many requests"]));
static RETRYABLE: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    compile(&[
        r"429|500|502|503|504|524",
        r"rate increased too quickly|rate limit|rate-limit|rate_limit|too many requests",
        r"overloaded|service unavailable|service_unavailable|service-unavailable|internal error|internal_error|internal server error|server error|server_error|server-error|provider returned error|provider_returned_error|provider-returned-error",
        r"terminated|fetch failed|failed to fetch|network[-_\s]error|upstream connect|connection error|connection refused|connection lost|socket connection was closed|socket hang up|reset before headers|getaddrinfo|enotfound|eai_again|econnrefused|econnreset|etimedout",
        r"^timeout$|\b(?:request|response|connection|network|stream|read) (?:timeout|timed out|time out)\b",
        r"try your request again|retry your request|resource exhausted|resource_exhausted",
        r"\btry again (?:later|in\b)|\b(?:currently|temporarily) at capacity\b",
    ])
});
static HTML: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^\s*<!doctype|^\s*<html").unwrap());
static FLOAT_PREFIX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*[+-]?(?:\d+\.?\d*|\.\d+)(?:[eE][+-]?\d+)?").unwrap());

fn any(patterns: &[Regex], text: &str) -> bool {
    patterns.iter().any(|p| p.is_match(text))
}

pub fn is_context_overflow(message: &str) -> bool {
    !any(&OVERFLOW_EXCLUSIONS, message) && (any(&OVERFLOW, message) || any(&OVERFLOW_NO_BODY, message))
}

pub fn matches_retryable_message(message: &str) -> bool {
    any(&RETRYABLE, message)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Retry {
    // Status text shown while waiting.
    pub message: String,
    pub link: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Classified {
    pub message: String,
    pub status: Option<u16>,
    pub overflow: bool,
    // Final decision of the session retry policy.
    pub retryable: bool,
    pub retry: Option<Retry>,
    // Response headers of an HTTP-level failure; drives `delay`.
    pub headers: Option<Vec<(String, String)>>,
}

// Number.parseFloat semantics: the longest numeric prefix.
fn parse_float(text: &str) -> Option<f64> {
    FLOAT_PREFIX.find(text)?.as_str().trim().parse().ok()
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str()).filter(|v| !v.is_empty())
}

fn exponential(attempt: u32, random: f64) -> f64 {
    let base = INITIAL_DELAY as f64 * BACKOFF_FACTOR.powi(attempt as i32 - 1);
    (base + base * JITTER_FACTOR * random).ceil()
}

fn cap(ms: f64) -> u64 {
    ms.clamp(0.0, MAX_DELAY as f64) as u64
}

// Milliseconds to wait before retry number `attempt` (1-based); `random` is in [0, 1).
pub fn delay(attempt: u32, headers: Option<&[(String, String)]>, random: f64) -> u64 {
    let Some(headers) = headers else {
        return cap(exponential(attempt, random).min(MAX_DELAY_NO_HEADERS as f64));
    };
    if let Some(ms) = header(headers, "retry-after-ms").and_then(parse_float) {
        return cap(ms);
    }
    if let Some(value) = header(headers, "retry-after") {
        if let Some(seconds) = parse_float(value) {
            return cap((seconds * 1000.0).ceil());
        }
        let until = httpdate::parse_http_date(value).ok().and_then(|at| at.duration_since(SystemTime::now()).ok());
        if let Some(wait) = until.filter(|w| !w.is_zero()) {
            return cap((wait.as_secs_f64() * 1000.0).ceil());
        }
    }
    cap(exponential(attempt, random))
}

fn status_text(status: u16) -> String {
    reqwest::StatusCode::from_u16(status).ok().and_then(|s| s.canonical_reason()).map(str::to_string).unwrap_or_else(|| format!("HTTP {status}"))
}

fn status_message(status: u16, body: &str, json: Option<&Value>) -> String {
    let field = |pointer: &str| json.and_then(|j| j.pointer(pointer)).and_then(Value::as_str).filter(|s| !s.is_empty());
    if let Some(message) = field("/error/message").or(field("/message")).or(field("/error")) {
        return message.trim().to_string();
    }
    if body.trim().is_empty() {
        return status_text(status);
    }
    if HTML.is_match(body) {
        return match status {
            401 => "Unauthorized: request was blocked by a gateway or proxy. Your authentication token may be missing or expired — try running `opencode auth login <your provider URL>` to re-authenticate.".into(),
            403 => "Forbidden: request was blocked by a gateway or proxy. You may not have permission to access this resource — check your account and provider settings.".into(),
            _ => status_text(status),
        };
    }
    format!("{}: {}", status_text(status), body.trim())
}

fn reset_in(seconds: f64) -> String {
    let seconds = seconds.ceil().max(0.0) as u64;
    let (days, hours, minutes) = (seconds / 86_400, seconds % 86_400 / 3_600, (seconds % 3_600).div_ceil(60));
    let unit = |value: u64, name: &str| format!("{value} {name}{}", if value == 1 { "" } else { "s" });
    match (days, hours, minutes) {
        (1.., 1.., _) => format!("{} {}", unit(days, "day"), unit(hours, "hour")),
        (1.., 0, _) => unit(days, "day"),
        (0, 1.., 1..) => format!("{} {}", unit(hours, "hour"), unit(minutes, "minute")),
        (0, 1.., 0) => unit(hours, "hour"),
        (0, 0, 1..) => unit(minutes, "minute"),
        (0, 0, 0) => "less than a minute".into(),
    }
}

fn api_retry(message: &str, body: &str, json: Option<&Value>, headers: &[(String, String)]) -> Retry {
    if body.contains("FreeUsageLimitError") {
        return Retry { message: GO_UPSELL_MESSAGE.into(), link: Some(GO_UPSELL_URL.into()) };
    }
    if body.contains("GoUsageLimitError") {
        let meta = |key: &str| json.and_then(|j| j.pointer(&format!("/metadata/{key}"))).and_then(Value::as_str).unwrap_or_default();
        let limit = match meta("limitName") {
            "" => "Usage limit".to_string(),
            name => format!("{name} usage limit"),
        };
        let reset = header(headers, "retry-after").and_then(parse_float).map(reset_in).unwrap_or_default();
        let link = format!("https://opencode.ai/workspace/{}/go", meta("workspace"));
        let message = format!("{limit} reached. It will reset in {reset}. To continue using this model now, enable usage from your available balance");
        return Retry { message: format!("{message} - {link}"), link: Some(link) };
    }
    Retry { message: if message.contains("Overloaded") { "Provider is overloaded".into() } else { message.into() }, link: None }
}

// Errors that are not API responses retry only on recognisable transient wording.
fn other_retry(message: &str) -> Option<Retry> {
    let lower = message.to_lowercase();
    let text = if lower.contains("too_many_requests") {
        "Too Many Requests"
    } else if lower.contains("exhausted") || lower.contains("unavailable") {
        "Provider is overloaded"
    } else if matches_retryable_message(message) {
        message
    } else {
        return None;
    };
    Some(Retry { message: text.into(), link: None })
}

fn finish(message: String, status: Option<u16>, overflow: bool, retry: Option<Retry>, headers: Option<Vec<(String, String)>>) -> Classified {
    let retry = retry.filter(|_| !overflow);
    Classified { message, status, overflow, retryable: retry.is_some(), retry, headers }
}

pub fn classify(error: &Error, provider_id: &str) -> Classified {
    match error {
        Error::Status { status, body, headers } => {
            let json: Option<Value> = serde_json::from_str::<Value>(body).ok().filter(Value::is_object);
            let message = status_message(*status, body, json.as_ref());
            let code = json.as_ref().and_then(|j| j.pointer("/error/code")).and_then(Value::as_str);
            let overflow = is_context_overflow(&message) || *status == 413 || code == Some("context_length_exceeded");
            // `APICallError.isRetryable` defaults to 408/409/429/5xx; OpenAI also 404s models that do exist.
            let flagged = matches!(status, 408 | 409 | 429) || *status >= 500 || (provider_id.starts_with("openai") && *status == 404);
            let retry = (flagged || matches_retryable_message(&message) || matches_retryable_message(body)).then(|| api_retry(&message, body, json.as_ref(), headers));
            finish(message, Some(*status), overflow, retry, Some(headers.clone()))
        }
        Error::Provider { code, message } => {
            let (text, retryable) = match code.as_deref() {
                Some("insufficient_quota") => ("Quota exceeded. Check your plan and billing details.".to_string(), false),
                Some("usage_not_included") => ("To use Codex with your ChatGPT plan, upgrade to Plus: https://chatgpt.com/explore/plus.".to_string(), false),
                Some("invalid_prompt") => (message.clone(), false),
                _ => (message.clone(), true),
            };
            let overflow = code.as_deref() == Some("context_length_exceeded") || is_context_overflow(message);
            let retry = retryable.then(|| api_retry(&text, "", None, &[]));
            finish(if overflow { "Input exceeds context window of this model".into() } else { text }, None, overflow, retry, None)
        }
        Error::Stream(message) => finish(message.clone(), None, false, Some(Retry { message: message.clone(), link: None }), None),
        Error::Http(e) => {
            let message = e.to_string();
            let transport = e.is_connect() || e.is_timeout() || e.is_body() || e.is_decode();
            let retry = if transport { Some(Retry { message: message.clone(), link: None }) } else { other_retry(&message) };
            finish(message, e.status().map(|s| s.as_u16()), false, retry, None)
        }
        Error::Config(message) => finish(message.clone(), None, false, None, None),
        // Header and stream-idle timeouts retry; an elapsed total timeout is final.
        Error::Timeout { code, message } => {
            let retry = (*code != "TimeoutError").then(|| Retry { message: message.clone(), link: None });
            finish(message.clone(), None, false, retry, None)
        }
    }
}

// The wait before the next attempt, or None when the error is final or attempts are exhausted.
pub fn next(error: &Error, provider_id: &str, attempt: u32, random: f64) -> Option<(Retry, u64)> {
    let class = classify(error, provider_id);
    let retry = class.retry.filter(|_| attempt <= MAX_RETRIES)?;
    Some((retry, delay(attempt, class.headers.as_deref(), random)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(status: u16, body: &str, headers: &[(&str, &str)]) -> Error {
        Error::Status { status, body: body.into(), headers: headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect() }
    }

    #[test]
    fn overflow_patterns() {
        for message in [
            "prompt is too long: 213462 tokens > 200000 maximum",
            "This model's maximum context length is 128000 tokens",
            "Your input exceeds the context window of this model",
            "400 (no body)",
            "413 status code (no body)",
            "Request Entity Too Large",
            "context_length_exceeded",
        ] {
            assert!(is_context_overflow(message), "{message}");
        }
        for message in ["rate limit: too many tokens", "Throttling error: too many tokens", "Too Many Requests: token limit exceeded", "hello"] {
            assert!(!is_context_overflow(message), "{message}");
        }
    }

    #[test]
    fn delays_follow_headers_then_backoff() {
        let h = |pairs: &[(&str, &str)]| pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<Vec<_>>();
        assert_eq!(delay(1, None, 0.0), 2000);
        assert_eq!(delay(2, None, 1.0), 5000);
        assert_eq!(delay(9, None, 0.0), 30_000);
        assert_eq!(delay(9, Some(&h(&[])), 0.0), 512_000);
        assert_eq!(delay(1, Some(&h(&[("Retry-After-Ms", "1500.5"), ("retry-after", "9")])), 0.0), 1500);
        assert_eq!(delay(1, Some(&h(&[("retry-after", "2.5")])), 0.0), 2500);
        assert_eq!(delay(1, Some(&h(&[("retry-after", "99999999999")])), 0.0), MAX_DELAY);
        let date = httpdate::fmt_http_date(SystemTime::now() + std::time::Duration::from_secs(120));
        let wait = delay(1, Some(&h(&[("retry-after", &date)])), 0.0);
        assert!((118_000..=120_000).contains(&wait), "{wait}");
        assert_eq!(delay(3, Some(&h(&[("retry-after", "Thu, 01 Jan 1970 00:00:00 GMT")])), 0.0), 8000);
        assert_eq!(delay(3, Some(&h(&[("retry-after", "soon")])), 0.5), 9000);
    }

    #[test]
    fn classifies_http_failures() {
        let c = classify(&status(429, r#"{"error":{"message":"Rate limit reached","type":"rate_limit_error"}}"#, &[("retry-after", "7")]), "anthropic");
        assert_eq!((c.message.as_str(), c.status, c.overflow, c.retryable), ("Rate limit reached", Some(429), false, true));
        assert_eq!(next(&status(429, "{}", &[("retry-after", "7")]), "x", 1, 0.0).unwrap().1, 7000);
        assert!(next(&status(429, "{}", &[]), "x", 6, 0.0).is_none());

        let c = classify(&status(400, r#"{"error":{"message":"prompt is too long: 300000 tokens > 200000 maximum"}}"#, &[]), "anthropic");
        assert!(c.overflow && !c.retryable && c.retry.is_none());
        assert!(classify(&status(413, "", &[]), "x").overflow);
        assert!(classify(&status(400, r#"{"error":{"code":"context_length_exceeded","message":"too big"}}"#, &[]), "openai").overflow);

        let c = classify(&status(401, r#"{"error":{"message":"invalid x-api-key"}}"#, &[]), "anthropic");
        assert!(!c.retryable && !c.overflow);
        assert!(classify(&status(404, "{}", &[]), "openai").retryable);
        assert!(!classify(&status(404, "{}", &[]), "deepseek").retryable);
        assert!(classify(&status(503, "", &[]), "x").retryable);
        assert_eq!(classify(&status(503, "", &[]), "x").message, "Service Unavailable");
        assert_eq!(classify(&status(529, r#"{"error":{"message":"Overloaded"}}"#, &[]), "x").retry.unwrap().message, "Provider is overloaded");
        assert!(classify(&status(400, r#"{"message":"upstream connect error"}"#, &[]), "x").retryable);
        assert!(classify(&status(403, "<!DOCTYPE html><html></html>", &[]), "x").message.starts_with("Forbidden: request was blocked"));
        assert_eq!(classify(&status(400, "plain failure", &[]), "x").message, "Bad Request: plain failure");
    }

    #[test]
    fn go_limits_and_stream_errors() {
        let c = classify(&status(429, r#"{"type":"error","error":{"type":"FreeUsageLimitError","message":"x"}}"#, &[]), "opencode");
        assert_eq!(c.retry.unwrap(), Retry { message: GO_UPSELL_MESSAGE.into(), link: Some(GO_UPSELL_URL.into()) });
        let body = r#"{"type":"error","error":{"type":"GoUsageLimitError","message":"x"},"metadata":{"workspace":"wrk_1","limitName":"Weekly"}}"#;
        let retry = classify(&status(429, body, &[("retry-after", "93600")]), "opencode-go").retry.unwrap();
        assert_eq!(
            retry.message,
            "Weekly usage limit reached. It will reset in 1 day 2 hours. To continue using this model now, enable usage from your available balance - https://opencode.ai/workspace/wrk_1/go"
        );
        assert_eq!([reset_in(30.0), reset_in(3600.0), reset_in(3700.0), reset_in(0.0), reset_in(172_800.0)], ["1 minute", "1 hour", "1 hour 2 minutes", "less than a minute", "2 days"]);

        let quota = Error::Provider { code: Some("insufficient_quota".into()), message: "insufficient_quota: no".into() };
        let c = classify(&quota, "openai");
        assert_eq!((c.message.as_str(), c.retryable), ("Quota exceeded. Check your plan and billing details.", false));
        assert!(classify(&Error::Provider { code: None, message: "server_error: oops".into() }, "openai").retryable);
        assert!(classify(&Error::Stream("connection lost".into()), "x").retryable);
        assert!(!classify(&Error::Config("bad".into()), "x").retryable);
    }
}
