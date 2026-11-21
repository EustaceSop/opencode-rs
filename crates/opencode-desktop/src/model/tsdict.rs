#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Str(String),
    Ident(String),
    Punct(char),
}

struct Lexer<'a> {
    src: &'a str,
    pos: usize,
    peeked: Option<Tok>,
}

pub fn pairs(source: &str) -> Vec<(String, String)> {
    let mut lex = Lexer::new(source);
    let mut out = Vec::new();
    let mut open = false;
    while let Some(tok) = lex.bump() {
        match tok {
            Tok::Str(key) if open => {
                open = false;
                if lex.at(':') {
                    lex.bump();
                    if let Some(value) = lex.value() {
                        out.push((key, value));
                    }
                }
            }
            Tok::Punct('{' | ',') => open = true,
            _ => open = false,
        }
    }
    out
}

pub fn object(source: &str, marker: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Some(mut lex) = Lexer::after(source, marker, '{') else {
        return out;
    };
    let mut depth = 1usize;
    let mut open = true;
    while let Some(tok) = lex.bump() {
        match tok {
            Tok::Str(key) | Tok::Ident(key) if open && depth == 1 => {
                open = false;
                if lex.at(':') {
                    lex.bump();
                    if let Some(value) = lex.value() {
                        out.push((key, value));
                    }
                }
            }
            Tok::Punct('{' | '[' | '(') => {
                depth += 1;
                open = false;
            }
            Tok::Punct('}' | ']' | ')') => {
                depth -= 1;
                open = false;
                if depth == 0 {
                    break;
                }
            }
            Tok::Punct(',') => open = true,
            _ => open = false,
        }
    }
    out
}

pub fn array(source: &str, marker: &str) -> Vec<String> {
    let mut out = Vec::new();
    let Some(mut lex) = Lexer::after(source, marker, '[') else {
        return out;
    };
    let mut depth = 1usize;
    loop {
        if depth == 1
            && let Some(value) = lex.value()
        {
            out.push(value);
            continue;
        }
        match lex.bump() {
            Some(Tok::Punct('{' | '[' | '(')) => depth += 1,
            Some(Tok::Punct('}' | ']' | ')')) => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            Some(_) => {}
            None => break,
        }
    }
    out
}

fn is_ident(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$' || byte >= 0x80
}

fn scalar(value: u32) -> char {
    char::from_u32(value).unwrap_or('\u{fffd}')
}

impl<'a> Lexer<'a> {
    fn new(src: &'a str) -> Self {
        Self {
            src: src.strip_prefix('\u{feff}').unwrap_or(src),
            pos: 0,
            peeked: None,
        }
    }

    // Positions the lexer just past the `open` bracket of the literal assigned after `marker`.
    fn after(src: &'a str, marker: &str, open: char) -> Option<Self> {
        let start = src.find(marker)? + marker.len();
        let mut lex = Self::new(&src[start..]);
        if lex.at(':') {
            let mut depth = 0usize;
            loop {
                match lex.bump()? {
                    Tok::Punct('{' | '[' | '(') => depth += 1,
                    Tok::Punct('}' | ']' | ')') => depth = depth.checked_sub(1)?,
                    Tok::Punct('=') if depth == 0 && !lex.at('>') && !lex.at('=') => break,
                    _ => {}
                }
            }
        }
        loop {
            if lex.bump()? == Tok::Punct(open) {
                return Some(lex);
            }
        }
    }

    fn peek(&mut self) -> Option<&Tok> {
        if self.peeked.is_none() {
            self.peeked = self.scan();
        }
        self.peeked.as_ref()
    }

    fn bump(&mut self) -> Option<Tok> {
        self.peeked.take().or_else(|| self.scan())
    }

    fn at(&mut self, punct: char) -> bool {
        matches!(self.peek(), Some(Tok::Punct(found)) if *found == punct)
    }

