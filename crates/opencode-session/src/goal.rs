// Rust build: `/goal` from opencode 2.x (`packages/core/src/plugin/goal.ts`, prompts in `plugin/goal/*.txt`, copied to
// `prompts/goal/`). When a run with an active goal ends, a reviewer reads the conversation and decides whether the
// objective is met; if not, a continuation message keeps the session working, up to `MAX_ROUNDS` rounds.
// Not ported: persistence across restarts (goals live in memory; v2 pauses a goal its process left behind), the
// 30-minute wait limit, and compacting when the reviewer's request is too large (that pauses the goal here).
use crate::history::{self, Row};
use crate::run::{Hooks, chat_hooks, system_transform};
use crate::{CommandInput, Ctx, Engine, Error, Host, PartInput, Submission, mcp, parse_model, profile, system, tools};
use opencode_core::command::Command;
use opencode_core::permission;
use opencode_llm::{self as llm, Event, Request, client, shape};
use opencode_storage::{Message, PartBody, Session, id};
use serde_json::json;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub const MAX_OBJECTIVE: usize = 4_000;
pub const MAX_ROUNDS: u32 = 50;
pub const MAX_STALLS: u32 = 3;
const USAGE: &str = "Usage: /goal <objective> | pause | resume | clear";
const JUDGE: &str = include_str!("../prompts/goal/judge.txt");
const CONTINUATION: &str = include_str!("../prompts/goal/continuation.txt");
const KICKOFF: &str = include_str!("../prompts/goal/kickoff.txt");
const REMINDER: &str = include_str!("../prompts/goal/reminder.txt");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Active,
    Paused,
    Complete,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Goal {
    pub id: String,
    pub objective: String,
    pub status: Status,
    pub rounds: u32,
    pub max_rounds: u32,
    pub stalls: u32,
    pub reason: Option<String>,
    // The kickoff or latest continuation message; tool calls after it count as progress.
    marker: Option<String>,
}

#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
struct Verdict {
    verdict: String,
    reason: String,
    #[serde(default)]
    next_step: Option<String>,
}

// The built-in command as the command list shows it.
pub(crate) fn command() -> Command {
    Command {
        name: "goal".into(),
        description: Some("keep working until an objective is met [pause|resume|clear]".into()),
        agent: None,
        model: None,
        source: None,
        template: String::new(),
        subtask: None,
        hints: Vec::new(),
    }
}

