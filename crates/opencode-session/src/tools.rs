// Tools that need the loop: definitions and output formats for `task`, `skill` and `question`.
use opencode_core::agent::{Agent, Agents};
use opencode_core::config::AgentMode;
use opencode_core::permission::{self, Action, Rule};
use opencode_storage::{PermissionAction, PermissionRule, Store, Todo};
use opencode_tools::ToolError;
use opencode_tools::todo::TodoStore;
use serde_json::{Value, json};

const SCHEMA: &str = "https://json-schema.org/draft/2020-12/schema";

macro_rules! tool_text {
    ($name:literal) => {
        include_str!(concat!("../../../../opencode-ts/packages/opencode/src/tool/", $name))
    };
}

const TASK: &str = tool_text!("task.txt");
const SKILL: &str = tool_text!("skill.txt");
const QUESTION: &str = tool_text!("question.txt");

pub const QUESTION_REJECTED: &str = "The user dismissed this question";

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct QuestionInfo {
    pub question: String,
    pub header: String,
    pub options: Vec<QuestionOption>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multiple: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom: Option<bool>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct QuestionRequest {
    pub id: String,
    pub session_id: String,
    pub questions: Vec<QuestionInfo>,
    // (message id, call id) of the asking tool call.
    pub tool: Option<(String, String)>,
}

// What the model sees for one tool.
#[derive(Debug, Clone)]
pub struct Def {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

pub(crate) fn locale_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    a.to_lowercase().cmp(&b.to_lowercase()).then_with(|| b.cmp(a))
}

// `- name: description` for every subagent the caller may start, sorted by name.
pub(crate) fn subagent_lines(agents: &Agents, caller: &Agent) -> Vec<String> {
    let mut list: Vec<&Agent> = agents
        .iter()
        .filter(|a| a.mode != AgentMode::Primary)
        .filter(|a| permission::evaluate("task", &a.name, &[&caller.permission]).action != Action::Deny)
        .collect();
    list.sort_by(|a, b| locale_cmp(&a.name, &b.name));
    list.iter()
        .map(|a| format!("- {}: {}", a.name, a.description.as_deref().unwrap_or("This subagent should only be called manually by the user.")))
        .collect()
}

pub fn task_def(agents: &Agents, caller: &Agent) -> Def {
    let lines = subagent_lines(agents, caller);
    Def {
        name: "task".into(),
        description: format!("{TASK}\nAvailable agent types and the tools they have access to:\n{}", lines.join("\n")),
        parameters: json!({
            "$schema": SCHEMA,
            "type": "object",
            "properties": {
                "description": {"type": "string", "description": "A short (3-5 words) description of the task"},
                "prompt": {"type": "string", "description": "The task for the agent to perform"},
                "subagent_type": {"type": "string", "description": "The type of specialized agent to use for this task"},
                "task_id": {"type": "string", "description": "This should only be set if you mean to resume a previous task (you can pass a prior task_id and the task will continue the same subagent session as before instead of creating a fresh one)"},
                "command": {"type": "string", "description": "The command that triggered this task"}
            },
            "required": ["description", "prompt", "subagent_type"]
        }),
    }
}

pub fn skill_def() -> Def {
    Def {
        name: "skill".into(),
        description: SKILL.into(),
        parameters: json!({
            "$schema": SCHEMA,
            "type": "object",
            "properties": {"name": {"type": "string", "description": "The name of the skill from available_skills"}},
            "required": ["name"]
        }),
    }
}

pub fn question_def() -> Def {
    Def {
        name: "question".into(),
        description: QUESTION.into(),
        parameters: json!({
            "$schema": SCHEMA,
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "description": "Questions to ask",
                    "items": {
                        "type": "object",
                        "properties": {
                            "question": {"type": "string", "description": "Complete question"},
                            "header": {"type": "string", "description": "Very short label (max 30 chars)"},
                            "options": {
                                "type": "array",
                                "description": "Available choices",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "label": {"type": "string", "description": "Display text (1-5 words, concise)"},
                                        "description": {"type": "string", "description": "Explanation of choice"}
                                    },
                                    "required": ["label", "description"]
                                }
                            },
                            "multiple": {"type": "boolean", "description": "Allow selecting multiple choices"}
                        },
                        "required": ["question", "header", "options"]
                    }
                }
            },
            "required": ["questions"]
        }),
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct TaskParams {
    pub description: String,
    pub prompt: String,
    pub subagent_type: String,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub background: Option<bool>,
}

