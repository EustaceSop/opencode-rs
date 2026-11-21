// AWS event-stream binary framing: [total:4][headers:4][prelude crc:4][headers][payload][message crc:4].
use crate::Error;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Frame {
    // String-valued headers only (`:message-type`, `:event-type`, `:exception-type`, `:content-type`).
    pub headers: Vec<(String, String)>,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

#[derive(Default)]
pub struct Decoder {
    buffer: Vec<u8>,
}

const PRELUDE: usize = 12;
const MIN_FRAME: usize = 16;

fn invalid(what: &str) -> Error {
    Error::Stream(format!("Failed to decode event-stream frame: {what}"))
}

fn be32(bytes: &[u8]) -> usize {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize
}

fn headers(mut bytes: &[u8]) -> Result<Vec<(String, String)>, Error> {
    let mut out = Vec::new();
    let take = |bytes: &mut &[u8], n: usize| -> Result<Vec<u8>, Error> {
        if bytes.len() < n {
            return Err(invalid("truncated header"));
        }
        let (head, rest) = bytes.split_at(n);
        *bytes = rest;
        Ok(head.to_vec())
    };
    while !bytes.is_empty() {
        let name_len = take(&mut bytes, 1)?[0] as usize;
        let name = String::from_utf8_lossy(&take(&mut bytes, name_len)?).to_string();
        let kind = take(&mut bytes, 1)?[0];
        let fixed = match kind {
            0 | 1 => Some(0),
            2 => Some(1),
            3 => Some(2),
            4 => Some(4),
            5 | 8 => Some(8),
            9 => Some(16),
            6 | 7 => None,
            _ => return Err(invalid("unknown header type")),
        };
        match fixed {
            Some(n) => {
                take(&mut bytes, n)?;
            }
            None => {
                let len = take(&mut bytes, 2)?;
                let value = take(&mut bytes, u16::from_be_bytes([len[0], len[1]]) as usize)?;
                if kind == 7 {
                    out.push((name, String::from_utf8_lossy(&value).to_string()));
                }
            }
        }
    }
    Ok(out)
}

impl Decoder {
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<Frame>, Error> {
        self.buffer.extend_from_slice(bytes);
        let mut out = Vec::new();
        let mut offset = 0;
        while self.buffer.len() - offset >= 4 {
            let view = &self.buffer[offset..];
            let total = be32(view);
            if total < MIN_FRAME {
                return Err(invalid("frame too short"));
            }
            if view.len() < total {
                break;
            }
            let frame = &view[..total];
            let header_len = be32(&frame[4..]);
            if PRELUDE + header_len + 4 > total {
                return Err(invalid("header length exceeds frame"));
            }
            if crc32fast::hash(&frame[..8]) as usize != be32(&frame[8..]) {
                return Err(invalid("prelude checksum mismatch"));
            }
            if crc32fast::hash(&frame[..total - 4]) as usize != be32(&frame[total - 4..]) {
                return Err(invalid("message checksum mismatch"));
            }
            out.push(Frame { headers: headers(&frame[PRELUDE..PRELUDE + header_len])?, payload: frame[PRELUDE + header_len..total - 4].to_vec() });
            offset += total;
        }
        self.buffer.drain(..offset);
        Ok(out)
    }

    // Bytes of an incomplete trailing frame.
    pub fn pending(&self) -> usize {
        self.buffer.len()
    }
}

#[cfg(test)]
pub(crate) fn encode(headers: &[(&str, &str)], payload: &[u8]) -> Vec<u8> {
    let mut head = Vec::new();
    for (name, value) in headers {
        head.push(name.len() as u8);
        head.extend_from_slice(name.as_bytes());
        head.push(7);
        head.extend_from_slice(&(value.len() as u16).to_be_bytes());
        head.extend_from_slice(value.as_bytes());
    }
    let total = PRELUDE + head.len() + payload.len() + 4;
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&(total as u32).to_be_bytes());
    out.extend_from_slice(&(head.len() as u32).to_be_bytes());
    let crc = crc32fast::hash(&out);
    out.extend_from_slice(&crc.to_be_bytes());
    out.extend_from_slice(&head);
    out.extend_from_slice(payload);
    let crc = crc32fast::hash(&out);
    out.extend_from_slice(&crc.to_be_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_split_frames_and_rejects_corruption() {
        let mut bytes = encode(&[(":event-type", "messageStart"), (":message-type", "event")], br#"{"role":"assistant"}"#);
        bytes.extend(encode(&[(":message-type", "exception"), (":exception-type", "throttlingException")], br#"{"message":"slow"}"#));
        let mut decoder = Decoder::default();
        let mut frames = Vec::new();
        for chunk in bytes.chunks(7) {
            frames.extend(decoder.feed(chunk).unwrap());
        }
        assert_eq!(decoder.pending(), 0);
        assert_eq!(frames.len(), 2);
        assert_eq!((frames[0].header(":event-type"), frames[0].payload.as_slice()), (Some("messageStart"), br#"{"role":"assistant"}"#.as_slice()));
        assert_eq!((frames[1].header(":message-type"), frames[1].header(":exception-type")), (Some("exception"), Some("throttlingException")));

        let mut broken = bytes.clone();
        let last = broken.len() - 1;
        broken[last] ^= 1;
        let mut decoder = Decoder::default();
        assert!(matches!(decoder.feed(&broken), Err(Error::Stream(m)) if m.contains("message checksum")));
        broken = bytes;
        broken[5] ^= 1;
        assert!(matches!(Decoder::default().feed(&broken), Err(Error::Stream(m)) if m.contains("checksum") || m.contains("header length")));
    }
}
