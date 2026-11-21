// Server-sent events: `data:` lines joined by `\n`, dispatched on a blank line; `event:`/`id:`/`retry:` and comments
// are ignored, as the servers send none of them. The servers keep no replay, so a reconnect starts fresh with a
// new `server.connected` frame, which is how consumers notice they have to resync.
use crate::{Client, Error, Frame, Result, check};
use reqwest::Method;
use std::time::Duration;
use tokio::sync::mpsc;

// Exponential backoff between connection attempts (`tui/src/context/sdk.tsx`: 1 s doubling up to 30 s).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reconnect {
    pub initial: Duration,
    pub max: Duration,
    pub enabled: bool,
}

impl Default for Reconnect {
    fn default() -> Self {
        Self { initial: Duration::from_secs(1), max: Duration::from_secs(30), enabled: true }
    }
}

impl Reconnect {
    pub fn never() -> Self {
        Self { enabled: false, ..Self::default() }
    }

    pub fn after(initial: Duration, max: Duration) -> Self {
        Self { initial, max, enabled: true }
    }
}

#[derive(Default)]
pub(crate) struct Parser {
    line: Vec<u8>,
    data: Vec<String>,
}

impl Parser {
    // Feeds raw bytes; returns the data of every event completed by them.
    pub(crate) fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        for &byte in bytes {
            if byte != b'\n' {
                self.line.push(byte);
                continue;
            }
            if self.line.last() == Some(&b'\r') {
                self.line.pop();
            }
            let line = String::from_utf8_lossy(&std::mem::take(&mut self.line)).into_owned();
            if line.is_empty() {
                if !self.data.is_empty() {
                    out.push(std::mem::take(&mut self.data).join("\n"));
                }
                continue;
            }
            if let Some(rest) = line.strip_prefix("data:") {
                self.data.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
            }
        }
        out
    }
}

// Frames in arrival order across reconnects; dropping the stream closes the connection.
pub struct EventStream {
    rx: mpsc::UnboundedReceiver<Frame>,
    task: tokio::task::JoinHandle<()>,
}

impl EventStream {
    // None once the stream has ended for good (reconnect disabled, or the consumer side closed).
    pub async fn next(&mut self) -> Option<Frame> {
        self.rx.recv().await
    }
}

impl Drop for EventStream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn open(client: &Client, path: &str) -> Result<reqwest::Response> {
    let request = client.request(Method::GET, path, &[])?.header("accept", "text/event-stream");
    check(request.send().await.map_err(Error::Transport)?).await
}

pub(crate) async fn subscribe(client: Client, path: &'static str, global: bool, reconnect: Reconnect) -> Result<EventStream> {
    let first = open(&client, path).await?;
    let (tx, rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        let mut response = Some(first);
        let mut delay = reconnect.initial;
        loop {
            if let Some(mut current) = response.take() {
                let mut parser = Parser::default();
                while let Ok(Some(chunk)) = current.chunk().await {
                    for data in parser.feed(&chunk) {
                        if let Some(frame) = Frame::parse(&data, global)
                            && tx.send(frame).is_err()
                        {
                            return;
                        }
                    }
                }
            }
            if !reconnect.enabled || tx.is_closed() {
                return;
            }
            tokio::time::sleep(delay).await;
            match open(&client, path).await {
                Ok(next) => {
                    response = Some(next);
                    delay = reconnect.initial;
                }
                Err(_) => delay = (delay * 2).min(reconnect.max),
            }
        }
    });
    Ok(EventStream { rx, task })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_split_chunks_crlf_and_multiline_data() {
        let mut parser = Parser::default();
        assert!(parser.feed(b"data: {\"a\":").is_empty());
        assert_eq!(parser.feed(b"1}\n\n: comment\n\n"), vec!["{\"a\":1}".to_string()]);
        assert_eq!(parser.feed(b"event: message\r\nid: 4\r\ndata:x\r\ndata: y\r\n\r\n"), vec!["x\ny".to_string()]);
        assert!(parser.feed(b"retry: 10\n\n").is_empty());
    }
}
