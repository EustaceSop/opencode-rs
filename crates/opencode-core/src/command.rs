use crate::config::Info as Config;
use crate::skill::{BUILTIN_LOCATION, Skill};
use indexmap::IndexMap;
use regex::{Captures, Regex};
use serde::Serialize;
use std::path::Path;
use std::sync::LazyLock;

pub const TEMPLATE_INITIALIZE: &str =
    include_str!("../../../../opencode-ts/packages/opencode/src/command/template/initialize.txt");
pub const TEMPLATE_REVIEW: &str =
    include_str!("../../../../opencode-ts/packages/opencode/src/command/template/review.txt");

pub const INIT: &str = "init";
pub const REVIEW: &str = "review";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Command,
    Mcp,
    Skill,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Command {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    pub template: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtask: Option<bool>,
    pub hints: Vec<String>,
}

static PLACEHOLDER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\$([0-9]+)").unwrap());
static ARGS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)\[Image\s+[0-9]+\]|"[^"]*"|'[^']*'|[^\s"']+"#).unwrap());
static SHELL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"!`([^`]+)`").unwrap());
static FILE_TAIL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\.?[^\s`,.]*(?:\.[^\s`,.]+)*").unwrap());

// Unique `$N` placeholders in string order (`$10` before `$2`), then `$ARGUMENTS`.
pub fn hints(template: &str) -> Vec<String> {
    let mut result: Vec<String> = PLACEHOLDER.find_iter(template).map(|m| m.as_str().to_string()).collect();
    result.sort();
    result.dedup();
    if template.contains("$ARGUMENTS") {
        result.push("$ARGUMENTS".to_string());
    }
    result
}

// Built-ins, then config commands, then extra (MCP) commands; skills only fill unused names.
pub fn registry(cfg: &Config, worktree: &Path, extra: Vec<Command>, skills: &[Skill]) -> IndexMap<String, Command> {
    let worktree = worktree.to_string_lossy();
    let mut commands: IndexMap<String, Command> = IndexMap::new();
    let builtin = |name: &str, description: &str, template: &str, subtask: Option<bool>| Command {
        name: name.to_string(),
        description: Some(description.to_string()),
        agent: None,
        model: None,
        source: Some(Source::Command),
        template: template.replacen("${path}", &worktree, 1),
        subtask,
        hints: hints(template),
    };
    commands.insert(INIT.into(), builtin(INIT, "guided AGENTS.md setup", TEMPLATE_INITIALIZE, None));
    commands.insert(
        REVIEW.into(),
        builtin(REVIEW, "review changes [commit|branch|pr], defaults to uncommitted", TEMPLATE_REVIEW, Some(true)),
    );
    for (name, command) in &cfg.command {
        commands.insert(
            name.clone(),
            Command {
                name: name.clone(),
                description: command.description.clone(),
                agent: command.agent.clone(),
                model: command.model.clone(),
                source: Some(Source::Command),
                template: command.template.clone(),
                subtask: command.subtask,
                hints: hints(&command.template),
            },
        );
    }
    for command in extra {
        commands.insert(command.name.clone(), command);
    }
    for skill in skills {
        if commands.contains_key(&skill.name) {
            continue;
        }
        let template = if skill.location == BUILTIN_LOCATION {
            skill.content.clone()
        } else {
            let dir = Path::new(&skill.location).parent().unwrap_or(Path::new("")).to_string_lossy().into_owned();
            format!(
                "{}\n\nBase directory for this skill: {dir}\nRelative paths in this skill (e.g., scripts/, references/) are relative to this base directory.",
                skill.content
            )
        };
        commands.insert(
            skill.name.clone(),
            Command {
                name: skill.name.clone(),
                description: skill.description.clone(),
                agent: None,
                model: None,
                source: Some(Source::Skill),
                template,
                subtask: None,
                hints: Vec::new(),
            },
        );
    }
    commands
}

// Tokens: `[Image N]`, quoted strings (quotes stripped), or runs of non-space characters.
pub fn parse_arguments(arguments: &str) -> Vec<String> {
    ARGS.find_iter(arguments)
        .map(|m| {
            let raw = m.as_str();
            let raw = raw.strip_prefix(['"', '\'']).unwrap_or(raw);
            raw.strip_suffix(['"', '\'']).unwrap_or(raw).to_string()
        })
        .collect()
}