    fn literal(&mut self) -> Option<String> {
        if !matches!(self.peek(), Some(Tok::Str(_))) {
            return None;
        }
        match self.bump() {
            Some(Tok::Str(value)) => Some(value),
            _ => None,
        }
    }

    fn value(&mut self) -> Option<String> {
        let mut out = self.literal()?;
        while self.at('+') {
            self.bump();
            match self.literal() {
                Some(next) => out.push_str(&next),
                None => break,
            }
        }
        Some(out)
    }

    fn scan(&mut self) -> Option<Tok> {
        let bytes = self.src.as_bytes();
        loop {
            let byte = *bytes.get(self.pos)?;
            match byte {
                b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c => self.pos += 1,
                b'/' if bytes.get(self.pos + 1) == Some(&b'/') => {
                    self.pos = self.src[self.pos..]
                        .find('\n')
                        .map_or(bytes.len(), |at| self.pos + at + 1);
                }
                b'/' if bytes.get(self.pos + 1) == Some(&b'*') => {
                    self.pos = self.src[self.pos + 2..]
                        .find("*/")
                        .map_or(bytes.len(), |at| self.pos + at + 4);
                }
                b'"' | b'\'' | b'`' => return Some(Tok::Str(self.string(byte))),
                _ if is_ident(byte) => {
                    let start = self.pos;
                    while bytes.get(self.pos).is_some_and(|next| is_ident(*next)) {
                        self.pos += 1;
                    }
                    return Some(Tok::Ident(self.src[start..self.pos].to_owned()));
                }
                _ => {
                    self.pos += 1;
                    return Some(Tok::Punct(byte as char));
                }
            }
        }
    }

    fn string(&mut self, quote: u8) -> String {
        let bytes = self.src.as_bytes();
        let template = quote == b'`';
        let mut out = String::new();
        self.pos += 1;
        loop {
            let start = self.pos;
            while bytes
                .get(self.pos)
                .is_some_and(|next| !matches!(next, b'\\' | b'\n' | b'\r' | b'$') && *next != quote)
            {
                self.pos += 1;
            }
            out.push_str(&self.src[start..self.pos]);
            let Some(&byte) = bytes.get(self.pos) else {
                break;
            };
            self.pos += 1;
            match byte {
                _ if byte == quote => break,
                b'\\' => self.escape(&mut out),
                b'$' if template && bytes.get(self.pos) == Some(&b'{') => {
                    self.interpolation(&mut out);
                }
                b'\r' if template => {
                    if bytes.get(self.pos) == Some(&b'\n') {
                        self.pos += 1;
                    }
                    out.push('\n');
                }
                b'\n' | b'\r' if !template => {
                    self.pos -= 1;
                    break;
                }
                _ => out.push(byte as char),
            }
        }
        out
    }

