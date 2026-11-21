// The embedded web UI, after `opencode/src/server/shared/ui.ts` and `public-ui.ts`.
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use sha2::{Digest, Sha256};

mod generated {
    include!(concat!(env!("OUT_DIR"), "/web_ui.rs"));
}

// Fetched by the browser without app credentials (manifest link and its icons).
const PUBLIC_UI_PATHS: [&str; 3] = ["/site.webmanifest", "/web-app-manifest-192x192.png", "/web-app-manifest-512x512.png"];

pub fn is_public_path(method: &axum::http::Method, path: &str) -> bool {
    method == axum::http::Method::GET && PUBLIC_UI_PATHS.contains(&path)
}

fn disabled() -> bool {
    std::env::var("OPENCODE_DISABLE_EMBEDDED_WEB_UI").is_ok_and(|v| matches!(v.to_lowercase().as_str(), "true" | "1"))
}

pub fn available() -> bool {
    !disabled() && !generated::FILES.is_empty()
}

fn lookup(name: &str) -> Option<(&'static str, &'static [u8])> {
    generated::FILES.iter().find(|(file, _)| *file == name).copied()
}

pub fn csp(hash: Option<&str>) -> String {
    let script = hash.map_or_else(String::new, |h| format!(" 'sha256-{h}'"));
    format!(
        "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'{script}; style-src 'self' 'unsafe-inline'; img-src 'self' data: https: blob:; font-src 'self' data:; media-src 'self' data:; connect-src * data: blob:"
    )
}

// Body of the inline `<script id="oc-theme-preload-script">`, whose hash the CSP allows.
fn theme_preload(body: &str) -> Option<&str> {
    let lower = body.to_ascii_lowercase();
    let mut from = 0;
    while let Some(start) = lower[from..].find("<script").map(|i| i + from) {
        let open_end = lower[start..].find('>')? + start;
        let tag = &lower[start..open_end];
        let close = lower[open_end..].find("</script>")? + open_end;
        let has_id = tag.contains("id=\"oc-theme-preload-script\"") || tag.contains("id='oc-theme-preload-script'");
        if has_id && !tag.contains(" src") {
            return Some(&body[open_end + 1..close]);
        }
        from = close;
    }
    None
}

pub fn csp_for_html(body: &str) -> String {
    let hash = theme_preload(body).map(|script| base64::engine::general_purpose::STANDARD.encode(Sha256::digest(script.as_bytes())));
    csp(hash.as_deref())
}

// Any path: the file itself, else `index.html` (client-side routing).
pub fn serve(path: &str) -> Response {
    let wanted = path.trim_start_matches('/');
    let Some((name, body)) = lookup(wanted).or_else(|| lookup("index.html")) else {
        return (StatusCode::NOT_FOUND, axum::Json(serde_json::json!({"error": "Not Found"}))).into_response();
    };
    let mime = mime_guess::from_path(name).first_or_octet_stream().essence_str().to_string();
    let mut response = body.into_response();
    let headers = response.headers_mut();
    let content_type = if mime.starts_with("text/") || mime == "application/javascript" { format!("{mime}; charset=utf-8") } else { mime.clone() };
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_str(&content_type).unwrap_or(HeaderValue::from_static("application/octet-stream")));
    if mime == "text/html" {
        let value = csp_for_html(&String::from_utf8_lossy(body));
        if let Ok(value) = HeaderValue::from_str(&value) {
            headers.insert(header::CONTENT_SECURITY_POLICY, value);
        }
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csp_hashes_only_the_theme_preload_script() {
        let html = "<html><head><script src=\"/a.js\"></script><script id=\"oc-theme-preload-script\">let a=1</script></head></html>";
        assert_eq!(theme_preload(html), Some("let a=1"));
        let expected = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(b"let a=1"));
        assert!(csp_for_html(html).contains(&format!("'sha256-{expected}'")));
        assert_eq!(csp_for_html("<html></html>"), csp(None));
    }

    #[test]
    fn public_paths_are_get_only() {
        assert!(is_public_path(&axum::http::Method::GET, "/site.webmanifest"));
        assert!(!is_public_path(&axum::http::Method::POST, "/site.webmanifest"));
        assert!(!is_public_path(&axum::http::Method::GET, "/index.html"));
    }
}