// `$N` (highest N swallows the remaining args), `$ARGUMENTS`, or append when neither is used.
pub fn expand_arguments(template: &str, arguments: &str) -> String {
    let args = parse_arguments(arguments);
    let position = |caps: &Captures| caps[1].parse::<usize>().unwrap_or(usize::MAX);
    let last = PLACEHOLDER.captures_iter(template).map(|c| position(&c)).max().unwrap_or(0);
    let has_placeholders = PLACEHOLDER.is_match(template);
    let with_args = PLACEHOLDER.replace_all(template, |caps: &Captures| {
        let position = position(caps);
        if position == 0 {
            // JS quirk: index -1 yields the last arg when only `$0` is used, else the text "undefined".
            return match (last == 0, args.last()) {
                (true, Some(arg)) => arg.clone(),
                (true, None) => String::new(),
                (false, _) => "undefined".to_string(),
            };
        }
        let index = position - 1;
        if index >= args.len() {
            String::new()
        } else if position == last {
            args[index..].join(" ")
        } else {
            args[index].clone()
        }
    });
    let mut out = with_args.replace("$ARGUMENTS", arguments);
    if !has_placeholders && !template.contains("$ARGUMENTS") && !arguments.trim().is_empty() {
        out.push_str("\n\n");
        out.push_str(arguments);
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellPlaceholder {
    pub start: usize,
    pub end: usize,
    pub command: String,
}

// `` !`cmd` `` placeholders in match order; executing them is the caller's job.
pub fn shell_placeholders(template: &str) -> Vec<ShellPlaceholder> {
    SHELL
        .captures_iter(template)
        .map(|caps| {
            let whole = caps.get(0).unwrap();
            ShellPlaceholder { start: whole.start(), end: whole.end(), command: caps[1].to_string() }
        })
        .collect()
}

// Replaces each shell placeholder with the matching output (missing outputs become empty).
pub fn substitute_shell(template: &str, outputs: &[String]) -> String {
    let mut index = 0;
    SHELL
        .replace_all(template, |_: &Captures| {
            let output = outputs.get(index).cloned().unwrap_or_default();
            index += 1;
            output
        })
        .into_owned()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expanded {
    // Arguments applied, shell placeholders still present, untrimmed.
    pub text: String,
    pub shell: Vec<ShellPlaceholder>,
}

impl Expanded {
    // Final prompt text: shell outputs spliced in, then trimmed.
    pub fn finish(&self, outputs: &[String]) -> String {
        substitute_shell(&self.text, outputs).trim().to_string()
    }
}

pub fn expand(template: &str, arguments: &str) -> Expanded {
    let text = expand_arguments(template, arguments);
    let shell = shell_placeholders(&text);
    Expanded { text, shell }
}

// `@path` references: `@` not preceded by a word character or backtick.
pub fn file_references(template: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut previous: Option<char> = None;
    let mut skip_until = 0;
    for (index, ch) in template.char_indices() {
        if index >= skip_until && ch == '@' && !previous.is_some_and(|p| p.is_ascii_alphanumeric() || p == '_' || p == '`')
        {
            let tail = FILE_TAIL.find(&template[index + 1..]).map_or("", |m| m.as_str());
            out.push(tail.to_string());
            skip_until = index + 1 + tail.len();
        }
        previous = Some(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hint_order() {
        assert_eq!(hints("a $2 $10 $1 $2 $ARGUMENTS"), ["$1", "$10", "$2", "$ARGUMENTS"]);
        assert_eq!(hints("plain"), Vec::<String>::new());
        assert_eq!(hints(TEMPLATE_INITIALIZE), ["$ARGUMENTS"]);
    }

    #[test]
    fn argument_tokens() {
        assert_eq!(
            parse_arguments(r#"one "two words" 'three more' [Image 2] [image  10] fo"o"#),
            ["one", "two words", "three more", "[Image 2]", "[image  10]", "fo", "o"]
        );
        assert!(parse_arguments("   ").is_empty());
    }

    #[test]
    fn positional_expansion() {
        assert_eq!(expand_arguments("a $1 b $2", "x y z"), "a x b y z");
        assert_eq!(expand_arguments("$2 then $1", "x y z"), "y z then x");
        assert_eq!(expand_arguments("$1 $3", "x"), "x ");
        assert_eq!(expand_arguments("$1/$1", "\"a b\" c"), "a b c/a b c");
        assert_eq!(expand_arguments("$1 and $ARGUMENTS", "x \"y z\""), "x y z and x \"y z\"");
        assert_eq!(expand_arguments("say: $ARGUMENTS!", ""), "say: !");
        assert_eq!(expand_arguments("no placeholders", " extra "), "no placeholders\n\n extra ");
        assert_eq!(expand_arguments("no placeholders", "  "), "no placeholders");
        assert_eq!(expand_arguments("$1", "$2 literal"), "$2 literal");
        assert_eq!(expand_arguments("$0|$1", "a b"), "undefined|a b");
        assert_eq!(expand_arguments("$0", "a b"), "b");
    }

    #[test]
    fn shell_placeholders_are_exposed_not_run() {
        let expanded = expand("  Status:\n!`git status $1`\nand !`date`\n", "--short");
        assert_eq!(
            expanded.shell.iter().map(|s| s.command.as_str()).collect::<Vec<_>>(),
            ["git status --short", "date"]
        );
        let first = &expanded.shell[0];
        assert_eq!(&expanded.text[first.start..first.end], "!`git status --short`");
        assert_eq!(expanded.finish(&["clean".into(), "today".into()]), "Status:\nclean\nand today");
        assert_eq!(expanded.finish(&[]), "Status:\n\nand");
        assert!(expand("plain `code` !``", "").shell.is_empty());
    }

    #[test]
    fn file_reference_scan() {
        assert_eq!(
            file_references("see @src/main.rs, @./a.b.c and @~/x.md. mail a@b.c `@code` @agent\n@.env"),
            ["src/main.rs", "./a.b.c", "~/x.md", "agent", ".env"]
        );
        assert_eq!(file_references("@"), [""]);
    }

    #[test]
    fn registry_order_and_sources() {
        let cfg = Config::from_value(&json!({
            "command": {
                "review": {"template": "my review $1", "agent": "plan"},
                "deploy": {"template": "ship $ARGUMENTS", "description": "d", "model": "a/b", "subtask": false}
            }
        }))
        .unwrap();
        let skills = [
            Skill { name: "deploy".into(), description: Some("skill".into()), location: "/s/deploy/SKILL.md".into(), content: "x".into() },
            Skill { name: "lint".into(), description: Some("l".into()), location: "/s/lint/SKILL.md".into(), content: "body".into() },
            Skill { name: "builtin".into(), description: None, location: BUILTIN_LOCATION.into(), content: "raw".into() },
        ];
        let mcp = Command {
            name: "srv:prompt".into(),
            description: None,
            agent: None,
            model: None,
            source: Some(Source::Mcp),
            template: "$1".into(),
            subtask: None,
            hints: vec!["$1".into()],
        };
        let commands = registry(&cfg, Path::new("/work/tree"), vec![mcp], &skills);
        assert_eq!(commands.keys().collect::<Vec<_>>(), ["init", "review", "deploy", "srv:prompt", "lint", "builtin"]);
        assert!(commands["init"].template.contains("exists at `/work/tree`"));
        assert_eq!(commands["init"].description.as_deref(), Some("guided AGENTS.md setup"));
        assert_eq!(commands["review"].template, "my review $1");
        assert_eq!(commands["review"].subtask, None);
        assert_eq!(commands["review"].hints, ["$1"]);
        assert_eq!(commands["deploy"].source, Some(Source::Command));
        assert_eq!(commands["lint"].source, Some(Source::Skill));
        let dir = Path::new("/s/lint/SKILL.md").parent().unwrap().to_string_lossy().into_owned();
        assert_eq!(
            commands["lint"].template,
            format!("body\n\nBase directory for this skill: {dir}\nRelative paths in this skill (e.g., scripts/, references/) are relative to this base directory.")
        );
        assert_eq!(commands["builtin"].template, "raw");
        let default = registry(&Config::default(), Path::new("/w"), Vec::new(), &[]);
        assert_eq!(default["review"].subtask, Some(true));
        assert_eq!(default["review"].hints, ["$ARGUMENTS"]);
    }
}