    fn interpolation(&mut self, out: &mut String) {
        let bytes = self.src.as_bytes();
        let start = self.pos - 1;
        let mut depth = 0usize;
        while let Some(&byte) = bytes.get(self.pos) {
            self.pos += 1;
            match byte {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
        }
        out.push_str(&self.src[start..self.pos]);
    }

    fn escape(&mut self, out: &mut String) {
        let Some(ch) = self.src[self.pos..].chars().next() else {
            return;
        };
        self.pos += ch.len_utf8();
        match ch {
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            'b' => out.push('\u{8}'),
            'f' => out.push('\u{c}'),
            'v' => out.push('\u{b}'),
            '0' => out.push('\0'),
            '\n' | '\u{2028}' | '\u{2029}' => {}
            '\r' => {
                if self.src.as_bytes().get(self.pos) == Some(&b'\n') {
                    self.pos += 1;
                }
            }
            'x' => match self.hex(2) {
                Some(value) => out.push(scalar(value)),
                None => out.push('x'),
            },
            'u' => self.unicode(out),
            other => out.push(other),
        }
    }

    fn hex(&mut self, len: usize) -> Option<u32> {
        let digits = self.src.get(self.pos..self.pos + len)?;
        if !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        let value = u32::from_str_radix(digits, 16).ok()?;
        self.pos += len;
        Some(value)
    }

    fn unicode(&mut self, out: &mut String) {
        let rest = &self.src[self.pos..];
        if rest.starts_with('{') {
            let digits = rest.find('}').map(|end| &rest[1..end]).unwrap_or_default();
            let valid = !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_hexdigit());
            match u32::from_str_radix(digits, 16) {
                Ok(value) if valid => {
                    self.pos += digits.len() + 2;
                    out.push(scalar(value));
                }
                _ => out.push('u'),
            }
            return;
        }
        let Some(high) = self.hex(4) else {
            out.push('u');
            return;
        };
        if (0xd800..0xdc00).contains(&high) && self.src[self.pos..].starts_with("\\u") {
            let saved = self.pos;
            self.pos += 2;
            match self.hex(4) {
                Some(low) if (0xdc00..0xe000).contains(&low) => {
                    out.push(scalar(0x10000 + ((high - 0xd800) << 10) + (low - 0xdc00)));
                    return;
                }
                _ => self.pos = saved,
            }
        }
        out.push(scalar(high));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn quotes() {
        let source = r#"export const dict = {
  "a": "double",
  "b": 'single "inner"',
  "c": `back 'tick'`,
}"#;
        assert_eq!(
            pairs(source),
            owned(&[
                ("a", "double"),
                ("b", "single \"inner\""),
                ("c", "back 'tick'")
            ])
        );
    }

    #[test]
    fn value_on_next_line_and_concatenation() {
        let source = "export const dict = {\n  \"a\":\n    \"next line\",\n  \"b\": \"one \" +\n    'two ' + `three`,\n  \"c\": \"last\"\n}";
        assert_eq!(
            pairs(source),
            owned(&[("a", "next line"), ("b", "one two three"), ("c", "last")])
        );
    }

