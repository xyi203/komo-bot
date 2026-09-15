//! The `todo` tool: the agent's working focus list for the *current session*
//! (roadmap §2/§8). Distinct from `task` (durable, cross-session): a todo dies
//! with the conversation. Shaped after hermes' `todo_tool` / Claude Code's
//! `TodoWrite` — full-list replace on write, list order is priority, at most one
//! item `in_progress`.
//!
//! The session comes from the call's explicit [`ToolContext`]. With no real
//! session (aux sub-agents, maintenance sweeps — a detached context) the tool
//! is inert.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use komo_core::domain::{
    context::ToolContext,
    todo::{SessionTodoRepository, TodoItem, TodoStatus, parse_todo_status, render_todo_list},
    tool::{Tool, ToolError, ToolOutput, parse_args},
};

#[derive(Deserialize)]
struct TodoArgs {
    /// Present → write (replace the whole list). Absent → read.
    todos: Option<Vec<TodoInput>>,
}

#[derive(Deserialize)]
struct TodoInput {
    content: String,
    status: Option<String>,
    #[serde(default)]
    active_form: String,
}

pub struct TodoTool {
    todos: Arc<dyn SessionTodoRepository>,
}

impl TodoTool {
    pub fn new(todos: Arc<dyn SessionTodoRepository>) -> Self {
        Self { todos }
    }
}

/// Render the list plus a one-line summary, the model's view after any op.
/// Shared with prompt assembly, which carries the same list at the tail of
/// every user message — see [`render_todo_list`].
fn render(items: &[TodoItem]) -> String {
    render_todo_list(items)
}

#[async_trait]
impl Tool for TodoTool {
    fn name(&self) -> &'static str {
        "todo"
    }

    fn description(&self) -> &'static str {
        "Working task list for this conversation only. Call with no arguments to \
         read it; pass `todos` to replace the whole list, sending every item each \
         time. Keep at most one item in_progress."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "description": "The full todo list, replacing the previous one. Omit to read.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {
                                "type": "string",
                                "description": "Imperative step, e.g. \"Write the parser\"."
                            },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed", "cancelled"],
                                "description": "Defaults to pending."
                            },
                            "active_form": {
                                "type": "string",
                                "description": "Present-continuous form shown while running, e.g. \"Writing the parser\"."
                            }
                        },
                        "required": ["content"]
                    }
                }
            },
            "required": []
        })
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let session_id = ctx.session.session_id.clone();
        // No session (aux sub-agents, maintenance sweeps carry an empty id):
        // the working list is a per-conversation concept, so stay inert.
        if session_id.is_empty() {
            return Ok(ToolOutput::text(
                "The todo tool is only available inside a conversation; nothing to track here.",
            ));
        }

        // The no-arg call arrives as JSON null; `{}` and a real list parse
        // normally. Both an absent `todos` field and null mean read.
        let args: TodoArgs = if input.is_null() {
            TodoArgs { todos: None }
        } else {
            parse_args(&input)?
        };

        let Some(inputs) = args.todos else {
            // Read.
            let items = self.todos.get(&session_id).await?;
            return Ok(ToolOutput::text(render(&items)));
        };

        // Write: build the new list, validating as we go.
        let mut items = Vec::with_capacity(inputs.len());
        for input in inputs {
            let status = match input.status {
                Some(s) => parse_todo_status(&s)?,
                None => TodoStatus::Pending,
            };
            items.push(TodoItem {
                content: input.content,
                status,
                active_form: input.active_form,
            });
        }

        let in_progress = items
            .iter()
            .filter(|t| t.status == TodoStatus::InProgress)
            .count();
        if in_progress > 1 {
            return Err(ToolError::InvalidInput(format!(
                "only one todo item can be in_progress at a time (got {in_progress})"
            )));
        }

        self.todos.set(&session_id, &items).await?;
        Ok(ToolOutput::text(render(&items)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_core::domain::approval::{ApprovalRequest, Approver, Decision};
    use komo_core::domain::context::{SessionContext, ToolContext};
    use std::sync::Arc;
    use std::sync::Mutex;

    struct DenyAll;
    #[async_trait]
    impl Approver for DenyAll {
        async fn decide(&self, _r: &ApprovalRequest) -> Decision {
            Decision::deny()
        }
    }

    /// A `ToolContext` bound to `session`, with a never-consulted approver
    /// (`todo` requests no approval).
    fn ctx(session: &str) -> ToolContext {
        ToolContext::new(SessionContext::detached(session), None, Arc::new(DenyAll))
    }

    fn v(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    #[derive(Default)]
    struct MemTodos(Mutex<std::collections::HashMap<String, Vec<TodoItem>>>);

    #[async_trait]
    impl SessionTodoRepository for MemTodos {
        async fn get(&self, session_id: &str) -> anyhow::Result<Vec<TodoItem>> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .get(session_id)
                .cloned()
                .unwrap_or_default())
        }
        async fn set(&self, session_id: &str, items: &[TodoItem]) -> anyhow::Result<()> {
            self.0
                .lock()
                .unwrap()
                .insert(session_id.to_string(), items.to_vec());
            Ok(())
        }
        async fn clear(&self, session_id: &str) -> anyhow::Result<()> {
            self.0.lock().unwrap().remove(session_id);
            Ok(())
        }
    }

    #[tokio::test]
    async fn write_then_read_roundtrips_in_session() {
        let repo = Arc::new(MemTodos::default());
        let tool = TodoTool::new(repo.clone());
        let ctx = ctx("s1");
        let out = tool
            .call(
                v(r#"{"todos":[{"content":"step one","status":"in_progress"},{"content":"step two"}]}"#),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.text.contains("step one"), "{}", out.text);
        let read = tool.call(Value::Null, &ctx).await.unwrap();
        assert!(read.text.contains("step two"), "{}", read.text);
        assert!(read.text.contains("1 in progress"), "{}", read.text);
    }

    #[tokio::test]
    async fn rejects_two_in_progress() {
        let repo = Arc::new(MemTodos::default());
        let tool = TodoTool::new(repo);
        let err = tool
            .call(
                v(r#"{"todos":[{"content":"a","status":"in_progress"},{"content":"b","status":"in_progress"}]}"#),
                &ctx("s1"),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("one todo item"), "{err}");
    }

    #[tokio::test]
    async fn inert_without_session_context() {
        let repo = Arc::new(MemTodos::default());
        let tool = TodoTool::new(repo);
        // Empty session id (aux sub-agents / sweeps) → the tool stays inert.
        let out = tool.call(Value::Null, &ctx("")).await.unwrap();
        assert!(
            out.text.contains("only available inside a conversation"),
            "{}",
            out.text
        );
    }

    #[tokio::test]
    async fn write_replaces_whole_list() {
        let repo = Arc::new(MemTodos::default());
        let tool = TodoTool::new(repo.clone());
        let ctx = ctx("s1");
        tool.call(v(r#"{"todos":[{"content":"a"},{"content":"b"}]}"#), &ctx)
            .await
            .unwrap();
        tool.call(
            v(r#"{"todos":[{"content":"c","status":"completed"}]}"#),
            &ctx,
        )
        .await
        .unwrap();
        let items = repo.get("s1").await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].content, "c");
    }

    #[test]
    fn the_model_facing_text_stays_short() {
        crate::test_support::assert_model_text_budget(&TodoTool::new(
            Arc::new(MemTodos::default()),
        ));
    }
}
