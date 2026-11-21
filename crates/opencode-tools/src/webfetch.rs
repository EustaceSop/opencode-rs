//! `webfetch` tool (`opencode/src/tool/webfetch.ts`).

use std::time::Duration;

use base64::Engine;
use htmd::element_handler::{HandlerResult, Handlers};
use htmd::options::{BulletListMarker, CodeBlockStyle, HeadingStyle, Options};
use htmd::{Element, HtmlToMarkdown};
use serde_json::{Value, json};

use crate::util::js_trim;
use crate::{Attachment, PermissionRequest, Tool, ToolContext, ToolError, ToolOutput, parse_args};

const MAX_RESPONSE_SIZE: usize = 5 * 1024 * 1024;
const DEFAULT_TIMEOUT_SECS: f64 = 30.0;
const MAX_TIMEOUT_MS: f64 = 120.0 * 1000.0;
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36";
const TOO_LARGE: &str = "Response too large (exceeds 5MB limit)";

#[derive(Debug, Clone, Copy, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
enum Format {
    Text,
    Markdown,
    Html,
}

fn default_format() -> Format {
    Format::Markdown
}

#[derive(Debug, serde::Deserialize)]
struct Params {
    url: String,
    #[serde(default = "default_format")]
    format: Format,
    #[serde(default)]
    timeout: Option<f64>,
}

pub struct WebFetchTool;

fn accept_header(format: Format) -> &'static str {
    match format {
        Format::Markdown => {
            "text/markdown;q=1.0, text/x-markdown;q=0.9, text/plain;q=0.8, text/html;q=0.7, */*;q=0.1"
        }
        Format::Text => "text/plain;q=1.0, text/markdown;q=0.9, text/html;q=0.8, */*;q=0.1",
        Format::Html => {
            "text/html;q=1.0, application/xhtml+xml;q=0.9, text/plain;q=0.8, text/markdown;q=0.7, */*;q=0.1"
        }
    }
}

/// `isImageAttachment` (`util/media.ts:11-13`).
fn is_image_attachment(mime: &str) -> bool {
    mime.starts_with("image/") && mime != "image/svg+xml" && mime != "image/vnd.fastbidsheet"
}

const SKIP_TAGS: [&str; 6] = ["script", "style", "noscript", "iframe", "object", "embed"];
const VOID_TAGS: [&str; 19] = [
    "area", "base", "basefont", "br", "col", "command", "embed", "frame", "hr", "img", "input",
    "isindex", "keygen", "link", "meta", "param", "source", "track", "wbr",
];

struct TextExtractor {
    text: String,
    skip: usize,
    stack: Vec<String>,
}

impl TextExtractor {
    fn emit(&mut self, raw: &str) {
        if self.skip == 0 && !raw.is_empty() {
            self.text.push_str(&html_escape::decode_html_entities(raw));
        }
    }

    fn open(&mut self, name: &str) {
        if self.skip > 0 || SKIP_TAGS.contains(&name) {
            self.skip += 1;
        }
        if VOID_TAGS.contains(&name) {
            self.skip = self.skip.saturating_sub(1);
        } else {
            self.stack.push(name.to_string());
        }
    }

    fn close(&mut self, name: &str) {
        let Some(idx) = self.stack.iter().rposition(|open| open == name) else {
            return;
        };
        for _ in idx..self.stack.len() {
            self.stack.pop();
            self.skip = self.skip.saturating_sub(1);
        }
    }
}

fn tag_name(rest: &str) -> String {
    let end = rest
        .find(|c: char| c.is_whitespace() || c == '/' || c == '>')
        .unwrap_or(rest.len());
    rest[..end].to_ascii_lowercase()
}

/// Index just past the `>` closing the tag that starts at `from`, honouring quoted attribute values.
fn tag_end(html: &str, from: usize) -> usize {
    let mut quote: Option<u8> = None;
    for (i, b) in html.as_bytes().iter().enumerate().skip(from) {
        match (quote, *b) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, b'"') | (None, b'\'') => quote = Some(*b),
            (None, b'>') => return i + 1,
            _ => {}
        }
    }
    html.len()
}