    #[test]
    fn escapes() {
        let source = r#"const dict = {
  "a": "line\nbreak\ttab\rcr",
  "b": "quote \" apostrophe \' slash \\ tick \`",
  "c": "\u00e9\u{1F600}\x41\uD83D\uDE00",
  "d": "joined \
continued",
  "e": "\q\b\f\v\0",
  "f": "\uZZZZ\xZZ\u{}",
}"#;
        assert_eq!(
            pairs(source),
            owned(&[
                ("a", "line\nbreak\ttab\rcr"),
                ("b", "quote \" apostrophe ' slash \\ tick `"),
                ("c", "\u{e9}\u{1F600}A\u{1F600}"),
                ("d", "joined continued"),
                ("e", "q\u{8}\u{c}\u{b}\0"),
                ("f", "uZZZZxZZu{}"),
            ])
        );
    }

    #[test]
    fn line_continuation_with_crlf_and_lone_surrogate() {
        assert_eq!(
            pairs("x = { \"a\": \"one\\\r\ntwo\", \"b\": \"\\uD83Dx\" }"),
            owned(&[("a", "onetwo"), ("b", "\u{fffd}x")])
        );
    }

    #[test]
    fn comments_and_trailing_commas() {
        let source = r#"// "ignored": "line comment",
export const dict = {
  /* "ignored": "block comment", */
  "a": "A", // trailing "x": "y"
  "b": /* inline */ "B",
  "url": "https://example.com/* not a comment */",
  // "c": "C",
}
"#;
        assert_eq!(
            pairs(source),
            owned(&[
                ("a", "A"),
                ("b", "B"),
                ("url", "https://example.com/* not a comment */")
            ])
        );
    }

    #[test]
    fn annotations_and_suffixes() {
        let annotated = "import { dict as en } from \"./en\"\ntype Keys = keyof typeof en\nexport const dict: Record<string, string> = {\n  \"a\": \"A\",\n}\n";
        assert_eq!(pairs(annotated), owned(&[("a", "A")]));
        let satisfies = "export const dict = {\n  \"a\": \"A\",\n  \"b\": \"B\" as const,\n} satisfies Partial<Record<Keys, string>>\n";
        assert_eq!(pairs(satisfies), owned(&[("a", "A"), ("b", "B")]));
        let constant = "\u{feff}export const dict = { \"a\": \"A\" } as const\n";
        assert_eq!(pairs(constant), owned(&[("a", "A")]));
    }

    #[test]
    fn nesting_and_non_literals() {
        let source = r#"export const dict = {
  ...BASE,
  ...Object.fromEntries(KEYS.map((key, index) => [key, values[index]])),
  "top": "T",
  "group": { "inner": "I", "deeper": { "leaf": "L" } },
  "number": 1,
  "call": fn("x"),
  bare: "skipped",
  "after": "Z",
}
const other = flag ? "yes" : "no"
"#;
        assert_eq!(
            pairs(source),
            owned(&[("top", "T"), ("inner", "I"), ("leaf", "L"), ("after", "Z")])
        );
    }

    #[test]
    fn placeholders_and_templates() {
        let source = "x = {\n  \"a\": \"Hello {{name}}\",\n  \"b\": `multi\r\nline ${a + {b: 1}.b} $5 end`,\n}";
        assert_eq!(
            pairs(source),
            owned(&[
                ("a", "Hello {{name}}"),
                ("b", "multi\nline ${a + {b: 1}.b} $5 end"),
            ])
        );
    }

    #[test]
    fn unterminated_input() {
        assert_eq!(
            pairs("x = { \"a\": \"open\n  \"b\": \"B\" }"),
            owned(&[("a", "open")])
        );
        assert_eq!(
            pairs("x = { \"a\": \"A\", \"b\": \"tail\\"),
            owned(&[("a", "A"), ("b", "tail")])
        );
        assert_eq!(pairs("x = { \"a\": `${"), owned(&[("a", "${")]));
        assert!(pairs("/* never closed { \"a\": \"A\" }").is_empty());
        assert!(pairs("").is_empty());
    }

    #[test]
    fn object_after_marker() {
        let source = r#"export const LIST = ["en", "zh"] as const
export type Code = (typeof LIST)[number]
export const LABELS: Record<Code, string> = {
  en: "English",
  "zh": '简体中文', // comment
  nested: { skip: "me" },
  fn: call({ skip: "me" }),
  $odd_1: "one " + "two",
}
export const TAGS: { [key: string]: string } = { en: "en", zh: "zh-Hans" }
export const ENGLISH = {
  "desktop.menu.file": "File",
} as const
"#;
        assert_eq!(
            object(source, "LABELS"),
            owned(&[("en", "English"), ("zh", "简体中文"), ("$odd_1", "one two")])
        );
        assert_eq!(
            object(source, "TAGS"),
            owned(&[("en", "en"), ("zh", "zh-Hans")])
        );
        assert_eq!(
            object(source, "ENGLISH"),
            owned(&[("desktop.menu.file", "File")])
        );
        assert!(object(source, "MISSING").is_empty());
        assert!(object("const LABELS: Record<A, B>", "LABELS").is_empty());
    }

    #[test]
    fn array_after_marker() {
        let source = r#"const desktop = [
  "OpenCode", // first
  'File',
  "Split " +
    "value",
  ["nested"],
  `tick`,
]
export const LIST: readonly string[] = ["en", "zh", "zht"] as const
export const dict = { "a": "A" }
"#;
        assert_eq!(
            array(source, "const desktop"),
            ["OpenCode", "File", "Split value", "tick"]
        );
        assert_eq!(array(source, "LIST"), ["en", "zh", "zht"]);
        assert!(array(source, "MISSING").is_empty());
        assert_eq!(array("const LIST = [\"open\", ", "LIST"), ["open"]);
    }
}
