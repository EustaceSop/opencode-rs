//! `todowrite` tool (`opencode/src/tool/todo.ts`).

use std::sync::Arc;

use serde_json::{Value, json};

use crate::{PermissionRequest, Tool, ToolContext, ToolError, ToolOutput, parse_args};

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Todo {
    pub content: String,
    pub status: String,
    pub priority: String,
}

#[derive(Debug, serde::Deserialize)]
struct Params {
    todos: Vec<Todo>,
}

/// Persistence hook standing in for `Todo.Service.update` (session storage + `todo.updated` event).
#[async_trait::async_trait]
pub trait TodoStore: Send + Sync {
    async fn update(&self, session_id: &str, todos: &[Todo]) -> Result<(), ToolError>;
}

#[derive(Default)]
pub struct TodoWriteTool {
    pub store: Option<Arc<dyn TodoStore>>,
}

impl TodoWriteTool {
    pub fn new(store: Arc<dyn TodoStore>) -> Self {
        Self { store: Some(store) }
    }
}

#[async_trait::async_trait]
impl Tool for TodoWriteTool {
    fn id(&self) -> &str {
        "todowrite"
    }

    fn description(&self) -> String {
        include_str!("../../../../opencode-ts/packages/opencode/src/tool/todowrite.txt").to_string()
    }

    fn parameters(&self) -> Value {
        json!({
            "$schema": crate::JSON_SCHEMA_URI,
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "description": "The updated todo list",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": { "type": "string", "description": "Brief description of the task" },
                            "status": {
                                "type": "string",
                                "description": "Current status of the task: pending, in_progress, completed, cancelled"
                            },
                            "priority": {
                                "type": "string",
                                "description": "Priority level of the task: high, medium, low"
                            }
                        },
                        "required": ["content", "status", "priority"]
                    }
                }
            },
            "required": ["todos"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let params: Params = parse_args("todowrite", args)?;
        ctx.permissions
            .ask(PermissionRequest {
                permission: "todowrite".into(),
                patterns: vec!["*".into()],
                always: vec!["*".into()],
                metadata: json!({}),
            })
            .await?;

        if let Some(store) = &self.store {
            store.update(&ctx.session_id, &params.todos).await?;
        }

        let open = params
            .todos
            .iter()
            .filter(|todo| todo.status != "completed")
            .count();
        let output = serde_json::to_string_pretty(&params.todos)
            .map_err(|e| ToolError::Other(e.to_string()))?;
        Ok(ToolOutput {
            title: format!("{open} todos"),
            output,
            metadata: json!({ "todos": params.todos }),
            attachments: vec![],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Fixture;
    use std::sync::Mutex;

    #[derive(Default)]
    struct MemoryStore(Mutex<Vec<(String, Vec<Todo>)>>);

    #[async_trait::async_trait]
    impl TodoStore for MemoryStore {
        async fn update(&self, session_id: &str, todos: &[Todo]) -> Result<(), ToolError> {
            self.0
                .lock()
                .unwrap()
                .push((session_id.to_string(), todos.to_vec()));
            Ok(())
        }
    }

    #[tokio::test]
    async fn writes_todos() {
        let t = Fixture::new();
        let store = Arc::new(MemoryStore::default());
        let tool = TodoWriteTool::new(store.clone());
        let args = json!({"todos": [
            {"priority": "high", "status": "in_progress", "content": "a", "extra": 1},
            {"content": "b", "status": "completed", "priority": "low"}
        ]});
        let out = tool.execute(args, &t.ctx()).await.unwrap();
        assert_eq!(out.title, "1 todos");
        assert_eq!(
            out.output,
            "[\n  {\n    \"content\": \"a\",\n    \"status\": \"in_progress\",\n    \"priority\": \"high\"\n  },\n  {\n    \"content\": \"b\",\n    \"status\": \"completed\",\n    \"priority\": \"low\"\n  }\n]"
        );
        assert_eq!(out.metadata["todos"][0]["content"], "a");
        let saved = store.0.lock().unwrap();
        assert_eq!(saved[0].0, "ses_test");
        assert_eq!(saved[0].1.len(), 2);
        let reqs = t.requests();
        assert_eq!(reqs.len(), 1);
        assert_eq!(
            reqs[0],
            PermissionRequest {
                permission: "todowrite".into(),
                patterns: vec!["*".into()],
                always: vec!["*".into()],
                metadata: json!({}),
            }
        );
    }

    #[tokio::test]
    async fn rejects_bad_shape_and_denied_permission() {
        let t = Fixture::new();
        let tool = TodoWriteTool::default();
        let res = tool
            .execute(json!({"todos": [{"content": "a"}]}), &t.ctx())
            .await;
        assert!(matches!(res, Err(ToolError::InvalidArguments { .. })));
        t.deny("todowrite");
        let res = tool.execute(json!({"todos": []}), &t.ctx()).await;
        assert!(matches!(res, Err(ToolError::Rejected)));
    }
}
