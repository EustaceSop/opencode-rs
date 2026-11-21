use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

const MAX_PER_FILE: usize = 20;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub range: Range,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub message: String,
    #[serde(flatten)]
    pub rest: Map<String, Value>,
}

pub fn pretty(diagnostic: &Diagnostic) -> String {
    let severity = match diagnostic.severity.filter(|s| *s != 0).unwrap_or(1) {
        1 => "ERROR",
        2 => "WARN",
        3 => "INFO",
        4 => "HINT",
        _ => "undefined",
    };
    let line = diagnostic.range.start.line + 1;
    let col = diagnostic.range.start.character + 1;
    format!("{severity} [{line}:{col}] {}", diagnostic.message)
}

/// `<diagnostics file="…">` block with errors only (≤ 20, then `... and N more`); empty when there are none.
pub fn report(file: &str, issues: &[Diagnostic]) -> String {
    let errors: Vec<&Diagnostic> = issues
        .iter()
        .filter(|item| item.severity == Some(1))
        .collect();
    if errors.is_empty() {
        return String::new();
    }
    let limited: Vec<String> = errors
        .iter()
        .take(MAX_PER_FILE)
        .map(|d| pretty(d))
        .collect();
    let suffix = if errors.len() > MAX_PER_FILE {
        format!("\n... and {} more", errors.len() - MAX_PER_FILE)
    } else {
        String::new()
    };
    format!(
        "<diagnostics file=\"{file}\">\n{}{suffix}\n</diagnostics>",
        limited.join("\n")
    )
}

pub(crate) fn dedupe(items: Vec<Diagnostic>) -> Vec<Diagnostic> {
    let mut seen = std::collections::HashSet::new();
    items
        .into_iter()
        .filter(|item| {
            let key = serde_json::json!([
                item.code,
                item.severity,
                item.message,
                item.source,
                item.range
            ])
            .to_string();
            seen.insert(key)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diag(severity: Option<u8>, line: u32, message: &str) -> Diagnostic {
        Diagnostic {
            range: Range {
                start: Position { line, character: 4 },
                end: Position { line, character: 5 },
            },
            severity,
            code: None,
            source: None,
            message: message.to_string(),
            rest: Map::new(),
        }
    }

    #[test]
    fn report_format() {
        assert_eq!(report("a.ts", &[diag(Some(2), 0, "warn")]), "");
        assert_eq!(
            report(
                "a.ts",
                &[
                    diag(Some(1), 0, "boom"),
                    diag(Some(2), 1, "warn"),
                    diag(Some(1), 9, "bang")
                ]
            ),
            "<diagnostics file=\"a.ts\">\nERROR [1:5] boom\nERROR [10:5] bang\n</diagnostics>"
        );
        let many: Vec<Diagnostic> = (0..23).map(|i| diag(Some(1), i, "e")).collect();
        let text = report("f", &many);
        assert_eq!(text.matches("ERROR").count(), 20);
        assert!(text.ends_with("ERROR [20:5] e\n... and 3 more\n</diagnostics>"));
    }

    #[test]
    fn pretty_defaults_to_error() {
        assert_eq!(pretty(&diag(None, 2, "m")), "ERROR [3:5] m");
        assert_eq!(pretty(&diag(Some(4), 2, "m")), "HINT [3:5] m");
    }

    #[test]
    fn dedupe_and_round_trip() {
        let raw = serde_json::json!({
            "range": {"start": {"line": 1, "character": 2}, "end": {"line": 1, "character": 3}},
            "severity": 1, "message": "x", "tags": [1]
        });
        let parsed: Diagnostic = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(serde_json::to_value(&parsed).unwrap(), raw);
        assert_eq!(dedupe(vec![parsed.clone(), parsed]).len(), 1);
    }
}