// One pass, so an objective that contains a placeholder is not expanded again.
fn render(template: &str, values: &[(&str, &str)]) -> String {
    let mut out = String::new();
    let mut rest = template;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after.find('}').filter(|end| after[..*end].chars().all(|c| c.is_alphanumeric() || c == '_'));
        match end.and_then(|end| values.iter().find(|(key, _)| *key == &after[..end]).map(|(_, value)| (end, value))) {
            Some((end, value)) => {
                out.push_str(value);
                rest = &after[end + 1..];
            }
            None => {
                out.push_str("${");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

// `/goal "Ship the release"` and `/goal Ship the release` set the same objective.
fn unquote(text: &str) -> String {
    let mut chars = text.chars();
    let (Some(first), Some(last)) = (chars.next(), text.chars().last()) else { return text.to_string() };
    let close = match first {
        '"' => '"',
        '\'' => '\'',
        '\u{201c}' => '\u{201d}',
        '\u{2018}' => '\u{2019}',
        _ => return text.to_string(),
    };
    if text.chars().count() < 2 || last != close {
        return text.to_string();
    }
    let inner: String = text.chars().skip(1).take(text.chars().count() - 2).collect();
    inner.trim().to_string()
}

fn truncate(text: &str, length: usize) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() > length { format!("{}…", line.chars().take(length - 1).collect::<String>()) } else { line }
}

fn describe(goal: &Goal) -> String {
    let status = match goal.status {
        Status::Active => "Goal active",
        Status::Paused => "Goal paused",
        Status::Complete => "Goal achieved",
    };
    std::iter::once(format!("{status}: {}", goal.objective)).chain(goal.reason.clone()).filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n")
}

fn same_reason(previous: Option<&str>, next: &str) -> bool {
    let normalize = |text: &str| text.trim().to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ");
    previous.is_some_and(|previous| normalize(previous) == normalize(next))
}

// Reviewers sometimes wrap the JSON in a code fence or add a sentence around it.
fn parse_verdict(text: &str) -> Option<Verdict> {
    let start = text.find('{')?;
    let end = text.rfind('}').filter(|end| *end >= start)?;
    let verdict: Verdict = serde_json::from_str(&text[start..=end]).ok()?;
    ["done", "continue", "blocked", "paused", "wait"].contains(&verdict.verdict.as_str()).then_some(verdict)
}

fn continuation(goal: &Goal, feedback: &[Option<String>]) -> String {
    let feedback = feedback.iter().flatten().filter(|s| !s.is_empty()).cloned().collect::<Vec<_>>().join("\n");
    render(CONTINUATION, &[("objective", &escape(&goal.objective)), ("feedback", &feedback)])
}

fn objective_block(objective: &str) -> String {
    format!("<untrusted_objective>\n{}\n</untrusted_objective>", escape(objective))
}

// Until the next continuation restates the objective, a compacted history may not show it: add the reminder near
// the tail of the request, without storing it.
pub(crate) fn remind(messages: &mut Vec<llm::Message>, goal: &Goal) {
    let block = objective_block(&goal.objective);
    let text = |message: &llm::Message| match message {
        llm::Message::User(text) => text.clone(),
        llm::Message::UserParts(parts) => parts
            .iter()
            .filter_map(|p| match p {
                llm::Part::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    };
    if messages.iter().any(|m| text(m).contains(&block)) {
        return;
    }
    let user = |m: &llm::Message| matches!(m, llm::Message::User(_) | llm::Message::UserParts(_));
    let at = if messages.last().is_some_and(user) { messages.len() - 1 } else { messages.len() };
    messages.insert(at, llm::Message::User(render(REMINDER, &[("objective", &escape(&goal.objective))])));
}

impl Engine {
    pub(crate) fn active_goal(&self, session_id: &str) -> Option<Goal> {
        self.goals.lock().unwrap_or_else(|e| e.into_inner()).get(session_id).filter(|g| g.status == Status::Active).cloned()
    }

    /// The session's goal, if any.
    pub fn goal(&self, session_id: &str) -> Option<Goal> {
        self.goals.lock().unwrap_or_else(|e| e.into_inner()).get(session_id).cloned()
    }

    fn set_goal(&self, session_id: &str, goal: Option<Goal>) {
        let mut goals = self.goals.lock().unwrap_or_else(|e| e.into_inner());
        match goal {
            Some(goal) => goals.insert(session_id.to_string(), goal),
            None => goals.remove(session_id),
        };
    }

    // The loop's own writes only apply to the goal they were decided on: a `/goal pause`, `clear` or new goal wins.
    fn replace_goal(&self, session_id: &str, expected: &Goal, next: Goal) -> bool {
        let mut goals = self.goals.lock().unwrap_or_else(|e| e.into_inner());
        match goals.get(session_id) {
            Some(current) if current.id == expected.id && current.status == expected.status && current.rounds == expected.rounds => {
                goals.insert(session_id.to_string(), next);
                true
            }
            _ => false,
        }
    }

    fn settle_goal(&self, session_id: &str, goal: &Goal, status: Status, reason: &str) {
        self.replace_goal(session_id, goal, Goal { status, reason: Some(reason.to_string()), ..goal.clone() });
    }

    // A run of a session with an active goal ended with an error or an abort.
    pub(crate) fn goal_interrupted(&self, session_id: &str, error: &Error) {
        if let Some(goal) = self.active_goal(session_id) {
            let reason = match error {
                Error::Aborted => "Stopped by the user".to_string(),
                other => format!("The session failed: {other}"),
            };
            self.settle_goal(session_id, &goal, Status::Paused, &reason);
        }
    }

    // `/goal <objective> | pause | resume | clear`.
    pub(crate) async fn goal_command(&self, ctx: Arc<Ctx>, input: CommandInput, host: Arc<dyn Host>, abort: CancellationToken) -> Result<Session, Error> {
        let session = self.open_session(&ctx, input.session_id.as_deref())?;
        let text = unquote(input.arguments.trim());
        let control = text.to_lowercase();
        let goal = self.goal(&session.id);
        if text.is_empty() {
            return Err(Error::Config(match &goal {
                Some(goal) => format!("{}\n{USAGE}", describe(goal)),
                None => USAGE.to_string(),
            }));
        }
        let submission = |text: String, message_id: String, files: Vec<opencode_storage::FilePart>| -> Result<Submission, Error> {
            let mut parts = vec![PartInput::Text { text, synthetic: true }];
            parts.extend(files.into_iter().map(PartInput::File));
            Ok(Submission {
                agent: input.agent.clone(),
                model: input.model.as_deref().map(parse_model).transpose()?,
                variant: input.variant.clone(),
                parts,
                message_id: Some(message_id),
                ..Default::default()
            })
        };
        match control.as_str() {
            "pause" => {
                let Some(goal) = goal.filter(|g| g.status == Status::Active) else { return Err(Error::Config("No active goal to pause.".into())) };
                self.set_goal(&session.id, Some(Goal { status: Status::Paused, reason: Some("Paused by the user".into()), ..goal }));
                Ok(session)
            }
            "resume" => {
                let Some(goal) = goal.filter(|g| g.status == Status::Paused) else { return Err(Error::Config("No paused goal to resume.".into())) };
                let message_id = id::message();
                // The resume continuation is the first round of the new budget.
                let resumed = Goal { status: Status::Active, rounds: 1, stalls: 0, reason: None, marker: Some(message_id.clone()), ..goal };
                let text = continuation(&resumed, &[Some("The user resumed the goal.".into())]);
                self.set_goal(&session.id, Some(resumed));
                let submission = submission(text, message_id, Vec::new())?;
                self.submit(ctx, session, submission, host, abort).await
            }
            "clear" => {
                if goal.is_none() {
                    return Err(Error::Config("No goal is set.".into()));
                }
                self.set_goal(&session.id, None);
                Ok(session)
            }
            _ => {
                let length = text.chars().count();
                if length > MAX_OBJECTIVE {
                    return Err(Error::Config(format!("A goal objective is limited to {MAX_OBJECTIVE} characters (got {length}).")));
                }
                let message_id = id::message();
                let created = Goal {
                    id: id::ascending(id::Prefix::Message),
                    objective: text.clone(),
                    status: Status::Active,
                    rounds: 0,
                    max_rounds: MAX_ROUNDS,
                    stalls: 0,
                    reason: None,
                    marker: Some(message_id.clone()),
                };
                self.set_goal(&session.id, Some(created));
                // Titles come from the first real user message, and the kickoff is not one, so name the session here.
                if session.has_default_title() && session.parent_id.is_none() {
                    let title = truncate(&text, 60);
                    self.store.patch_session(&session.id, |s| s.title = title.clone())?;
                }
                let kickoff = render(KICKOFF, &[("objective", &escape(&text))]);
                let submission = submission(kickoff, message_id, input.files)?;
                self.submit(ctx, session, submission, host, abort).await
            }
        }
    }

    // Whether the model called any tool since the goal's kickoff or latest continuation.
    fn used_tools(&self, rows: &[Row], goal: &Goal) -> bool {
        let Some(start) = goal.marker.as_deref().and_then(|marker| rows.iter().position(|(m, _)| m.id() == marker)) else { return true };
        rows[start + 1..].iter().any(|(m, parts)| matches!(m, Message::Assistant(_)) && parts.iter().any(|p| matches!(p.body, PartBody::Tool(_))))
    }

    // The idle hook: the run ended with nothing pending. True when a continuation was posted and the loop should go on.
    pub(crate) async fn goal_round(&self, ctx: &Arc<Ctx>, session_id: &str, abort: &CancellationToken) -> Result<bool, Error> {
        let Some(goal) = self.active_goal(session_id) else { return Ok(false) };
        if goal.rounds >= goal.max_rounds {
            self.settle_goal(session_id, &goal, Status::Paused, &format!("Reached the limit of {} rounds", goal.max_rounds));
            return Ok(false);
        }
        let shells = opencode_tools::bash::background::running(session_id);
        let background = if shells.is_empty() {
            "No background shells are running.".to_string()
        } else {
            let mut lines = vec!["Background shells still running:".to_string()];
            lines.extend(shells.iter().map(|command| format!("- {command}")));
            lines.push(String::new());
            lines.push("Answer \"wait\" only when the goal needs the result of one of these shells. Never wait for a long-running server such as a dev server.".into());
            lines.join("\n")
        };
        let wait = if shells.is_empty() { "" } else { "{\"verdict\":\"wait\",\"reason\":\"<the running shell whose result the goal needs>\"}\n" };
        let prompt = render(JUDGE, &[("objective", &escape(&goal.objective)), ("background", &background), ("wait", wait)]);
        let mut verdict = None;
        for _ in 0..2 {
            let text = tokio::select! {
                text = self.session_generate(ctx, session_id, &prompt) => text,
                _ = abort.cancelled() => return Err(Error::Aborted),
            };
            let text = match text {
                Ok(text) => text,
                Err(Error::Llm(error)) if opencode_llm::retry::classify(&error, "").overflow => {
                    self.settle_goal(session_id, &goal, Status::Paused, "The goal reviewer's request is too large for the model");
                    return Ok(false);
                }
                Err(error) => return Err(error),
            };
            // "wait" is only offered while a shell runs; without one, nothing would wake the session to judge again.
            verdict = parse_verdict(&text).filter(|v| v.verdict != "wait" || !shells.is_empty());
            if verdict.is_some() {
                break;
            }
        }
        // `/goal pause`, `clear` or a new goal may have landed while the reviewer ran.
        let Some(latest) = self.active_goal(session_id).filter(|g| g.id == goal.id) else { return Ok(false) };
        let Some(verdict) = verdict else {
            self.settle_goal(session_id, &latest, Status::Paused, "The goal reviewer gave no usable verdict");
            return Ok(false);
        };
        match verdict.verdict.as_str() {
            "wait" => return Ok(false),
            "done" => {
                self.settle_goal(session_id, &latest, Status::Complete, &verdict.reason);
                return Ok(false);
            }
            "continue" => {}
            _ => {
                self.settle_goal(session_id, &latest, Status::Paused, &verdict.reason);
                return Ok(false);
            }
        }
        let rows = self.store.list_messages(session_id)?;
        let stalled = same_reason(latest.reason.as_deref(), &verdict.reason) || !self.used_tools(&rows, &latest);
        let stalls = if stalled { latest.stalls + 1 } else { 0 };
        if stalls >= MAX_STALLS {
            self.settle_goal(session_id, &latest, Status::Paused, &format!("Not making progress: {}", verdict.reason));
            return Ok(false);
        }
        let mut next = Goal { rounds: latest.rounds + 1, stalls, reason: Some(verdict.reason.clone()), ..latest.clone() };
        let text = continuation(&next, &[Some(verdict.reason), verdict.next_step.map(|step| format!("Next step: {step}"))]);
        let user = self.post_synthetic(session_id, &text, json!({"source": "goal", "goalID": next.id}))?;
        next.marker = Some(user.id);
        Ok(self.replace_goal(session_id, &latest, next))
    }

    // v2 `SessionGenerate.generate`: one answer from the session's current context, without changing its history.
    pub(crate) async fn session_generate(&self, ctx: &Arc<Ctx>, session_id: &str, prompt: &str) -> Result<String, Error> {
        let session = self.session(session_id)?;
        let rows = history::filter_compacted(self.store.list_messages(session_id)?);
        let latest = history::latest(&rows);
        let user = latest.user.ok_or_else(|| Error::Config("The session has no messages yet".into()))?;
        let (provider, model) = ctx.model(&user.model.provider_id, &user.model.model_id)?;
        let agent = ctx.agent(&user.agent)?;
        let manager = self.mcp_for(ctx).await;
        let session_rules = tools::to_rules(session.permission.as_deref().unwrap_or_default());
        let ruleset = permission::merge(&[&agent.permission, &session_rules]);
        let mcp_tools = if ctx.flags.experimental_code_mode { Default::default() } else { manager.mcp_tools() };
        let mut defs = Self::definitions(ctx, agent, model, &ruleset, user.tools.as_ref(), &manager, &mcp_tools);
        self.tool_definitions(ctx, &mut defs, &mcp_tools).await?;
        let head = agent.prompt.clone().unwrap_or_else(|| if ctx.info.original_tools() { system::provider_prompt(model) } else { profile::SYSTEM.to_string() });
        let mut parts = vec![head, system::environment(model, &ctx.cwd, &ctx.root, ctx.worktree.is_some())];
        parts.extend(self.instructions(ctx).await);
        parts.extend(mcp::system(&manager.instructions(), &ruleset));
        parts.extend(system::skills(&ctx.skills, &agent.permission));
        let hooks = Hooks { engine: Some(self), ctx: ctx.as_ref() };
        let joined = parts.into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n");
        let system = system_transform(&hooks, session_id, model, vec![joined]).await.map_err(Error::Config)?;
        let mut messages = history::to_model_messages(&rows, model);
        profile::replay(&mut messages, &ctx.names);
        messages.push(llm::Message::User(prompt.to_string()));
        let mut request = Request {
            model: model.api_id.clone(),
            system,
            messages,
            tools: defs
                .iter()
                .filter(|d| d.name != "invalid")
                .map(|d| llm::Tool { name: ctx.names.exposed(&d.name).to_string(), description: d.description.clone(), parameters: d.parameters.clone() })
                .collect(),
            ..Default::default()
        };
        let meta = shape::Session {
            session_id,
            parent_session_id: session.parent_id.as_deref(),
            request_id: Some(&user.id),
            project_id: Some(&session.project_id),
        };
        shape::apply(&mut request, provider, model, &meta);
        chat_hooks(&hooks, session_id, &agent.name, provider, model, Some(&user), &mut request).await.map_err(Error::Config)?;
        let loader = crate::plugins::PluginAuth::new(ctx.plugins.as_ref());
        let mut text = String::new();
        client::stream_via(&self.http, &loader, provider, model, &request, |event| {
            if let Event::TextDelta(delta) = event {
                text.push_str(&delta);
            }
        })
        .await?;
        Ok(text)
    }

    /// Rust build: v2 `session.generate` for a session by id (the TUI's `/btw`): answers `prompt` from the session's
    /// context; nothing is stored.
    pub async fn generate_in_session(&self, directory: &std::path::Path, session_id: &str, prompt: &str) -> Result<String, Error> {
        let ctx = self.instance(&crate::plain_directory(directory.to_path_buf()), None).await?;
        self.session_generate(&ctx, session_id, prompt).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompts_render_once_and_escape_the_objective() {
        assert_eq!(render("a ${x} ${y} ${x}", &[("x", "${y}"), ("y", "2")]), "a ${y} 2 ${y}");
        let judge = render(JUDGE, &[("objective", &escape("fix <b> & ship")), ("background", "No background shells are running."), ("wait", "")]);
        assert!(judge.contains("<untrusted_objective>\nfix &lt;b&gt; &amp; ship\n</untrusted_objective>\n\nNo background shells are running.\n"));
        assert!(!judge.contains("${"));
        assert!(KICKOFF.starts_with("A goal is now active for this session.") && REMINDER.starts_with("<system-reminder>"));
    }

    #[test]
    fn verdicts_and_objectives_parse_leniently() {
        let v = parse_verdict("Sure:\n```json\n{\"verdict\":\"continue\",\"reason\":\"tests fail\",\"next_step\":\"fix\"}\n```").unwrap();
        assert_eq!((v.verdict.as_str(), v.reason.as_str(), v.next_step.as_deref()), ("continue", "tests fail", Some("fix")));
        assert!(parse_verdict("{\"verdict\":\"maybe\",\"reason\":\"x\"}").is_none());
        assert!(parse_verdict("no json").is_none());
        assert_eq!(unquote("\"Ship the release\""), "Ship the release");
        assert_eq!(unquote("\u{201c}Ship\u{201d}"), "Ship");
        assert_eq!(unquote("\"mismatched'"), "\"mismatched'");
        assert!(same_reason(Some(" Tests  FAIL"), "tests fail"));
        assert_eq!(truncate("a  b\nc", 60), "a b c");
        assert_eq!(truncate("abcdef", 4), "abc…");
    }

    #[test]
    fn reminder_goes_before_the_last_user_message() {
        let goal = Goal {
            id: "g".into(),
            objective: "ship".into(),
            status: Status::Active,
            rounds: 0,
            max_rounds: MAX_ROUNDS,
            stalls: 0,
            reason: None,
            marker: None,
        };
        let mut messages = vec![llm::Message::User("hi".into()), llm::Message::assistant("ok", Vec::new()), llm::Message::User("next".into())];
        remind(&mut messages, &goal);
        assert!(matches!(&messages[2], llm::Message::User(text) if text.contains("<untrusted_objective>\nship\n</untrusted_objective>")));
        assert_eq!(messages.len(), 4);
        // Once a message shows the objective, nothing is added.
        remind(&mut messages, &goal);
        assert_eq!(messages.len(), 4);
    }
}
