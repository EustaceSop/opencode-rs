// Incremental server-sent-events decoder: feed raw bytes, get complete `data` payloads.
#[derive(Default)]
pub struct Decoder {
    buffer: Vec<u8>,
    data: Vec<String>,
}

impl Decoder {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buffer.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(end) = self.buffer.iter().position(|b| *b == b'\n') {
            let raw: Vec<u8> = self.buffer.drain(..=end).collect();
            let line = String::from_utf8_lossy(&raw);
            let line = line.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                if !self.data.is_empty() {
                    events.push(self.data.join("\n"));
                    self.data.clear();
                }
            } else if let Some(value) = line.strip_prefix("data:") {
                self.data.push(value.strip_prefix(' ').unwrap_or(value).to_string());
            }
        }
        events
    }

    pub fn finish(&mut self) -> Vec<String> {
        let mut events = self.feed(b"\n\n");
        events.retain(|e| !e.is_empty());
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_across_chunks() {
        let mut d = Decoder::default();
        assert!(d.feed(b"data: {\"a\"").is_empty());
        assert_eq!(d.feed(b":1}\r\n\r\n: ping\n\ndata: x\n"), vec!["{\"a\":1}"]);
        assert_eq!(d.finish(), vec!["x"]);
    }
}
