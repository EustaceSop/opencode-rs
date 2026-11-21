// Port of `util/transcript.ts`.
use crate::catalog::Catalog;
use crate::state::ModelKey;
use crate::text::titlecase;
use opencode_storage::{Message, Part, PartBody, Session, ToolState, iso_timestamp};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    pub thinking: bool,
    pub tool_details: bool,
    pub assistant_metadata: bool,
}

fn part(part: &Part, options: Options) -> String {
    match &part.body {
        PartBody::Text(text) if text.synthetic != Some(true) => format!("{}\n\n", text.text),
        PartBody::Reasoning(reasoning) if options.thinking => format!("_Thinking:_\n\n{}\n\n", reasoning.text),
        PartBody::Tool(tool) => {
            let mut out = format!("**Tool: {}**\n", tool.tool);
            if options.tool_details {
                let input = serde_json::to_string_pretty(tool.state.input()).unwrap_or_default();
                out.push_str(&format!("\n**Input:**\n```json\n{input}\n```\n"));
                match &tool.state {
                    ToolState::Completed(done) if !done.output.is_empty() => out.push_str(&format!("\n**Output:**\n```\n{}\n```\n", done.output)),
                    ToolState::Error(failed) if !failed.error.is_empty() => out.push_str(&format!("\n**Error:**\n```\n{}\n```\n", failed.error)),
                    _ => {}
                }
            }
            out.push('\n');
            out
        }
        _ => String::new(),
    }
}

// Dates are ISO-8601 UTC; the original prints them in the process locale.
pub fn format_transcript(session: &Session, messages: &[(Message, Vec<Part>)], catalog: &Catalog, options: Options) -> String {
    let mut out = format!("# {}\n\n", session.title);
    out.push_str(&format!("**Session ID:** {}\n", session.id));
    out.push_str(&format!("**Created:** {}\n", iso_timestamp(session.time.created)));
    out.push_str(&format!("**Updated:** {}\n\n", iso_timestamp(session.time.updated)));
    out.push_str("---\n\n");
    for (message, parts) in messages {
        match message {
            Message::User(_) => out.push_str("## User\n\n"),
            Message::Assistant(assistant) if options.assistant_metadata => {
                let key = ModelKey::new(&assistant.provider_id, &assistant.model_id);
                let model = catalog.model(&key).map_or_else(|| assistant.model_id.clone(), |(_, m)| m.name.clone());
                let duration = match assistant.time.completed {
                    Some(completed) if assistant.time.created > 0 => format!(" · {:.1}s", completed.saturating_sub(assistant.time.created) as f64 / 1000.0),
                    _ => String::new(),
                };
                out.push_str(&format!("## Assistant ({} · {model}{duration})\n\n", titlecase(&assistant.agent)));
            }
            Message::Assistant(_) => out.push_str("## Assistant\n\n"),
        }
        for stored in parts {
            out.push_str(&part(stored, options));
        }
        out.push_str("---\n\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::testing::session;
    use crate::transcript::testing::{assistant, text_part, tool_part, user};
    use serde_json::json;

    #[test]
    fn formats_a_transcript_like_the_original() {
        let mut info = session("ses_1", "Demo");
        info.time.created = 0;
        info.time.updated = 1000;
        let messages = vec![
            user("msg_1", "hi"),
            assistant("msg_2", "msg_1", vec![text_part("msg_2", "hello"), tool_part("msg_2", "c1", "bash", json!({"command": "ls"}), "out", json!({}))]),
        ];
        let all = Options { thinking: true, tool_details: true, assistant_metadata: true };
        let text = format_transcript(&info, &messages, &crate::catalog::fixture(), all);
        let expected = "# Demo\n\n**Session ID:** ses_1\n**Created:** 1970-01-01T00:00:00.000Z\n**Updated:** 1970-01-01T00:00:01.000Z\n\n---\n\n## User\n\nhi\n\n---\n\n## Assistant (Build · Claude Sonnet 4.5 · 12.3s)\n\nhello\n\n**Tool: bash**\n\n**Input:**\n```json\n{\n  \"command\": \"ls\"\n}\n```\n\n**Output:**\n```\nout\n```\n\n---\n\n";
        assert_eq!(text, expected);
        let bare = format_transcript(&info, &messages, &crate::catalog::fixture(), Options { thinking: false, tool_details: false, assistant_metadata: false });
        assert!(bare.contains("## Assistant\n\nhello\n\n**Tool: bash**\n\n---"), "{bare}");
    }
}