pub fn task_output(session_id: &str, text: &str) -> String {
    [&format!("<task id=\"{session_id}\" state=\"completed\">"), "<task_result>", text, "</task_result>", "</task>"].join("\n")
}

pub fn skill_output(name: &str, content: &str, dir: &str, files: &[String]) -> String {
    [
        &format!("<skill_content name=\"{name}\">"),
        &format!("# Skill: {name}"),
        "",
        content.trim(),
        "",
        &format!("Base directory for this skill: {dir}"),
        "Relative paths in this skill (e.g., scripts/, reference/) are relative to this base directory.",
        "Note: file list is sampled.",
        "",
        "<skill_files>",
        &files.iter().map(|file| format!("<file>{file}</file>")).collect::<Vec<_>>().join("\n"),
        "</skill_files>",
        "</skill_content>",
    ]
    .join("\n")
}

pub fn question_output(questions: &[QuestionInfo], answers: &[Vec<String>]) -> String {
    let formatted: Vec<String> = questions
        .iter()
        .enumerate()
        .map(|(i, q)| {
            let answer = answers.get(i).filter(|a| !a.is_empty()).map_or_else(|| "Unanswered".to_string(), |a| a.join(", "));
            format!("\"{}\"=\"{answer}\"", q.question)
        })
        .collect();
    format!("User has answered your questions: {}. You can now continue with the user's answers in mind.", formatted.join(", "))
}

pub fn to_rules(rules: &[PermissionRule]) -> Vec<Rule> {
    rules
        .iter()
        .map(|r| {
            let action = match r.action {
                PermissionAction::Allow => Action::Allow,
                PermissionAction::Deny => Action::Deny,
                PermissionAction::Ask => Action::Ask,
            };
            Rule::new(&r.permission, &r.pattern, action)
        })
        .collect()
}

fn deny(permission: &str) -> PermissionRule {
    PermissionRule { permission: permission.into(), pattern: "*".into(), action: PermissionAction::Deny }
}

// Session rules of a subagent session spawned by the task tool.
pub fn subagent_permission(parent: &[PermissionRule], subagent: &Agent, primary_tools: &[String]) -> Vec<PermissionRule> {
    let mentions = |name: &str| subagent.permission.iter().any(|r| r.permission == name);
    let mut rules: Vec<PermissionRule> =
        parent.iter().filter(|r| r.permission == "external_directory" || r.action == PermissionAction::Deny).cloned().collect();
    let base = [(!mentions("todowrite")).then(|| deny("todowrite")), (!mentions("task")).then(|| deny("task"))];
    rules.extend(base.into_iter().flatten());
    let derived = rules.clone();
    rules.extend(primary_tools.iter().map(|tool| deny(tool)).filter(|rule| !derived.contains(rule)));
    rules
}

pub struct StoreTodos {
    pub store: Store,
}

#[async_trait::async_trait]
impl TodoStore for StoreTodos {
    async fn update(&self, session_id: &str, todos: &[opencode_tools::todo::Todo]) -> Result<(), ToolError> {
        let rows: Vec<Todo> =
            todos.iter().map(|t| Todo { content: t.content.clone(), status: t.status.clone(), priority: t.priority.clone() }).collect();
        self.store.set_todos(session_id, &rows).map_err(|e| ToolError::Other(e.to_string()))
    }
}
