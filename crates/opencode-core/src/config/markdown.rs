use regex::Regex;
use serde_json::{Map, Number, Value};
use std::sync::LazyLock;

#[derive(Debug, Clone, PartialEq)]
pub struct Markdown {
    pub data: Map<String, Value>,
    pub content: String,
}

fn yaml_key(key: serde_yaml_ng::Value) -> String {
    match key {
        serde_yaml_ng::Value::String(s) => s,
        other => match yaml_to_json(other) {
            Value::String(s) => s,
            v => v.to_string(),
        },
    }
}

fn yaml_to_json(value: serde_yaml_ng::Value) -> Value {
    use serde_yaml_ng::Value as Y;
    match value {
        Y::Null => Value::Null,
        Y::Bool(b) => Value::Bool(b),
        Y::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Number(i.into())
            } else if let Some(u) = n.as_u64() {
                Value::Number(u.into())
            } else {
                n.as_f64().and_then(Number::from_f64).map_or(Value::Null, Value::Number)
            }
        }
        Y::String(s) => Value::String(s),
        Y::Sequence(list) => Value::Array(list.into_iter().map(yaml_to_json).collect()),
        Y::Mapping(map) => Value::Object(map.into_iter().map(|(k, v)| (yaml_key(k), yaml_to_json(v))).collect()),
        Y::Tagged(tagged) => yaml_to_json(tagged.value),
    }
}

fn parse_yaml(text: &str) -> Result<Map<String, Value>, String> {
    let value: serde_yaml_ng::Value = serde_yaml_ng::from_str(text).map_err(|e| e.to_string())?;
    Ok(match yaml_to_json(value) {
        Value::Object(map) => map,
        _ => Map::new(),
    })
}

// gray-matter: `---` opener, optional language tag, body up to the first "\n---".
fn matter(input: &str) -> Result<Markdown, String> {
    let input = input.strip_prefix('\u{feff}').unwrap_or(input);
    let plain = || Markdown { data: Map::new(), content: input.to_string() };
    let Some(rest) = input.strip_prefix("---") else { return Ok(plain()) };
    if rest.starts_with('-') {
        return Ok(plain());
    }
    let first_line = rest.find('\n').map_or("", |i| rest[..i].strip_suffix('\r').unwrap_or(&rest[..i]));
    let language = first_line.trim();
    let rest = &rest[first_line.len()..];
    let close = rest.find("\n---");
    let block = &rest[..close.unwrap_or(rest.len())];
    let content = match close {
        None => String::new(),
        Some(i) => {
            let after = &rest[i + 4..];
            let after = after.strip_prefix('\r').unwrap_or(after);
            after.strip_prefix('\n').unwrap_or(after).to_string()
        }
    };
    let blank = block.lines().all(|line| line.trim().is_empty() || line.trim_start().starts_with('#'));
    let data = if blank {
        Map::new()
    } else {
        match language {
            "" | "yaml" | "yml" => parse_yaml(block)?,
            "json" => match serde_json::from_str::<Value>(block).map_err(|e| e.to_string())? {
                Value::Object(map) => map,
                _ => Map::new(),
            },
            other => return Err(format!("unsupported frontmatter language: {other}")),
        }
    };
    Ok(Markdown { data, content })
}

static ENTRY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^([a-zA-Z_][a-zA-Z0-9_]*)\s*:\s*(.*)$").unwrap());

// Rewrites top-level `key: value: with colon` lines into `key: |-` block scalars.
pub fn sanitize(content: &str) -> String {
    let Some(open) = content.strip_prefix("---\r\n").or_else(|| content.strip_prefix("---\n")) else {
        return content.to_string();
    };
    let Some(end) = open.find("\n---") else { return content.to_string() };
    let frontmatter = open[..end].strip_suffix('\r').unwrap_or(&open[..end]);
    let mut lines: Vec<String> = Vec::new();
    for raw in frontmatter.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        let trimmed = line.trim();
        let indented = line.chars().next().is_some_and(char::is_whitespace);
        let entry = if trimmed.starts_with('#') || trimmed.is_empty() || indented { None } else { ENTRY.captures(line) };
        let Some(entry) = entry else {
            lines.push(line.to_string());
            continue;
        };
        let value = entry[2].trim();
        let keep = value.is_empty() || value == ">" || value == "|" || value.starts_with(['"', '\'']);
        if keep || !value.contains(':') {
            lines.push(line.to_string());
            continue;
        }
        lines.push(format!("{}: |-", &entry[1]));
        lines.push(format!("  {value}"));
    }
    content.replacen(frontmatter, &lines.join("\n"), 1)
}

pub fn parse(content: &str) -> Result<Markdown, String> {
    matter(content).or_else(|_| matter(&sanitize(content)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn frontmatter_and_body() {
        let md = parse("---\ndescription: hi\nmode: subagent\ntemperature: 0.2\ntools:\n  bash: false\n---\n\nBody\n").unwrap();
        assert_eq!(
            Value::Object(md.data),
            json!({"description": "hi", "mode": "subagent", "temperature": 0.2, "tools": {"bash": false}})
        );
        assert_eq!(md.content, "\nBody\n");
    }

    #[test]
    fn no_frontmatter_variants() {
        assert_eq!(parse("just text").unwrap().content, "just text");
        assert!(parse("just text").unwrap().data.is_empty());
        assert_eq!(parse("----\nx").unwrap().content, "----\nx");
        let md = parse("---\n# only a comment\n---\r\nbody").unwrap();
        assert!(md.data.is_empty());
        assert_eq!(md.content, "body");
        let md = parse("---\nname: x").unwrap();
        assert_eq!(md.data["name"], "x");
        assert_eq!(md.content, "");
        assert_eq!(parse("\u{feff}---\nname: y\n---\nz").unwrap().data["name"], "y");
    }

    #[test]
    fn key_order_and_yaml_core_scalars() {
        let md = parse("---\nz: 1\na: yes\nb: true\nc: ~\n---\n").unwrap();
        assert_eq!(md.data.keys().collect::<Vec<_>>(), ["z", "a", "b", "c"]);
        assert_eq!(md.data["a"], "yes");
        assert_eq!(md.data["b"], true);
        assert_eq!(md.data["c"], Value::Null);
    }

    #[test]
    fn sanitize_retry_on_unquoted_colon() {
        let text = "---\ndescription: Use when: things break\nmodel: a/b\n---\nprompt";
        assert!(matter(text).is_err());
        let md = parse(text).unwrap();
        assert_eq!(md.data["description"], "Use when: things break");
        assert_eq!(md.data["model"], "a/b");
        assert_eq!(md.content, "prompt");
    }

    #[test]
    fn sanitize_rules() {
        let text = "---\r\n# c: d: e\r\na: \"x: y\"\r\nb: p: q\r\n  nested: r: s\r\nc: |\r\nd: plain\r\n---\r\nbody";
        assert_eq!(
            sanitize(text),
            "---\r\n# c: d: e\na: \"x: y\"\nb: |-\n  p: q\n  nested: r: s\nc: |\nd: plain\r\n---\r\nbody"
        );
        assert_eq!(sanitize("no frontmatter: a: b"), "no frontmatter: a: b");
    }

    #[test]
    fn unrecoverable_yaml_is_an_error() {
        assert!(parse("---\na: [1, 2\n---\nbody").is_err());
    }
}
