use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{LazyLock, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Allow,
    Deny,
    Ask,
}

impl Action {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "allow" => Some(Self::Allow),
            "deny" => Some(Self::Deny),
            "ask" => Some(Self::Ask),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Rule {
    pub permission: String,
    pub pattern: String,
    pub action: Action,
}

impl Rule {
    pub fn new(permission: impl Into<String>, pattern: impl Into<String>, action: Action) -> Self {
        Self { permission: permission.into(), pattern: pattern.into(), action }
    }
}

pub type Ruleset = Vec<Rule>;

static CACHE: LazyLock<Mutex<HashMap<String, Option<Regex>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn compile(pattern: &str) -> Option<Regex> {
    let mut escaped = String::with_capacity(pattern.len() + 8);
    for c in pattern.chars() {
        match c {
            '\\' => escaped.push('/'),
            '*' => escaped.push_str(".*"),
            '?' => escaped.push('.'),
            '.' | '+' | '^' | '$' | '{' | '}' | '(' | ')' | '|' | '[' | ']' => {
                escaped.push('\\');
                escaped.push(c);
            }
            _ => escaped.push(c),
        }
    }
    if escaped.ends_with(" .*") {
        escaped.truncate(escaped.len() - 3);
        escaped.push_str("( .*)?");
    }
    let flags = if cfg!(windows) { "(?si)" } else { "(?s)" };
    Regex::new(&format!("{flags}^{escaped}$")).ok()
}

// core/src/util/wildcard.ts `match`: `*` crosses separators, a trailing " *" is optional.
pub fn wildcard_match(input: &str, pattern: &str) -> bool {
    let regex = {
        let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if cache.len() > 4096 {
            cache.clear();
        }
        cache.entry(pattern.to_string()).or_insert_with(|| compile(pattern)).clone()
    };
    regex.is_some_and(|r| r.is_match(&input.replace('\\', "/")))
}

// Last matching rule across all rulesets wins; no match yields `ask` with pattern "*".
pub fn evaluate(permission: &str, pattern: &str, rulesets: &[&[Rule]]) -> Rule {
    rulesets
        .iter()
        .rev()
        .flat_map(|set| set.iter().rev())
        .find(|rule| wildcard_match(permission, &rule.permission) && wildcard_match(pattern, &rule.pattern))
        .cloned()
        .unwrap_or_else(|| Rule::new(permission, "*", Action::Ask))
}

fn expand(pattern: &str, home: &str) -> String {
    if let Some(rest) = pattern.strip_prefix("~/") {
        return format!("{home}/{rest}");
    }
    if pattern == "~" {
        return home.to_string();
    }
    match pattern.strip_prefix("$HOME") {
        Some(rest) => format!("{home}{rest}"),
        None => pattern.to_string(),
    }
}

// Rules in object-entry order; entries that are neither an action nor a pattern map are skipped.
pub fn from_config(permission: &Map<String, Value>, home: &Path) -> Ruleset {
    let home = home.to_string_lossy();
    let mut rules = Vec::new();
    for (key, value) in permission {
        match value {
            Value::String(action) => {
                if let Some(action) = Action::parse(action) {
                    rules.push(Rule::new(key, "*", action));
                }
            }
            Value::Object(patterns) => {
                for (pattern, action) in patterns {
                    if let Some(action) = action.as_str().and_then(Action::parse) {
                        rules.push(Rule::new(key, expand(pattern, &home), action));
                    }
                }
            }
            _ => {}
        }
    }
    rules
}

pub fn merge(rulesets: &[&[Rule]]) -> Ruleset {
    rulesets.iter().flat_map(|set| set.iter().cloned()).collect()
}

fn permission_key(tool: &str) -> &str {
    match tool {
        "edit" | "write" | "apply_patch" => "edit",
        "list_mcp_resources" | "list_mcp_resource_templates" | "read_mcp_resource" => "read",
        other => other,
    }
}

// Tools hidden from the model: last rule matching the permission key is a blanket deny.
pub fn disabled<'a>(tools: impl IntoIterator<Item = &'a str>, ruleset: &[Rule]) -> HashSet<String> {
    tools
        .into_iter()
        .filter(|tool| {
            let key = permission_key(tool);
            ruleset
                .iter()
                .rev()
                .find(|rule| wildcard_match(key, &rule.permission))
                .is_some_and(|rule| rule.pattern == "*" && rule.action == Action::Deny)
        })
        .map(str::to_string)
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Allow,
    Ask,
    // Carries the ruleset entries relevant to the permission, as in `PermissionDeniedError`.
    Deny(Ruleset),
}

