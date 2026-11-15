//! EnterPlanMode tool — switch the session into plan mode.

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;

/// Use this tool proactively when you're about to start a non-trivial implementation task.
#[derive(Debug)]
pub struct EnterPlanModeTool;

#[async_trait]
impl Tool for EnterPlanModeTool {
    fn description(&self) -> Option<&'static str> {
        Some(
            "Switches the session into plan mode, which allows only read-only tools. Use it when \
             the task needs design work before any edit, so the plan can be reviewed before \
             anything on disk changes.",
        )
    }

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "EnterPlanMode".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "entered": { "type": "boolean" },
                    "mode": { "type": "string" }
                }
            }),
            requires_approval: false,
            is_enabled: true,
            is_read_only: true,
            is_concurrency_safe: true,
            interrupt_behavior: crate::tool::InterruptBehavior::RunToCompletion,
        }
    }

    async fn execute(
        &self,
        _input: ToolInput,
        ctx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        // Announcing plan mode is not entering it: the mode is enforced by the
        // session's permission checker, and until this tool drove it the
        // session kept editing while the model believed it was planning.
        let Some(switch) = crate::tool::plan_mode_switch_from(&ctx) else {
            return Err(ToolError::Execution {
                name: "EnterPlanMode".into(),
                message: "this session has no plan-mode switch installed, so plan mode was not \
                          entered"
                    .into(),
            });
        };
        let change = switch(codape_core::permissions::PlanModeRequest::Enter);
        let output = if change.changed {
            format!("Turned on plan mode (now `{}`).", change.mode)
        } else {
            format!("Already in plan mode (`{}`).", change.mode)
        };
        Ok(ToolOutput {
            output,
            is_display: true,
            images: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ExecutionContext;
    use crate::tool::ToolContext;
    use codape_core::permissions::{PermissionChecker, PermissionMode, PlanModeRequest};
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::Arc;

    fn ctx_with_switch(checker: Arc<std::sync::Mutex<PermissionChecker>>) -> ToolContext<'static> {
        let switch: codape_core::permissions::PlanModeSwitch = Arc::new(move |request| {
            let mut checker = checker.lock().unwrap();
            match request {
                PlanModeRequest::Enter => {
                    let was_plan = checker.mode() == PermissionMode::Plan;
                    checker.enter_plan_mode();
                    codape_core::permissions::PlanModeChange {
                        changed: !was_plan,
                        mode: checker.mode(),
                    }
                }
                PlanModeRequest::Exit => {
                    let restored = checker.exit_plan_mode();
                    codape_core::permissions::PlanModeChange {
                        changed: restored.is_some(),
                        mode: checker.mode(),
                    }
                }
            }
        });
        let mut extra = HashMap::new();
        extra.insert(
            crate::tool::PLAN_MODE_SWITCH_KEY.to_string(),
            Box::new(switch) as crate::tool::SharedState,
        );
        ToolContext {
            elicit_tx: None,
            path_verifier: None,
            execution_context: ExecutionContext::Normal,
            cwd: Path::new("/tmp"),
            extra: Arc::new(extra),
        }
    }

    fn test_ctx() -> ToolContext<'static> {
        ToolContext {
            elicit_tx: None,
            path_verifier: None,
            execution_context: ExecutionContext::Normal,
            cwd: Path::new("/tmp"),
            extra: Arc::new(HashMap::new()),
        }
    }

    fn input() -> ToolInput {
        ToolInput {
            call_id: "1".into(),
            name: "EnterPlanMode".into(),
            arguments: serde_json::json!({}),
        }
    }

    #[tokio::test]
    async fn enter_plan_mode_switches_the_session_mode() {
        let checker = Arc::new(std::sync::Mutex::new(PermissionChecker::new(
            PermissionMode::Default,
        )));
        let output = EnterPlanModeTool
            .execute(input(), ctx_with_switch(Arc::clone(&checker)))
            .await
            .unwrap();

        assert_eq!(checker.lock().unwrap().mode(), PermissionMode::Plan);
        assert!(output.output.contains("plan mode"), "{}", output.output);
        // Every mutating tool is now refused by the same checker, which is
        // what makes the switch more than a sentence in the transcript.
        assert!(matches!(
            checker.lock().unwrap().check("Write", None),
            codape_core::permissions::PermissionResult::Deny { .. }
        ));
    }

    #[tokio::test]
    async fn entering_plan_mode_twice_says_it_was_already_on() {
        let checker = Arc::new(std::sync::Mutex::new(PermissionChecker::new(
            PermissionMode::Plan,
        )));
        let output = EnterPlanModeTool
            .execute(input(), ctx_with_switch(Arc::clone(&checker)))
            .await
            .unwrap();
        assert!(output.output.starts_with("Already"), "{}", output.output);
    }

    #[tokio::test]
    async fn a_session_without_a_switch_is_told_so() {
        let result = EnterPlanModeTool.execute(input(), test_ctx()).await;
        assert!(matches!(result, Err(ToolError::Execution { .. })));
    }
}
