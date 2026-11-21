// AWS Signature Version 4 for header-signed requests.
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, PartialEq, Default)]
pub struct Credentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials").field("access_key_id", &self.access_key_id).finish_non_exhaustive()
    }
}

pub struct Signing<'a> {
    pub method: &'a str,
    pub url: &'a str,
    pub body: &'a [u8],
    pub region: &'a str,
    pub service: &'a str,
    // Seconds since the Unix epoch.
    pub time: u64,
}

// Headers no proxy is required to forward unchanged; they stay out of the signature.
const UNSIGNED: [&str; 10] =
    ["authorization", "content-type", "content-length", "user-agent", "presigned-expires", "expect", "x-amzn-trace-id", "range", "connection", "x-amz-user-agent"];

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn sha256(bytes: &[u8]) -> Vec<u8> {
    Sha256::digest(bytes).to_vec()
}

pub(crate) fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut block = [0u8; 64];
    if key.len() > 64 {
        block[..32].copy_from_slice(&sha256(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(block.map(|b| b ^ 0x36));
    inner.update(data);
    let mut outer = Sha256::new();
    outer.update(block.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().to_vec()
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

// "YYYYMMDDTHHMMSSZ"
pub fn timestamp(time: u64) -> String {
    let days = time / 86_400;
    let rest = time % 86_400;
    // Civil-from-days (Howard Hinnant).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z", rest / 3600, rest % 3600 / 60, rest % 60)
}

fn encode(input: &str, keep_slash: bool) -> String {
    let mut out = String::new();
    for byte in input.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') || (keep_slash && byte == b'/') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let pair = (bytes[i] == b'%').then(|| input.get(i + 1..i + 3)).flatten().and_then(|h| u8::from_str_radix(h, 16).ok());
        match pair {
            Some(byte) => {
                out.push(byte);
                i += 3;
            }
            None => {
                out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn lookup<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
}

// Adds `x-amz-date`, `x-amz-security-token` (when present) and `authorization` to `headers`.
pub fn sign(headers: &mut Vec<(String, String)>, credentials: &Credentials, input: &Signing) -> Result<(), crate::Error> {
    let url = reqwest::Url::parse(input.url).map_err(|e| crate::Error::Config(format!("SigV4 signing failed: {e}")))?;
    let host = match url.port() {
        Some(port) => format!("{}:{port}", url.host_str().unwrap_or_default()),
        None => url.host_str().unwrap_or_default().to_string(),
    };
    let stamp = timestamp(input.time);
    let date = &stamp[..8];
    headers.retain(|(k, _)| !["x-amz-date", "x-amz-security-token", "authorization"].contains(&k.to_lowercase().as_str()));
    headers.push(("x-amz-date".into(), stamp.clone()));
    if let Some(token) = &credentials.session_token {
        headers.push(("x-amz-security-token".into(), token.clone()));
    }
    let s3 = input.service == "s3";
    let payload_hash = lookup(headers, "x-amz-content-sha256").map(str::to_string).unwrap_or_else(|| hex(&sha256(input.body)));
    if s3 && lookup(headers, "x-amz-content-sha256").is_none() {
        headers.push(("x-amz-content-sha256".into(), payload_hash.clone()));
    }

    let mut signable: Vec<(String, String)> = vec![("host".into(), host)];
    for (name, value) in headers.iter() {
        let name = name.to_lowercase();
        if !UNSIGNED.contains(&name.as_str()) && name != "host" {
            signable.push((name, value.split_whitespace().collect::<Vec<_>>().join(" ")));
        }
    }
    signable.sort();
    let signed_headers = signable.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");
    let canonical_headers: String = signable.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();

    // Every service but S3 signs the already-encoded path encoded once more.
    let path = if s3 { decode(url.path()) } else { url.path().to_string() };
    let canonical_path = encode(&path, true);
    let mut query: Vec<(String, String)> = url.query_pairs().map(|(k, v)| (encode(&k, false), encode(&v, false))).collect();
    query.sort();
    let canonical_query = query.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&");

    let canonical = format!("{}\n{canonical_path}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}", input.method.to_uppercase());
    let scope = format!("{date}/{}/{}/aws4_request", input.region, input.service);
    let to_sign = format!("AWS4-HMAC-SHA256\n{stamp}\n{scope}\n{}", hex(&sha256(canonical.as_bytes())));
    let mut key = hmac(format!("AWS4{}", credentials.secret_access_key).as_bytes(), date.as_bytes());
    for part in [input.region, input.service, "aws4_request"] {
        key = hmac(&key, part.as_bytes());
    }
    let signature = hex(&hmac(&key, to_sign.as_bytes()));
    headers.push(("authorization".into(), format!("AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}", credentials.access_key_id)));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example() -> Credentials {
        Credentials { access_key_id: "AKIDEXAMPLE".into(), secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(), session_token: None }
    }

    // 2015-08-30T12:36:00Z, the instant of the published SigV4 test suite.
    const SUITE_TIME: u64 = 1_440_938_160;

    #[test]
    fn matches_the_published_test_suite() {
        assert_eq!(timestamp(SUITE_TIME), "20150830T123600Z");
        assert_eq!(timestamp(0), "19700101T000000Z");
        assert_eq!(timestamp(1_709_210_096), "20240229T123456Z");
        assert_eq!(hex(&hmac(b"key", b"The quick brown fox jumps over the lazy dog")), "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8");

        // get-vanilla
        let mut headers = Vec::new();
        sign(&mut headers, &example(), &Signing { method: "GET", url: "https://example.amazonaws.com/", body: b"", region: "us-east-1", service: "service", time: SUITE_TIME }).unwrap();
        assert_eq!(
            lookup(&headers, "authorization"),
            Some("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-date, Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31")
        );
        // post-vanilla-query
        let mut headers = Vec::new();
        sign(&mut headers, &example(), &Signing { method: "POST", url: "https://example.amazonaws.com/?Param1=value1", body: b"", region: "us-east-1", service: "service", time: SUITE_TIME }).unwrap();
        assert!(lookup(&headers, "authorization").unwrap().ends_with("Signature=28038455d6de14eafc1f9222cf5aa6f1a96197d7deb8263271d420d138af7f11"));
    }

    #[test]
    fn session_tokens_and_double_encoded_paths() {
        let mut headers = vec![("Content-Type".to_string(), "application/json".to_string()), ("User-Agent".to_string(), "x".to_string())];
        let credentials = Credentials { session_token: Some("tok".into()), ..example() };
        let url = "https://bedrock-runtime.us-east-1.amazonaws.com/model/us.amazon.nova-micro-v1%3A0/converse-stream";
        sign(&mut headers, &credentials, &Signing { method: "POST", url, body: b"{}", region: "us-east-1", service: "bedrock", time: SUITE_TIME }).unwrap();
        let auth = lookup(&headers, "authorization").unwrap();
        assert!(auth.contains("Credential=AKIDEXAMPLE/20150830/us-east-1/bedrock/aws4_request, SignedHeaders=host;x-amz-date;x-amz-security-token,"), "{auth}");
        assert_eq!((lookup(&headers, "x-amz-date"), lookup(&headers, "x-amz-security-token")), (Some("20150830T123600Z"), Some("tok")));
        assert_eq!(encode("/model/us.amazon.nova-micro-v1%3A0/converse-stream", true), "/model/us.amazon.nova-micro-v1%253A0/converse-stream");
        assert!(format!("{credentials:?}").contains("AKIDEXAMPLE") && !format!("{credentials:?}").contains("wJalr"));
    }
}
