//! `invalid` tool (`opencode/src/tool/invalid.ts`): target of repaired tool calls.

use serde_json::{Value, json};

use crate::{Tool, ToolContext, ToolError, ToolOutput, parse_args};

#[derive(Debug, serde::Deserialize)]
struct Params {
    #[allow(dead_code)]
    tool: String,
    error: String,
}

pub struct InvalidTool;

#[async_trait::async_trait]
impl Tool for InvalidTool {
    fn id(&self) -> &str {
        "invalid"
    }

    fn description(&self) -> String {
        "Do not use".to_string()
    }

    fn parameters(&self) -> Value {
        json!({
            "$schema": crate::JSON_SCHEMA_URI,
            "type": "object",
            "properties": { "tool": { "type": "string" }, "error": { "type": "string" } },
            "required": ["tool", "error"]
        })
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let params: Params = parse_args("invalid", args)?;
        Ok(ToolOutput {
            title: "Invalid Tool".to_string(),
            output: format!(
                "The arguments provided to the tool are invalid: {}",
                params.error
            ),
            metadata: json!({}),
            attachments: vec![],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reports_error() {
        let t = crate::testing::Fixture::new();
        let out = InvalidTool
            .execute(json!({"tool": "x", "error": "bad"}), &t.ctx())
            .await
            .unwrap();
        assert_eq!(out.title, "Invalid Tool");
        assert_eq!(
            out.output,
            "The arguments provided to the tool are invalid: bad"
        );
        assert!(t.requests().is_empty());
    }
}