/// Text extraction equivalent of the htmlparser2 handler in `webfetch.ts:158-180`.
pub(crate) fn extract_text(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut state = TextExtractor {
        text: String::new(),
        skip: 0,
        stack: Vec::new(),
    };
    let mut pos = 0;
    while pos < html.len() {
        let Some(lt) = html[pos..].find('<').map(|i| i + pos) else {
            state.emit(&html[pos..]);
            break;
        };
        let after = &html[lt + 1..];
        let mut chars = after.chars();
        let first = chars.next();
        let second = chars.next();
        if after.starts_with("!--") {
            state.emit(&html[pos..lt]);
            pos = html[lt + 4..]
                .find("-->")
                .map_or(html.len(), |i| lt + 4 + i + 3);
        } else if matches!(first, Some('!' | '?')) {
            state.emit(&html[pos..lt]);
            pos = html[lt..].find('>').map_or(html.len(), |i| lt + i + 1);
        } else if first == Some('/') && second.is_some_and(|c| c.is_ascii_alphabetic()) {
            state.emit(&html[pos..lt]);
            let name = tag_name(&after[1..]);
            pos = html[lt..].find('>').map_or(html.len(), |i| lt + i + 1);
            state.close(&name);
        } else if first.is_some_and(|c| c.is_ascii_alphabetic()) {
            state.emit(&html[pos..lt]);
            let name = tag_name(after);
            pos = tag_end(html, lt + 1);
            state.open(&name);
            if matches!(name.as_str(), "script" | "style" | "title") {
                let end = lower[pos..]
                    .find(&format!("</{name}"))
                    .map_or(html.len(), |i| pos + i);
                state.emit(&html[pos..end]);
                pos = end;
            }
        } else {
            state.emit(&html[pos..=lt]);
            pos = lt + 1;
        }
    }
    js_trim(&state.text).to_string()
}

/// Stand-in for Turndown with `headingStyle:"atx", hr:"---", bulletListMarker:"-", codeBlockStyle:"fenced"`.
pub(crate) fn html_to_markdown(html: &str) -> Result<String, ToolError> {
    let converter = HtmlToMarkdown::builder()
        .skip_tags(vec!["script", "style", "meta", "link"])
        .options(Options {
            heading_style: HeadingStyle::Atx,
            bullet_list_marker: BulletListMarker::Dash,
            code_block_style: CodeBlockStyle::Fenced,
            ..Default::default()
        })
        .add_handler(
            vec!["hr"],
            |_: &dyn Handlers, _: Element| -> Option<HandlerResult> { Some("\n\n---\n\n".into()) },
        )
        .build();
    converter
        .convert(html)
        .map_err(|e| ToolError::Other(e.to_string()))
}

async fn fetch(
    client: &reqwest::Client,
    url: &str,
    format: Format,
) -> Result<(String, Vec<u8>), ToolError> {
    let send = |user_agent: &'static str| {
        client
            .get(url)
            .header("User-Agent", user_agent)
            .header("Accept", accept_header(format))
            .header("Accept-Language", "en-US,en;q=0.9")
            .send()
    };
    let request_error = |e: reqwest::Error| ToolError::Other(format!("Request failed: {e}"));
    let mut response = send(USER_AGENT).await.map_err(request_error)?;
    let challenged = response.status().as_u16() == 403
        && response
            .headers()
            .get("cf-mitigated")
            .and_then(|v| v.to_str().ok())
            == Some("challenge");
    if challenged {
        response = send("opencode").await.map_err(request_error)?;
    }
    if !response.status().is_success() {
        return Err(ToolError::Other(format!(
            "Request failed with status code: {}",
            response.status().as_u16()
        )));
    }

    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .unwrap_or_default()
    };
    if header("content-length")
        .trim()
        .parse::<usize>()
        .is_ok_and(|len| len > MAX_RESPONSE_SIZE)
    {
        return Err(ToolError::other(TOO_LARGE));
    }
    let content_type = header("content-type");

    let mut body: Vec<u8> = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(request_error)? {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_RESPONSE_SIZE {
            return Err(ToolError::other(TOO_LARGE));
        }
    }
    Ok((content_type, body))
}

#[async_trait::async_trait]
impl Tool for WebFetchTool {
    fn id(&self) -> &str {
        "webfetch"
    }

    fn description(&self) -> String {
        include_str!("../../../../opencode-ts/packages/opencode/src/tool/webfetch.txt").to_string()
    }

