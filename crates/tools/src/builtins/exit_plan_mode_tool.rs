//! ExitPlanMode tool — leave plan mode and hand the plan over for approval.

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;

/// Use this tool when you are in plan mode and have finished writing your plan.
#[derive(Debug)]
pub struct ExitPlanModeTool;

#[async_trait]
impl Tool for ExitPlanModeTool {
    fn description(&self) -> Option<&'static str> {
        Some(
            "Leaves plan mode and hands the plan to the person for approval before edits resume. \
             Call it when the plan is complete and you are ready to carry it out.",
        )
    }

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "ExitPlanMode".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "exited": { "type": "boolean" },
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
        // The mode that was in force before plan mode is restored, not a fixed
        // default: a session started in `accept-edits` that plans and then
        // exits must come back to `accept-edits`, or leaving plan mode would
        // silently reset a choice the person made.
        let Some(switch) = crate::tool::plan_mode_switch_from(&ctx) else {
            return Err(ToolError::Execution {
                name: "ExitPlanMode".into(),
                message: "this session has no plan-mode switch installed, so plan mode was not \
                          exited"
                    .into(),
            });
        };
        let change = switch(codape_core::permissions::PlanModeRequest::Exit);
        let output = if change.changed {
            format!("Left plan mode (now `{}`).", change.mode)
        } else {
            format!("Not in plan mode (`{}`).", change.mode)
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
            name: "ExitPlanMode".into(),
            arguments: serde_json::json!({}),
        }
    }

    #[tokio::test]
    async fn exit_plan_mode_restores_the_previous_mode() {
        let checker = Arc::new(std::sync::Mutex::new(PermissionChecker::new(
            PermissionMode::AcceptEdits,
        )));
        checker.lock().unwrap().enter_plan_mode();

        let output = ExitPlanModeTool
            .execute(input(), ctx_with_switch(Arc::clone(&checker)))
            .await
            .unwrap();

        assert_eq!(
            checker.lock().unwrap().mode(),
            PermissionMode::AcceptEdits,
            "the mode saved before plan mode was not restored"
        );
        assert!(
            output.output.contains("Left plan mode"),
            "{}",
            output.output
        );
    }

    #[tokio::test]
    async fn exiting_when_not_planning_changes_nothing() {
        let checker = Arc::new(std::sync::Mutex::new(PermissionChecker::new(
            PermissionMode::Default,
        )));
        let output = ExitPlanModeTool
            .execute(input(), ctx_with_switch(Arc::clone(&checker)))
            .await
            .unwrap();
        assert_eq!(checker.lock().unwrap().mode(), PermissionMode::Default);
        assert!(
            output.output.starts_with("Not in plan mode"),
            "{}",
            output.output
        );
    }

    #[tokio::test]
    async fn a_session_without_a_switch_is_told_so() {
        let result = ExitPlanModeTool.execute(input(), test_ctx()).await;
        assert!(matches!(result, Err(ToolError::Execution { .. })));
    }
}