// In-memory "always" approvals; appended after the caller's ruleset on every check.
#[derive(Debug, Default)]
pub struct Approvals {
    rules: Mutex<Ruleset>,
}

impl Approvals {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn rules(&self) -> Ruleset {
        self.rules.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn approve<'a>(&self, permission: &str, always: impl IntoIterator<Item = &'a str>) {
        let mut rules = self.rules.lock().unwrap_or_else(|e| e.into_inner());
        rules.extend(always.into_iter().map(|pattern| Rule::new(permission, pattern, Action::Allow)));
    }

    // `Permission.ask` up to the point of prompting: any deny wins, else ask if any pattern asks.
    pub fn check(&self, permission: &str, patterns: &[&str], ruleset: &[Rule]) -> Decision {
        let approved = self.rules();
        let mut ask = false;
        for pattern in patterns {
            match evaluate(permission, pattern, &[ruleset, &approved]).action {
                Action::Deny => {
                    let relevant =
                        ruleset.iter().filter(|r| wildcard_match(permission, &r.permission)).cloned().collect();
                    return Decision::Deny(relevant);
                }
                Action::Allow => {}
                Action::Ask => ask = true,
            }
        }
        if ask { Decision::Ask } else { Decision::Allow }
    }

    // After an "always" reply, a pending request resolves iff approvals alone allow every pattern.
    pub fn covers(&self, permission: &str, patterns: &[&str]) -> bool {
        let approved = self.rules();
        patterns.iter().all(|pattern| evaluate(permission, pattern, &[&approved]).action == Action::Allow)
    }
}

pub const REJECTED_MESSAGE: &str = "The user rejected permission to use this specific tool call.";

pub fn corrected_message(feedback: &str) -> String {
    format!("The user rejected permission to use this specific tool call with the following feedback: {feedback}")
}