    fn parameters(&self) -> Value {
        json!({
            "$schema": crate::JSON_SCHEMA_URI,
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "The URL to fetch content from" },
                "format": {
                    "type": "string",
                    "enum": ["text", "markdown", "html"],
                    "default": "markdown",
                    "description": "The format to return the content in (text, markdown, or html). Defaults to markdown."
                },
                "timeout": { "type": "number", "description": "Optional timeout in seconds (max 120)" }
            },
            "required": ["url"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let params: Params = parse_args("webfetch", args)?;
        if !params.url.starts_with("http://") && !params.url.starts_with("https://") {
            return Err(ToolError::other("URL must start with http:// or https://"));
        }

        let mut metadata = json!({ "url": params.url, "format": params.format });
        if let Some(timeout) = params.timeout {
            metadata["timeout"] = json!(timeout);
        }
        ctx.permissions
            .ask(PermissionRequest {
                permission: "webfetch".into(),
                patterns: vec![params.url.clone()],
                always: vec!["*".into()],
                metadata,
            })
            .await?;

        let timeout_ms =
            (params.timeout.unwrap_or(DEFAULT_TIMEOUT_SECS) * 1000.0).min(MAX_TIMEOUT_MS);
        let timeout = Duration::from_secs_f64(if timeout_ms.is_finite() {
            timeout_ms.max(0.0) / 1000.0
        } else {
            0.0
        });

        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| ToolError::Other(e.to_string()))?;
        let (content_type, body) = tokio::select! {
            result = tokio::time::timeout(timeout, fetch(&client, &params.url, params.format)) => {
                result.map_err(|_| ToolError::other("Request timed out"))??
            }
            _ = ctx.abort.cancelled() => return Err(ToolError::Aborted),
        };

        let mime = content_type
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_lowercase();
        let title = format!("{} ({})", params.url, content_type);

        if is_image_attachment(&mime) {
            let url = format!(
                "data:{mime};base64,{}",
                base64::engine::general_purpose::STANDARD.encode(&body)
            );
            return Ok(ToolOutput {
                title,
                output: "Image fetched successfully".to_string(),
                metadata: json!({}),
                attachments: vec![Attachment { mime, url }],
            });
        }

        let content = String::from_utf8_lossy(&body);
        let content = content.strip_prefix('\u{feff}').unwrap_or(&content);
        let is_html = content_type.contains("text/html");
        let output = match params.format {
            Format::Markdown if is_html => html_to_markdown(content)?,
            Format::Text if is_html => extract_text(content),
            _ => content.to_string(),
        };
        Ok(ToolOutput {
            title,
            output,
            metadata: json!({}),
            attachments: vec![],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Fixture;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const PAGE: &str = "<html><head><title>T</title><style>p{}</style><script>var x = '<p>no</p>';</script></head><body><h1>Hello</h1><p>World &amp; <b>all</b></p><noscript><p>hidden</p></noscript><ul><li>one</li><li>two</li></ul><hr><embed src=\"x\">tail</body></html>";

    struct Server {
        base: String,
        hits: Arc<AtomicUsize>,
    }

    fn response(status: &str, headers: &str, body: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n",
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(body);
        out
    }

    async fn serve() -> Server {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let counter = counter.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 2048];
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        match socket.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }
                    counter.fetch_add(1, Ordering::SeqCst);
                    let request = String::from_utf8_lossy(&buf).to_string();
                    let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let lower = request.to_lowercase();
                    let reply = match path.as_str() {
                        "/page" => response("200 OK", "Content-Type: text/html; charset=utf-8\r\n", PAGE.as_bytes()),
                        "/plain" => response("200 OK", "Content-Type: text/plain\r\n", b"<b>raw</b>"),
                        "/img" => response("200 OK", "Content-Type: image/png\r\n", &[1, 2, 3]),
                        "/big" => b"HTTP/1.1 200 OK\r\nContent-Length: 6000000\r\nConnection: close\r\n\r\n".to_vec(),
                        "/cf" if lower.contains("user-agent: opencode\r\n") => {
                            response("200 OK", "Content-Type: text/plain\r\n", b"passed")
                        }
                        "/cf" => response("403 Forbidden", "cf-mitigated: challenge\r\n", b"blocked"),
                        "/headers" => response("200 OK", "Content-Type: text/plain\r\n", lower.as_bytes()),
                        "/slow" => {
                            tokio::time::sleep(Duration::from_secs(5)).await;
                            response("200 OK", "", b"late")
                        }
                        _ => response("404 Not Found", "", b"nope"),
                    };
                    let _ = socket.write_all(&reply).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        Server { base, hits }
    }

    async fn run(t: &Fixture, args: Value) -> Result<ToolOutput, ToolError> {
        WebFetchTool.execute(args, &t.ctx()).await
    }

    #[test]
    fn text_extraction_skips_hidden_subtrees() {
        assert_eq!(extract_text(PAGE), "THelloWorld & allonetwotail");
        assert_eq!(
            extract_text("a < b <!-- c --> d<br/>e<iframe><p>x</p></iframe>f"),
            "a < b  def"
        );
        assert_eq!(extract_text("<p title=\"a>b\">ok</p></div>"), "ok");
    }

    #[test]
    fn markdown_conversion() {
        let md = html_to_markdown(PAGE).unwrap();
        assert!(md.contains("# Hello"), "{md}");
        assert!(md.contains("World & **all**"), "{md}");
        assert!(md.contains("-   one") || md.contains("- one"), "{md}");
        assert!(md.contains("\n---\n"), "{md}");
        assert!(!md.contains("var x") && !md.contains("p{}"), "{md}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fetches_markdown_text_and_html() {
        let server = serve().await;
        let t = Fixture::new();
        let url = format!("{}/page", server.base);
        let out = run(&t, json!({"url": url})).await.unwrap();
        assert!(out.output.contains("# Hello"));
        assert_eq!(out.title, format!("{url} (text/html; charset=utf-8)"));
        assert_eq!(out.metadata, json!({}));
        let reqs = t.requests();
        assert_eq!(reqs[0].permission, "webfetch");
        assert_eq!(reqs[0].patterns, vec![url.clone()]);
        assert_eq!(reqs[0].always, vec!["*"]);
        assert_eq!(reqs[0].metadata, json!({"url": url, "format": "markdown"}));

        let out = run(&t, json!({"url": url, "format": "text"}))
            .await
            .unwrap();
        assert_eq!(out.output, "THelloWorld & allonetwotail");
        let out = run(&t, json!({"url": url, "format": "html", "timeout": 5}))
            .await
            .unwrap();
        assert_eq!(out.output, PAGE);
        assert_eq!(t.requests()[2].metadata["timeout"], 5.0);

        let out = run(
            &t,
            json!({"url": format!("{}/plain", server.base), "format": "markdown"}),
        )
        .await
        .unwrap();
        assert_eq!(out.output, "<b>raw</b>");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sends_browser_headers() {
        let server = serve().await;
        let t = Fixture::new();
        let out = run(
            &t,
            json!({"url": format!("{}/headers", server.base), "format": "text"}),
        )
        .await
        .unwrap();
        assert!(
            out.output
                .contains(&format!("user-agent: {}", USER_AGENT.to_lowercase()))
        );
        assert!(
            out.output.contains(
                "accept: text/plain;q=1.0, text/markdown;q=0.9, text/html;q=0.8, */*;q=0.1"
            )
        );
        assert!(out.output.contains("accept-language: en-us,en;q=0.9"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn image_attachment_and_errors() {
        let server = serve().await;
        let t = Fixture::new();
        let out = run(&t, json!({"url": format!("{}/img", server.base)}))
            .await
            .unwrap();
        assert_eq!(out.output, "Image fetched successfully");
        assert_eq!(
            out.attachments,
            vec![Attachment {
                mime: "image/png".into(),
                url: "data:image/png;base64,AQID".into()
            }]
        );

        let err = run(&t, json!({"url": format!("{}/missing", server.base)}))
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "Request failed with status code: 404");
        let err = run(&t, json!({"url": format!("{}/big", server.base)}))
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "Response too large (exceeds 5MB limit)");
        let err = run(
            &t,
            json!({"url": format!("{}/slow", server.base), "timeout": 0.3}),
        )
        .await
        .unwrap_err();
        assert_eq!(err.to_string(), "Request timed out");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn retries_cloudflare_challenge_with_honest_agent() {
        let server = serve().await;
        let t = Fixture::new();
        let out = run(&t, json!({"url": format!("{}/cf", server.base)}))
            .await
            .unwrap();
        assert_eq!(out.output, "passed");
        assert_eq!(server.hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn validates_before_asking() {
        let t = Fixture::new();
        let err = run(&t, json!({"url": "ftp://x"})).await.unwrap_err();
        assert_eq!(err.to_string(), "URL must start with http:// or https://");
        assert!(t.requests().is_empty());
        assert!(matches!(
            run(&t, json!({"url": "http://x", "format": "pdf"})).await,
            Err(ToolError::InvalidArguments { .. })
        ));
        t.deny("webfetch");
        assert!(matches!(
            run(&t, json!({"url": "http://127.0.0.1:9/"})).await,
            Err(ToolError::Rejected)
        ));
    }

    #[tokio::test]
    async fn abort_cancels_request() {
        let server = serve().await;
        let t = Fixture::new();
        t.abort.cancel();
        let res = run(&t, json!({"url": format!("{}/slow", server.base)})).await;
        assert!(matches!(res, Err(ToolError::Aborted)));
    }
}
