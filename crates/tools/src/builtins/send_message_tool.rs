//! SendMessage tool — continue a sub-agent this session started.

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;
use std::sync::Arc;

/// Send a message to a sub-agent, resuming it if its last turn has finished.
///
/// The manager is held behind the same shared handle `Agent` uses, so this is
/// the tool layer's counterpart to a persona continuing the agent it spawned:
/// a running agent receives the message mid-turn, a finished one is reopened
/// and its conversation is continued.
pub struct SendMessageTool {
    manager: Option<Arc<tokio::sync::Mutex<codape_agents::manager::AgentManager>>>,
}

impl std::fmt::Debug for SendMessageTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SendMessageTool")
            .field("has_manager", &self.manager.is_some())
            .finish()
    }
}

impl Default for SendMessageTool {
    fn default() -> Self {
        Self::new()
    }
}

impl SendMessageTool {
    /// Create a tool with no manager attached; every call reports why it
    /// cannot do anything rather than pretending a message was delivered.
    #[must_use]
    pub const fn new() -> Self {
        Self { manager: None }
    }

    /// Attach the session's manager, the same one `Agent` is given.
    #[must_use]
    pub fn with_manager(
        mut self,
        manager: Arc<tokio::sync::Mutex<codape_agents::manager::AgentManager>>,
    ) -> Self {
        self.manager = Some(manager);
        self
    }
}

#[async_trait]
impl Tool for SendMessageTool {
    fn description(&self) -> Option<&'static str> {
        Some(
            "Continues a sub-agent this session started. Use it after an agent reported back \
             when the next step belongs in that agent's conversation rather than this one — \
             it already holds the context, so a follow-up costs far less than a fresh agent. \
             A running agent receives the message mid-turn; a finished one is resumed from \
             the transcript it built.",
        )
    }

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "SendMessage".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["to", "message"],
                "properties": {
                    "to": {
                        "type": "string",
                        "description": "The agent id the Agent tool reported, or an agent name for a teammate"
                    },
                    "message": { "type": "string" }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "sent": { "type": "boolean" },
                    "resumed": { "type": "boolean" },
                    "agent_id": { "type": "string" }
                }
            }),
            requires_approval: false,
            is_enabled: true,
            is_read_only: false,
            is_concurrency_safe: false,
            interrupt_behavior: crate::tool::InterruptBehavior::RunToCompletion,
        }
    }

    async fn execute(
        &self,
        input: ToolInput,
        _ctx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let to = extract_string(&input.arguments, "to")?;
        let message = extract_string(&input.arguments, "message")?;

        let Some(manager) = self.manager.as_ref() else {
            // The previous implementation answered "true" here, with nothing
            // behind it: the model believed a follow-up reached an agent that
            // never received one. Say what is actually true instead.
            return Err(ToolError::InvalidInput {
                message: format!(
                    "cannot message '{to}': this session has no agent manager attached, \
                     so no agent was contacted"
                ),
            });
        };

        let mut manager = manager.lock().await;
        // Whether the agent was already finished decides how to report this:
        // a resumed agent starts new work, a running one just got a note.
        let was_running = manager
            .get_agent(&to)
            .is_some_and(|agent| agent.status() == codape_agents::AgentStatus::Running);

        manager
            .send_message(&to, &message)
            .await
            .map_err(|error| ToolError::InvalidInput {
                message: format!("could not message '{to}': {error}"),
            })?;

        let agent_id = to.clone();
        let resumed = !was_running;
        Ok(ToolOutput {
            output: serde_json::json!({
                "sent": true,
                "resumed": resumed,
                "agent_id": agent_id,
            })
            .to_string(),
            is_display: true,
            images: Vec::new(),
        })
    }
}

fn extract_string(args: &serde_json::Value, key: &str) -> Result<String, ToolError> {
    args.get(key)
        .and_then(serde_json::Value::as_str)
        .map(String::from)
        .ok_or_else(|| ToolError::InvalidInput {
            message: format!("missing required field: {key}"),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ExecutionContext;
    use crate::tool::ToolContext;
    use std::collections::HashMap;
    use std::path::Path;

    fn test_ctx() -> ToolContext<'static> {
        ToolContext {
            elicit_tx: None,
            path_verifier: None,
            execution_context: ExecutionContext::Normal,
            cwd: Path::new("/tmp"),
            extra: Arc::new(HashMap::new()),
        }
    }

    fn input(to: &str, message: &str) -> ToolInput {
        ToolInput {
            call_id: "1".into(),
            name: "SendMessage".into(),
            arguments: serde_json::json!({ "to": to, "message": message }),
        }
    }

    /// Without a manager there is nothing to deliver to, and the tool says so
    /// instead of reporting a send that never happened.
    #[tokio::test]
    async fn without_a_manager_the_call_fails() {
        let error = SendMessageTool::new()
            .execute(input("agent-1", "hello"), test_ctx())
            .await
            .expect_err("a send that reached nothing must fail");
        assert!(
            matches!(error, ToolError::InvalidInput { ref message } if message.contains("no agent manager")),
            "{error}"
        );
    }

    /// An id the manager never spawned is refused with the manager's own
    /// error, so the model learns the id is wrong rather than that sending is
    /// unsupported.
    #[tokio::test]
    async fn an_unknown_agent_is_reported_from_the_manager() {
        let tool = SendMessageTool::new().with_manager(Arc::new(tokio::sync::Mutex::new(
            codape_agents::AgentManager::standalone(),
        )));
        let error = tool
            .execute(input("nope", "hello"), test_ctx())
            .await
            .expect_err("an unknown agent cannot receive a message");
        assert!(
            matches!(error, ToolError::InvalidInput { ref message } if message.contains("not found")),
            "{error}"
        );
    }

    #[tokio::test]
    async fn send_message_missing_fields() {
        let result = SendMessageTool::new()
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "SendMessage".into(),
                    arguments: serde_json::json!({}),
                },
                test_ctx(),
            )
            .await;
        assert!(result.is_err());
    }
}