pub fn denied_message(ruleset: &[Rule]) -> String {
    format!(
        "The user has specified a rule which prevents you from using this specific tool call. Here are some of the relevant rules {}",
        serde_json::to_string(ruleset).unwrap_or_default()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    fn cfg(value: Value) -> Ruleset {
        from_config(value.as_object().unwrap(), &PathBuf::from("/home/u"))
    }

    fn rules(list: &[(&str, &str, Action)]) -> Ruleset {
        list.iter().map(|(p, pat, a)| Rule::new(*p, *pat, *a)).collect()
    }

    fn eval(permission: &str, pattern: &str, list: &[(&str, &str, Action)]) -> Action {
        evaluate(permission, pattern, &[&rules(list)]).action
    }

    use Action::{Allow, Ask, Deny};

    #[test]
    fn wildcard_glob_tokens() {
        assert!(wildcard_match("file1.txt", "file?.txt"));
        assert!(!wildcard_match("file12.txt", "file?.txt"));
        assert!(wildcard_match("foo+bar", "foo+bar"));
        assert!(!wildcard_match("fooXbar", "foo.bar"));
        assert!(wildcard_match("a[1]{2}(3)|$^", "a[1]{2}(3)|$^"));
        assert!(wildcard_match("src/a/b.ts", "src/*"));
        assert!(wildcard_match("line1\nline2", "line1*"));
        assert!(wildcard_match("C:\\Users\\x\\f.txt", "C:/Users/*"));
        assert!(wildcard_match("C:/Users/x/f.txt", "C:\\Users\\*"));
        assert_eq!(wildcard_match("LS", "ls"), cfg!(windows));
    }

    #[test]
    fn wildcard_trailing_space_star() {
        assert!(wildcard_match("ls", "ls *"));
        assert!(wildcard_match("ls -la", "ls *"));
        assert!(wildcard_match("ls foo bar", "ls *"));
        assert!(wildcard_match("ls", "ls*"));
        assert!(wildcard_match("lstmeval", "ls*"));
        assert!(!wildcard_match("lstmeval", "ls *"));
        assert!(wildcard_match("git status", "git *"));
        assert!(wildcard_match("git", "git *"));
        assert!(wildcard_match("git commit -m foo", "git *"));
        assert!(!wildcard_match("git", "git * x"));
    }

    #[test]
    fn from_config_shapes() {
        assert_eq!(cfg(json!({"bash": "allow"})), rules(&[("bash", "*", Allow)]));
        assert_eq!(
            cfg(json!({"bash": {"*": "allow", "rm": "deny"}, "edit": "allow", "webfetch": "ask"})),
            rules(&[("bash", "*", Allow), ("bash", "rm", Deny), ("edit", "*", Allow), ("webfetch", "*", Ask)])
        );
        assert_eq!(cfg(json!({})), vec![]);
        assert_eq!(cfg(json!({"bash": 3, "edit": "nope", "read": {"*": 1}})), vec![]);
    }

    #[test]
    fn from_config_home_expansion() {
        let one = |pattern: &str| cfg(json!({"external_directory": {pattern: "allow"}}))[0].pattern.clone();
        assert_eq!(one("~/projects/*"), "/home/u/projects/*");
        assert_eq!(one("$HOME/projects/*"), "/home/u/projects/*");
        assert_eq!(one("$HOME"), "/home/u");
        assert_eq!(one("$HOMEx"), "/home/ux");
        assert_eq!(one("~"), "/home/u");
        assert_eq!(one("/some/~/path"), "/some/~/path");
        let set = cfg(json!({"external_directory": {"~/projects/*": "allow"}}));
        assert_eq!(evaluate("external_directory", "/home/u/projects/file.txt", &[&set]).action, Allow);
    }

    #[test]
    fn from_config_order_is_precedence() {
        let wildcard_first = cfg(json!({"*": "deny", "bash": "allow"}));
        let specific_first = cfg(json!({"bash": "allow", "*": "deny"}));
        assert_eq!(evaluate("bash", "ls", &[&wildcard_first]).action, Allow);
        assert_eq!(evaluate("bash", "ls", &[&specific_first]).action, Deny);

        let set = cfg(json!({"bash": "allow", "*": "ask", "edit": "deny", "mcp_*": "allow"}));
        assert_eq!(set.iter().map(|r| r.permission.as_str()).collect::<Vec<_>>(), ["bash", "*", "edit", "mcp_*"]);

        let set = cfg(json!({"bash": {"*": "deny", "git *": "allow"}}));
        assert_eq!(evaluate("bash", "rm foo", &[&set]).action, Deny);
        assert_eq!(evaluate("bash", "git status", &[&set]).action, Allow);

        let set = cfg(json!({"*": "ask", "bash": "allow", "edit": "deny"}));
        assert_eq!(evaluate("bash", "ls", &[&set]).action, Allow);
        assert_eq!(evaluate("edit", "foo.ts", &[&set]).action, Deny);
        assert_eq!(evaluate("read", "foo.ts", &[&set]).action, Ask);
    }

    #[test]
    fn evaluate_patterns() {
        assert_eq!(eval("bash", "rm", &[("bash", "rm", Deny)]), Deny);
        assert_eq!(eval("bash", "rm", &[("bash", "*", Allow)]), Allow);
        assert_eq!(eval("bash", "rm", &[("bash", "*", Allow), ("bash", "rm", Deny)]), Deny);
        assert_eq!(eval("bash", "rm", &[("bash", "rm", Deny), ("bash", "*", Allow)]), Allow);
        assert_eq!(eval("edit", "src/foo.ts", &[("edit", "src/*", Allow)]), Allow);
        assert_eq!(
            eval("edit", "src/components/Button.tsx", &[("edit", "src/*", Deny), ("edit", "src/components/*", Allow)]),
            Allow
        );
        assert_eq!(
            eval("edit", "src/components/Button.tsx", &[("edit", "src/components/*", Allow), ("edit", "src/*", Deny)]),
            Deny
        );
        assert_eq!(eval("unknown_tool", "anything", &[("bash", "*", Allow)]), Ask);
        assert_eq!(eval("bash", "rm", &[]), Ask);
        assert_eq!(eval("edit", "etc/passwd", &[("edit", "src/*", Allow)]), Ask);
        assert_eq!(
            eval("edit", "src/secret.ts", &[("edit", "*", Ask), ("edit", "src/*", Allow), ("edit", "src/secret.ts", Deny)]),
            Deny
        );
        assert_eq!(
            eval("edit", "src/foo.ts", &[("edit", "*", Ask), ("edit", "test/*", Deny), ("edit", "src/*", Allow)]),
            Allow
        );
        assert_eq!(eval("bash", "/bin/rm", &[("bash", "*", Allow), ("bash", "/bin/rm", Deny)]), Deny);
        assert_eq!(eval("bash", "/bin/rm", &[("bash", "/bin/rm", Deny), ("bash", "*", Allow)]), Allow);
    }

    #[test]
    fn evaluate_wildcard_permissions() {
        assert_eq!(eval("bash", "rm", &[("*", "*", Deny)]), Deny);
        assert_eq!(eval("bash", "rm", &[("*", "rm", Deny)]), Deny);
        assert_eq!(eval("mcp_server_tool", "anything", &[("mcp_*", "*", Allow)]), Allow);
        assert_eq!(eval("bash", "rm", &[("*", "*", Deny), ("bash", "*", Allow)]), Allow);
        assert_eq!(eval("edit", "src/foo.ts", &[("*", "*", Deny), ("edit", "src/*", Allow)]), Allow);
        assert_eq!(
            eval("mcp_dangerous", "anything", &[("*", "*", Ask), ("mcp_*", "*", Allow), ("mcp_dangerous", "*", Deny)]),
            Deny
        );
        assert_eq!(eval("unknown_tool", "anything", &[("*", "*", Ask), ("bash", "*", Allow)]), Ask);
        assert_eq!(eval("bash", "rm", &[("bash", "*", Allow), ("*", "*", Deny)]), Deny);
    }

    #[test]
    fn evaluate_default_and_multiple_rulesets() {
        let config = rules(&[("bash", "*", Allow)]);
        let approved = rules(&[("bash", "rm", Deny)]);
        assert_eq!(evaluate("bash", "rm", &[&config, &approved]).action, Deny);
        assert_eq!(evaluate("bash", "rm", &[&approved, &config]).action, Allow);
        assert_eq!(evaluate("x", "y", &[]), Rule::new("x", "*", Ask));
        let merged = merge(&[&config, &approved]);
        assert_eq!(merged.len(), 2);
        assert_eq!(evaluate("bash", "rm", &[&merged]).action, Deny);
    }

    #[test]
    fn disabled_tools() {
        let check = |tools: &[&str], list: &[(&str, &str, Action)]| disabled(tools.iter().copied(), &rules(list));
        assert!(check(&["bash", "edit", "read"], &[("*", "*", Allow)]).is_empty());
        let set = check(&["bash", "edit", "read"], &[("*", "*", Allow), ("bash", "*", Deny)]);
        assert!(set.contains("bash") && set.len() == 1);
        let set = check(&["edit", "write", "apply_patch", "bash"], &[("*", "*", Allow), ("edit", "*", Deny)]);
        assert!(set.contains("edit") && set.contains("write") && set.contains("apply_patch") && !set.contains("bash"));
        assert!(check(&["bash"], &[("bash", "*", Allow), ("bash", "rm *", Deny)]).is_empty());
        assert!(check(&["bash", "edit"], &[("*", "*", Ask)]).is_empty());
        assert!(check(&["bash"], &[("bash", "*", Deny), ("bash", "echo *", Allow)]).is_empty());
        assert!(check(&["bash"], &[("bash", "rm *", Deny), ("bash", "*", Allow)]).is_empty());
        assert_eq!(check(&["bash", "edit", "read"], &[("*", "*", Deny)]).len(), 3);
        let set = check(&["bash", "edit", "read"], &[("*", "*", Deny), ("bash", "*", Allow)]);
        assert!(!set.contains("bash") && set.contains("edit") && set.contains("read"));
        let set = check(&["read_mcp_resource", "list_mcp_resources"], &[("read", "*", Deny)]);
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn approvals_flow() {
        let approvals = Approvals::new();
        let set = rules(&[("bash", "*", Ask), ("bash", "rm *", Deny), ("read", "*", Allow)]);
        assert_eq!(approvals.check("read", &["a.txt"], &set), Decision::Allow);
        assert_eq!(approvals.check("bash", &["ls"], &set), Decision::Ask);
        assert_eq!(approvals.check("bash", &[], &set), Decision::Allow);
        match approvals.check("bash", &["ls", "rm -rf /"], &set) {
            Decision::Deny(relevant) => assert_eq!(relevant.len(), 2),
            other => panic!("unexpected {other:?}"),
        }
        assert!(!approvals.covers("bash", &["ls -la"]));
        approvals.approve("bash", ["ls *"]);
        assert_eq!(approvals.check("bash", &["ls -la"], &set), Decision::Allow);
        assert_eq!(approvals.check("bash", &["ls -la", "cat x"], &set), Decision::Ask);
        assert!(approvals.covers("bash", &["ls", "ls -la"]));
        assert!(!approvals.covers("edit", &["ls"]));
        // Approvals are appended last, so an "always" overrides a configured deny.
        approvals.approve("bash", ["rm *"]);
        assert_eq!(approvals.check("bash", &["rm x"], &set), Decision::Allow);
        assert_eq!(approvals.rules().len(), 2);
    }

    #[test]
    fn messages() {
        let text = denied_message(&rules(&[("bash", "*", Deny)]));
        assert!(text.ends_with(r#"relevant rules [{"permission":"bash","pattern":"*","action":"deny"}]"#));
        assert!(corrected_message("no").ends_with("feedback: no"));
    }
}
