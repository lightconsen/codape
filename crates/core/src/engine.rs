//! TurnState state machine — lifecycle states, terminal reasons, and transitions.
//!
//! The turn state machine drives a single conversation turn through:
//! 1. Context building (system prompts, memory, MCP, tools)
//! 2. API request (streaming or non-streaming)
//! 3. Tool execution (with permission checks)
//! 4. Continuation evaluation (should we keep going?)
//! 5. Compact/recovery if needed
//! 6. Finalize and emit events

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::api_client::{ApiClient, ApiError, ApiResponse, RequestContext, StreamEvent};
use crate::budget::TokenBudget;
use crate::compact::{self, CompactResult, CompactStrategy};
use crate::confirmation_bus::{ConfirmationBus, ConfirmationRequest, ConfirmationResponse};
use crate::cost::CostTracker;
use crate::elicitation::{ElicitationHandler, ElicitationResult, ReplElicitationHandler};
use crate::hook_events;
use crate::hook_executor::{HookExecutor, HookFireResult, NoOpHookExecutor};
use crate::loop_detector::{LoopDetection, LoopDetector};
use crate::permission_engine::{PermissionEngine, PermissionResponse};
use crate::permissions::{PermissionChecker, PermissionMode, PermissionResult};
use crate::prompt::{AssembledContext, CompactConfig, PromptCompiler, PromptContext};
use crate::state::SessionState;
use crate::stop_hook::{self, StopHookContext, StopHookDecision};
use crate::tool_executor::{NoOpToolExecutor, ToolExecutor, ToolInfo};
use crate::tool_scheduler;
use crate::types::{
    CompactionEvent, ContextWindowResolver, EngineEvent, Message, ModelId, ThinkingBlock, ToolCall,
    ToolResult, UsageSummary,
};
use std::path::PathBuf;

// ─── Structured Output Validation ───────────────────────────────────────────

/// Result of validating output against a JSON schema.
#[derive(Debug, Clone)]
pub enum ValidationResult {
    /// Output is valid according to the schema.
    Valid(serde_json::Value),
    /// Output is invalid — contains the error and retry count.
    Invalid {
        /// Validation error message.
        error: String,
        /// How many times this output has been retried.
        retries: u32,
    },
}

/// Validates structured output against a JSON schema.
///
/// Supports: type checking, required fields, enum constraints,
/// and nested object validation.
#[derive(Debug, Clone)]
pub struct StructuredOutputValidator {
    schema: serde_json::Value,
    max_retries: u32,
}

impl StructuredOutputValidator {
    /// Create a new validator with the given schema.
    #[must_use]
    pub const fn new(schema: serde_json::Value) -> Self {
        Self {
            schema,
            max_retries: 3,
        }
    }

    /// Set the maximum number of retries for invalid output.
    #[must_use]
    pub const fn with_max_retries(mut self, max: u32) -> Self {
        self.max_retries = max;
        self
    }

    /// Validate output against the schema.
    #[must_use]
    pub fn validate(&self, output: &str, retries: u32) -> ValidationResult {
        if retries >= self.max_retries {
            return ValidationResult::Invalid {
                error: format!("max retries ({}) exceeded", self.max_retries),
                retries,
            };
        }

        let value: serde_json::Value = match serde_json::from_str(output) {
            Ok(v) => v,
            Err(e) => {
                return ValidationResult::Invalid {
                    error: format!("invalid JSON: {e}"),
                    retries,
                };
            }
        };

        match Self::validate_value(&value, &self.schema) {
            Ok(()) => ValidationResult::Valid(value),
            Err(error) => ValidationResult::Invalid { error, retries },
        }
    }

    /// Recursively validate a JSON value against a schema.
    fn validate_value(value: &serde_json::Value, schema: &serde_json::Value) -> Result<(), String> {
        let Some(schema_obj) = schema.as_object() else {
            return Ok(());
        };

        // Type check
        if let Some(expected_type) = schema_obj.get("type")
            && let Some(type_str) = expected_type.as_str()
        {
            Self::check_type(value, type_str)?;
        }

        // Enum check
        if let Some(enum_values) = schema_obj.get("enum")
            && let Some(arr) = enum_values.as_array()
            && !arr.contains(value)
        {
            return Err(format!("value not in enum: {value}"));
        }

        // Object required fields
        if let Some(properties) = schema_obj.get("properties")
            && let Some(props) = properties.as_object()
            && let Some(obj) = value.as_object()
        {
            for (key, prop_schema) in props {
                if let Some(val) = obj.get(key) {
                    Self::validate_value(val, prop_schema)?;
                }
            }
        }

        // Required fields check
        if let Some(required) = schema_obj.get("required")
            && let Some(arr) = required.as_array()
            && let Some(obj) = value.as_object()
        {
            for field in arr {
                if let Some(field_str) = field.as_str()
                    && !obj.contains_key(field_str)
                {
                    return Err(format!("missing required field: {field_str}"));
                }
            }
        }

        Ok(())
    }

    /// Check if a value matches the expected JSON type.
    fn check_type(value: &serde_json::Value, expected: &str) -> Result<(), String> {
        let actual = match value {
            serde_json::Value::Null => "null",
            serde_json::Value::Bool(_) => "boolean",
            serde_json::Value::Number(_) => "number",
            serde_json::Value::String(_) => "string",
            serde_json::Value::Array(_) => "array",
            serde_json::Value::Object(_) => "object",
        };
        if actual != expected {
            return Err(format!("expected type '{expected}', got '{actual}'"));
        }
        Ok(())
    }

    /// Generate a retry instruction message for the model.
    #[must_use]
    pub fn retry_instruction(&self, error: &str) -> String {
        format!(
            "Your previous response did not match the required JSON schema. Error: {error}. Please respond with ONLY valid JSON matching the schema."
        )
    }
}

// ─── Streaming Tool Call Assembler ──────────────────────────────────────────

/// Accumulates streaming tool call deltas into complete tool calls.
///
/// During streaming, tool calls arrive incrementally:
/// 1. `ToolCallBlockStart` gives id + name
/// 2. `ToolCallInputDelta` gives JSON fragments
/// 3. `ToolCallBlockEnd` gives complete JSON (or assembler builds it)
#[derive(Debug, Clone)]
pub struct StreamingToolCallAssembler {
    /// Accumulating tool call buffers: id -> (name, input_json_builder).
    pending: HashMap<String, (String, String)>,
    /// Completed tool calls.
    completed: Vec<ToolCall>,
    /// Ids already completed (success or malformed-completion) — duplicate
    /// `ToolCallBlockEnd`s for the same id (OpenAI Responses emits the final
    /// item twice in some paths) are ignored instead of re-completed.
    completed_ids: HashSet<String>,
    /// Number of tool calls whose accumulated JSON failed to parse.
    malformed_count: usize,
    /// Ids of tool calls whose accumulated JSON failed to parse.
    malformed_ids: Vec<String>,
    /// Text content accumulated alongside tool calls.
    text_content: String,
    /// Thinking text accumulated from an extended-thinking block.
    thinking: String,
    /// Signature accumulated for the thinking block.
    thinking_signature: String,
    /// Whether a thinking block was seen at all, even if empty.
    has_thinking: bool,
    /// Whether any tool calls have been seen.
    has_tool_calls: bool,
}

impl StreamingToolCallAssembler {
    /// Create a new assembler.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
            completed: Vec::new(),
            text_content: String::new(),
            thinking: String::new(),
            thinking_signature: String::new(),
            has_thinking: false,
            has_tool_calls: false,
            completed_ids: HashSet::new(),
            malformed_count: 0,
            malformed_ids: Vec::new(),
        }
    }

    /// Process a stream event and return any completed tool calls.
    ///
    /// Returns `Vec<ToolCall>` when a tool call is fully assembled.
    /// Also returns `true` if the stream has ended (no more events expected).
    pub fn process_event(&mut self, event: StreamEvent) -> Vec<ToolCall> {
        match event {
            StreamEvent::TextDelta(text) => {
                self.text_content.push_str(&text);
                Vec::new()
            }
            StreamEvent::ThinkingDelta(text) => {
                self.has_thinking = true;
                self.thinking.push_str(&text);
                Vec::new()
            }
            StreamEvent::SignatureDelta(signature) => {
                self.has_thinking = true;
                self.thinking_signature.push_str(&signature);
                Vec::new()
            }
            StreamEvent::ToolCallBlockStart { id, name } => {
                self.has_tool_calls = true;
                self.pending.insert(id, (name, String::new()));
                Vec::new()
            }
            StreamEvent::ToolCallInputDelta { id, delta } => {
                if let Some((_, builder)) = self.pending.get_mut(&id) {
                    builder.push_str(&delta);
                }
                Vec::new()
            }
            StreamEvent::ToolCallBlockEnd { id, input } => {
                let Some((name, builder)) = self.pending.remove(&id) else {
                    if self.completed_ids.contains(&id) {
                        // Duplicate BlockEnd for an already-completed call
                        // (OpenAI Responses emits it twice).
                        return Vec::new();
                    }
                    tracing::warn!("tool call block end for unseen id {id} — ignored");
                    return Vec::new();
                };
                let input = if input.is_null() {
                    match serde_json::from_str(&builder) {
                        Ok(v) => v,
                        Err(e) => {
                            // Complete with empty input instead of dropping:
                            // a dropped call leaves a persisted tool_use
                            // without a tool_result, breaking the next
                            // request's message contract. An empty input
                            // flows through the normal pipeline and fails
                            // cleanly (missing required args -> error
                            // ToolResult back to the model).
                            tracing::warn!(
                                "tool call {id} ({name}) has malformed JSON input: {e}\naccumulated: {builder}"
                            );
                            self.malformed_count += 1;
                            self.malformed_ids.push(id.clone());
                            serde_json::Value::Object(serde_json::Map::default())
                        }
                    }
                } else {
                    input
                };
                self.completed_ids.insert(id.clone());
                let call = ToolCall { id, name, input };
                self.completed.push(call.clone());
                vec![call]
            }
            StreamEvent::StreamEnd => {
                // Finalize any pending tool calls from accumulated JSON
                let mut calls = Vec::new();
                for (id, (name, builder)) in self.pending.drain() {
                    let input = match serde_json::from_str(&builder) {
                        Ok(input) => input,
                        Err(e) => {
                            // Same empty-input completion as BlockEnd: the
                            // message contract must hold.
                            tracing::warn!(
                                "tool call {id} ({name}) has malformed JSON input at stream end: {e}\naccumulated: {builder}"
                            );
                            self.malformed_count += 1;
                            self.malformed_ids.push(id.clone());
                            let call = ToolCall {
                                id: id.clone(),
                                name,
                                input: serde_json::Value::Object(serde_json::Map::default()),
                            };
                            self.completed.push(call.clone());
                            calls.push(call);
                            continue;
                        }
                    };
                    let call = ToolCall { id, name, input };
                    self.completed.push(call.clone());
                    calls.push(call);
                }
                calls
            }
            StreamEvent::UsageSummary(_) | StreamEvent::Error(_) | StreamEvent::StopReason(_) => {
                Vec::new()
            }
        }
    }

    /// Get the accumulated text content.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text_content
    }

    /// The thinking block assembled from this stream, if one was seen.
    #[must_use]
    pub fn thinking(&self) -> Option<ThinkingBlock> {
        self.has_thinking.then(|| ThinkingBlock {
            thinking: self.thinking.clone(),
            signature: self.thinking_signature.clone(),
        })
    }

    /// Check if any tool calls have been seen.
    #[must_use]
    pub const fn has_tool_calls(&self) -> bool {
        self.has_tool_calls
    }

    /// Number of tool calls whose accumulated JSON failed to parse.
    #[must_use]
    pub const fn malformed_count(&self) -> usize {
        self.malformed_count
    }

    /// Get completed tool calls (consumes them).
    pub fn take_completed(&mut self) -> Vec<ToolCall> {
        std::mem::take(&mut self.completed)
    }
}

impl Default for StreamingToolCallAssembler {
    fn default() -> Self {
        Self::new()
    }
}

// ─── TurnState ──────────────────────────────────────────────────────────────

/// Lifecycle states for a single conversation turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnState {
    /// Initial state — preparing the turn.
    Initialize,
    /// Building the prompt context (system prompts, memory, MCP, etc.).
    BuildContext,
    /// Running pre-submit hooks.
    RunPreSubmitHooks,
    /// Sending the request to the API.
    StartRequest,
    /// Streaming assistant response tokens.
    StreamAssistant,
    /// Parsing and executing tool calls.
    ExecuteTools,
    /// Waiting for user permission to execute a tool.
    AwaitPermission,
    /// Waiting for user elicitation (clarification question).
    AwaitElicitation,
    /// Applying tool results back into the message history.
    ApplyToolResults,
    /// Evaluating whether to continue (more turns/continuations).
    EvaluateContinuation,
    /// Running compact/recovery if context is too large.
    CompactRecovery,
    /// Running stop hooks before finalizing.
    StopHooks,
    /// Finalizing the turn — emitting end events.
    Finalize,
    /// The turn has terminated (successfully or with error).
    Terminated,
}

impl TurnState {
    /// The name of this state, for logs.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Initialize => "initialize",
            Self::BuildContext => "build_context",
            Self::RunPreSubmitHooks => "run_pre_submit_hooks",
            Self::StartRequest => "start_request",
            Self::StreamAssistant => "stream_assistant",
            Self::ExecuteTools => "execute_tools",
            Self::AwaitPermission => "await_permission",
            Self::AwaitElicitation => "await_elicitation",
            Self::ApplyToolResults => "apply_tool_results",
            Self::EvaluateContinuation => "evaluate_continuation",
            Self::CompactRecovery => "compact_recovery",
            Self::StopHooks => "stop_hooks",
            Self::Finalize => "finalize",
            Self::Terminated => "terminated",
        }
    }

    /// Whether this state may be followed by `next`.
    ///
    /// The graph is what a turn actually does:
    ///
    /// ```text
    /// Initialize → BuildContext → RunPreSubmitHooks → StartRequest → StreamAssistant
    /// StreamAssistant → ExecuteTools → ApplyToolResults → EvaluateContinuation
    /// StreamAssistant → EvaluateContinuation            (no tools to run)
    /// EvaluateContinuation → CompactRecovery → StartRequest   (context too large)
    /// CompactRecovery / StartRequest → StopHooks              (recovery gave up)
    /// EvaluateContinuation → StopHooks → Finalize → Terminated
    /// ExecuteTools → AwaitPermission ─┐                 (a decision is needed)
    /// StartRequest → AwaitElicitation ┘→ back where they came from
    /// ```
    ///
    /// Any state may be terminated, and any state may be re-entered from
    /// `Initialize` for a new turn.
    #[must_use]
    pub const fn can_transition_to(self, next: Self) -> bool {
        // A turn can always end, and a new turn always starts the same way.
        if matches!(next, Self::Terminated) || matches!(self, Self::Initialize) {
            return matches!(next, Self::Terminated | Self::BuildContext);
        }
        match self {
            Self::Initialize => unreachable!(),
            Self::BuildContext => matches!(next, Self::RunPreSubmitHooks),
            // RunPreSubmitHooks → CompactRecovery is the cache-TTL pass: it
            // runs before the request, while the token line has not been
            // crossed.
            Self::RunPreSubmitHooks => matches!(next, Self::StartRequest | Self::CompactRecovery),
            // A recovery retry is re-requested from `StartRequest`; when that
            // retry is still over budget the turn closes from here, so the
            // stop hooks are reachable from this state too.
            Self::StartRequest => matches!(
                next,
                Self::StreamAssistant | Self::AwaitElicitation | Self::StopHooks
            ),
            // A provider that rejected the prompt as too long sends the turn
            // back through compaction and a fresh request — the same recovery
            // move as crossing the token line.
            Self::StreamAssistant => matches!(
                next,
                Self::ExecuteTools
                    | Self::EvaluateContinuation
                    | Self::AwaitElicitation
                    | Self::CompactRecovery
            ),
            // A permission decision is answered and the tools run.
            Self::AwaitPermission => matches!(next, Self::ExecuteTools | Self::ApplyToolResults),
            Self::AwaitElicitation => matches!(next, Self::StreamAssistant | Self::StartRequest),
            Self::ExecuteTools => matches!(next, Self::ApplyToolResults | Self::AwaitPermission),
            Self::ApplyToolResults => matches!(next, Self::EvaluateContinuation),
            // The stop hooks run on the way out, from the last decision point.
            Self::EvaluateContinuation => {
                matches!(next, Self::CompactRecovery | Self::StopHooks)
            }
            // A compaction that fails (circuit breaker open / stop hook) ends
            // the turn from `CompactRecovery`, and the closing path enters
            // the stop hooks from here.
            Self::CompactRecovery => {
                matches!(next, Self::StartRequest | Self::StopHooks | Self::Finalize)
            }
            Self::StopHooks => matches!(next, Self::Finalize),
            // Terminating an ended turn is a no-op, not a mistake.
            Self::Finalize | Self::Terminated => matches!(next, Self::Terminated),
        }
    }

    /// Returns true if this state waits for user input.
    #[must_use]
    pub const fn is_user_waiting(&self) -> bool {
        matches!(self, Self::AwaitPermission | Self::AwaitElicitation)
    }

    /// Returns true if this state is a terminal state.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Terminated)
    }
}

// ─── TerminalReason ─────────────────────────────────────────────────────────

/// Reason why a turn terminated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalReason {
    /// The turn completed normally (no more tool calls, response finished).
    Completed,
    /// The user interrupted the turn (Ctrl+C or explicit cancel).
    UserInterrupted,
    /// The token budget was exhausted.
    BudgetExhausted,
    /// The maximum number of turns was reached.
    MaxTurnsReached,
    /// An error occurred during execution.
    Error(String),
    /// A stop hook decided the conversation should end.
    StopHook,
    /// The model's response was stopped due to token limit.
    ///
    /// `produced` is the distinction the variant used to lose. A truncated
    /// answer and a turn that spent its whole ceiling thinking are both
    /// `max_tokens`, and only one of them is worth saying anything about: the
    /// first is a real answer cut short, the second is a run that did no work.
    /// Two terminal-bench runs ended this way with zero text and zero tool
    /// calls, and both were reported exactly like a finished task.
    TokenLimitStop {
        /// Whether the turn produced any text or tool call before the ceiling.
        produced: bool,
    },
    /// A repetitive loop of tool calls was detected.
    LoopDetected {
        /// Period of the repeating pattern (1 = exact repeat, 2 = A-B, etc.).
        period: usize,
        /// How many times the pattern repeated.
        repetitions: usize,
    },
    /// The API is unavailable after retry exhaustion.
    ApiUnavailable {
        /// Human-readable explanation.
        message: String,
    },
    /// The provider paused the turn (`pause_turn`/`compaction`) and asked for
    /// the same request again. Not an end: the loop re-sends with the same
    /// context, and this is the signal that says so.
    Resume,
}

impl std::fmt::Display for TerminalReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Completed => write!(f, "completed"),
            Self::UserInterrupted => write!(f, "user interrupted"),
            Self::BudgetExhausted => write!(f, "budget exhausted"),
            Self::MaxTurnsReached => write!(f, "max turns reached"),
            Self::Error(msg) => write!(f, "error: {msg}"),
            Self::StopHook => write!(f, "stop hook"),
            Self::TokenLimitStop { produced } => {
                if *produced {
                    write!(f, "token limit stop")
                } else {
                    // The reason a headless run shows its operator. "cut off"
                    // would be wrong here: nothing was cut, the ceiling went
                    // to thinking and there was never an answer to truncate.
                    write!(f, "token limit stop (no output)")
                }
            }
            Self::LoopDetected {
                period,
                repetitions,
            } => {
                write!(
                    f,
                    "loop detected (period={period}, repetitions={repetitions})"
                )
            }
            Self::ApiUnavailable { message } => {
                write!(f, "api unavailable: {message}")
            }
            Self::Resume => write!(f, "resumed"),
        }
    }
}

// ─── PendingItem ────────────────────────────────────────────────────────────

/// An assembled tool call ready for execution.
#[derive(Debug, Clone)]
pub struct PendingToolCall {
    /// Unique tool call ID.
    pub id: String,
    /// Tool name.
    pub name: String,
    /// JSON input arguments.
    pub input: serde_json::Value,
}

impl From<&ToolCall> for PendingToolCall {
    fn from(tc: &ToolCall) -> Self {
        Self {
            id: tc.id.clone(),
            name: tc.name.clone(),
            input: tc.input.clone(),
        }
    }
}

/// A resolved permission response.
#[derive(Debug, Clone)]
pub struct ResolvedPermission {
    /// The tool call ID this permission is for.
    pub tool_call_id: String,
    /// Whether the user approved.
    pub response: PermissionResponse,
}

/// A pending elevation request awaiting user response.
#[derive(Debug, Clone)]
pub struct PendingElevation {
    /// The tool call ID that needs elevation.
    pub tool_call_id: String,
    /// Tool name.
    pub tool_name: String,
    /// JSON input arguments.
    pub input: serde_json::Value,
    /// Request ID for the event.
    pub request_id: String,
    /// Why this call needs an explicit elevation decision.
    pub reason: String,
}

/// A resolved elicitation response.
#[derive(Debug, Clone)]
pub struct ResolvedElicitation {
    /// The elicitation request ID.
    pub request_id: String,
    /// User's response (if approved).
    pub response: Option<String>,
}

// ─── Time budget ────────────────────────────────────────────────────────────

/// The run's wall-clock budget and the milestones already announced.
#[derive(Debug, Clone, Copy)]
struct TimeBudgetClock {
    /// When the budget started counting.
    started: std::time::Instant,
    /// How long the run may take.
    budget: Duration,
    /// Milestones already told to the model, in order: 25 %, 50 %, 75 %, 90 %.
    announced: [bool; 4],
}

impl TimeBudgetClock {
    /// Fractions of the budget the model is told about, in order.
    const MILESTONES: [f64; 4] = [0.25, 0.5, 0.75, 0.9];

    /// Fraction of the budget already spent, as a number of at least zero.
    fn fraction(&self) -> f64 {
        let budget = self.budget.as_secs_f64();
        if budget <= 0.0 {
            return 1.0;
        }
        (self.started.elapsed().as_secs_f64() / budget).clamp(0.0, 1.0)
    }

    /// The highest milestone crossed since the last call, if any.
    ///
    /// A slow tool call can cross two milestones at once; only the later one
    /// is reported, because the earlier one's advice is already stale.
    fn next_milestone(&mut self) -> Option<u32> {
        let fraction = self.fraction();
        let mut due = None;
        for (index, milestone) in Self::MILESTONES.iter().enumerate() {
            if fraction >= *milestone && !self.announced[index] {
                self.announced[index] = true;
                due = Some(*milestone);
            }
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        due.map(|milestone| (milestone * 100.0) as u32)
    }
}

#[cfg(test)]
mod time_budget_tests {
    use super::TimeBudgetClock;
    use std::time::{Duration, Instant};

    fn clock_at(elapsed_secs: u64) -> TimeBudgetClock {
        TimeBudgetClock {
            started: Instant::now()
                .checked_sub(Duration::from_secs(elapsed_secs))
                .expect("instant arithmetic"),
            budget: Duration::from_secs(100),
            announced: [false; 4],
        }
    }

    #[test]
    fn each_milestone_is_reported_once() {
        let mut clock = clock_at(30);
        assert_eq!(clock.next_milestone(), Some(25));
        assert_eq!(clock.next_milestone(), None);
        assert!((clock.fraction() - 0.3).abs() < 0.05);
    }

    #[test]
    fn crossing_two_milestones_reports_only_the_later_one() {
        let mut clock = clock_at(60);
        assert_eq!(clock.next_milestone(), Some(50));
        assert_eq!(clock.next_milestone(), None);
    }

    #[test]
    fn the_last_stretch_is_announced() {
        let mut clock = clock_at(95);
        assert_eq!(clock.next_milestone(), Some(90));
        assert_eq!(clock.next_milestone(), None);
    }
}

// ─── SubmitOptions ──────────────────────────────────────────────────────────

/// Options for submitting a turn to the QueryEngine.
#[derive(Debug, Clone)]
pub struct SubmitOptions {
    /// Maximum number of turns to run (0 = unlimited).
    pub max_turns: u32,
    /// Maximum budget in USD for this submission (0 = use session default).
    pub max_budget_usd: f64,
    /// Token budget override (0 = use default thresholds).
    pub task_budget_tokens: u64,
    /// Optional JSON schema for structured output.
    pub json_schema: Option<serde_json::Value>,
    /// Custom system prompt to append.
    pub custom_system_prompt: Option<String>,
    /// System prompt to append for this turn only.
    pub append_system_prompt: Option<String>,
    /// User messages to replay (for session resume).
    pub replay_user_messages: Option<Vec<Message>>,
    /// Include partial messages in output.
    pub include_partial_messages: bool,
    /// Whether to handle elicitation requests.
    pub handle_elicitation: bool,
    /// Recover orphaned permission requests from disk.
    pub orphaned_permission: bool,
    /// Restrict tool execution to the given tool names for this submission.
    pub allowed_tools_override: Option<Vec<String>>,
    /// Optional agent id for forked/background flows.
    pub agent_id: Option<String>,
    /// Provider-specific request hints.
    pub provider_hints: HashMap<String, String>,
}

impl Default for SubmitOptions {
    fn default() -> Self {
        Self {
            max_turns: 0,
            max_budget_usd: 0.0,
            task_budget_tokens: 0,
            json_schema: None,
            custom_system_prompt: None,
            append_system_prompt: None,
            replay_user_messages: None,
            include_partial_messages: false,
            handle_elicitation: true,
            orphaned_permission: false,
            allowed_tools_override: None,
            agent_id: None,
            provider_hints: HashMap::new(),
        }
    }
}

/// Callback invoked for every state transition a turn makes.
///
/// The engine's internal recorder is `#[cfg(test)]`; this observer is the
/// production-safe way for an embedder (or an integration test) to see the
/// sequence a real turn took.
pub type TransitionObserver = Arc<dyn Fn(TurnState, TurnState) + Send + Sync>;

// ─── TurnRuntime ────────────────────────────────────────────────────────────

/// Where the request stood when the provider last measured it.
///
/// Compaction needs to know how big the next request will be. Guessing from
/// characters is unreliable — CJK is roughly one token per character where
/// ASCII is one per four, so the same byte count can mean 2–3× the tokens —
/// and the provider already charges for the exact request it received. This
/// records that measurement and the shape of the request it described, so the
/// estimate for the next one is "measured prefix + estimated suffix".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsageAnchor {
    /// Messages of history the measured request carried.
    pub history_messages: usize,
    /// Estimated tokens outside the history at the time.
    pub prefix_tokens: usize,
    /// Tokens the provider reported for the input, cached parts included.
    pub prompt_tokens: usize,
    /// Provider-measured overhead that is not history.
    ///
    /// This is `prompt_tokens` minus the measured request's estimated history:
    /// the system prompt, the per-turn injections, the serialized tool schemas
    /// and the request scaffolding the tokenizer charges for. It is the part
    /// compaction cannot touch, and targets have to be translated against it
    /// rather than against the message-only prefix — which undercounts a
    /// tool-heavy request by several thousand tokens.
    ///
    /// Zero until the provider answers; see
    /// [`QueryEngine::measured_fixed_tokens`].
    pub fixed_tokens: usize,
}

/// Per-turn runtime data — state, messages, pending items, budget.
#[allow(missing_debug_implementations)]
pub struct TurnRuntime {
    /// Current state machine state.
    pub state: TurnState,
    /// Messages for this turn.
    pub messages: Vec<Message>,
    /// Where `messages` holds the conversation history.
    ///
    /// `messages` is the whole request — preamble, system prompt, history,
    /// and per-turn injections. `(start, end)` marks the range that came from
    /// `session.messages`; everything outside it is rebuilt every turn and
    /// must survive compaction untouched. `None` means the list was not
    /// assembled by `build_context` (a test, or a caller supplying its own),
    /// and the whole list is treated as history.
    pub history_range: Option<(usize, usize)>,
    /// Estimated tokens the serialized tool schemas add to the request.
    ///
    /// Tools are not messages: the API client sends them in a separate `tools`
    /// array built from the registry. Leaving them out of the prefix made the
    /// estimate look comfortable while the provider measured a request several
    /// thousand tokens larger — on every request of the session.
    pub tool_schema_tokens: usize,
    /// Consecutive compaction passes that removed nothing, and the measured
    /// request size the most recent one ran at.
    ///
    /// A pass that removed nothing will remove nothing again at the same size;
    /// retrying it on every turn is what turned a two-hour benchmark run into
    /// 241 compaction passes and almost no work. See `no_progress_retry_at`.
    pub compact_no_progress: u32,
    /// The measured size at which the last no-progress pass ran.
    pub compact_no_progress_tokens: usize,
    /// The provider rejected this turn's request as over the window.
    ///
    /// Compaction after a rejection is not speculative: the request provably
    /// cannot be sent, whatever the estimate says, so the "no pass can reach
    /// the landing line" stand-down must not apply.
    pub provider_overflowed: bool,
    /// Pending tool calls awaiting execution.
    pub pending_tool_calls: Vec<PendingToolCall>,
    /// Pending permission request.
    pub pending_permission: Option<ResolvedPermission>,
    /// Pending elevation request awaiting user response.
    pub pending_elevation: Option<PendingElevation>,
    /// Pending elicitation request.
    pub pending_elicitation: Option<ResolvedElicitation>,
    /// Token budget tracker.
    pub budget: TokenBudget,
    /// Compact/recovery retry counter.
    pub recovery_count: u32,
    /// Consecutive compact failures (circuit breaker for auto-compaction).
    pub compact_failures: u32,
    /// Whether the stop hook is active.
    pub stop_hook_active: bool,
    /// The agent this turn belongs to, when it is a forked or background run.
    ///
    /// Permission decisions read this: a background run is one the person is
    /// not watching, which is a different question from the one they answer
    /// interactively.
    pub agent_id: Option<String>,
    /// Terminal reason (set when state transitions to Terminated).
    pub terminal_reason: Option<TerminalReason>,
    /// The last provider stop reason, for compaction strategy selection.
    ///
    /// `Reactive` compaction is for a response the model ran out of room for;
    /// that fact lives on the response, not in the context estimate.
    pub last_stop_reason: Option<String>,
    /// The strategy of the last compaction in this turn, if any.
    ///
    /// Strategy selection reads it so a pass that did not bring the context
    /// below the line escalates (the next attempt is more aggressive) rather
    /// than repeating the pass that just failed.
    pub last_compact_strategy: Option<crate::compact::CompactStrategy>,
    /// Optional observer called after every transition.
    pub transition_observer: Option<TransitionObserver>,
    /// How many times this turn re-asked after a `max_tokens` turn produced
    /// nothing.
    ///
    /// Separate from `recovery_count`, which bounds compaction passes: this
    /// one bounds a different resource (output room), and a turn that has
    /// spent its compaction budget has not necessarily spent this one.
    empty_answer_retries: u32,
}

impl TurnRuntime {
    /// Create a new turn runtime.
    #[must_use]
    pub const fn new(budget: TokenBudget) -> Self {
        Self {
            state: TurnState::Initialize,
            messages: Vec::new(),
            history_range: None,
            tool_schema_tokens: 0,
            compact_no_progress: 0,
            compact_no_progress_tokens: 0,
            provider_overflowed: false,
            pending_tool_calls: Vec::new(),
            pending_permission: None,
            pending_elevation: None,
            pending_elicitation: None,
            budget,
            recovery_count: 0,
            compact_failures: 0,
            stop_hook_active: false,
            agent_id: None,
            terminal_reason: None,
            last_stop_reason: None,
            last_compact_strategy: None,
            transition_observer: None,
            empty_answer_retries: 0,
        }
    }

    /// The slice of `messages` that is the conversation history.
    ///
    /// `messages` is a per-request list that also carries the parts of the
    /// prompt that are rebuilt every turn: the CLAUDE.md preamble, the system
    /// prompt, and the per-turn injections. Only the range `build_context`
    /// populated from `session.messages` is history, and only history may be
    /// compacted — replacing the whole list would drop the system prompt and
    /// duplicate the preamble on the next turn.
    #[must_use]
    pub fn history(&self) -> &[Message] {
        match self.history_range {
            Some((start, end)) if end <= self.messages.len() => &self.messages[start..end],
            _ => {
                // Not built by `build_context` (tests, callers that assemble
                // their own list): the whole list is the history.
                &self.messages
            }
        }
    }

    /// Replace the conversation history, leaving the preamble and the
    /// per-turn injections in place.
    pub fn replace_history(&mut self, history: Vec<Message>) {
        match self.history_range.take() {
            Some((start, end)) if end <= self.messages.len() => {
                let mut messages =
                    Vec::with_capacity(start + history.len() + (self.messages.len() - end));
                messages.extend(self.messages[..start].iter().cloned());
                messages.extend(history.iter().cloned());
                messages.extend(self.messages[end..].iter().cloned());
                let new_end = start + history.len();
                self.messages = messages;
                self.history_range = Some((start, new_end));
            }
            _ => self.messages = history,
        }
    }

    /// Record where `build_context` put the conversation history.
    pub const fn set_history_range(&mut self, start: usize, end: usize) {
        self.history_range = Some((start, end));
    }

    /// The context outside the history (preamble, system prompt, injections,
    /// serialized tool schemas), as it will be sent with the next request.
    #[must_use]
    pub fn context_prefix_tokens(&self) -> usize {
        let messages = match self.history_range {
            Some((start, end)) if end <= self.messages.len() => {
                PromptCompiler::estimate_message_tokens(&self.messages[..start])
                    + PromptCompiler::estimate_message_tokens(&self.messages[end..])
            }
            _ => 0,
        };
        messages + self.tool_schema_tokens
    }

    /// The request as it will be sent, measured where possible.
    ///
    /// `chars / 4` is a heuristic and it is wrong by 2–3× on CJK text and on
    /// JSON-heavy tool output — which is exactly the workload that decides
    /// when compaction fires. The provider's own count for the last request is
    /// a measurement, so everything it covered is taken as read and only what
    /// was appended since is estimated. With no measurement yet, this is the
    /// heuristic over the whole list.
    ///
    /// The anchor lives on the engine, not the turn: a measurement from the
    /// previous turn still describes the start of this one's history, and that
    /// is the common case — the request that overflows is the one after the
    /// measurement, not the one that produced it. Pass `None` to measure the
    /// whole list heuristically.
    #[must_use]
    pub fn context_tokens(&self, anchor: Option<UsageAnchor>) -> usize {
        // The tool schemas ride outside the message list; without them the
        // estimate is several thousand tokens short of what is sent.
        let whole_list =
            || PromptCompiler::estimate_message_tokens(&self.messages) + self.tool_schema_tokens;
        let Some(anchor) = anchor else {
            return whole_list();
        };
        let history = self.history();
        // A shorter history means compaction replaced the messages the
        // measurement described; the anchor no longer accounts for this list.
        // A zero count means the shape was recorded but the provider has not
        // answered yet.
        if anchor.history_messages > history.len() || anchor.prompt_tokens == 0 {
            return whole_list();
        }
        let appended = PromptCompiler::estimate_message_tokens(&history[anchor.history_messages..]);
        // Per-turn injections (denied-tool summaries, custom system prompts)
        // sit outside the history and are rebuilt each turn. Only growth can
        // make the request bigger than what the provider measured; a shrink
        // over-estimates slightly, which errs toward compacting early.
        let prefix_growth = self
            .context_prefix_tokens()
            .saturating_sub(anchor.prefix_tokens);
        anchor
            .prompt_tokens
            .saturating_add(appended)
            .saturating_add(prefix_growth)
    }

    /// Reset for a new turn.
    ///
    /// `messages` is the per-request message list, rebuilt from scratch by
    /// `build_context` each turn. It must be cleared here: the turn runtime is
    /// reused across the whole `submit_streaming` loop, so a list left in place
    /// would be extended again on every turn — duplicating the context
    /// preamble, the system prompt, and the entire session history once per
    /// turn, and growing the request body superlinearly.
    pub fn reset(&mut self) {
        self.state = TurnState::Initialize;
        self.messages.clear();
        self.history_range = None;
        self.pending_tool_calls.clear();
        self.pending_permission = None;
        self.pending_elevation = None;
        self.pending_elicitation = None;
        self.last_stop_reason = None;
        self.empty_answer_retries = 0;
        self.provider_overflowed = false;
        // `last_compact_strategy` deliberately survives the turn boundary: it
        // is the escalation ladder, and a pass that made no progress must be
        // followed by a heavier one even when the next attempt starts a new
        // turn. Clearing it here made every turn retry the same ineffective
        // strategy, three times, until the circuit breaker ended the run.
        self.budget.reset_turn();
    }

    /// Transition to a new state, reporting a move the graph does not allow.
    ///
    /// The state is a diagnostic — what the engine is doing, for the UI and for
    /// whoever reads a log — and this used to be a bare assignment, so any
    /// state could follow any other and the value meant nothing. The legal
    /// moves are in [`TurnState::can_transition_to`].
    ///
    /// An illegal move is reported rather than refused: refusing would turn a
    /// bookkeeping mistake into a failed turn, and the work is not what is
    /// wrong.
    pub fn transition_to(&mut self, new_state: TurnState) {
        if let Some(observer) = &self.transition_observer {
            observer(self.state, new_state);
        }
        #[cfg(test)]
        record_transition(self.state, new_state);

        if !self.state.can_transition_to(new_state) {
            tracing::warn!(
                from = self.state.as_str(),
                to = new_state.as_str(),
                "the turn moved between states the graph does not allow"
            );
        }
        self.state = new_state;
    }

    /// Terminate with a reason.
    pub fn terminate(&mut self, reason: TerminalReason) {
        self.transition_to(TurnState::Terminated);
        self.terminal_reason = Some(reason);
    }
}

/// Why a turn could not get a response from the model.
///
/// The turn loop used to tell these apart by reading the message —
/// `Err(e) if e.starts_with("The AI service is temporarily unavailable")` —
/// which meant the two halves of the engine were joined by a sentence: change
/// the wording where it is produced and the loop quietly stops recognising it,
/// reporting a provider outage as a plain error instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnError {
    /// The provider could not be reached, after retrying. The same request may
    /// work later, and the session is intact.
    ApiUnavailable {
        /// What to show a person.
        message: String,
    },
    /// The request did not fit the model's context window. This is not fatal:
    /// the provider has just said how large its window is, and compacting can
    /// make the request fit, so the turn retries after compaction instead of
    /// ending.
    ContextOverflow {
        /// What to show a person.
        message: String,
        /// The input size the provider measured, when it said so.
        actual: Option<usize>,
        /// The limit the request exceeded; zero when the provider gave none.
        limit: usize,
    },
    /// Anything else: a bad request, a refusal, a bug here.
    Fatal {
        /// What to show a person.
        message: String,
    },
}

impl TurnError {
    /// The message a person sees.
    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            Self::ApiUnavailable { message }
            | Self::ContextOverflow { message, .. }
            | Self::Fatal { message } => message,
        }
    }

    /// The reason this turn ended.
    #[must_use]
    pub fn terminal_reason(&self) -> TerminalReason {
        match self {
            Self::ApiUnavailable { message } => TerminalReason::ApiUnavailable {
                message: message.clone(),
            },
            Self::ContextOverflow { .. } => TerminalReason::ApiUnavailable {
                message: self.message().to_string(),
            },
            Self::Fatal { message } => TerminalReason::Error(message.clone()),
        }
    }

    /// The same error as a plain message.
    #[must_use]
    pub fn into_message(self) -> String {
        match self {
            Self::ApiUnavailable { message }
            | Self::ContextOverflow { message, .. }
            | Self::Fatal { message } => message,
        }
    }
}

impl From<String> for TurnError {
    /// A message that is not a provider outage — a tool that could not run, a
    /// hook that failed — is the engine's own trouble.
    fn from(message: String) -> Self {
        Self::Fatal { message }
    }
}

impl From<TurnError> for String {
    fn from(error: TurnError) -> Self {
        error.into_message()
    }
}

impl std::fmt::Display for TurnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

// Every transition a turn made, for the test that checks the graph against
// what the engine actually does.
#[cfg(test)]
thread_local! {
    static RECORDED_TRANSITIONS: std::cell::RefCell<Vec<(TurnState, TurnState)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
fn record_transition(from: TurnState, to: TurnState) {
    RECORDED_TRANSITIONS.with(|recorded| recorded.borrow_mut().push((from, to)));
}

#[cfg(test)]
fn take_recorded_transitions() -> Vec<(TurnState, TurnState)> {
    RECORDED_TRANSITIONS.with(|recorded| std::mem::take(&mut *recorded.borrow_mut()))
}

/// What one streamed request produced.
///
/// The stop reason is part of it: the blocking path has always had it on the
/// response, and the streaming path used to drop it — which is why a truncated
/// answer looked like a finished turn.
#[derive(Debug, Clone)]
struct StreamedResponse {
    content: String,
    tool_calls: Vec<ToolCall>,
    /// Calls that were executed while the stream was still running.
    ///
    /// Their `tool_use` messages still have to be written to the session: the
    /// assistant turn and its `tool_result` are a pair, and a history with the
    /// result but not the call is rejected by the provider.
    early_calls: Vec<ToolCall>,
    early_results: Vec<ToolResult>,
    usage: UsageSummary,
    stop_reason: String,
    /// A thinking block the provider requires to be passed back, if any.
    thinking: Option<ThinkingBlock>,
}

/// The output ceiling a `max_tokens` turn that produced nothing should be
/// re-asked with, if re-asking is worth anything.
///
/// A reasoning model on a hard step can spend the whole ceiling thinking and
/// emit neither text nor a tool call. Re-asking it at the same ceiling is a
/// coin flip that costs a full request, so this only offers a retry when the
/// request can actually carry a larger one. `None` means "do not retry": the
/// turn limits it spent are real and the answer is that this run produced
/// nothing.
///
/// The bound is the provider's own rule — `input + max_tokens ≤ window` — plus
/// the ceiling `CODAPE_MAX_TOKENS` already applies statically: never above half
/// the window. That second bound is not cosmetic. The context size used here is
/// an estimate, and the heuristic under-counts JSON- and code-heavy traffic by
/// 30-40 %, so a request sized to the last available token would be rejected on
/// arrival. Half leaves the estimate room to be wrong, and it is the only
/// ceiling compaction can recover from — the compaction line never drops below
/// half the window, so a ceiling above it would leave nothing to compact into.
///
/// The window comes from the engine rather than the model, so a window the
/// provider stated after an overflow, or one configured in settings, is the
/// one used. A window nobody established is not used at all: guessing one
/// small would invent a limit, and guessing one large would send a request
/// the provider rejects.
#[must_use]
fn retry_ceiling(
    used: u32,
    context_tokens: usize,
    window: crate::types::ResolvedContextWindow,
) -> Option<u32> {
    if window.tokens == 0 || window.source.is_guess() {
        return None;
    }
    let available = window.tokens.saturating_sub(context_tokens);
    let recoverable = window.tokens / 2;
    let room = available.min(recoverable);
    let candidate = used
        .saturating_mul(2)
        .min(u32::try_from(room).unwrap_or(u32::MAX));
    // No headroom means a bigger number would be rejected, and the same one
    // would be spent the same way.
    (candidate > used).then_some(candidate)
}

/// Whether a response produced anything a person or the loop can act on.
///
/// Text and tool calls both count; a thinking block does not. That asymmetry
/// is the whole point of the distinction: thinking is what consumed the
/// budget, not what the run came back with.
#[must_use]
fn produced_output(content: &str, tool_calls: usize) -> bool {
    !content.trim().is_empty() || tool_calls > 0
}

/// Whether a provider's stop reason means "the answer was cut off".
///
/// Providers spell it two ways: `max_tokens` is Anthropic's, `length` is
/// OpenAI's. Reading only one of them means a truncated answer from the other
/// provider is reported as a finished turn.
#[must_use]
fn stop_reason_eq(stop_reason: &str, expected: &str) -> bool {
    match expected {
        "max_tokens" => stop_reason == "max_tokens" || stop_reason == "length",
        other => stop_reason == other,
    }
}

/// Combine text delivered before a retry with the text the retry produced.
///
/// A stream cannot be resumed at a byte offset, so a retry re-requests the
/// turn from the beginning and the model regenerates the opening it already
/// produced. The two texts therefore share a prefix, and taking the longer one
/// keeps the reader from being shown the same sentence twice. When the retry
/// instead picks up past where the first attempt stopped — or diverges — both
/// are kept, so nothing already delivered is lost.
///
/// This is the all-at-once form, kept as the reference behaviour for tests.
/// The streaming loop uses [`StreamTextMerger`], which reaches the same result
/// one delta at a time.
#[cfg(test)]
#[must_use]
fn merge_stream_text(delivered: &str, retried: &str) -> String {
    if retried.starts_with(delivered) {
        retried.to_string()
    } else if delivered.starts_with(retried) {
        delivered.to_string()
    } else {
        let mut merged = String::with_capacity(delivered.len() + retried.len());
        merged.push_str(delivered);
        merged.push_str(retried);
        merged
    }
}

/// Incrementally merge the text of a retried stream.
///
/// A retry regenerates the answer from the beginning. Comparing the whole
/// accumulated attempt against the whole delivered text on every delta — as
/// the loop used to — re-applied the repeated opening once per delta, so a
/// divergent retry emitted `x`, then `xy`, then `xyz` instead of the three
/// fragments once, and the output grew quadratically.
///
/// This tracks the attempt separately from what the reader has seen. Each
/// delta is examined exactly once: it is skipped while it still reproduces
/// the delivered prefix, and from the point the retry diverges (or picks up
/// past what was delivered) the new suffix is emitted once and appended to
/// the delivered text.
#[derive(Debug, Default)]
struct StreamTextMerger {
    /// Everything emitted to the reader so far, across attempts.
    delivered: String,
    /// The current attempt's text. A retry starts it again from empty.
    attempt: String,
    /// Set once the attempt stops matching the delivered prefix.
    diverged: bool,
}

impl StreamTextMerger {
    fn new() -> Self {
        Self::default()
    }

    /// Start a fresh attempt. A retry regenerates the answer from the top, so
    /// the comparison with what was delivered starts over.
    fn begin_attempt(&mut self) {
        self.attempt.clear();
        self.diverged = false;
    }

    /// The text emitted to the reader so far.
    fn delivered(&self) -> &str {
        &self.delivered
    }

    /// Finish streaming, returning the full delivered text.
    fn into_delivered(self) -> String {
        self.delivered
    }

    /// Merge one delta from the current attempt.
    ///
    /// Returns only the bytes the reader has not seen yet, or `None` when the
    /// delta merely repeats the already-delivered opening.
    fn push_delta(&mut self, delta: &str) -> Option<String> {
        if delta.is_empty() {
            return None;
        }

        let start = self.attempt.len();
        self.attempt.push_str(delta);

        if self.diverged {
            self.delivered.push_str(delta);
            return Some(delta.to_string());
        }

        // Until the attempt diverges, its prefix matches the delivered text
        // byte for byte, so `position` is a char boundary in both strings.
        let mut position = start;
        for ch in delta.chars() {
            match self
                .delivered
                .get(position..)
                .and_then(|s| s.chars().next())
            {
                Some(delivered_ch) if delivered_ch == ch => {
                    position += ch.len_utf8();
                }
                _ => {
                    // The retry has either diverged from the delivered answer
                    // or run past its end. Everything from here is new.
                    self.diverged = true;
                    let new_text = self.attempt[position..].to_string();
                    self.delivered.push_str(&new_text);
                    return Some(new_text);
                }
            }
        }

        None
    }
}

/// What a provider's stop reason asks the turn loop to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopDisposition {
    /// The model wants tools run before it can go on.
    RunTools,
    /// The provider paused the turn and expects the same request again: it
    /// ran out of a server-side budget (`pause_turn`) or compacted its own
    /// context (`compaction`). Neither is the end of the turn — treating them
    /// as one silently truncates the answer.
    Resume,
    /// The turn is over, whether the model finished or was cut short.
    Stop,
}

/// Read a provider's stop reason for what it asks the loop to do.
///
/// Anthropic spells the paused cases `pause_turn` and `compaction`; OpenAI
/// compatible providers spell a tool call `tool_calls`. Anything not named
/// here — `end_turn`, `max_tokens`, `length`, `refusal`, `stop`, and whatever
/// a provider invents next — ends the turn, which is the safe default: an
/// unknown reason must not make the loop re-request forever.
#[must_use]
pub fn classify_stop_reason(stop_reason: &str) -> StopDisposition {
    match stop_reason {
        "tool_use" | "tool_calls" => StopDisposition::RunTools,
        "pause_turn" | "compaction" => StopDisposition::Resume,
        _ => StopDisposition::Stop,
    }
}

// ─── Compaction outcome ─────────────────────────────────────────────────────

/// What asked for compaction to be considered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactTrigger {
    /// The context crossed the token line.
    TokenThreshold,
    /// The prompt cache went cold while the session sat idle.
    CacheTtl,
}

/// What became of the LLM summary step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummaryOutcome {
    /// The model wrote the summary.
    Summarized,
    /// The model was not asked — this trigger does not use a summary.
    NotRequested,
    /// There was too little conversation between the preserved head and tail
    /// to be worth a call.
    RangeTooSmall {
        /// Messages available.
        messages: usize,
    },
    /// The summary call failed.
    Failed {
        /// The failure.
        error: String,
    },
    /// The summary came back empty.
    Empty,
}

impl SummaryOutcome {
    /// The stable name this outcome carries on the event stream.
    ///
    /// The error text of a failed summary is useful in logs but is not a stable
    /// protocol value, and the event is what an external host records. The
    /// label says which path the pass took; the failure detail stays in
    /// `tracing`.
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Summarized => "summarized",
            Self::NotRequested => "not_requested",
            Self::RangeTooSmall { .. } => "range_too_small",
            Self::Failed { .. } => "failed",
            Self::Empty => "empty",
        }
    }
}

/// What compaction did, or why it did not happen.
///
/// The engine used to answer this only in `tracing::warn!` strings — a caller
/// cannot act on a log line and a test cannot assert one. Every path through
/// `run_compact_recovery` now returns one of these instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactOutcome {
    /// Compaction ran and removed tokens.
    Compacted {
        /// The rung that was requested.
        strategy: CompactStrategy,
        /// What actually ran (an `Auto` request can stop after its micro pass).
        effective: CompactStrategy,
        /// What triggered it.
        trigger: CompactTrigger,
        /// Estimated tokens removed.
        tokens_removed: usize,
        /// Messages left after compaction.
        messages_remaining: usize,
        /// What became of the summary step.
        summary: SummaryOutcome,
        /// Whole-request size before the pass.
        tokens_before: usize,
        /// Whole-request size after the pass.
        tokens_after: usize,
        /// The landing line the pass was judged against.
        target_tokens: usize,
        /// The non-compactable prefix included in both sizes.
        prefix_tokens: usize,
        /// Provider-anchored size before the pass, when there is a measurement
        /// to anchor on. `tokens_before` is an estimate; this is the number the
        /// trigger was judged against, and the two can differ by the tokenizer's
        /// ratio. `None` before the first answered request.
        measured_tokens: Option<usize>,
        /// The fixed overhead the targets subtracted, in strategy units.
        fixed_tokens: usize,
    },
    /// The check ran but this trigger stood down: the context is not in the
    /// band this trigger acts on, and the other path owns it.
    ///
    /// The cache-TTL pass is the one that defers — once the token line has
    /// been crossed, compacting for the threshold is the right response and
    /// doing both would compact twice for one reason.
    DeferredToThresholdPath {
        /// Estimated tokens at the time.
        estimated_tokens: usize,
    },
    /// The recovery retry budget is spent.
    SkippedRecoveryBudget {
        /// Recovery attempts already made.
        recovery_count: u32,
        /// The limit.
        max: u32,
    },
    /// Auto-compaction is paused after consecutive failures.
    CircuitBreakerOpen {
        /// Consecutive failures so far.
        consecutive_failures: u32,
        /// The limit.
        max: u32,
    },
    /// The Stop hook ended the turn instead of compacting.
    StopHookTerminated {
        /// The hook's reason.
        reason: String,
    },
    /// A recent pass removed nothing, and the context has not changed enough
    /// for another one to come out differently.
    BackedOff {
        /// Consecutive passes that removed nothing so far.
        consecutive_no_progress: u32,
        /// Measured size of the request now.
        estimated_tokens: usize,
        /// Measured size at which the last no-progress pass ran.
        last_tokens: usize,
        /// Measured size at which a pass is worth another attempt.
        retry_at: usize,
    },
    /// No pass can put the request under the landing line.
    ///
    /// The request's fixed part — system prompt, injections, serialized tool
    /// schemas and request scaffolding — already fills the line, so even a
    /// pass that kept only a quarter of the history would leave the request
    /// above it. Running one would spend the recovery budget and destroy
    /// working context to prove arithmetic that is already known. The caller
    /// decides what a context that cannot be compacted means; only the
    /// blocked line makes it fatal.
    Uncompactable {
        /// The request's non-history overhead (strategy units).
        fixed_tokens: usize,
        /// The landing line the overhead already fills (strategy units).
        landing_tokens: usize,
        /// The smallest whole request a pass keeping a quarter of the history
        /// could leave (strategy units).
        minimum_tokens: usize,
    },
    /// Compaction ran but removed nothing — the context is irrecoverable by
    /// this strategy, and the circuit breaker counts it.
    NoProgress {
        /// The rung that was requested.
        strategy: CompactStrategy,
        /// What actually ran.
        effective: CompactStrategy,
        /// What triggered it.
        trigger: CompactTrigger,
        /// Tokens before.
        tokens_before: usize,
        /// Tokens after.
        tokens_after: usize,
        /// What became of the summary step.
        summary: SummaryOutcome,
    },
}

impl CompactOutcome {
    /// The message to terminate the turn with, when this outcome is a failure.
    ///
    /// This is the only failure test the engine uses: every terminal consumer
    /// needs the reason, and `None` is exactly "not a failure".
    #[must_use]
    pub fn failure_message(&self) -> Option<String> {
        match self {
            Self::CircuitBreakerOpen {
                consecutive_failures,
                max,
            } => Some(format!(
                "CompactRecovery: {consecutive_failures}/{max} consecutive compact failures — \
                 stopping auto-retry. Use /compact manually or start a new session."
            )),
            Self::StopHookTerminated { reason } => Some(reason.clone()),
            _ => None,
        }
    }

    /// Whether this outcome means auto-compaction has been paused.
    ///
    /// The breaker is compaction's own stop signal, not the turn's terminal
    /// reason: a caller that asked for recovery because the token budget is
    /// spent still ends as `BudgetExhausted`, and one that asked because the
    /// provider rejected the request still reports that error.
    #[must_use]
    pub const fn is_circuit_breaker_open(&self) -> bool {
        matches!(self, Self::CircuitBreakerOpen { .. })
    }
}

/// The terminal reason a failed compaction leaves a budget-exhausted turn.
///
/// The breaker is compaction's stop signal, not the turn's reason. The budget
/// is still spent, and `BudgetExhausted` is what the caller can act on;
/// reporting compaction's message instead named a remedy ("/compact manually")
/// that a headless run does not have.
#[must_use]
fn budget_terminal_reason(outcome: &CompactOutcome) -> Option<TerminalReason> {
    if outcome.is_circuit_breaker_open() {
        Some(TerminalReason::BudgetExhausted)
    } else {
        outcome.failure_message().map(TerminalReason::Error)
    }
}

/// The next, heavier rung of the rule-based compaction ladder.
///
/// The ladder is monotone in what it removes, so stepping down is the move
/// that can actually succeed after a pass failed to make progress. `Reactive`
/// keeps whole recent turns and sits between `Auto` and `Snip` in force.
#[must_use]
const fn escalate_compact_strategy(strategy: CompactStrategy) -> CompactStrategy {
    match strategy {
        CompactStrategy::Micro => CompactStrategy::Auto,
        CompactStrategy::Auto | CompactStrategy::Reactive => CompactStrategy::Snip,
        CompactStrategy::Snip | CompactStrategy::LastResort => CompactStrategy::LastResort,
    }
}

/// The wire name for a compaction trigger.
#[must_use]
const fn compact_trigger_name(trigger: CompactTrigger) -> &'static str {
    match trigger {
        CompactTrigger::TokenThreshold => "token_threshold",
        CompactTrigger::CacheTtl => "cache_ttl",
    }
}

/// Summarize a conversation interval, through a backend when one is attached.
///
/// With no backend this is exactly the built-in local LLM summary. With a
/// backend, a failure or an empty summary falls back to the built-in local
/// path — the backend is an optimization, never a new failure mode. Both paths
/// collapse to `Result<String, String>` so the PTL retry and the rule-based
/// fallback treat them uniformly.
///
/// This is a free function rather than a method because the background
/// pre-generation runs on a spawned task: it has to capture the client and the
/// backend before they are borrowed by anything else, and it holds no engine
/// state.
async fn summarize_conversation(
    api_client: &dyn ApiClient,
    backend: Option<&Arc<dyn crate::compaction_backend::CompactionBackend>>,
    conversation: &str,
    max_tokens: u32,
) -> Result<String, String> {
    let local = || async {
        crate::prompts::generate_compact_summary(api_client, conversation, None, max_tokens)
            .await
            .map_err(|e| e.to_string())
    };
    let Some(backend) = backend else {
        return local().await;
    };
    tracing::info!(
        "CompactRecovery: summarizing via compaction backend '{}'",
        backend.name()
    );
    match backend
        .summarize(crate::compaction_backend::CompactionRequest {
            conversation: conversation.to_string(),
            focus: None,
            max_output_tokens: max_tokens,
        })
        .await
    {
        Ok(text) if !text.trim().is_empty() => Ok(text),
        Ok(_) => {
            tracing::warn!(
                "CompactionBackend '{}': empty summary — falling back to local summary",
                backend.name()
            );
            local().await
        }
        Err(e) => {
            tracing::warn!(
                "CompactionBackend '{}': failed ({e}) — falling back to local summary",
                backend.name()
            );
            local().await
        }
    }
}

/// Project a [`CompactOutcome`] onto the event stream's [`CompactionEvent`].
///
/// The strategies and triggers become strings because the event crosses the
/// wire protocol; the outcome stays the engine's own typed vocabulary.
#[must_use]
fn compaction_event_from_outcome(outcome: &CompactOutcome) -> CompactionEvent {
    match outcome {
        CompactOutcome::Compacted {
            strategy,
            effective,
            trigger,
            tokens_removed,
            messages_remaining,
            summary,
            tokens_before,
            tokens_after,
            target_tokens,
            prefix_tokens,
            measured_tokens,
            fixed_tokens,
        } => CompactionEvent::Compacted {
            strategy: format!("{strategy:?}"),
            effective: format!("{effective:?}"),
            trigger: compact_trigger_name(*trigger).to_string(),
            tokens_removed: *tokens_removed,
            messages_remaining: *messages_remaining,
            tokens_before: *tokens_before,
            tokens_after: *tokens_after,
            target_tokens: *target_tokens,
            prefix_tokens: *prefix_tokens,
            summary: summary.label().to_string(),
            measured_tokens: *measured_tokens,
            fixed_tokens: *fixed_tokens,
        },
        CompactOutcome::DeferredToThresholdPath { estimated_tokens } => {
            CompactionEvent::DeferredToThresholdPath {
                estimated_tokens: *estimated_tokens,
            }
        }
        CompactOutcome::SkippedRecoveryBudget {
            recovery_count,
            max,
        } => CompactionEvent::SkippedRecoveryBudget {
            recovery_count: *recovery_count,
            max: *max,
        },
        CompactOutcome::CircuitBreakerOpen {
            consecutive_failures,
            max,
        } => CompactionEvent::CircuitBreakerOpen {
            consecutive_failures: *consecutive_failures,
            max: *max,
        },
        CompactOutcome::StopHookTerminated { reason } => CompactionEvent::StopHookTerminated {
            reason: reason.clone(),
        },
        CompactOutcome::BackedOff {
            consecutive_no_progress,
            estimated_tokens,
            last_tokens,
            retry_at,
        } => CompactionEvent::BackedOff {
            consecutive_no_progress: *consecutive_no_progress,
            estimated_tokens: *estimated_tokens,
            last_tokens: *last_tokens,
            retry_at: *retry_at,
        },
        CompactOutcome::Uncompactable {
            fixed_tokens,
            landing_tokens,
            minimum_tokens,
        } => CompactionEvent::Uncompactable {
            fixed_tokens: *fixed_tokens,
            landing_tokens: *landing_tokens,
            minimum_tokens: *minimum_tokens,
        },
        CompactOutcome::NoProgress {
            strategy,
            effective,
            trigger,
            tokens_before,
            tokens_after,
            summary: _,
        } => CompactionEvent::NoProgress {
            strategy: format!("{strategy:?}"),
            effective: format!("{effective:?}"),
            trigger: compact_trigger_name(*trigger).to_string(),
            tokens_before: *tokens_before,
            tokens_after: *tokens_after,
        },
    }
}

/// Whether the idle cache-TTL timer should ask for compaction.
///
/// Pure, so the timing rule can be tested without a clock: `true` when there
/// has been a request, the session has been idle for at least `wake_after`,
/// and the timer has not already asked for *this* idle window (its last
/// request predates the last time the timer fired).
#[must_use]
fn cache_ttl_timer_should_request(
    last_request: Option<Instant>,
    last_fired: Option<Instant>,
    wake_after: Duration,
) -> bool {
    let Some(last) = last_request else {
        return false;
    };
    if last.elapsed() < wake_after {
        return false;
    }
    last_fired.is_none_or(|fired| fired < last)
}

/// A running cache-TTL compaction timer.
///
/// Dropping the handle stops the timer; the session that owns the engine keeps
/// it alive for as long as the engine should watch its cache.
#[derive(Debug)]
pub struct CacheTtlTimer {
    cancel: Arc<AtomicBool>,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for CacheTtlTimer {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
        self.handle.abort();
    }
}

/// How many provider-paused turns the loop will resume before it gives up.
///
/// A provider that answers `pause_turn` forever must not spin the loop
/// forever; past this many consecutive resumes the pause is reported as the
/// end of the run.
pub const MAX_PROVIDER_RESUMES: u32 = 8;

/// How many times one request is retried after the provider says its input
/// does not fit.
///
/// Each retry is preceded by a compaction, and a compaction may not remove
/// anything — so the retries are bounded separately from the recovery budget,
/// which only spends an attempt when the pass made progress. Two is enough:
/// one pass learns the real window, the second compacts to it.
const MAX_CONTEXT_OVERFLOW_RETRIES: u32 = 2;

/// How many times a turn that produced nothing while truncated is re-asked at
/// a larger output ceiling.
///
/// One. The first re-ask doubles the ceiling (as far as the window allows);
/// a second answer that is still empty says the model cannot finish the step at
/// any ceiling this conversation can carry, and paying twice more to learn the
/// same thing is not worth it.
const MAX_EMPTY_ANSWER_RETRIES: u32 = 1;

/// What the outer turn loop does after a turn returns a reason.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LoopStep {
    /// Run another turn.
    Continue,
    /// Stop the run, reporting this reason.
    Break(TerminalReason),
}

/// Update the count of provider-paused turns that have run back to back.
///
/// `resumes` must count *consecutive* pauses: a `Resume` increments it, and
/// every other reason resets it to zero. A counter that only ever increments
/// would eventually stop resuming after a handful of unrelated turns, even
/// though the cap is meant to bound one run of provider pauses.
const fn track_provider_resumes(reason: &TerminalReason, resumes: &mut u32) {
    if matches!(reason, TerminalReason::Resume) {
        *resumes += 1;
    } else {
        *resumes = 0;
    }
}

/// Decide whether the loop runs another turn after `reason`.
///
/// `last_message_is_tool_result` is the old rule for `Completed`: a turn that
/// ended on a tool result is not finished, the model has not seen the result
/// yet. `resumes` is how many consecutive provider-paused turns have already
/// run, which bounds the `Resume` case.
fn next_loop_step(
    reason: &TerminalReason,
    last_message_is_tool_result: bool,
    resumes: u32,
) -> LoopStep {
    let step = next_loop_step_unlogged(reason, last_message_is_tool_result, resumes);
    // The loop's decision is the last thing that happens before a run ends,
    // and it used to leave no trace at all: a run that stopped after a tool
    // result looked identical to one the provider had truncated.
    tracing::debug!(
        reason = %reason,
        last_message_is_tool_result,
        provider_resumes = resumes,
        step = ?step,
        "turn loop decided"
    );
    step
}

fn next_loop_step_unlogged(
    reason: &TerminalReason,
    last_message_is_tool_result: bool,
    resumes: u32,
) -> LoopStep {
    if matches!(reason, TerminalReason::Resume) {
        // A paused turn continues with the same context. The bound keeps a
        // provider that never stops pausing from looping forever.
        return if resumes >= MAX_PROVIDER_RESUMES {
            LoopStep::Break(TerminalReason::Completed)
        } else {
            LoopStep::Continue
        };
    }

    if matches!(
        reason,
        TerminalReason::Completed
            | TerminalReason::Error(_)
            | TerminalReason::TokenLimitStop { .. }
            | TerminalReason::LoopDetected { .. }
            | TerminalReason::ApiUnavailable { .. }
    ) {
        if matches!(reason, TerminalReason::Completed) && last_message_is_tool_result {
            LoopStep::Continue
        } else {
            LoopStep::Break(reason.clone())
        }
    } else {
        LoopStep::Continue
    }
}

// ─── Interrupts ─────────────────────────────────────────────────────────────

/// A way to stop the turn an engine is running, from outside it.
///
/// `QueryEngine::interrupt` used to send an event and nothing else: the event
/// was emitted, nothing listened for it, and the turn ran to completion — so
/// Ctrl+C abandoned the *result* while the work carried on. This handle is
/// what a caller outside the engine (a key handler, a parent task) can hold,
/// and what the turn loop actually looks at.
#[derive(Debug, Clone, Default)]
pub struct InterruptHandle(Arc<std::sync::atomic::AtomicBool>);

impl InterruptHandle {
    /// Ask the running turn to stop at its next safe point.
    pub fn interrupt(&self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Whether a stop has been asked for.
    #[must_use]
    pub fn is_interrupted(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Clear the request, so the next turn runs.
    pub fn clear(&self) {
        self.0.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

// ─── Turn counting ──────────────────────────────────────────────────────────

/// A live view of an engine's turn counter, readable after the engine moves.
///
/// The engine is handed to a task for the duration of a turn, so the caller
/// that writes the closing `result` frame no longer holds it. Cloning this
/// handle before the handoff keeps the count visible; it reads the same atomic
/// the engine writes, so it cannot drift.
#[derive(Debug, Clone)]
pub struct TurnCounter(Arc<std::sync::atomic::AtomicU32>);

impl TurnCounter {
    /// The turn number the engine has reached for this session.
    #[must_use]
    pub fn value(&self) -> u32 {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// How many turns were consumed since `self` was captured at `before`.
    #[must_use]
    pub fn consumed_since(&self, before: u32) -> u32 {
        self.value().saturating_sub(before)
    }
}

// ─── QueryEngine ────────────────────────────────────────────────────────────

/// QueryEngine — manages conversation turns with the AI model.
///
/// The engine handles:
/// - Building context from session state
/// - Sending requests to the API
/// - Streaming responses and parsing tool calls
/// - Permission checks before tool execution
/// - Tool execution via the registry
/// - Token budget and cost tracking
/// - Compact/recovery logic
/// - Elicitation requests
#[allow(missing_debug_implementations)]
pub struct QueryEngine {
    /// API client for making requests.
    api_client: Box<dyn ApiClient>,
    /// Model to use for API requests.
    model: ModelId,
    /// Model to retry with, once, when the primary is unavailable.
    ///
    /// After a successful fallback retry the engine keeps using this model for
    /// the rest of the turn and session.
    fallback_model: Option<ModelId>,
    /// Session state for the current conversation.
    session: SessionState,
    /// Cost tracker for billing.
    cost_tracker: CostTracker,
    /// Whether the running turn has been asked to stop.
    interrupt: InterruptHandle,
    /// Permission engine for tool approval.
    permission_engine: PermissionEngine,
    /// Event sender for streaming events to UI/SDK.
    event_tx: mpsc::UnboundedSender<EngineEvent>,
    /// Current turn number (1-indexed), shared so a caller can read it after
    /// handing the engine to a task. `--max-turns` bounds this counter and the
    /// closing `result` frame reports it, so both must see the same number.
    current_turn: Arc<std::sync::atomic::AtomicU32>,
    /// Denied tool calls from previous turns, injected into next prompt.
    denied_tools: Vec<DeniedToolCall>,
    /// Schema validator for structured output.
    structured_output: Option<StructuredOutputValidator>,
    /// Prompt compiler for system prompt assembly.
    prompt_compiler: PromptCompiler,
    /// Compact configuration.
    compact_config: CompactConfig,
    /// Explicit context-window override chain (settings/experiment included).
    ///
    /// When absent, compaction uses the default resolver (environment, client
    /// data, built-in table). The CLI installs one built from resolved
    /// settings and experiment flags.
    context_window_resolver: Option<ContextWindowResolver>,
    /// The real context window, learned from a provider's overflow error.
    ///
    /// A provider enforces the window itself, and its rejection is the only
    /// authoritative statement of what that window is. When one arrives the
    /// engine records the limit here; from then on thresholds use it instead
    /// of the table or the unknown-model guess. It outlives the turn: the
    /// session already paid for the failed request, and the next turn should
    /// not repeat the mistake.
    learned_context_window: Option<usize>,
    /// Whether a learned window is also written to the client-data file.
    ///
    /// Off by default, on in the product: the engine construction path is also
    /// what unit tests and embedders use, and an engine that writes to the
    /// config home as a side effect of a constructor is a trap — a test suite
    /// would then change the window the next real session starts with. The CLI
    /// turns it on for the sessions it runs, which is where remembering the
    /// window for the next session is the point.
    persist_learned_context_window: bool,
    /// Hook executor for firing lifecycle hooks.
    ///
    /// Shared rather than owned so a session can hand a clone to a background
    /// observer — the headless `FileChanged` watcher fires through the same
    /// registry while the turn runs.
    hooks: Arc<dyn HookExecutor + Send + Sync>,
    /// Absolute paths a `SessionStart` hook returned in `watchPaths`.
    ///
    /// Merged with the executor's static configuration by
    /// [`watch_paths`](Self::watch_paths).
    session_watch_paths: Vec<String>,
    /// Tool executor for dispatching tool calls.
    tools: Box<dyn ToolExecutor + Send + Sync>,
    /// Handler for elicitation requests.
    elicitation_handler: Box<dyn ElicitationHandler + Send + Sync>,
    /// Optional transcript base path. When set, compact updates the
    /// transcript file on disk by removing old messages and writing a
    /// compact boundary marker.
    transcript_path: Option<PathBuf>,
    /// Usage summary from the most recent model request.
    last_usage: Option<UsageSummary>,
    /// Where the last measured request stood, for token estimation.
    ///
    /// The provider measures the exact request it receives; the next request's
    /// size is that measurement plus an estimate of what was appended since.
    /// Held on the engine because a turn ends between the two, and because a
    /// new turn's history begins with the messages the measurement covered.
    usage_anchor: Option<UsageAnchor>,
    /// Loop detector for detecting repetitive tool call patterns.
    ///
    /// Behind a lock so compaction can clear it from the `&self` methods that
    /// run the compaction path; the detector itself is only touched once per
    /// turn and once per compaction.
    loop_detector: std::sync::Mutex<Option<LoopDetector>>,
    /// Wall-clock budget the model is reminded of, when the caller set one.
    ///
    /// The engine is not what enforces a deadline — the caller's `timeout`
    /// is — but a model that never hears the clock cannot budget. The
    /// reminder is injected between turns, at fixed fractions of the budget.
    time_budget: Option<TimeBudgetClock>,
    /// Last successful response content and usage (for graceful degradation).
    last_successful_response: Option<(String, UsageSummary)>,
    /// Optional team context for multi-agent collaboration.
    ///
    /// Installed by the CLI when `team_collaboration` is on. Each turn reads
    /// its shared notes, in-progress work and recent activity into the request
    /// context. Absent (the default) → no team context is injected.
    team_context: Option<Arc<std::sync::Mutex<crate::team_context::TeamContext>>>,
    /// Execute tool calls while the streaming response is still arriving.
    ///
    /// Off (the default, matching the reference) → calls are assembled during
    /// the stream and executed once it completes. On → `execute_early_tool_call`
    /// runs each completed call while the stream is open.
    streaming_tool_execution: bool,
    /// Ask the lightweight model for a one-line summary of each tool use.
    ///
    /// Off (the default) → `ToolCallStart.summary` is the fixed
    /// `ToolCall::activity_summary()` label. On → the engine emits
    /// [`EngineEvent::ToolUseSummary`] and uses that summary as the label,
    /// falling back to `activity_summary()` when summarization fails.
    tool_use_summaries: bool,
    /// Optional coordinator state for multi-phase workflows.
    coordinator: Option<crate::coordinator::CoordinatorState>,
    /// Optional persistent session memory that survives compaction.
    /// Implemented by `codape-memory`; absent → no memory work happens.
    session_memory: Option<Arc<dyn crate::session_memory::SessionMemoryBackend>>,
    /// Optional compaction backend (the local/remote dual-track seam).
    /// Absent → the built-in local LLM summary path is used.
    compaction_backend: Option<Arc<dyn crate::compaction_backend::CompactionBackend>>,
    /// When the last provider request started, for the prompt-cache TTL
    /// trigger: every request refreshes the cache, so the clock starts there.
    ///
    /// Shared with the cache-TTL timer, which needs the same instant to tell
    /// an idle session from a busy one.
    last_request_at: Arc<Mutex<Option<Instant>>>,
    /// Whether the `SessionStart` hook has run. The engine has no explicit
    /// "the session begins" call, so the session's first turn fires it, once.
    session_started: bool,
    /// The background prompt-cache TTL timer, once a session has sent a turn
    /// and a TTL is configured. Dropping the engine stops it.
    cache_ttl_timer: Option<CacheTtlTimer>,
    /// A summary computed ahead of the compaction that will spend it.
    ///
    /// Filled by the background pre-generation once the context crosses the
    /// warning line, consumed by the next LLM-summary compaction whose interval
    /// it still describes. Shared because the task that produces it outlives
    /// the call that started it.
    pending_summary: Arc<Mutex<Option<compact::SummaryDigest>>>,
    /// Whether a pre-generation task is already running.
    ///
    /// The warning check runs before every request, several times per turn; a
    /// second task would produce the same summary for the same interval.
    summary_prefetch_in_flight: Arc<AtomicBool>,
    /// Why the background summary could not be produced, for the next
    /// compaction to stop asking for one.
    ///
    /// A provider that refuses a summarization request will refuse it again a
    /// turn later; retrying every turn would spend a request per turn on a
    /// call known to fail.
    summary_prefetch_failure: Arc<Mutex<Option<String>>>,
    /// Maximum output tokens requested from the provider per request.
    ///
    /// Defaults to [`Self::DEFAULT_MAX_TOKENS`]; a session selected with
    /// `--agent` may lower or raise it from the agent definition.
    /// Optional observer for turn state transitions, used by integration tests
    /// and embedders that render the lifecycle.
    transition_observer: Option<TransitionObserver>,
    max_tokens: u32,
}

/// A denied tool call that should be summarized in the next turn.
#[derive(Debug, Clone)]
pub struct DeniedToolCall {
    /// Tool name.
    pub name: String,
    /// Reason for denial.
    pub reason: String,
}

/// Error markers that identify the requested model itself as unavailable.
///
/// Deliberately narrow: a transport or authentication failure would fail the
/// same way on another model, so it must not trigger a fallback.
const MODEL_UNAVAILABLE_MARKERS: [&str; 9] = [
    "model_not_found",
    "not_found_error",
    "model not found",
    "unknown model",
    "invalid model",
    "model_not_available",
    "model is not available",
    "unsupported model",
    "overloaded_error",
];

/// Whether an error message says the requested model is unavailable.
#[must_use]
pub fn is_model_unavailable_message(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    MODEL_UNAVAILABLE_MARKERS
        .iter()
        .any(|marker| message.contains(marker))
}

/// Choose the fallback model for a retry, if `error_message` indicates the
/// primary model itself is unavailable.
///
/// The caller retries at most once: after switching, the primary and fallback
/// are the same model, so this returns `None` for the next failure.
#[must_use]
pub fn fallback_after_model_error(
    primary: &ModelId,
    fallback: Option<&ModelId>,
    error_message: &str,
) -> Option<ModelId> {
    let fallback = fallback?;
    if fallback.0 == primary.0 {
        return None;
    }
    is_model_unavailable_message(error_message).then(|| fallback.clone())
}

/// Normalize a provider content value into a block array.
///
/// A plain string is a single text block; an array is already a block list.
fn content_blocks(value: serde_json::Value) -> Vec<serde_json::Value> {
    match value {
        serde_json::Value::Array(blocks) => blocks,
        serde_json::Value::String(text) => {
            if text.is_empty() {
                Vec::new()
            } else {
                vec![serde_json::json!({"type": "text", "text": text})]
            }
        }
        other => vec![other],
    }
}

/// Prepend a thinking block to an assistant message's content.
fn prepend_thinking_block(block: &ThinkingBlock, content: serde_json::Value) -> serde_json::Value {
    let mut blocks = vec![serde_json::json!({
        "type": "thinking",
        "thinking": &block.thinking,
        "signature": &block.signature,
    })];
    blocks.extend(content_blocks(content));
    serde_json::Value::Array(blocks)
}

/// Append assistant content, merging consecutive assistant messages.
///
/// The engine stores one `Message` per content kind (text, then one per tool
/// call), but a provider turn is a single assistant message whose content is
/// `[thinking, text, tool_use, ...]`.
fn push_assistant_content(out: &mut Vec<(String, serde_json::Value)>, content: serde_json::Value) {
    if let Some((role, existing)) = out.last_mut()
        && role == "assistant"
    {
        let mut blocks = content_blocks(std::mem::take(existing));
        blocks.extend(content_blocks(content));
        *existing = serde_json::Value::Array(blocks);
        return;
    }
    out.push(("assistant".to_string(), content));
}

impl QueryEngine {
    /// Append one history message to both the session and this turn's request.
    ///
    /// Writing both lists is what keeps the next request and `close_turn`
    /// agreeing. The write to `turn.messages` lands at the end of the history
    /// range rather than at the end of the list: the per-turn injections that
    /// follow the history (denied-tool summaries, a caller's extra system
    /// prompt, the JSON-schema instruction) are rebuilt every turn and must
    /// not be absorbed into the range compaction may replace.
    fn push_history_message(&mut self, turn: &mut TurnRuntime, msg: Message) {
        self.session.messages.push(msg.clone());
        if let Some((start, end)) = turn.history_range
            && start <= end
            && end <= turn.messages.len()
        {
            turn.messages.insert(end, msg);
            turn.history_range = Some((start, end + 1));
        } else {
            turn.messages.push(msg);
        }
    }

    fn persist_assistant_tool_calls(&mut self, turn: &mut TurnRuntime, tool_calls: &[ToolCall]) {
        for tool_call in tool_calls {
            let msg = Message {
                role: crate::types::Role::Assistant,
                content: crate::types::MessageContent::ToolCall(tool_call.clone()),
            };
            self.push_history_message(turn, msg);
        }
    }

    /// Replace the most recent user text message in `messages`.
    fn replace_last_user_text(messages: &mut [Message], updated: &str) -> bool {
        for message in messages.iter_mut().rev() {
            if message.role == crate::types::Role::User
                && let crate::types::MessageContent::Text(text) = &mut message.content
            {
                *text = updated.to_string();
                return true;
            }
        }
        false
    }

    fn provider_messages(messages: &[Message]) -> Vec<serde_json::Value> {
        // Thinking is not a message of its own: it is the first block of the
        // assistant turn that follows it. Hold it until that turn arrives.
        let mut pending_thinking: Option<ThinkingBlock> = None;
        let mut out: Vec<(String, serde_json::Value)> = Vec::new();
        // The engine stores one tool result per message, but a provider turn
        // has one user message holding every `tool_result` for the preceding
        // assistant turn. Without this, two calls in one assistant message
        // leave the second `tool_use` unanswered in the next message and the
        // provider rejects the request.
        let mut last_user_is_tool_results = false;

        for m in messages
            .iter()
            .filter(|m| !matches!(m.role, crate::types::Role::System))
        {
            if let crate::types::MessageContent::Thinking(block) = &m.content {
                pending_thinking =
                    matches!(m.role, crate::types::Role::Assistant).then(|| block.clone());
                continue;
            }

            let Some(role_str) = (match (&m.role, &m.content) {
                (crate::types::Role::User, _)
                | (crate::types::Role::Tool, crate::types::MessageContent::ToolResult(_)) => {
                    Some("user")
                }
                (crate::types::Role::Assistant, _) => Some("assistant"),
                _ => None,
            }) else {
                pending_thinking = None;
                continue;
            };

            let content = match &m.content {
                crate::types::MessageContent::Text(t) => serde_json::Value::String(t.clone()),
                crate::types::MessageContent::ToolCall(tc) => serde_json::json!([
                    {"type": "tool_use", "id": &tc.id, "name": &tc.name, "input": &tc.input}
                ]),
                crate::types::MessageContent::ToolResult(tr) if tr.images.is_empty() => {
                    serde_json::json!([
                        {"type": "tool_result", "tool_use_id": &tr.tool_call_id, "content": &tr.output}
                    ])
                }
                crate::types::MessageContent::ToolResult(tr) => {
                    let mut blocks: Vec<serde_json::Value> = Vec::new();
                    if !tr.output.is_empty() {
                        blocks.push(serde_json::json!({"type": "text", "text": &tr.output}));
                    }
                    for image in &tr.images {
                        blocks.push(serde_json::json!({
                            "type": "image",
                            "source": {
                                "type": "base64",
                                "media_type": &image.media_type,
                                "data": &image.data,
                            }
                        }));
                    }
                    serde_json::json!([
                        {"type": "tool_result", "tool_use_id": &tr.tool_call_id, "content": blocks}
                    ])
                }
                // Handled above; kept so the match stays exhaustive.
                crate::types::MessageContent::Thinking(_) => unreachable!(),
            };

            if role_str == "assistant" {
                last_user_is_tool_results = false;
                if let Some(block) = pending_thinking.take() {
                    let content = prepend_thinking_block(&block, content);
                    push_assistant_content(&mut out, content);
                } else {
                    push_assistant_content(&mut out, content);
                }
            } else if matches!(m.role, crate::types::Role::Tool) {
                if last_user_is_tool_results
                    && let Some((role, existing)) = out.last_mut()
                    && role == "user"
                {
                    let mut blocks = content_blocks(std::mem::take(existing));
                    blocks.extend(content_blocks(content));
                    *existing = serde_json::Value::Array(blocks);
                    continue;
                }
                pending_thinking = None;
                out.push((role_str.to_string(), content));
                last_user_is_tool_results = true;
            } else {
                // Thinking can only belong to an assistant turn; a user or
                // tool message means there is no turn to attach it to.
                pending_thinking = None;
                last_user_is_tool_results = false;
                out.push((role_str.to_string(), content));
            }
        }

        out.into_iter()
            .map(|(role, content)| serde_json::json!({"role": role, "content": content}))
            .collect()
    }

    fn build_request_context(&self, turn_number: u32, options: &SubmitOptions) -> RequestContext {
        RequestContext {
            session_id: Some(self.session.session_id.to_string()),
            device_id: std::env::var("CODAPE_DEVICE_ID").ok(),
            turn_id: Some(turn_number.to_string()),
            agent_id: options.agent_id.clone(),
            request_id: None,
            provider_hints: options.provider_hints.clone().into_iter().collect(),
        }
    }

    /// Create a new QueryEngine.
    #[must_use]
    pub fn new(
        api_client: Box<dyn ApiClient>,
        model: ModelId,
        session: SessionState,
        permission_engine: PermissionEngine,
        event_tx: mpsc::UnboundedSender<EngineEvent>,
    ) -> Self {
        let config = CompactConfig::default();
        Self {
            api_client,
            model,
            fallback_model: None,
            session,
            cost_tracker: CostTracker::new(),
            interrupt: InterruptHandle::default(),
            permission_engine,
            event_tx,
            current_turn: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            denied_tools: Vec::new(),
            structured_output: None,
            prompt_compiler: PromptCompiler::new(config),
            compact_config: config,
            context_window_resolver: None,
            learned_context_window: None,
            persist_learned_context_window: false,
            hooks: Arc::new(NoOpHookExecutor),
            session_watch_paths: Vec::new(),
            tools: Box::new(NoOpToolExecutor),
            elicitation_handler: Box::new(ReplElicitationHandler::default()),
            transcript_path: None,
            last_usage: None,
            usage_anchor: None,
            loop_detector: std::sync::Mutex::new(Some(LoopDetector::new())),
            time_budget: None,
            last_successful_response: None,
            team_context: None,
            streaming_tool_execution: false,
            tool_use_summaries: false,
            coordinator: None,
            session_memory: None,
            compaction_backend: None,
            last_request_at: Arc::new(Mutex::new(None)),
            session_started: false,
            cache_ttl_timer: None,
            pending_summary: Arc::new(Mutex::new(None)),
            summary_prefetch_in_flight: Arc::new(AtomicBool::new(false)),
            summary_prefetch_failure: Arc::new(Mutex::new(None)),
            transition_observer: None,
            max_tokens: Self::DEFAULT_MAX_TOKENS,
        }
    }

    /// The default maximum output tokens per provider request.
    ///
    /// `max_tokens` is a ceiling the provider enforces and the model cannot
    /// see: it is not a target to aim at, it is how much room a single answer
    /// is allowed before it is cut off mid-thought. A turn that only needs a
    /// paragraph still costs a paragraph.
    ///
    /// It used to be 4096. A reasoning model spends that budget on thinking
    /// before it emits anything, so a hard step in a long run could return
    /// `max_tokens` with no text and no tool call at all — a turn that is
    /// discarded, and, when it was the last one, a run reported as `success`.
    /// 32K leaves room for reasoning plus a long tool call while staying well
    /// inside the output ceiling of every model Codape targets, and a 120 s
    /// request timeout still bounds a stream that genuinely runs long.
    pub const DEFAULT_MAX_TOKENS: u32 = 32_768;

    /// Set the elicitation handler.
    /// Replace the client requests are sent through.
    ///
    /// Used when a session picks a provider after construction, and by tests
    /// that need a model which behaves a particular way mid-turn.
    pub fn set_api_client(&mut self, client: Box<dyn ApiClient>) {
        self.api_client = client;
    }

    /// Replace the model used for provider requests.
    ///
    /// Used when a named custom agent selected with `--agent` supplies its own
    /// model, and by tests that need to observe the model a request carried.
    pub fn set_model(&mut self, model: ModelId) {
        self.model = model;
    }

    /// The model used for provider requests.
    #[must_use]
    pub const fn model(&self) -> &ModelId {
        &self.model
    }

    /// Configure the one-retry fallback model (`--fallback-model`).
    ///
    /// The engine retries the same request with this model when the primary
    /// model is unavailable, and then continues the turn and session on it.
    pub fn set_fallback_model(&mut self, fallback: Option<ModelId>) {
        self.fallback_model = fallback;
    }

    /// The configured fallback model, if any.
    #[must_use]
    pub const fn fallback_model(&self) -> Option<&ModelId> {
        self.fallback_model.as_ref()
    }

    /// The model to retry with for this failure, if the primary is unavailable.
    fn fallback_for_api_error(&self, error: &ApiError) -> Option<ModelId> {
        fallback_after_model_error(
            &self.model,
            self.fallback_model.as_ref(),
            &error.to_string(),
        )
    }

    /// Switch the engine to the fallback model for this and later requests.
    fn switch_to_fallback_model(&mut self, fallback: ModelId) {
        tracing::warn!(
            primary = %self.model.0,
            fallback = %fallback.0,
            "primary model unavailable; retrying once with the fallback model"
        );
        self.model = fallback;
    }

    /// Classify a provider failure the way the turn loop expects.
    ///
    /// A retry-exhausted provider failure is the one case that also raises an
    /// advisory [`EngineEvent::Notification`], so the `Notification` hook fires
    /// with the same message the UI is shown.
    async fn turn_error_from_api_error(&self, error: ApiError) -> TurnError {
        match error {
            ApiError::RetryExhausted {
                attempts,
                last_error_message: _,
            } => {
                let message = format!(
                    "The AI service is temporarily unavailable after {attempts} attempts. Please try again later."
                );
                let _ = self.event_tx.send(EngineEvent::Notification {
                    message: message.clone(),
                });
                let mut ctx = std::collections::HashMap::new();
                ctx.insert("notification".to_string(), message.clone());
                let result = self
                    .fire_hook(hook_events::NOTIFICATION, &message, &ctx)
                    .await;
                if !result.errors.is_empty() {
                    tracing::warn!("Notification hook errors: {:?}", result.errors);
                }
                TurnError::ApiUnavailable { message }
            }
            // The provider said the input does not fit. That is recoverable —
            // compact and retry — and the message carries the real window.
            error
                if matches!(
                    crate::api_client::classify_error(&error),
                    crate::api_client::ErrorClass::ContextOverflow
                ) =>
            {
                let message = error.to_string();
                let overflow = crate::context_overflow::detect(&message);
                TurnError::ContextOverflow {
                    message,
                    actual: overflow.and_then(|o| o.actual),
                    limit: overflow.map_or(0, |o| o.limit),
                }
            }
            // A transport failure that ran out of retries is the same
            // question — try later — and is classified by kind rather than
            // by reading the message.
            error if crate::api_client::is_retryable(&error) => TurnError::ApiUnavailable {
                message: error.to_string(),
            },
            error => TurnError::Fatal {
                message: error.to_string(),
            },
        }
    }

    /// Set the maximum output tokens requested from the provider.
    pub const fn set_max_tokens(&mut self, max_tokens: u32) {
        self.max_tokens = max_tokens;
    }

    /// The maximum output tokens requested from the provider.
    #[must_use]
    pub const fn max_tokens(&self) -> u32 {
        self.max_tokens
    }

    /// Replace the elicitation handler — how the engine asks the person a
    /// question mid-turn.
    pub fn set_elicitation_handler(&mut self, handler: Box<dyn ElicitationHandler + Send + Sync>) {
        self.elicitation_handler = handler;
    }

    /// Observe every turn state transition.
    ///
    /// The callback runs on the turn's thread after each legal or illegal
    /// `transition_to` and after `terminate`. It is a diagnostic surface: the
    /// engine does not wait for it and must not be mutated from it.
    pub fn set_transition_observer(&mut self, observer: TransitionObserver) {
        self.transition_observer = Some(observer);
    }

    /// Set the hook executor for lifecycle hooks.
    ///
    /// The executor is shared (`Arc`) so an observer outside the turn — the
    /// headless `FileChanged` watcher — can fire through the same registry.
    pub fn set_hooks(&mut self, hooks: Arc<dyn HookExecutor + Send + Sync>) {
        self.hooks = hooks;
    }

    /// A clone of the installed hook executor.
    ///
    /// Used by the session to fire `FileChanged` from a watcher task while the
    /// turn owns the engine exclusively.
    #[must_use]
    pub fn hook_executor(&self) -> Arc<dyn HookExecutor + Send + Sync> {
        Arc::clone(&self.hooks)
    }

    /// Absolute paths a `FileChanged` hook should observe.
    ///
    /// Merges the static `watch` configuration on the installed hooks with any
    /// `watchPaths` a `SessionStart` hook returned. Relative paths never reach
    /// this list: both sources reject them at parse time.
    #[must_use]
    pub fn watch_paths(&self) -> Vec<String> {
        let mut paths = self.hooks.watch_paths();
        paths.extend(self.session_watch_paths.iter().cloned());
        // Preserve first-seen order while dropping duplicates.
        let mut seen = std::collections::HashSet::new();
        paths.retain(|path| seen.insert(path.clone()));
        paths
    }

    /// Set the tool executor for tool call dispatch.
    pub fn set_tool_executor(&mut self, tools: Box<dyn ToolExecutor + Send + Sync>) {
        self.tools = tools;
        // File tools open a handle and hand its real path back here before
        // using it. If that path is not the one the pre-dispatch check saw, it
        // is checked again; the pre-dispatch path alone is a TOCTOU snapshot.
        let checker = self.permission_engine.checker();
        let verifier: crate::tool_executor::PathVerifier = Arc::new(move |tool, paths| {
            let subjects: Vec<crate::permissions::RuleSubject> = paths
                .iter()
                .map(|path| {
                    crate::permissions::RuleSubject::single(path.to_string_lossy().to_string())
                })
                .collect();
            let checker = checker.lock().unwrap();
            match checker.check_with_subjects(tool, &subjects, None) {
                crate::permissions::PermissionResult::Allow
                | crate::permissions::PermissionResult::AllowForPath { .. } => Ok(()),
                other => Err(format!(
                    "the opened path differs from the permission-checked path and is not                      allowed: {other:?}"
                )),
            }
        });
        self.tools.set_path_verifier(Some(verifier));
    }

    /// Set the loop detector for detecting repetitive tool call patterns.
    ///
    /// Pass `None` to disable loop detection.
    pub fn set_loop_detector(&mut self, detector: Option<LoopDetector>) {
        *self
            .loop_detector
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = detector;
    }

    /// Start a wall-clock budget the model is told about between turns.
    ///
    /// The engine does not enforce the deadline — the caller's `timeout` does.
    /// This only makes the clock visible to the model, because a run that
    /// cannot see how much time is left cannot decide when to stop exploring
    /// and write the deliverable.
    pub const fn set_time_budget(&mut self, started: std::time::Instant, budget: Duration) {
        self.time_budget = Some(TimeBudgetClock {
            started,
            budget,
            announced: [false; 4],
        });
    }

    /// The next budget milestone to tell the model about, if one is due.
    ///
    /// Milestones fire once each and are reported on the turn boundary after
    /// they pass, so a long tool call does not delay or repeat them.
    fn take_time_budget_reminder(&mut self) -> Option<String> {
        let clock = self.time_budget.as_mut()?;
        let percent = clock.next_milestone()?;
        let budget = clock.budget.as_secs();
        let used = clock.started.elapsed().as_secs().min(budget);
        let left = budget.saturating_sub(used);
        let urgency = if percent >= 90 {
            "The budget is nearly spent: stop exploring and make sure the deliverable is on disk"
        } else if percent >= 50 {
            "If the deliverable is not on disk yet, write the best version you have before continuing"
        } else {
            "Keep the remaining work inside the budget"
        };
        Some(format!(
            "Time check ({percent} % of the budget used): {used} s of {budget} s have passed, \
             about {left} s remain. {urgency}."
        ))
    }

    /// Set the transcript base path for persisting compact boundaries.
    ///
    /// When set, compact recovery will update the transcript file on disk
    /// by removing old messages and writing a compact boundary marker.
    pub fn set_transcript_path(&mut self, path: PathBuf) {
        self.transcript_path = Some(path);
    }

    /// Stop persisting compaction results.
    pub fn clear_transcript_path(&mut self) {
        self.transcript_path = None;
    }

    /// The transcript directory compaction uses, if one is configured.
    #[must_use]
    pub fn transcript_path(&self) -> Option<&std::path::Path> {
        self.transcript_path.as_deref()
    }

    /// Attach a persistent session-memory backend.
    ///
    /// When set (and `CompactConfig::session_memory_enabled`), compaction
    /// refreshes the memory document and injects it alongside the summary.
    pub fn set_session_memory(
        &mut self,
        backend: Arc<dyn crate::session_memory::SessionMemoryBackend>,
    ) {
        self.session_memory = Some(backend);
    }

    /// Attach a compaction backend (the local/remote dual-track seam).
    ///
    /// When set, compaction consults this backend for the summary step
    /// before falling back to the built-in local LLM path; on backend error
    /// or an empty summary the built-in path (then rules) still runs. No
    /// backend attached (the default) → behavior unchanged.
    pub fn set_compaction_backend(
        &mut self,
        backend: Arc<dyn crate::compaction_backend::CompactionBackend>,
    ) {
        self.compaction_backend = Some(backend);
    }

    /// Replace the compaction configuration (thresholds, reserves, retries).
    pub const fn set_compact_config(&mut self, config: CompactConfig) {
        self.compact_config = config;
    }

    /// Install a context-window override chain that includes settings and
    /// experiment values.
    ///
    /// The CLI builds this from the resolved `Settings` and `FeatureFlags`;
    /// without it the engine still honors the environment, client-data file,
    /// and built-in table.
    pub fn set_context_window_resolver(&mut self, resolver: ContextWindowResolver) {
        self.context_window_resolver = Some(resolver);
    }

    /// Whether a window learned from a provider is also written to the
    /// client-data file, for the next session to find.
    ///
    /// Disabled by default; the CLI enables it. While disabled the window is
    /// still learned for the running session.
    pub const fn set_persist_learned_context_window(&mut self, persist: bool) {
        self.persist_learned_context_window = persist;
    }

    /// Compaction thresholds for this engine's model and override chain.
    ///
    /// A window learned from a provider's overflow error beats every other
    /// source: it is the one value the provider itself stated. Anything else
    /// is resolved through the override chain (env → settings → client data →
    /// experiment → built-in table → unknown guess), and the source travels
    /// with the value so the blocked line is only enforced on a real one.
    fn compact_thresholds(&self) -> compact::CompactThresholds {
        if let Some(window) = self.learned_context_window {
            return compact::CompactThresholds::resolve_detailed(
                &self.compact_config,
                crate::types::ResolvedContextWindow::new(
                    window,
                    crate::types::ContextWindowSource::Experiment,
                ),
            );
        }
        self.context_window_resolver.as_ref().map_or_else(
            || compact::CompactThresholds::resolve(&self.model, &self.compact_config),
            |resolver| {
                compact::CompactThresholds::resolve_with(
                    &self.model,
                    &self.compact_config,
                    resolver,
                )
            },
        )
    }

    /// The context window this engine is currently working against.
    #[must_use]
    pub fn context_window(&self) -> crate::types::ResolvedContextWindow {
        if let Some(window) = self.learned_context_window {
            return crate::types::ResolvedContextWindow::new(
                window,
                crate::types::ContextWindowSource::Experiment,
            );
        }
        self.context_window_resolver.as_ref().map_or_else(
            || {
                crate::types::ContextWindowResolver::from_env_and_client_data()
                    .resolve_detailed(&self.model.0)
            },
            |resolver| resolver.resolve_detailed(&self.model.0),
        )
    }

    /// Record a window the provider stated, so the rest of the session uses it.
    ///
    /// Also writes it to the client-data file (see
    /// `persist_learned_context_window`), so the next session does not have to
    /// be rejected once to learn the same thing.
    pub fn learn_context_window(&mut self, window: usize) {
        if window > 0 && self.learned_context_window != Some(window) {
            tracing::info!(
                model = %self.model.0,
                previous = ?self.context_window(),
                learned = window,
                "provider reported the real context window"
            );
            self.learned_context_window = Some(window);
            self.persist_window_override(window);
        }
    }

    /// Remember a learned window for the next session.
    ///
    /// A provider that rejects a request has already cost the session that
    /// request; making the next one pay for the same lesson is the whole
    /// complaint this answers. The write goes to the client-data file, which
    /// sits below settings and the environment — an explicit window in either
    /// still wins, and the user's settings file is never edited.
    fn persist_window_override(&self, window: usize) {
        if !self.persist_learned_context_window {
            return;
        }
        if let Err(error) = crate::types::ContextWindowOverrides::remember_provider_window(
            &self.model.0,
            window,
            &crate::types::context_window_client_data_path(),
        ) {
            tracing::warn!(
                model = %self.model.0,
                %error,
                "could not remember the learned context window; this session still uses it"
            );
        }
    }

    /// Raise the window estimate to a lower bound an accepted request proved.
    ///
    /// A provider accepts a request only when its input plus the requested
    /// output fits the window, so an accepted request proves
    /// `window >= prompt_tokens + max_tokens`. That is weaker than the exact
    /// value an overflow error states, but it is the only evidence available
    /// before one, and it moves the guess in the safe direction: a window
    /// that is too small costs context, one that is too large loses the
    /// conversation.
    ///
    /// The estimate only ever rises here. A configured window that is larger
    /// than the bound stays in charge; learning only overtakes it when
    /// accepted requests prove the configured value was too small.
    fn raise_context_window_lower_bound(&mut self, lower_bound: usize) {
        let current = self.context_window().tokens;
        if lower_bound <= current {
            return;
        }
        tracing::info!(
            model = %self.model.0,
            previous = current,
            learned = lower_bound,
            "an accepted request raised the context-window lower bound"
        );
        self.learned_context_window = Some(lower_bound);
        self.persist_window_override(lower_bound);
    }

    /// Refresh the session-memory document at compaction time and return the
    /// summary text with the memory injected.
    ///
    /// No backend attached (or disabled in config) → returns the summary
    /// unchanged with zero extra LLM calls, which keeps existing mock tests
    /// structurally unaffected. On any LLM or backend failure the summary is
    /// returned unchanged — compaction always succeeds, and stale memory is
    /// never injected as if current.
    async fn refresh_session_memory(&self, summary_text: &str, summary_input: &[String]) -> String {
        let Some(backend) = self
            .session_memory
            .as_ref()
            .filter(|_| self.compact_config.session_memory_enabled)
        else {
            return summary_text.to_string();
        };

        let excerpt = summary_input.join("\n\n");
        let session_id = self.session.session_id.to_string();
        let current = backend.load_memory(&session_id).await.unwrap_or_default();

        let update = if current.trim().is_empty() {
            crate::prompts::create_session_memory(self.api_client.as_ref(), &excerpt).await
        } else {
            crate::prompts::update_session_memory(self.api_client.as_ref(), &current, &excerpt)
                .await
        };

        match update {
            Ok(updated) if !updated.trim().is_empty() => {
                backend.save_memory(&session_id, &updated).await;
                format!("{summary_text}\n\n<session_memory>\n{updated}\n</session_memory>")
            }
            Ok(_) => {
                tracing::warn!("SessionMemory: update produced empty content — skipping injection");
                summary_text.to_string()
            }
            Err(e) => {
                tracing::warn!("SessionMemory: update failed ({e}) — skipping injection");
                summary_text.to_string()
            }
        }
    }

    /// Micro-compact parameters derived from `CompactConfig`.
    const fn micro_params(&self) -> compact::MicroCompactParams {
        compact::MicroCompactParams {
            keep_recent: 5,
            min_result_tokens: self.compact_config.micro_min_result_tokens,
            head_tokens: self.compact_config.micro_head_tokens,
            tail_tokens: self.compact_config.micro_tail_tokens,
        }
    }

    /// Set the team context for multi-agent collaboration.
    ///
    /// Installed by the CLI when `team_collaboration` is on. The engine reads
    /// the context in `Self::build_context` and injects its shared notes,
    /// in-progress work and recent activity into the request each turn.
    pub fn set_team_context(
        &mut self,
        context: Arc<std::sync::Mutex<crate::team_context::TeamContext>>,
    ) {
        self.team_context = Some(context);
    }

    /// The installed team context, if any.
    #[must_use]
    pub const fn team_context(
        &self,
    ) -> Option<&Arc<std::sync::Mutex<crate::team_context::TeamContext>>> {
        self.team_context.as_ref()
    }

    /// Execute tool calls while the response stream is still arriving.
    ///
    /// Set by the CLI from `streaming_tool_execution`. Off (the default,
    /// matching the reference) assembles calls during the stream and executes
    /// them after it completes.
    pub const fn set_streaming_tool_execution(&mut self, enabled: bool) {
        self.streaming_tool_execution = enabled;
    }

    /// Whether tool calls execute as they arrive in a streamed response.
    #[must_use]
    pub const fn streaming_tool_execution(&self) -> bool {
        self.streaming_tool_execution
    }

    /// Ask the lightweight model for a one-line summary of each tool use.
    ///
    /// Set by the CLI from `tool_use_summaries`. Off leaves the fixed
    /// `ToolCall::activity_summary()` label byte-for-byte unchanged.
    pub const fn set_tool_use_summaries(&mut self, enabled: bool) {
        self.tool_use_summaries = enabled;
    }

    /// Whether the lightweight model summarizes each tool use.
    #[must_use]
    pub const fn tool_use_summaries(&self) -> bool {
        self.tool_use_summaries
    }

    /// Get a formatted cost summary from the cost tracker.
    #[must_use]
    pub fn cost_summary(&self) -> String {
        self.cost_tracker.format_summary()
    }

    /// Get a reference to the current session state.
    #[must_use]
    pub const fn session(&self) -> &SessionState {
        &self.session
    }

    /// Replace the current session state.
    pub fn set_session(&mut self, session: SessionState) {
        self.session = session;
    }

    /// Get the permission checker (shared, for mode changes).
    #[must_use]
    pub fn permission_checker(&self) -> Arc<std::sync::Mutex<PermissionChecker>> {
        self.permission_engine.checker()
    }

    /// The permission engine, to install the per-call pieces a session owns.
    ///
    /// A rule exemption is a property of a *call* — the same Artifact tool
    /// refuses a whole-tool allow rule for a publish and honours it for a read
    /// — and the engine is where the call's arguments are, so the resolver has
    /// to be installed here rather than copied into the checker's table.
    pub const fn permission_engine_mut(&mut self) -> &mut PermissionEngine {
        &mut self.permission_engine
    }

    /// Configure the confirmation bus used for interactive permission prompts.
    pub fn set_confirmation_bus(&mut self, bus: ConfirmationBus) {
        self.permission_engine.set_confirmation_bus(bus);
    }

    /// Disable interactive confirmation bus routing.
    pub fn clear_confirmation_bus(&mut self) {
        self.permission_engine.clear_confirmation_bus();
    }

    /// Set the coordinator state for multi-phase workflows.
    pub fn set_coordinator(&mut self, coordinator: crate::coordinator::CoordinatorState) {
        self.coordinator = Some(coordinator);
    }

    /// The installed coordinator state, if any.
    ///
    /// Installed by the CLI when `coordinator_mode` is on, which is what makes
    /// the engine's per-turn phase advance run.
    #[must_use]
    pub const fn coordinator(&self) -> Option<&crate::coordinator::CoordinatorState> {
        self.coordinator.as_ref()
    }

    /// Prefer the snip strategy as soon as compaction is warranted.
    ///
    /// Set by the CLI when `history_snip` is on. Off leaves strategy selection
    /// byte-for-byte identical to the pre-flag behaviour.
    pub const fn set_history_snip(&mut self, enabled: bool) {
        self.compact_config.prefer_snip = enabled;
    }

    /// Whether compaction prefers the snip strategy as soon as it is warranted.
    #[must_use]
    pub const fn history_snip(&self) -> bool {
        self.compact_config.prefer_snip
    }

    /// Set the structured output validator from a JSON schema.
    pub fn set_structured_output(&mut self, schema: serde_json::Value) {
        self.structured_output = Some(StructuredOutputValidator::new(schema));
    }

    /// Recover orphaned permission requests from disk and emit them as
    /// `EngineEvent::PermissionRequest` so the UI can surface them.
    pub fn recover_orphaned_permissions(&mut self) {
        let Ok(requests) = self.permission_engine.recover_orphaned() else {
            return;
        };
        for req in requests {
            let _ = self.event_tx.send(EngineEvent::PermissionRequest {
                id: req.id,
                tool: req.tool_name,
                args: req.arguments.parse().unwrap_or(serde_json::Value::Null),
            });
        }
    }

    /// Forget the turn history the loop detector has seen.
    ///
    /// Called after compaction, which replaces the messages the model sees
    /// with a summary: a repeat judged against turns that are no longer in the
    /// context is not a loop. The detector then judges the context the model
    /// actually has.
    fn reset_loop_history(&self) {
        let mut guard = self
            .loop_detector
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(detector) = guard.as_mut() {
            detector.clear();
        }
    }

    /// How many turns the loop detector is holding, for tests.
    #[cfg(test)]
    fn loop_history_len(&self) -> Option<usize> {
        self.loop_detector
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(LoopDetector::len)
    }

    /// Record tool calls from a turn and check for loops.
    ///
    /// Returns `Some(TerminalReason::LoopDetected)` if a loop is detected.
    fn record_and_check_loop(
        &self,
        tool_calls: &[PendingToolCall],
        results: &[ToolResult],
    ) -> Option<TerminalReason> {
        let mut guard = self
            .loop_detector
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let detector = guard.as_mut()?;

        let calls: Vec<ToolCall> = tool_calls
            .iter()
            .map(|c| ToolCall {
                id: c.id.clone(),
                name: c.name.clone(),
                input: c.input.clone(),
            })
            .collect();

        if calls.is_empty() {
            detector.record_empty_turn();
        } else {
            // The answers are part of the turn: a poll that returns a different
            // status each time is progress, not a loop.
            detector.record_turn_with_results(&calls, results);
        }

        match detector.check() {
            LoopDetection::Ok => None,
            LoopDetection::LoopDetected {
                period,
                repetitions,
                description,
            } => {
                tracing::warn!("Loop detected: {description}");
                Some(TerminalReason::LoopDetected {
                    period,
                    repetitions,
                })
            }
        }
    }

    /// Compact session messages by keeping only the first user message
    /// and the most recent `keep_recent_turns` turns.
    ///
    /// Returns the compact result including how many tokens were removed
    /// and whether a boundary was inserted.
    pub fn compact_session_messages(&mut self, keep_recent_turns: usize) -> CompactResult {
        let result = compact::reactive_compact(&self.session.messages, keep_recent_turns);
        self.session.messages.clone_from(&result.messages);
        result
    }

    /// Handle an elicitation request using the configured elicitation handler.
    ///
    /// Transitions the turn to `AwaitElicitation`, sends the elicitation via
    /// the handler, and stores the response.
    async fn handle_elicitation(
        &self,
        turn: &mut TurnRuntime,
        request_id: String,
        message: String,
    ) -> ElicitationResult {
        turn.transition_to(TurnState::AwaitElicitation);

        // Emit elicitation event
        let _ = self.event_tx.send(EngineEvent::ElicitationRequest {
            id: request_id.clone(),
            message: message.clone(),
        });

        // The `Elicitation` hook runs with the question, before it is asked.
        let mut ctx = std::collections::HashMap::new();
        ctx.insert("elicitation_id".to_string(), request_id.clone());
        ctx.insert("message".to_string(), message.clone());
        let hook_result = self
            .fire_hook(hook_events::ELICITATION, &request_id, &ctx)
            .await;
        if !hook_result.errors.is_empty() {
            tracing::warn!("Elicitation hook errors: {:?}", hook_result.errors);
        }

        // Use the elicitation handler
        let question = crate::elicitation::ElicitationQuestion {
            question: message,
            header: "Query".into(),
            options: vec![
                crate::elicitation::ElicitationOption {
                    label: "Continue".into(),
                    description: "Proceed with the current approach".into(),
                },
                crate::elicitation::ElicitationOption {
                    label: "Clarify".into(),
                    description: "Provide additional clarification".into(),
                },
            ],
            multi_select: false,
        };

        let result = self.elicitation_handler.request(vec![question]).await;

        // The `ElicitationResult` hook sees what came back, before it is
        // stored on the turn.
        let mut ctx = std::collections::HashMap::new();
        ctx.insert("elicitation_id".to_string(), request_id.clone());
        ctx.insert(
            "outcome".to_string(),
            match &result {
                ElicitationResult::Responded(_) => "responded",
                ElicitationResult::Cancelled => "cancelled",
                ElicitationResult::TimedOut => "timed_out",
            }
            .to_string(),
        );
        if let ElicitationResult::Responded(resp) = &result
            && let Some(answer) = resp
                .other_text
                .clone()
                .or_else(|| resp.selected.first().cloned())
        {
            ctx.insert("response".to_string(), answer);
        }
        let hook_result = self
            .fire_hook(hook_events::ELICITATION_RESULT, &request_id, &ctx)
            .await;
        if !hook_result.errors.is_empty() {
            tracing::warn!("ElicitationResult hook errors: {:?}", hook_result.errors);
        }

        // Store the response
        match &result {
            ElicitationResult::Responded(resp) => {
                turn.pending_elicitation = Some(ResolvedElicitation {
                    request_id,
                    response: resp
                        .other_text
                        .clone()
                        .or_else(|| resp.selected.first().cloned()),
                });
            }
            ElicitationResult::Cancelled | ElicitationResult::TimedOut => {
                turn.pending_elicitation = Some(ResolvedElicitation {
                    request_id,
                    response: None,
                });
            }
        }

        result
    }

    /// Fire a hook event and return the result.
    async fn fire_hook(
        &self,
        event_name: &str,
        input: &str,
        context: &std::collections::HashMap<String, String>,
    ) -> HookFireResult {
        let result = self.hooks.fire_hook(event_name, input, context).await;
        // A mod's UI is a snapshot the host can render without the runtime.
        // Only a non-empty tree is worth an event; an empty one would make
        // every ordinary hook look like it wanted to draw something.
        if !result.ui.is_empty() {
            let _ = self.event_tx.send(EngineEvent::ModUi(result.ui.clone()));
        }
        result
    }

    /// Fire a lifecycle hook on behalf of the session that owns this engine.
    ///
    /// Some lifecycle events are not part of a turn. The REPL owns their
    /// triggers — `/init` is the product's setup action, and the settings
    /// watcher reports files that changed — but only the engine owns the hook
    /// executor. This is the supported bridge: the caller passes the event's
    /// wire name (from `HookEvent::as_str`) plus the input and context the
    /// hook should see, and receives the same aggregate the engine's own
    /// firing sites read.
    pub async fn fire_lifecycle_hook(
        &self,
        event_name: &str,
        input: &str,
        context: &std::collections::HashMap<String, String>,
    ) -> HookFireResult {
        self.fire_hook(event_name, input, context).await
    }

    /// Run the stop hooks, and enter the state that names them.
    ///
    /// `TurnState::StopHooks` was declared and never entered, and `Stop` was
    /// one of the hook events nothing fired — the same gap seen from two
    /// sides. The state is real now, and the hook fires in it: this is the
    /// last point at which a hook can say something about the turn before it
    /// is closed.
    async fn run_stop_hooks(&self, turn: &mut TurnRuntime, turn_number: u32, stop_reason: &str) {
        turn.transition_to(TurnState::StopHooks);

        let mut ctx = std::collections::HashMap::new();
        ctx.insert("turn_number".to_string(), turn_number.to_string());
        ctx.insert("stop_reason".to_string(), stop_reason.to_string());
        let result = self
            .fire_hook(hook_events::STOP, &format!("turn_{turn_number}"), &ctx)
            .await;
        if !result.errors.is_empty() {
            tracing::warn!("Stop hook errors: {:?}", result.errors);
        }
    }

    /// Fire the TurnEnd hook.
    async fn fire_turn_end_hook(&self, turn_number: u32, stop_reason: &str) -> HookFireResult {
        let mut ctx = std::collections::HashMap::new();
        ctx.insert("turn_number".to_string(), turn_number.to_string());
        ctx.insert("stop_reason".to_string(), stop_reason.to_string());
        self.fire_hook(hook_events::TURN_END, &format!("turn_{turn_number}"), &ctx)
            .await
    }

    /// Fire `SessionStart`, once, before the session's first turn.
    ///
    /// The engine has no explicit "the session begins" call — a session is
    /// this engine plus the prompts submitted to it — so the first turn is the
    /// earliest honest point at which a session hook can run. Its partner,
    /// `SessionEnd`, is emitted by [`interrupt`](Self::interrupt).
    async fn fire_session_start_hook(&mut self) {
        if self.session_started {
            return;
        }
        self.session_started = true;
        let session_id = self.session.session_id.to_string();
        let mut ctx = std::collections::HashMap::new();
        ctx.insert("session_id".to_string(), session_id.clone());
        ctx.insert("cwd".to_string(), self.session.cwd.display().to_string());
        let result = self
            .fire_hook(hook_events::SESSION_START, &session_id, &ctx)
            .await;
        if !result.errors.is_empty() {
            tracing::warn!("SessionStart hook errors: {:?}", result.errors);
        }
        self.session_watch_paths.extend(result.watch_paths);
        let mut seen = std::collections::HashSet::new();
        self.session_watch_paths
            .retain(|path| seen.insert(path.clone()));
    }

    /// Fire `SessionStart` now, before the first turn.
    ///
    /// The engine fires it lazily on the first turn. A workload that must
    /// know the hook's `watchPaths` before that turn starts — the headless
    /// `FileChanged` watcher — calls this explicitly; the lazy call then
    /// becomes a no-op. Firing twice is not observable because of the
    /// `session_started` latch.
    pub async fn ensure_session_started(&mut self) {
        self.fire_session_start_hook().await;
    }

    /// Set the JSON schema for structured output validation.
    pub fn set_structured_output_schema(&mut self, schema: serde_json::Value) {
        self.structured_output = Some(StructuredOutputValidator::new(schema));
    }

    /// Get the denied tool calls summary.
    #[must_use]
    pub fn denied_tools(&self) -> &[DeniedToolCall] {
        &self.denied_tools
    }

    /// Clear denied tool calls (called after they've been injected).
    pub fn clear_denied_tools(&mut self) {
        self.denied_tools.clear();
    }

    /// Get the usage summary recorded for the most recent request.
    #[must_use]
    pub const fn last_usage(&self) -> Option<UsageSummary> {
        self.last_usage
    }

    /// Get the total token usage accumulated across all completed turns.
    #[must_use]
    pub fn total_usage(&self) -> UsageSummary {
        let mut total = UsageSummary {
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        };
        for turn in self.cost_tracker.get_turn_history() {
            total.input_tokens += turn.usage.input_tokens;
            total.output_tokens += turn.usage.output_tokens;
            total.cache_read_tokens += turn.usage.cache_read_tokens;
            total.cache_creation_tokens += turn.usage.cache_creation_tokens;
        }
        total
    }

    /// Submit a user message and run the full turn loop.
    ///
    /// This is the main entry point for conversation turns. It will:
    /// 1. Add the user message to the session
    /// 2. Build context (system prompts, memory, etc.)
    /// 3. Send to API and stream response
    /// 4. Execute any tool calls (with permission checks)
    /// 5. Evaluate continuation
    /// 6. Repeat until done or max_turns reached
    pub async fn submit(
        &mut self,
        prompt: String,
        options: SubmitOptions,
    ) -> Result<TerminalReason, String> {
        let budget = if options.task_budget_tokens > 0 {
            let mut budget = TokenBudget::with_defaults();
            budget.thresholds.max_tokens_per_session = options.task_budget_tokens;
            budget
        } else {
            TokenBudget::with_defaults()
        };

        let mut turn = TurnRuntime::new(budget);
        turn.agent_id = options.agent_id.clone();
        turn.transition_observer = self.transition_observer.clone();
        turn.transition_observer = self.transition_observer.clone();
        let mut turn_number = self.current_turn.load(std::sync::atomic::Ordering::SeqCst) + 1;
        let mut last_reason = TerminalReason::Completed;
        let mut provider_resumes: u32 = 0;

        // A session that sits idle still has a cache to keep warm; the timer
        // asks for compaction when the TTL is nearly up.
        self.ensure_cache_ttl_timer();
        // The session's first turn is where `SessionStart` fires: the engine
        // has no earlier hook-capable point.
        self.fire_session_start_hook().await;

        // Add user message
        self.session.messages.push(Message::user(prompt.clone()));

        // Main turn loop
        // A request that arrives after an interrupt is a fresh one.
        self.interrupt.clear();

        loop {
            if options.max_turns > 0 && turn_number > options.max_turns {
                turn.terminate(TerminalReason::MaxTurnsReached);
                break;
            }

            self.cost_tracker.start_turn();
            turn.reset();

            // The model has no clock of its own: hand it the budget at the
            // milestones. The message is part of the next request, so the
            // model reads it before deciding what this turn should be.
            if let Some(reminder) = self.take_time_budget_reminder() {
                self.session.messages.push(Message::user(reminder));
            }

            // Execute the turn
            match self.execute_turn(&mut turn, &options, turn_number).await {
                Ok(reason) => {
                    self.current_turn
                        .store(turn_number, std::sync::atomic::Ordering::SeqCst);
                    last_reason = reason.clone();

                    // An interrupt that arrived while this turn was running —
                    // during a tool, most often — ends the run here rather
                    // than starting another turn.
                    if self.interrupt.is_interrupted() {
                        turn.terminate(TerminalReason::UserInterrupted);
                        last_reason = TerminalReason::UserInterrupted;
                        break;
                    }

                    // Advance coordinator phase if present
                    if let Some(ref mut coordinator) = self.coordinator {
                        let summary = format!("Turn {turn_number}: {reason}");
                        let _ = coordinator.advance(&summary);
                    }

                    let last_is_tool_result = self
                        .session
                        .messages
                        .last()
                        .is_some_and(|m| matches!(m.role, crate::types::Role::Tool));
                    let step = next_loop_step(&reason, last_is_tool_result, provider_resumes);
                    // Update only after deciding the step: the cap is checked
                    // against the number of resumes that have already run.
                    track_provider_resumes(&reason, &mut provider_resumes);
                    match step {
                        LoopStep::Continue => {
                            turn_number += 1;
                        }
                        LoopStep::Break(reason) => {
                            last_reason = reason;
                            break;
                        }
                    }
                }
                // The error says which kind it is; the loop does not read the
                // message to find out. A failed request still ended a turn:
                // close it through the same hooks and bookkeeping as any
                // other, with no usage because nothing was billed.
                Err(error) => {
                    let reason = error.terminal_reason();
                    let stop_reason = match &error {
                        TurnError::ApiUnavailable { .. } => "api_unavailable",
                        TurnError::ContextOverflow { .. } => "context_overflow",
                        TurnError::Fatal { .. } => "error",
                    };
                    let _ = self
                        .finish_turn(
                            &mut turn,
                            turn_number,
                            stop_reason,
                            &UsageSummary::default(),
                            &[],
                            reason.clone(),
                        )
                        .await;
                    self.current_turn
                        .store(turn_number, std::sync::atomic::Ordering::SeqCst);
                    return match error {
                        TurnError::ApiUnavailable { .. } | TurnError::ContextOverflow { .. } => {
                            Ok(reason)
                        }
                        TurnError::Fatal { message } => Err(message),
                    };
                }
            }

            if turn.state.is_terminal()
                && let Some(reason) = turn.terminal_reason.take()
            {
                return Ok(reason);
            }
        }

        if !turn.state.is_terminal() {
            turn.terminate(last_reason.clone());
        }
        Ok(turn.terminal_reason.unwrap_or(last_reason))
    }

    /// Submit a user message and run the full turn loop using streaming API.
    ///
    /// This is the streaming variant — tokens are emitted as they arrive,
    /// and tool calls are assembled incrementally during the stream.
    pub async fn submit_streaming(
        &mut self,
        prompt: String,
        options: SubmitOptions,
    ) -> Result<TerminalReason, String> {
        let budget = if options.task_budget_tokens > 0 {
            let mut budget = TokenBudget::with_defaults();
            budget.thresholds.max_tokens_per_session = options.task_budget_tokens;
            budget
        } else {
            TokenBudget::with_defaults()
        };

        let mut turn = TurnRuntime::new(budget);
        turn.agent_id = options.agent_id.clone();
        let mut turn_number = self.current_turn.load(std::sync::atomic::Ordering::SeqCst) + 1;
        let mut last_reason = TerminalReason::Completed;
        let mut provider_resumes: u32 = 0;

        // A session that sits idle still has a cache to keep warm; the timer
        // asks for compaction when the TTL is nearly up.
        self.ensure_cache_ttl_timer();
        // The session's first turn is where `SessionStart` fires: the engine
        // has no earlier hook-capable point.
        self.fire_session_start_hook().await;

        // Add user message
        self.session.messages.push(Message::user(prompt.clone()));

        // Main turn loop
        // A request that arrives after an interrupt is a fresh one.
        self.interrupt.clear();

        loop {
            if options.max_turns > 0 && turn_number > options.max_turns {
                turn.terminate(TerminalReason::MaxTurnsReached);
                break;
            }

            self.cost_tracker.start_turn();
            turn.reset();

            // The model has no clock of its own: hand it the budget at the
            // milestones. The message is part of the next request, so the
            // model reads it before deciding what this turn should be.
            if let Some(reminder) = self.take_time_budget_reminder() {
                self.session.messages.push(Message::user(reminder));
            }

            // Execute the turn using streaming
            match self
                .execute_turn_streaming(&mut turn, &options, turn_number)
                .await
            {
                Ok(reason) => {
                    self.current_turn
                        .store(turn_number, std::sync::atomic::Ordering::SeqCst);
                    last_reason = reason.clone();

                    // A turn that was interrupted ends here rather than
                    // continuing into the work it was about to do.
                    if self.interrupt.is_interrupted() {
                        turn.terminate(TerminalReason::UserInterrupted);
                        last_reason = TerminalReason::UserInterrupted;
                        break;
                    }

                    // Advance coordinator phase if present
                    if let Some(ref mut coordinator) = self.coordinator {
                        let summary = format!("Turn {turn_number}: {reason}");
                        let _ = coordinator.advance(&summary);
                    }

                    let last_is_tool_result = self
                        .session
                        .messages
                        .last()
                        .is_some_and(|m| matches!(m.role, crate::types::Role::Tool));
                    let step = next_loop_step(&reason, last_is_tool_result, provider_resumes);
                    // Update only after deciding the step: the cap is checked
                    // against the number of resumes that have already run.
                    track_provider_resumes(&reason, &mut provider_resumes);
                    match step {
                        LoopStep::Continue => {
                            turn_number += 1;
                        }
                        LoopStep::Break(reason) => {
                            last_reason = reason;
                            break;
                        }
                    }
                }
                // The error says which kind it is; the loop does not read the
                // message to find out. As in `submit`, a failed request still
                // closes the turn through Stop/TurnEnd and the cost and loop
                // bookkeeping, with zero usage.
                Err(error) => {
                    let reason = error.terminal_reason();
                    let stop_reason = match &error {
                        TurnError::ApiUnavailable { .. } => "api_unavailable",
                        TurnError::ContextOverflow { .. } => "context_overflow",
                        TurnError::Fatal { .. } => "error",
                    };
                    let _ = self
                        .finish_turn(
                            &mut turn,
                            turn_number,
                            stop_reason,
                            &UsageSummary::default(),
                            &[],
                            reason.clone(),
                        )
                        .await;
                    self.current_turn
                        .store(turn_number, std::sync::atomic::Ordering::SeqCst);
                    return match error {
                        TurnError::ApiUnavailable { .. } | TurnError::ContextOverflow { .. } => {
                            Ok(reason)
                        }
                        TurnError::Fatal { message } => Err(message),
                    };
                }
            }

            if turn.state.is_terminal()
                && let Some(reason) = turn.terminal_reason.take()
            {
                return Ok(reason);
            }
        }

        if !turn.state.is_terminal() {
            turn.terminate(last_reason.clone());
        }
        Ok(turn.terminal_reason.unwrap_or(last_reason))
    }

    /// Execute a single turn through the state machine.
    async fn execute_turn(
        &mut self,
        turn: &mut TurnRuntime,
        options: &SubmitOptions,
        turn_number: u32,
    ) -> Result<TerminalReason, TurnError> {
        let _start = Instant::now();

        // Phase 1: Initialize → BuildContext
        turn.transition_to(TurnState::BuildContext);
        self.build_context(turn, options);

        // Phase 2: BuildContext → RunPreSubmitHooks
        turn.transition_to(TurnState::RunPreSubmitHooks);

        // Fire UserPromptSubmit hook
        let user_prompt = self.submitted_prompt();
        let ctx = std::collections::HashMap::new();
        let hook_result = self
            .fire_hook(hook_events::USER_PROMPT_SUBMIT, &user_prompt, &ctx)
            .await;
        if hook_result.stop {
            // Say why a gate closed the turn: without this the only symptom is
            // "no request was made".
            tracing::warn!(
                errors = ?hook_result.errors,
                failed = hook_result.failed,
                "UserPromptSubmit hook stopped the turn before a request"
            );
            // The hook stopped the turn before a request. Close it like any
            // other turn so Stop/TurnEnd and the bookkeeping run; the closing
            // path enters StopHooks from `CompactRecovery`, the graph-legal
            // predecessor reachable from the pre-submit state.
            if !matches!(turn.state, TurnState::CompactRecovery) {
                turn.transition_to(TurnState::CompactRecovery);
            }
            return Ok(self
                .finish_turn(
                    turn,
                    turn_number,
                    "user_prompt_submit_stop",
                    &UsageSummary::default(),
                    &[],
                    TerminalReason::Completed,
                )
                .await);
        }

        // `updatedInput` replaces the submitted prompt before it reaches the
        // provider. The hook's input was the original, so a hook can normalise
        // or redact what the model sees without changing the visible history.
        if let Some(updated) = hook_result.updated_input.as_deref() {
            self.apply_updated_prompt(turn, updated);
        }

        // The prompt cache may have gone cold while this session sat idle:
        // the cheapest moment to compact is just before a request that is
        // going to miss the cache anyway.
        if let Some(outcome) = self
            .maybe_compact_on_cache_ttl(turn, options, turn_number)
            .await
        {
            tracing::debug!("cache-TTL compaction: {outcome:?}");
        }

        // The context-size lines have to be checked before the request, not
        // only after an output-token budget trips: by then the input may
        // already be too large to send.
        if let Some(reason) = self
            .maybe_compact_on_threshold(turn, options, turn_number)
            .await
        {
            // A context that cannot be brought under the window ends the turn
            // here, before a request. Close it like any other turn so the
            // Stop/TurnEnd hooks, cost bookkeeping, and loop check run, and so
            // the loop sees a terminal reason instead of re-running forever.
            //
            // The retry-budget exit returns without having run compaction, so
            // the turn is still in `RunPreSubmitHooks`; StopHooks is not a
            // legal move from there. `CompactRecovery` is the recovery
            // decision this turn reached and the predecessor the closing path
            // leaves from.
            if !matches!(turn.state, TurnState::CompactRecovery) {
                turn.transition_to(TurnState::CompactRecovery);
            }
            return Ok(self
                .finish_turn(
                    turn,
                    turn_number,
                    "context_blocked",
                    &UsageSummary::default(),
                    &[],
                    reason,
                )
                .await);
        }

        // Phase 3: RunPreSubmitHooks → StartRequest
        turn.transition_to(TurnState::StartRequest);

        // Phase 4: StartRequest → StreamAssistant
        turn.transition_to(TurnState::StreamAssistant);
        let mut overflow_retries = 0u32;
        let api_response = loop {
            match self.send_request(turn, options).await {
                Ok(response) => break response,
                Err(error @ TurnError::ContextOverflow { .. }) => {
                    overflow_retries += 1;
                    if overflow_retries > MAX_CONTEXT_OVERFLOW_RETRIES {
                        return Err(error);
                    }
                    self.recover_from_context_overflow(turn, error, options, turn_number)
                        .await?;
                    turn.transition_to(TurnState::StartRequest);
                    turn.transition_to(TurnState::StreamAssistant);
                }
                Err(error) => return Err(error),
            }
        };
        turn.last_stop_reason = Some(api_response.stop_reason.clone());

        // Record budget and cost
        turn.budget.record_tokens(&api_response.usage);
        self.cost_tracker
            .record_usage(&api_response.usage, &self.model);
        self.last_usage = Some(api_response.usage);

        // Parse the response
        let content = self.validate_structured_output(api_response.content.clone());
        self.push_thinking_message(turn, api_response.thinking.clone());
        self.push_visible_assistant_message(turn, content.clone());

        // Elicitation heuristic: if the assistant asks a short question, prompt the user
        let mut api_response = api_response;
        if options.handle_elicitation && content.trim().ends_with('?') && content.len() < 200 {
            let result = self
                .handle_elicitation(turn, format!("elic_{turn_number}"), content.clone())
                .await;
            if let ElicitationResult::Responded(resp) = result
                && let Some(user_text) = resp.other_text.or_else(|| resp.selected.first().cloned())
            {
                let user_msg = Message::user(user_text);
                self.push_history_message(turn, user_msg);
                // Re-send request with clarified context
                api_response = self.send_request(turn, options).await?;
                turn.budget.record_tokens(&api_response.usage);
                self.cost_tracker
                    .record_usage(&api_response.usage, &self.model);
                self.last_usage = Some(api_response.usage);
                let new_content = self.validate_structured_output(api_response.content.clone());
                self.push_thinking_message(turn, api_response.thinking.clone());
                self.push_visible_assistant_message(turn, new_content);
            }
        }

        // Use structured tool calls from the API response (blocking path).
        // Tool calls come from the API response and nowhere else. The model
        // decides what to call; the engine does not re-derive calls from the
        // assistant's prose or from the user's wording.
        let tool_calls = api_response.tool_calls;

        if tool_calls.is_empty() {
            // No tool calls — turn is complete
            turn.transition_to(TurnState::EvaluateContinuation);
            if !turn.budget.should_continue() {
                // Try compact recovery before giving up
                if turn.recovery_count < self.compact_config.max_recovery_retries {
                    turn.transition_to(TurnState::CompactRecovery);
                    let outcome = self
                        .run_compact_recovery(
                            turn,
                            options,
                            turn_number,
                            CompactTrigger::TokenThreshold,
                        )
                        .await;
                    if let Some(reason) = budget_terminal_reason(&outcome) {
                        return Ok(self
                            .finish_turn(
                                turn,
                                turn_number,
                                &api_response.stop_reason,
                                &api_response.usage,
                                &[],
                                reason,
                            )
                            .await);
                    }
                    // Retry the request with compacted context
                    turn.transition_to(TurnState::StartRequest);
                    let api_response = self.send_request(turn, options).await?;
                    turn.budget.record_tokens(&api_response.usage);
                    self.cost_tracker
                        .record_usage(&api_response.usage, &self.model);
                    self.last_usage = Some(api_response.usage);
                    let content = self.validate_structured_output(api_response.content.clone());
                    self.push_thinking_message(turn, api_response.thinking.clone());
                    self.push_visible_assistant_message(turn, content);

                    // After recovery retry, check if we still need to compact
                    if !turn.budget.should_continue() {
                        return Ok(self
                            .finish_turn(
                                turn,
                                turn_number,
                                "budget_exhausted",
                                &api_response.usage,
                                &[],
                                TerminalReason::BudgetExhausted,
                            )
                            .await);
                    }

                    if let Some(reason) = self
                        .close_turn(
                            turn,
                            turn_number,
                            &api_response.stop_reason,
                            &api_response.usage,
                            &[],
                        )
                        .await
                    {
                        return Ok(reason);
                    }
                    // Inside `tool_calls.is_empty()`: no call was made, so
                    // text is the only thing that could have produced output.
                    return Ok(Self::reason_for(
                        &api_response.stop_reason,
                        produced_output(&api_response.content, 0),
                    ));
                }

                return Ok(self
                    .finish_turn(
                        turn,
                        turn_number,
                        "budget_exhausted",
                        &api_response.usage,
                        &[],
                        TerminalReason::BudgetExhausted,
                    )
                    .await);
            }

            if let Some(reason) = self
                .close_turn(
                    turn,
                    turn_number,
                    &api_response.stop_reason,
                    &api_response.usage,
                    &[],
                )
                .await
            {
                return Ok(reason);
            }
            return Ok(Self::reason_for(
                &api_response.stop_reason,
                // Reaching here means no tool call was made, so the answer
                // either has text or has nothing.
                produced_output(&api_response.content, 0),
            ));
        }

        // Phase 5: Execute tools
        self.persist_assistant_tool_calls(turn, &tool_calls);
        turn.pending_tool_calls = tool_calls.iter().map(PendingToolCall::from).collect();
        let pending_for_loop = turn.pending_tool_calls.clone();

        turn.transition_to(TurnState::ExecuteTools);
        let tool_results = self.execute_tool_calls(turn, options).await?;

        // Phase 6: Apply tool results
        turn.transition_to(TurnState::ApplyToolResults);
        for result in &tool_results {
            let msg = Message::tool(result.clone());
            // Both histories: the session's, which is what the next request is
            // built from, and the turn's, which is what `close_turn` reads to
            // pair results with the calls that produced them. Writing only to
            // the session left `close_turn` with no results at all, so every
            // turn's loop fingerprint was built from the calls alone — and a
            // poll whose answer changes each time (`sleep 55; tail …` while a
            // download runs) looked identical to a stuck one and ended the run.
            self.push_history_message(turn, msg);
        }

        // Phase 7: Evaluate continuation
        turn.transition_to(TurnState::EvaluateContinuation);
        if !turn.budget.should_continue() {
            // Try compact recovery before giving up
            if turn.recovery_count < self.compact_config.max_recovery_retries {
                turn.transition_to(TurnState::CompactRecovery);
                let outcome = self
                    .run_compact_recovery(
                        turn,
                        options,
                        turn_number,
                        CompactTrigger::TokenThreshold,
                    )
                    .await;
                if let Some(reason) = budget_terminal_reason(&outcome) {
                    return Ok(self
                        .finish_turn(
                            turn,
                            turn_number,
                            &api_response.stop_reason,
                            &api_response.usage,
                            &pending_for_loop,
                            reason,
                        )
                        .await);
                }
                // After compact, tool results are still pending — the caller
                // will start the next turn with the compacted context
            } else {
                return Ok(self
                    .finish_turn(
                        turn,
                        turn_number,
                        "budget_exhausted",
                        &api_response.usage,
                        &pending_for_loop,
                        TerminalReason::BudgetExhausted,
                    )
                    .await);
            }
        }

        if let Some(reason) = self
            .close_turn(
                turn,
                turn_number,
                &api_response.stop_reason,
                &api_response.usage,
                &pending_for_loop,
            )
            .await
        {
            return Ok(reason);
        }
        // Tool results are pending: the caller starts the next turn.
        Ok(TerminalReason::Completed)
    }

    /// Execute a single turn using the streaming API.
    ///
    /// This variant uses `create_message_streaming()` for real-time token
    /// streaming and the `StreamingToolCallAssembler` to incrementally
    /// parse tool calls from the response.
    ///
    /// Key difference from `execute_turn`:
    /// - Text is streamed and emitted as it arrives
    /// - Tool calls are assembled incrementally during the stream
    /// - Early tool execution is possible (tool calls can be executed
    ///   as they complete, before the full response is received)
    async fn execute_turn_streaming(
        &mut self,
        turn: &mut TurnRuntime,
        options: &SubmitOptions,
        turn_number: u32,
    ) -> Result<TerminalReason, TurnError> {
        let _start = Instant::now();

        // Phase 1: Initialize → BuildContext
        turn.transition_to(TurnState::BuildContext);
        self.build_context(turn, options);

        // Phase 2: BuildContext → RunPreSubmitHooks
        turn.transition_to(TurnState::RunPreSubmitHooks);

        // Fire UserPromptSubmit hook
        let user_prompt = self.submitted_prompt();
        let ctx = std::collections::HashMap::new();
        let hook_result = self
            .fire_hook(hook_events::USER_PROMPT_SUBMIT, &user_prompt, &ctx)
            .await;
        if hook_result.stop {
            tracing::warn!(
                errors = ?hook_result.errors,
                failed = hook_result.failed,
                "UserPromptSubmit hook stopped the turn before a request"
            );
            // See `execute_turn`: a stopped pre-submit turn still closes
            // through Stop/TurnEnd and the bookkeeping.
            if !matches!(turn.state, TurnState::CompactRecovery) {
                turn.transition_to(TurnState::CompactRecovery);
            }
            return Ok(self
                .finish_turn(
                    turn,
                    turn_number,
                    "user_prompt_submit_stop",
                    &UsageSummary::default(),
                    &[],
                    TerminalReason::Completed,
                )
                .await);
        }

        // `updatedInput` replaces the submitted prompt before it reaches the
        // provider. The hook's input was the original, so a hook can normalise
        // or redact what the model sees without changing the visible history.
        if let Some(updated) = hook_result.updated_input.as_deref() {
            self.apply_updated_prompt(turn, updated);
        }

        // The prompt cache may have gone cold while this session sat idle:
        // the cheapest moment to compact is just before a request that is
        // going to miss the cache anyway.
        if let Some(outcome) = self
            .maybe_compact_on_cache_ttl(turn, options, turn_number)
            .await
        {
            tracing::debug!("cache-TTL compaction: {outcome:?}");
        }

        // Check the context lines before sending: an input that is already
        // past the window cannot be fixed by compacting after the fact.
        if let Some(reason) = self
            .maybe_compact_on_threshold(turn, options, turn_number)
            .await
        {
            // See `execute_turn`: a blocked context closes the turn through
            // the normal path so hooks and bookkeeping are not skipped and the
            // loop terminates. The retry-budget exit never reached a
            // compaction, so it still needs the graph-legal predecessor.
            if !matches!(turn.state, TurnState::CompactRecovery) {
                turn.transition_to(TurnState::CompactRecovery);
            }
            return Ok(self
                .finish_turn(
                    turn,
                    turn_number,
                    "context_blocked",
                    &UsageSummary::default(),
                    &[],
                    reason,
                )
                .await);
        }

        // Phase 3: RunPreSubmitHooks → StartRequest → StreamAssistant
        turn.transition_to(TurnState::StartRequest);
        turn.transition_to(TurnState::StreamAssistant);

        // Streaming request. A prompt too long for the provider is the one
        // request failure with a thing to do about it: learn the real window,
        // compact, and ask again.
        let mut overflow_retries = 0u32;
        let streamed = loop {
            match self.send_request_streaming(turn, options).await {
                Ok(response) => break response,
                Err(error @ TurnError::ContextOverflow { .. }) => {
                    overflow_retries += 1;
                    if overflow_retries > MAX_CONTEXT_OVERFLOW_RETRIES {
                        return Err(error);
                    }
                    self.recover_from_context_overflow(turn, error, options, turn_number)
                        .await?;
                    turn.transition_to(TurnState::StartRequest);
                    turn.transition_to(TurnState::StreamAssistant);
                }
                Err(error) => return Err(error),
            }
        };
        let StreamedResponse {
            mut content,
            mut tool_calls,
            mut early_calls,
            mut early_results,
            mut usage,
            mut stop_reason,
            thinking,
        } = streamed;
        turn.last_stop_reason = Some(stop_reason.clone());
        // How the provider says the answer ended decides whether the loop
        // continues. A truncated response (`max_tokens`) and a finished one
        // look the same to the turn: this is the only place that difference
        // is visible, so it is worth a line in the log.
        tracing::debug!(
            turn = turn_number,
            stop_reason = %stop_reason,
            tool_calls = tool_calls.len(),
            early_tool_calls = early_calls.len(),
            content_chars = content.len(),
            "provider answer ended"
        );

        // Record budget and cost
        turn.budget.record_tokens(&usage);
        self.cost_tracker.record_usage(&usage, &self.model);
        self.last_usage = Some(usage);

        // The thinking block belongs to the same assistant turn as the text
        // that follows it and must precede the visible message.
        self.push_thinking_message(turn, thinking);

        // Validate structured output if a schema is configured
        content = self.validate_structured_output(content);

        // Save assistant message
        self.push_visible_assistant_message(turn, content.clone());

        // Elicitation heuristic: if the assistant asks a short question, prompt the user
        if options.handle_elicitation && content.trim().ends_with('?') && content.len() < 200 {
            let result = self
                .handle_elicitation(turn, format!("elic_{turn_number}"), content.clone())
                .await;
            if let ElicitationResult::Responded(resp) = result
                && let Some(user_text) = resp.other_text.or_else(|| resp.selected.first().cloned())
            {
                let user_msg = Message::user(user_text);
                self.push_history_message(turn, user_msg);
                // Re-send request with clarified context
                let answered = self.send_request_streaming(turn, options).await?;
                turn.budget.record_tokens(&answered.usage);
                self.cost_tracker.record_usage(&answered.usage, &self.model);
                self.last_usage = Some(answered.usage);
                content = answered.content;
                tool_calls = answered.tool_calls;
                early_calls = answered.early_calls;
                early_results = answered.early_results;
                usage = answered.usage;
                stop_reason = answered.stop_reason;
                self.push_thinking_message(turn, answered.thinking);
                turn.last_stop_reason = Some(stop_reason.clone());
                self.push_visible_assistant_message(turn, content.clone());
            }
        }

        // Apply early-executed tool results immediately so the model can see them.
        // Their `tool_use` messages go first: a tool_result without the call it
        // answers is not a valid assistant/user pair, and the next request
        // would be rejected by the provider.
        tracing::info!("early_results count: {}", early_results.len());
        self.persist_assistant_tool_calls(turn, &early_calls);
        for result in &early_results {
            let msg = Message::tool(result.clone());
            self.push_history_message(turn, msg);
        }
        tracing::info!(
            "session.messages after early_results: {:?}",
            self.session.messages.len()
        );

        // Tool calls are whatever the model emitted during the stream. The
        // engine does not synthesise calls from text or from the user prompt.

        // Loop detection must see every call the model made, including the
        // ones already executed while the stream was open. Recording only the
        // deferred set made a model that repeated an early-executed call look
        // like a model that made no calls at all — and the loop ran forever.
        let pending_for_loop: Vec<PendingToolCall> = early_calls
            .iter()
            .chain(tool_calls.iter())
            .map(PendingToolCall::from)
            .collect();

        if tool_calls.is_empty() {
            // No tool calls — turn complete
            turn.transition_to(TurnState::EvaluateContinuation);
            if !turn.budget.should_continue() {
                if turn.recovery_count < self.compact_config.max_recovery_retries {
                    turn.transition_to(TurnState::CompactRecovery);
                    let outcome = self
                        .run_compact_recovery(
                            turn,
                            options,
                            turn_number,
                            CompactTrigger::TokenThreshold,
                        )
                        .await;
                    if let Some(reason) = budget_terminal_reason(&outcome) {
                        return Ok(self
                            .finish_turn(
                                turn,
                                turn_number,
                                &stop_reason,
                                &usage,
                                &pending_for_loop,
                                reason,
                            )
                            .await);
                    }
                    turn.transition_to(TurnState::StartRequest);
                    let retried = self.send_request_streaming(turn, options).await?;
                    turn.budget.record_tokens(&retried.usage);
                    self.cost_tracker.record_usage(&retried.usage, &self.model);
                    self.last_usage = Some(retried.usage);
                    // Whether the re-ask produced anything, read before the
                    // content is moved into the history. Its tool calls are
                    // not carried into the loop (the original answer made
                    // none), so only its text can count as output here.
                    let produced = produced_output(&retried.content, 0);
                    self.push_thinking_message(turn, retried.thinking);
                    self.push_visible_assistant_message(turn, retried.content);
                    let usage = retried.usage;

                    if !turn.budget.should_continue() {
                        return Ok(self
                            .finish_turn(
                                turn,
                                turn_number,
                                "budget_exhausted",
                                &usage,
                                &pending_for_loop,
                                TerminalReason::BudgetExhausted,
                            )
                            .await);
                    }

                    let stop_reason = retried.stop_reason.clone();
                    turn.last_stop_reason = Some(stop_reason.clone());
                    if let Some(reason) = self
                        .close_turn(turn, turn_number, &stop_reason, &usage, &pending_for_loop)
                        .await
                    {
                        return Ok(reason);
                    }
                    return Ok(Self::reason_for(&stop_reason, produced));
                }
                return Ok(self
                    .finish_turn(
                        turn,
                        turn_number,
                        "budget_exhausted",
                        &usage,
                        &pending_for_loop,
                        TerminalReason::BudgetExhausted,
                    )
                    .await);
            }
            // A tool call is output too, including one already executed while
            // the stream was open — it is how the run made progress.
            let produced = produced_output(&content, early_calls.len() + tool_calls.len());
            if let Some(reason) = self
                .close_turn(turn, turn_number, &stop_reason, &usage, &pending_for_loop)
                .await
            {
                return Ok(reason);
            }
            return Ok(Self::reason_for(&stop_reason, produced));
        }

        // Phase 5: Execute tools
        self.persist_assistant_tool_calls(turn, &tool_calls);
        turn.pending_tool_calls = tool_calls.iter().map(PendingToolCall::from).collect();

        turn.transition_to(TurnState::ExecuteTools);
        let tool_results = self.execute_tool_calls(turn, options).await?;

        // Phase 6: Apply tool results
        turn.transition_to(TurnState::ApplyToolResults);
        for result in &tool_results {
            let msg = Message::tool(result.clone());
            // Both histories: the session's, which is what the next request is
            // built from, and the turn's, which is what `close_turn` reads to
            // pair results with the calls that produced them. Writing only to
            // the session left `close_turn` with no results at all, so every
            // turn's loop fingerprint was built from the calls alone — and a
            // poll whose answer changes each time (`sleep 55; tail …` while a
            // download runs) looked identical to a stuck one and ended the run.
            self.push_history_message(turn, msg);
        }

        // Phase 7: Evaluate continuation
        turn.transition_to(TurnState::EvaluateContinuation);
        if !turn.budget.should_continue() {
            if turn.recovery_count < self.compact_config.max_recovery_retries {
                turn.transition_to(TurnState::CompactRecovery);
                let outcome = self
                    .run_compact_recovery(
                        turn,
                        options,
                        turn_number,
                        CompactTrigger::TokenThreshold,
                    )
                    .await;
                if let Some(reason) = budget_terminal_reason(&outcome) {
                    return Ok(self
                        .finish_turn(
                            turn,
                            turn_number,
                            &stop_reason,
                            &usage,
                            &pending_for_loop,
                            reason,
                        )
                        .await);
                }
            } else {
                return Ok(self
                    .finish_turn(
                        turn,
                        turn_number,
                        "budget_exhausted",
                        &usage,
                        &pending_for_loop,
                        TerminalReason::BudgetExhausted,
                    )
                    .await);
            }
        }

        if let Some(reason) = self
            .close_turn(turn, turn_number, &stop_reason, &usage, &pending_for_loop)
            .await
        {
            return Ok(reason);
        }
        // Tool results are pending: the caller starts the next turn.
        Ok(TerminalReason::Completed)
    }

    /// Close a turn: stop hooks, finalize, cost, turn-end hook, loop check.
    ///
    /// Both turn paths end here, and they had drifted apart: the streaming one
    /// passed a hardcoded `"end_turn"` as the reason and never reported a
    /// truncated answer as [`TerminalReason::TokenLimitStop`], so a response
    /// the model ran out of room for looked exactly like a finished turn.
    /// Returns a reason when the loop detector has had enough.
    async fn close_turn(
        &mut self,
        turn: &mut TurnRuntime,
        turn_number: u32,
        stop_reason: &str,
        usage: &UsageSummary,
        pending_for_loop: &[PendingToolCall],
    ) -> Option<TerminalReason> {
        self.run_stop_hooks(turn, turn_number, stop_reason).await;
        turn.transition_to(TurnState::Finalize);
        self.cost_tracker.end_turn(turn_number, usage, &self.model);
        let _ = self.fire_turn_end_hook(turn_number, stop_reason).await;

        // A provider-paused turn is not an empty turn. The model answered and
        // the provider asked for the same request again; recording that as a
        // tool-call-free turn would let the loop detector call a legitimate
        // pause a repetition and end the run after three of them.
        if matches!(classify_stop_reason(stop_reason), StopDisposition::Resume) {
            return None;
        }

        // The results belong to the calls of this turn, not to earlier turns
        // whose calls happen to share a direction.
        let pending_ids: std::collections::HashSet<&str> = pending_for_loop
            .iter()
            .map(|call| call.id.as_str())
            .collect();
        let results: Vec<ToolResult> = turn
            .messages
            .iter()
            .filter_map(|message| match &message.content {
                crate::types::MessageContent::ToolResult(result)
                    if pending_ids.contains(result.tool_call_id.as_str()) =>
                {
                    Some(result.clone())
                }
                _ => None,
            })
            .collect();
        self.record_and_check_loop(pending_for_loop, &results)
    }

    /// Close a turn that ends for a reason other than how the model stopped.
    ///
    /// Budget exhaustion, recovery failure, a pre-submit hook stop, and an
    /// outright request failure used to return straight out of the turn loop —
    /// `turn.terminate(...)` and a `return` — without running the Stop/TurnEnd
    /// hooks, the cost bookkeeping, or the loop check every model-ended turn
    /// goes through. They now take the same closing path and terminate
    /// afterwards. Request failures pass zero usage because nothing was billed.
    ///
    /// A loop the detector reports takes precedence over the reason passed in:
    /// it is the more specific diagnosis of why the run must end.
    async fn finish_turn(
        &mut self,
        turn: &mut TurnRuntime,
        turn_number: u32,
        stop_reason: &str,
        usage: &UsageSummary,
        pending_for_loop: &[PendingToolCall],
        reason: TerminalReason,
    ) -> TerminalReason {
        let detected = self
            .close_turn(turn, turn_number, stop_reason, usage, pending_for_loop)
            .await;
        let reason = detected.unwrap_or(reason);
        turn.terminate(reason.clone());
        reason
    }

    /// The reason a closed turn reports, given how the model stopped and
    /// whether the turn produced anything.
    ///
    /// A paused turn reports [`TerminalReason::Resume`] rather than
    /// `Completed`: both are "not an error", but only one of them tells the
    /// loop to send the request again.
    ///
    /// `produced` is only consulted for a `max_tokens` stop. It used to be
    /// dropped here, which is why a response that spent its whole ceiling on
    /// thinking reached the host indistinguishable from an answer that was
    /// merely cut short.
    #[must_use]
    fn reason_for(stop_reason: &str, produced: bool) -> TerminalReason {
        if stop_reason_eq(stop_reason, "max_tokens") {
            TerminalReason::TokenLimitStop { produced }
        } else if matches!(classify_stop_reason(stop_reason), StopDisposition::Resume) {
            TerminalReason::Resume
        } else {
            TerminalReason::Completed
        }
    }

    /// Validate structured output if a schema is configured.
    ///
    /// Returns the validated content string. If validation fails and retries
    /// are exhausted, returns the original content with a warning appended.
    fn validate_structured_output(&self, content: String) -> String {
        let Some(ref validator) = self.structured_output else {
            return content;
        };

        // First attempt (retries tracked per-turn)
        let result = validator.validate(&content, 0);
        match result {
            ValidationResult::Valid(_) => content,
            ValidationResult::Invalid { error, retries } => {
                tracing::warn!("Structured output validation failed: {error} (retry {retries})");
                let warning = format!("\n\n<!-- Structured output validation failed: {error} -->");
                format!("{content}{warning}")
            }
        }
    }

    /// Record a provider thinking block as part of the assistant turn.
    ///
    /// It is stored as its own [`Message`] so the runtime keeps the block
    /// intact; [`Self::provider_messages`] folds it into the next assistant
    /// message when the request is built.
    fn push_thinking_message(&mut self, turn: &mut TurnRuntime, thinking: Option<ThinkingBlock>) {
        let Some(block) = thinking else {
            return;
        };
        let msg = Message {
            role: crate::types::Role::Assistant,
            content: crate::types::MessageContent::Thinking(block),
        };
        self.push_history_message(turn, msg);
    }

    fn push_visible_assistant_message(&mut self, turn: &mut TurnRuntime, content: String) {
        if content.trim().is_empty() {
            return;
        }
        let assistant_msg = Message::assistant(content);
        turn.messages.push(assistant_msg.clone());
        self.session.messages.push(assistant_msg);
    }

    /// Format the installed team context for injection into the request.
    ///
    /// Returns `None` when no context is installed or it carries no notes,
    /// work items, or activity, so an empty context never changes the prompt.
    /// The lists are bounded: a long-lived team can accumulate hundreds of
    /// entries, and the point of the injection is shared working state, not a
    /// full audit log.
    fn team_context_block(&self) -> Option<String> {
        use std::fmt::Write as _;

        const MAX_NOTES: usize = 20;
        const MAX_WORK_ITEMS: usize = 10;
        const MAX_ACTIVITY: usize = 10;

        let context = self.team_context.as_ref()?.lock().ok()?;
        if context.shared_notes.is_empty()
            && context.in_progress.is_empty()
            && context.recent_activity.is_empty()
        {
            return None;
        }

        let mut block = format!("Team context (team {}):", context.team_id);
        if !context.shared_notes.is_empty() {
            block.push_str("\nShared notes:");
            for note in context.shared_notes.iter().take(MAX_NOTES) {
                let _ = write!(
                    block,
                    "\n- {} (by {}): {}",
                    note.title, note.author_id, note.content
                );
            }
        }
        if !context.in_progress.is_empty() {
            block.push_str("\nIn progress:");
            for item in context.in_progress.iter().take(MAX_WORK_ITEMS) {
                let _ = write!(
                    block,
                    "\n- {} (worker {}, {} steps remaining)",
                    item.description, item.worker_id, item.remaining_steps
                );
            }
        }
        if !context.recent_activity.is_empty() {
            block.push_str("\nRecent activity:");
            for entry in context.recent_activity.iter().take(MAX_ACTIVITY) {
                let _ = write!(block, "\n- {}: {}", entry.member_id, entry.description);
            }
        }
        Some(block)
    }

    /// Build the prompt context from session state.
    /// The prompt this turn is answering.
    ///
    /// `execute_turn` takes no prompt parameter — the prompt is the last user
    /// message on the session — and the `UserPromptSubmit` hook used to be
    /// handed `custom_system_prompt` instead. That is almost always empty, so a
    /// hook that inspects the prompt it is supposedly being told about saw
    /// nothing at all.
    fn submitted_prompt(&self) -> String {
        self.session
            .messages
            .iter()
            .rev()
            .find_map(|message| match (&message.role, &message.content) {
                (crate::types::Role::User, crate::types::MessageContent::Text(text)) => {
                    Some(text.clone())
                }
                _ => None,
            })
            .unwrap_or_default()
    }

    /// Replace the prompt the user just submitted with a hook's
    /// `updatedInput`.
    ///
    /// The prompt is already in the session and, because `build_context` runs
    /// before the pre-submit hook, in this turn's request snapshot. Both are
    /// updated so the model sees the replacement and later turns keep it.
    fn apply_updated_prompt(&mut self, turn: &mut TurnRuntime, updated: &str) {
        if Self::replace_last_user_text(&mut self.session.messages, updated) {
            Self::replace_last_user_text(&mut turn.messages, updated);
        }
    }

    /// Estimate the tokens the serialized tool schemas add to a request.
    ///
    /// The API client builds this array from the same [`ToolInfo`] values, in
    /// the same shape, so the estimate covers exactly what is sent: the
    /// schemas are not messages, and a request's biggest fixed cost after the
    /// system prompt is usually this array.
    fn tool_schema_tokens(tools: &[ToolInfo]) -> usize {
        if tools.is_empty() {
            return 0;
        }
        let schemas: Vec<serde_json::Value> = tools
            .iter()
            .map(|tool| {
                serde_json::json!({
                    "name": tool.name,
                    "description": tool.description,
                    "input_schema": tool.input_schema,
                })
            })
            .collect();
        PromptCompiler::estimate_tokens(&serde_json::Value::Array(schemas).to_string())
    }

    fn build_context(&mut self, turn: &mut TurnRuntime, options: &SubmitOptions) {
        // Assemble context using PromptCompiler
        let available_tools = self.tools.list_tools();
        turn.tool_schema_tokens = Self::tool_schema_tokens(&available_tools);
        let ctx = PromptContext {
            cwd: self.session.cwd.as_path(),
            available_tools,
            denied_tools: &self.denied_tools,
            model: &self.model,
            budget: &turn.budget,
        };
        let assembled: AssembledContext = self.prompt_compiler.assemble(&ctx);

        let denied_count = self.denied_tools.len();

        // Prepend context messages (CLAUDE.md wrapped as user message)
        for msg in &assembled.context_messages {
            turn.messages.push(msg.clone());
        }

        // Add system prompt
        turn.messages.push(Message::system(assembled.system_prompt));

        // Team collaboration: the CLI installs a `TeamContext` only when the
        // flag is on. Reading it here makes the shared notes, in-progress work
        // and recent activity part of every request. With no context installed
        // (the default) this adds nothing, so the prompt is unchanged.
        if let Some(team_context) = self.team_context_block() {
            turn.messages.push(Message::system(team_context));
        }

        // Add session conversation messages (after system context). Note the
        // range: this is the only part of the request that is history, and
        // the only part compaction may replace.
        let history_start = turn.messages.len();
        turn.messages.extend(self.session.messages.iter().cloned());
        turn.set_history_range(history_start, turn.messages.len());

        // Inject denied tool call summaries from previous turns
        if !self.denied_tools.is_empty() {
            let denied_summary = self.format_denied_tools_summary();
            turn.messages.push(Message::system(denied_summary));
            self.denied_tools.clear();
        }

        // Append custom system prompt if provided
        if let Some(ref prompt) = options.custom_system_prompt {
            turn.messages.push(Message::system(prompt.clone()));
        }

        // Append turn-specific system prompt
        if let Some(ref prompt) = options.append_system_prompt {
            turn.messages.push(Message::system(prompt.clone()));
        }

        // Inject JSON schema instructions for structured output
        if let Some(ref schema) = options.json_schema {
            let instruction = format!(
                "You must respond with ONLY valid JSON that matches this schema:\n{}",
                serde_json::to_string_pretty(schema).unwrap_or_default()
            );
            turn.messages.push(Message::system(instruction));
        }

        // Replay user messages if provided (for session resume)
        if let Some(ref replay) = options.replay_user_messages {
            for msg in replay {
                turn.messages.push(msg.clone());
            }
        }

        tracing::debug!(
            "BuildContext: {} messages, {} estimated tokens ({} denied tools injected)",
            turn.messages.len(),
            assembled.estimated_tokens,
            denied_count,
        );
    }

    /// Summarize a conversation interval via the attached compaction
    /// backend (local/remote dual-track seam).
    ///
    /// With no backend attached this is exactly the built-in local LLM
    /// summary. With a backend attached, a backend failure or empty summary
    /// falls back to the built-in local path — the backend is an
    /// optimization, never a new failure mode. Both paths collapse to
    /// `Result<String, String>` so the PTL retry and rule-based fallback
    /// below treat them uniformly. No remote compaction implementation
    /// exists yet; a backend is opt-in via `set_compaction_backend` and
    /// absent by default.
    async fn summarize_for_compaction(
        &self,
        conversation: &str,
        max_tokens: u32,
    ) -> Result<String, String> {
        summarize_conversation(
            self.api_client.as_ref(),
            self.compaction_backend.as_ref(),
            conversation,
            max_tokens,
        )
        .await
    }

    /// React to a provider-reported context overflow.
    ///
    /// The provider's error is the one authoritative statement about how large
    /// its window is, so the first thing done here is learning it — every
    /// later request and every later compaction threshold uses the real value
    /// instead of the table's guess. Then the turn compacts once and returns,
    /// leaving the caller to retry the request.
    ///
    /// Returns the original error when no compaction happened, because
    /// retrying an unchanged request fails the same way and only spends a
    /// request.
    async fn recover_from_context_overflow(
        &mut self,
        turn: &mut TurnRuntime,
        error: TurnError,
        options: &SubmitOptions,
        turn_number: u32,
    ) -> Result<(), TurnError> {
        let TurnError::ContextOverflow { actual, limit, .. } = &error else {
            return Err(error);
        };
        if *limit > 0 {
            let learned = actual.map_or(*limit, |a| a.max(*limit));
            self.learn_context_window(learned);
        }
        if turn.recovery_count >= self.compact_config.max_recovery_retries {
            tracing::warn!(
                recovery_count = turn.recovery_count,
                "context overflow with the recovery budget spent — not compacting again"
            );
            return Err(error);
        }

        // The request provably does not fit: whatever the estimate says, a
        // pass that is worth trying must not be skipped as speculative.
        turn.provider_overflowed = true;

        // The turn is past `StartRequest` when the request fails, and the
        // recovery decision is only reachable from there.
        turn.transition_to(TurnState::CompactRecovery);
        let outcome = self
            .run_compact_recovery(turn, options, turn_number, CompactTrigger::TokenThreshold)
            .await;
        if let Some(reason) = outcome.failure_message() {
            // The breaker says compaction has stopped trying; the provider's
            // overflow is still the reason this turn failed, and it is the
            // reason the caller can act on.
            if outcome.is_circuit_breaker_open() {
                return Err(error);
            }
            return Err(TurnError::Fatal { message: reason });
        }
        if !matches!(outcome, CompactOutcome::Compacted { .. }) {
            tracing::warn!(
                outcome = ?outcome,
                "context overflow: compaction made no progress — failing the turn"
            );
            return Err(error);
        }
        Ok(())
    }

    /// Run compact recovery when context is too large.
    ///
    /// Returns a named [`CompactOutcome`] for every path, including the ones
    /// that do nothing: "why compaction did not happen" is a question a caller
    /// and a test both need answered, and a log line answers it for neither.
    /// Run compact recovery and report its outcome on the event stream.
    ///
    /// Every path through the recovery — including the ones that do nothing —
    /// emits a [`CompactionEvent`], so a caller can observe why compaction did
    /// or did not happen.
    async fn run_compact_recovery(
        &mut self,
        turn: &mut TurnRuntime,
        options: &SubmitOptions,
        turn_number: u32,
        trigger: CompactTrigger,
    ) -> CompactOutcome {
        let outcome = self
            .run_compact_recovery_inner(turn, options, turn_number, trigger)
            .await;
        self.emit_compaction(compaction_event_from_outcome(&outcome));
        outcome
    }

    #[allow(clippy::unused_async)]
    async fn run_compact_recovery_inner(
        &mut self,
        turn: &mut TurnRuntime,
        _options: &SubmitOptions,
        _turn_number: u32,
        trigger: CompactTrigger,
    ) -> CompactOutcome {
        // Only the history is compactable: the preamble, the system prompt and
        // the per-turn injections around it are rebuilt next turn and would
        // both distort the estimate and be lost if the whole list were
        // replaced.
        let history = turn.history().to_vec();
        let prefix_tokens = turn.context_prefix_tokens();
        let estimated_tokens = PromptCompiler::estimate_message_tokens(&history) + prefix_tokens;
        // `estimated_tokens` is the heuristic over the message content; the
        // trigger that brought us here measured the whole request with the
        // provider's own count. They are different rulers: on JSON- and
        // code-heavy tool traffic the provider's tokenizer can report 30–40 %
        // more than `chars / 4`. The thresholds and the strategy ladder are
        // stated in the provider's units, so selection and the strategies'
        // targets are translated into heuristic units below; choosing either
        // from the raw heuristic made a context over the line look like it was
        // still in the `Ok` band and selected a pass that removed nothing.
        let measured_tokens = self.turn_context_tokens(turn);

        let breaker_open = turn.compact_failures >= self.compact_config.max_consecutive_failures;
        let budget_spent = turn.recovery_count >= self.compact_config.max_recovery_retries;
        // A spent budget or an open breaker is compaction's stop signal for
        // the *named* strategies, not permission to stop trying. The last
        // resort makes no model calls and cannot fail the way a summary can,
        // so it is always allowed one attempt — and at the blocked line it is
        // the difference between recovering and abandoning a productive run
        // a few hundred tokens over the line.
        let forced_last_resort =
            trigger == CompactTrigger::TokenThreshold && (breaker_open || budget_spent);

        if trigger == CompactTrigger::CacheTtl && breaker_open {
            return CompactOutcome::CircuitBreakerOpen {
                consecutive_failures: turn.compact_failures,
                max: self.compact_config.max_consecutive_failures,
            };
        }

        // A threshold pass can only rewrite the history. When everything that
        // is not history already fills the landing line, the smallest request
        // a pass could leave is still over it: the target in history space is
        // zero or negative. Running the pass would spend the recovery budget
        // and delete working context to prove arithmetic already known — and
        // then do it again next turn, which is exactly what a 32k guess on a
        // 128k model looked like from the outside (241 passes, 73 of them
        // removing nothing). Report it and let the caller decide; only the
        // blocked line makes it fatal.
        if trigger == CompactTrigger::TokenThreshold {
            let thresholds = self.compact_thresholds();
            // At the blocked line the request cannot be sent at all, and a
            // heavy pass that leaves the history above the landing line can
            // still bring it below the blocked one. Skipping there would turn
            // a recoverable turn into a failed one, so the arithmetic below
            // only stands down when the request can go out as it is.
            let blocked = thresholds.level(measured_tokens) == compact::ContextLevel::Blocked;
            let fixed_tokens =
                self.fixed_overhead_tokens(measured_tokens, estimated_tokens, prefix_tokens);
            let landing_tokens =
                Self::heuristic_budget(thresholds.landing(), measured_tokens, estimated_tokens);
            let floor = estimated_tokens.saturating_sub(fixed_tokens) / 4;
            let minimum_tokens = fixed_tokens.saturating_add(floor);
            if !blocked && !turn.provider_overflowed && landing_tokens <= minimum_tokens {
                tracing::warn!(
                    measured_tokens,
                    fixed_tokens,
                    landing_tokens,
                    minimum_tokens,
                    "compaction cannot reach the landing line; the request's fixed part fills it"
                );
                return CompactOutcome::Uncompactable {
                    fixed_tokens,
                    landing_tokens,
                    minimum_tokens,
                };
            }
        }

        // The strategy follows the trigger. A cold cache is a moment to tidy
        // up cheaply, not a reason to rewrite history: it takes the lightest
        // pass and never calls the summarizer. Crossing the token line takes
        // the strategy the thresholds name.
        // `Reactive` is the strategy for a response the model ran out of room
        // for. It has to see the actual stop reason, not a placeholder.
        let stop_reason = turn.last_stop_reason.as_deref().unwrap_or("end_turn");
        let mut strategy = match trigger {
            CompactTrigger::CacheTtl => CompactStrategy::Micro,
            CompactTrigger::TokenThreshold => self.context_window_resolver.as_ref().map_or_else(
                || {
                    compact::select_compact_strategy(
                        stop_reason,
                        measured_tokens,
                        &self.model,
                        &self.compact_config,
                    )
                },
                |resolver| {
                    compact::select_compact_strategy_with(
                        stop_reason,
                        measured_tokens,
                        &self.model,
                        &self.compact_config,
                        resolver,
                    )
                },
            ),
        };

        // Escalation: a pass that already ran and did not reach the line is
        // repeated with a cheaper, more aggressive strategy. Two `Auto` passes
        // in a row would spend the recovery budget on the same arithmetic that
        // just failed; the rule strategies are monotone in what they remove,
        // so stepping down the ladder is the move that can actually succeed.
        if trigger == CompactTrigger::TokenThreshold && !forced_last_resort {
            strategy = match turn.last_compact_strategy {
                Some(CompactStrategy::Micro) => CompactStrategy::Auto,
                Some(CompactStrategy::Auto | CompactStrategy::Reactive) => CompactStrategy::Snip,
                Some(CompactStrategy::Snip) => CompactStrategy::LastResort,
                _ => strategy,
            };
        }
        if forced_last_resort {
            strategy = CompactStrategy::LastResort;
        }

        // Execute Stop hook with dead loop protection
        let hook_ctx = StopHookContext {
            recovery_count: turn.recovery_count,
            max_recovery_retries: self.compact_config.max_recovery_retries,
            estimated_tokens,
            compact_strategy: strategy,
            stop_reason: stop_reason.to_string(),
        };
        let decision = stop_hook::evaluate_stop_hook(&hook_ctx);

        match decision {
            // The dead-loop guard is aimed at retrying the same paying
            // strategy. The last resort is local and free, and it is what an
            // exhausted budget leaves available, so the guard must not take
            // away the one attempt the budget did not spend.
            StopHookDecision::Terminate { reason } if !forced_last_resort => {
                tracing::warn!("Stop hook: {}", reason);
                return CompactOutcome::StopHookTerminated { reason };
            }
            StopHookDecision::Terminate { reason } => {
                tracing::warn!("Stop hook: {reason}; the last resort is still attempted");
            }
            StopHookDecision::Continue { .. } => {
                tracing::info!(
                    "CompactRecovery: applying {:?} strategy (estimated {} tokens, recovery #{})",
                    strategy,
                    estimated_tokens,
                    turn.recovery_count + 1,
                );
            }
        }

        // Fire PreCompact hook
        let mut ctx = std::collections::HashMap::new();
        ctx.insert("strategy".to_string(), format!("{strategy:?}"));
        ctx.insert("estimated_tokens".to_string(), estimated_tokens.to_string());
        ctx.insert("trigger".to_string(), format!("{trigger:?}"));
        let pre_compact_result = self
            .fire_hook(hook_events::PRE_COMPACT, "compact_recovery", &ctx)
            .await;
        if !pre_compact_result.errors.is_empty() {
            tracing::warn!("PreCompact hook errors: {:?}", pre_compact_result.errors);
        }

        // ── LLM summary compaction (preferred path) ──
        // Preserve head (first user message) and tail (recent turns) verbatim;
        // LLM-summarize the interval in between. Falls back to rule-based
        // strategies on any failure.
        //
        // The TTL trigger skips this: a summary is a model call, and a
        // proactive tidy-up is not worth one — and the lightest rule-based
        // pass is the whole point of compacting while the cache is cold.
        let tokens_before = estimated_tokens;
        let window = self.context_window().tokens;
        let summary_budget = self.compact_config.resolve_summary_input_budget(window);
        let tail_tokens = self.compact_config.resolve_summary_tail_tokens(window);
        let split = compact::summary_split_bounded(
            &history,
            3,
            self.compact_config.keep_recent_messages,
            tail_tokens,
        );
        let summary_input = compact::build_summary_input(&history, &split, summary_budget);

        let mut summary_failure: Option<SummaryOutcome> = None;
        // The history the rule-based fallback compacts. A summary that did not
        // reach the line is still worth keeping — it elides the interval it
        // covered — so the heavier pass starts from it rather than from the
        // original history, which would throw the paid-for summary away.
        let mut escalated_history: Option<Vec<Message>> = None;
        if !forced_last_resort && trigger == CompactTrigger::TokenThreshold {
            if summary_input.len() >= 2 {
                // A summary computed in the background while the context was
                // still below the line describes this exact interval: the
                // whole point of making it early is that this turn does not
                // have to wait for the round-trip. An interval that moved on
                // since is not used — it would elide messages it never saw.
                let prefetched = self.take_matching_summary(&history, &split);
                if prefetched.is_some() {
                    tracing::debug!("CompactRecovery: using the pre-generated summary");
                }
                // PTL resilience: if the summary request fails (e.g., the input
                // itself is too long), retry once with half the input budget —
                // `build_summary_input` re-caps by dropping the oldest messages.
                let mut summary_result = match &prefetched {
                    Some((summary, _)) => Ok(summary.clone()),
                    None => {
                        self.summarize_for_compaction(
                            &summary_input.join("\n\n"),
                            self.compact_config.summary_output_max_tokens,
                        )
                        .await
                    }
                };
                if summary_result.is_err() {
                    tracing::warn!(
                        "CompactRecovery: summary failed — retrying with half input budget"
                    );
                    let halved = compact::build_summary_input(&history, &split, summary_budget / 2);
                    if halved.len() >= 2 {
                        summary_result = self
                            .summarize_for_compaction(
                                &halved.join("\n\n"),
                                self.compact_config.summary_output_max_tokens,
                            )
                            .await;
                    }
                }
                // A pre-generated summary covered the split it was made for,
                // not necessarily today's: keep verbatim whatever lies between
                // its end and the recomputed one.
                let applied_split = prefetched
                    .as_ref()
                    .map_or_else(|| split.clone(), |(_, split)| split.clone());
                match summary_result {
                    Ok(summary_text) if !summary_text.trim().is_empty() => {
                        // Refresh the persistent session memory from the same
                        // summarized interval and merge it into the summary.
                        // Box::pin breaks the future-size cycle through the
                        // nested background-feature call chain.
                        let summary_text =
                            Box::pin(self.refresh_session_memory(&summary_text, &summary_input))
                                .await;
                        let mut result = compact::assemble_summary_result(
                            &history,
                            &applied_split,
                            &summary_text,
                            tokens_before,
                            0,
                        );
                        // Post-compact restore: breadcrumb the file paths whose
                        // contents were compacted away so the agent can re-read.
                        let restored_paths =
                            compact::extract_recent_file_paths(&history, &applied_split, 5);
                        if let Some(restore_msg) = compact::file_restore_message(&restored_paths) {
                            result.messages.push(restore_msg);
                        }
                        // A summary that leaves the context above the line and
                        // removes less than a tenth of it has not recovered
                        // anything. The rules can remove more than the summary
                        // is willing to when the bulk sits in the preserved
                        // tail, so step down the ladder now instead of
                        // spending a whole turn on the same ineffective pass.
                        if !self.result_needs_heavier_pass(turn, &history, &result.messages) {
                            return self
                                .finish_compaction(
                                    turn,
                                    result,
                                    trigger,
                                    SummaryOutcome::Summarized,
                                )
                                .await;
                        }
                        tracing::debug!(
                            "CompactRecovery: summary made no progress — escalating to the rule-based pass"
                        );
                        summary_failure = Some(SummaryOutcome::Summarized);
                        // Keep the summary as the input to the heavier pass.
                        // It cost a model call and already elides the interval
                        // it covered; running the rules over the original
                        // history would discard it and redo that work.
                        escalated_history = Some(result.messages);
                        strategy = escalate_compact_strategy(strategy);
                    }
                    Ok(_) => {
                        summary_failure = Some(SummaryOutcome::Empty);
                        tracing::warn!(
                            "CompactRecovery: LLM summary empty — falling back to rules"
                        );
                    }
                    Err(e) => {
                        summary_failure = Some(SummaryOutcome::Failed { error: e.clone() });
                        tracing::warn!(
                            "CompactRecovery: LLM summary failed ({e}) — falling back to rules"
                        );
                    }
                }
            } else {
                tracing::debug!("CompactRecovery: summarize range too small — rule-based path");
                summary_failure = Some(SummaryOutcome::RangeTooSmall {
                    messages: summary_input.len(),
                });
            }
        }

        // ── Rule-based fallback (existing strategies) ──
        // The rule-based strategies take a target. Use the resolved line for
        // this model, not the configured fallback, which is sized for a window
        // that may not be this one. The targets are provider-unit budgets and
        // the strategies count in their own units, so translate them with the
        // same ruler that selected the strategy: without this, `Snip`'s 80 000
        // target was met by a heuristic history the provider scored at 30 %
        // more, and the pass was a no-op that counted as a failure.
        let (auto_target, snip_target) =
            self.rule_based_targets(measured_tokens, estimated_tokens, prefix_tokens);
        let history = escalated_history.unwrap_or_else(|| turn.history().to_vec());
        let result: CompactResult = match strategy {
            CompactStrategy::Micro => compact::micro_compact(&history, self.micro_params()),
            CompactStrategy::Auto => compact::auto_compact(&history, auto_target),
            CompactStrategy::Reactive => compact::reactive_compact(&history, 3),
            CompactStrategy::Snip => compact::snip_compact(&history, snip_target),
            CompactStrategy::LastResort => compact::last_resort_compact(&history, snip_target),
        };

        let summary = summary_failure.unwrap_or(SummaryOutcome::NotRequested);
        self.finish_compaction(turn, result, trigger, summary).await
    }

    /// Whether a candidate result should give way to a heavier strategy.
    ///
    /// The judgement is deliberately narrower than [`Self::context_back_below_line`]:
    /// a result already under the line is a recovery even if the summary text
    /// is momentarily larger than the interval it replaced, and escalating
    /// from it would drop the summary for no benefit. The heavier pass is for
    /// a result that is still past the line and removed less than a tenth of
    /// the context.
    fn result_needs_heavier_pass(
        &self,
        turn: &TurnRuntime,
        before: &[Message],
        after: &[Message],
    ) -> bool {
        let prefix_tokens = turn.context_prefix_tokens();
        let tokens_before = PromptCompiler::estimate_message_tokens(before) + prefix_tokens;
        let tokens_after = PromptCompiler::estimate_message_tokens(after) + prefix_tokens;
        // The same translated line the progress rule uses: a result that the
        // provider would still measure over the landing line has not recovered
        // the context, however comfortable it looks to `chars / 4`.
        let line = self.heuristic_landing_line(self.turn_context_tokens(turn), tokens_before);
        let below_line = tokens_after <= line;
        let removed = tokens_before.saturating_sub(tokens_after);
        let removed_a_meaningful_share = removed > 0 && removed.saturating_mul(10) >= tokens_before;
        !(below_line || removed_a_meaningful_share)
    }

    /// Translate a token budget stated in the provider's units into the
    /// heuristic units the compaction strategies count in.
    ///
    /// The trigger and the window are provider-measured, while `Auto`, `Snip`
    /// and the strategies' target arithmetic count `chars / 4`. On JSON- and
    /// code-heavy tool traffic the provider's tokenizer can report 30–40 %
    /// more than that heuristic, so a budget copied unchanged is satisfied by
    /// a history the provider still measures over the line — and a strategy
    /// whose target the history already meets removes nothing. Scaling every
    /// budget by the measured ratio puts both on one ruler.
    ///
    /// A measurement at or below the heuristic is not evidence of a larger
    /// real context (the heuristic can overcount on prose), so the budget is
    /// left alone unless the provider's count is the larger one.
    const fn heuristic_budget(budget: usize, measured: usize, heuristic: usize) -> usize {
        if heuristic == 0 || measured <= heuristic {
            return budget;
        }
        budget.saturating_mul(heuristic) / measured
    }

    /// The landing line expressed in the heuristic units the strategies use.
    fn heuristic_landing_line(&self, measured: usize, heuristic: usize) -> usize {
        Self::heuristic_budget(self.compact_thresholds().landing(), measured, heuristic)
    }

    /// The non-history overhead of the request, in the strategies' units.
    ///
    /// Preferred over the message-level prefix when the provider has measured
    /// a request: the measurement covers the serialized tool schemas and the
    /// request scaffolding that the prefix estimate does not, and targets
    /// translated against the smaller number ask the history for room that
    /// the fixed part of the request has already spent.
    fn fixed_overhead_tokens(
        &self,
        measured: usize,
        heuristic: usize,
        prefix_tokens: usize,
    ) -> usize {
        self.measured_fixed_tokens().map_or(prefix_tokens, |fixed| {
            Self::heuristic_budget(fixed, measured, heuristic)
        })
    }

    /// The targets the rule-based strategies trim to, in the strategies' own
    /// units and in *history* space.
    ///
    /// Two translations happen here, and skipping either produces a target
    /// the pass cannot reach:
    ///
    /// 1. provider units → heuristic units (the strategies count `chars / 4`);
    /// 2. whole request → history (only the history is compactable; the
    ///    preamble and system prompt are not). A target stated for the whole
    ///    request but handed to a history-only strategy is too large by the
    ///    prefix, which is why a pass could "meet" it without removing
    ///    anything.
    ///
    /// The landing line is below the trigger line, so a successful pass buys
    /// real headroom instead of landing exactly where the next tool result
    /// crosses again. `Snip`'s configured target is capped by that line.
    fn rule_based_targets(
        &self,
        measured: usize,
        heuristic: usize,
        prefix_tokens: usize,
    ) -> (usize, usize) {
        let thresholds = self.compact_thresholds();
        let landing = thresholds.landing();
        // What the fixed part of the request costs on the strategies' ruler.
        // Measurement first: the prefix alone misses the tool schemas.
        let fixed = self.fixed_overhead_tokens(measured, heuristic, prefix_tokens);
        let history_heuristic = heuristic.saturating_sub(fixed);
        // Never ask a pass to keep less than a quarter of the history: below
        // that it is deleting working state, not reclaiming slack.
        let floor = history_heuristic / 4;

        let auto_target = Self::heuristic_budget(landing, measured, heuristic)
            .saturating_sub(fixed)
            .max(floor);
        let snip_budget = if self.compact_config.snip_target_tokens < landing {
            self.compact_config.snip_target_tokens
        } else {
            landing
        };
        let snip_target = Self::heuristic_budget(snip_budget, measured, heuristic)
            .saturating_sub(fixed)
            .max(floor);
        (auto_target, snip_target)
    }

    /// The progress rule shared by every compaction strategy.
    ///
    /// Success is "back under the compaction line, or this pass removed a
    /// meaningful share of the context" — not merely "it got smaller". A pass
    /// that removes 35 tokens of 20 000 is not success: it spends a recovery
    /// attempt, resets the circuit breaker, and lets the next turn grow past
    /// where it started.
    ///
    /// `line` is the translated landing line (`warn`): the heuristic size at
    /// which the provider would measure the request at the landing target.
    /// Landing exactly on the trigger line is not progress — the next tool
    /// result crosses it again.
    const fn context_back_below_line(
        tokens_before: usize,
        tokens_after: usize,
        line: usize,
    ) -> bool {
        let below_line = tokens_after <= line;
        let removed = tokens_before.saturating_sub(tokens_after);
        let removed_a_meaningful_share = removed > 0 && removed.saturating_mul(10) >= tokens_before;
        (below_line || removed_a_meaningful_share) && tokens_after < tokens_before
    }

    /// Shared tail of every compaction path: tokens bookkeeping, boundary
    /// event, transcript persistence, retry counters, and the PostCompact hook.
    ///
    /// The compaction-failure circuit breaker is driven here: an attempt that
    /// fails to make real progress counts as a failure and resets on progress,
    /// so `run_compact_recovery` stops retrying once the context is
    /// irrecoverable.
    ///
    /// Progress is "the context is back under the compaction line, or this
    /// pass removed a meaningful share of it" — not merely "it got smaller".
    /// A pass that removes 35 tokens of 20 000 is not success: it spends a
    /// recovery attempt, resets the circuit breaker, and lets the next turn
    /// grow past where it started. That is how a session used to burn its
    /// whole retry budget on passes that never approached the line.
    ///
    /// Only a threshold-triggered compaction spends a recovery retry, and only
    /// when it made progress. A cache-TTL pass is maintenance, and charging it
    /// to the recovery budget would let tidying up starve a real recovery
    /// later.
    async fn finish_compaction(
        &mut self,
        turn: &mut TurnRuntime,
        mut result: CompactResult,
        trigger: CompactTrigger,
        summary: SummaryOutcome,
    ) -> CompactOutcome {
        // Recompute tokens removed from the actual post-compaction messages.
        // The estimate includes the context outside the history (system
        // prompt, preamble) because the window is spent on the whole request.
        let prefix_tokens = turn.context_prefix_tokens();
        let history_tokens_before = PromptCompiler::estimate_message_tokens(turn.history());
        let tokens_after =
            PromptCompiler::estimate_message_tokens(&result.messages) + prefix_tokens;
        let tokens_before = history_tokens_before + prefix_tokens;
        result.tokens_removed = tokens_before.saturating_sub(tokens_after);

        // The provider's measurement is taken before the history is replaced,
        // while the anchor still describes it. The strategies count in
        // heuristic units, so the line they are judged against is translated
        // the same way their targets were. It is the *landing* line, not the
        // trigger: landing exactly on the trigger buys no headroom and the
        // next tool result crosses again.
        let measured_tokens = self.turn_context_tokens(turn);
        let line = self.heuristic_landing_line(measured_tokens, tokens_before);
        // The measurement is not the estimate: the provider's tokenizer is the
        // authority on how full the request is, and a run's post-mortem needs
        // both rulers. `None` when no answered request anchors the size yet.
        let measured_before = self
            .usage_anchor
            .filter(|anchor| {
                anchor.prompt_tokens > 0 && anchor.history_messages <= turn.history().len()
            })
            .map(|_| measured_tokens);
        let fixed_tokens =
            self.fixed_overhead_tokens(measured_tokens, tokens_before, prefix_tokens);

        // Circuit breaker accounting: a pass that neither reached the landing
        // line nor removed a tenth of the context counts as a failure.
        let made_progress = Self::context_back_below_line(tokens_before, tokens_after, line);
        let maintenance = trigger == CompactTrigger::CacheTtl;
        let last_resort = matches!(result.effective, CompactStrategy::LastResort);
        if !maintenance {
            // The backoff state outlives the pass and the turn: it is what
            // stops the next turn from repeating arithmetic that just failed.
            // A maintenance pass below the line says nothing about whether a
            // threshold pass can work, so it neither sets nor clears it.
            if made_progress {
                turn.compact_no_progress = 0;
                turn.compact_no_progress_tokens = 0;
            } else {
                turn.compact_no_progress = turn.compact_no_progress.saturating_add(1);
                turn.compact_no_progress_tokens = measured_tokens;
            }
            if made_progress {
                turn.compact_failures = 0;
                // The context is back where it belongs; the next crossing
                // starts from the lightest strategy the line allows.
                turn.last_compact_strategy = None;
            } else if !last_resort {
                turn.compact_failures = turn.compact_failures.saturating_add(1);
                // Keep the rung that failed so the next attempt steps down
                // the ladder instead of repeating the same arithmetic. The
                // rung is part of the turn runtime's cross-turn state: a new
                // turn must escalate too, or three turns spend the whole
                // breaker on the same ineffective strategy.
                turn.last_compact_strategy = Some(result.strategy);
            }
            // A forced last-resort pass is the breaker's own escape hatch. It
            // must not add to the count it is trying to clear; if it fails,
            // the caller decides what a still-blocked context means.
        }

        let messages_remaining = result.messages.len();
        let strategy = result.strategy;
        let effective = result.effective;
        // A pass that leaves the history exactly as it found it is not a
        // compaction: writing the same messages back would reset the loop
        // detector, drop a running summary prefetch and rewrite the transcript
        // for nothing. A changed message list counts even when the token
        // estimate is flat — the engine and the session must agree on what the
        // turn actually carries.
        let changed = result.tokens_removed > 0 || result.messages.len() != turn.history().len();
        if changed {
            // Check if compact boundary was inserted
            if result.boundary_inserted {
                // Emit compact boundary event
                let _ = self.event_tx.send(EngineEvent::CompactBoundary);
                tracing::debug!("CompactRecovery: boundary marker inserted");
            }

            // Replace only the history. The preamble, system prompt and
            // per-turn injections stay where they are, and the session — what
            // the next turn rebuilds from — gets the compacted history too.
            // Without that second write the next turn restores everything this
            // pass removed, which is why an automatic compaction used to last
            // exactly one request.
            turn.replace_history(result.messages);
            // The summary answered the messages it summarized. A pre-generation
            // still running for that interval would describe history that no
            // longer exists, and a stale failure must not block the next
            // attempt either — compaction just made a fresh one possible.
            self.pending_summary
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            self.summary_prefetch_failure
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            self.session.messages.clone_from(&turn.history().to_vec());

            // The detector's history describes turns that are no longer in the
            // context. Keeping it would let a repeat the model can no longer
            // see (because the tool result was compacted away) count toward a
            // loop.
            self.reset_loop_history();

            // Persist compact to transcript file on disk
            if let Some(ref base_path) = self.transcript_path {
                let session_id_str = self.session.session_id.to_string();
                let transcript_file = base_path.join(format!("{session_id_str}.jsonl"));

                // Back up first: compaction overwrites the transcript, and the
                // messages it drops are gone from the live file with no undo.
                match Self::write_transcript_backup(&transcript_file) {
                    Ok(Some(path)) => {
                        tracing::info!("PreCompact: transcript backed up to {}", path.display());
                    }
                    Ok(None) => { /* nothing on disk yet — nothing to lose */ }
                    Err(e) => tracing::warn!("PreCompact: could not back up transcript: {e}"),
                }

                // Write the compacted session — the same list the next turn
                // builds from — so the file and the session cannot disagree.
                if let Err(e) =
                    Self::write_transcript_replacement(&transcript_file, &self.session.messages)
                {
                    tracing::warn!("Failed to update transcript after compact: {e}");
                }

                // Append boundary marker if one was inserted
                if result.boundary_inserted
                    && let Err(e) = Self::append_transcript_boundary(&transcript_file)
                {
                    tracing::warn!("Failed to write compact boundary to transcript: {e}");
                }
            }

            // The measurement described the history that was just replaced. It
            // no longer accounts for what is there now, and pretending it does
            // would undercount a context that shrank rather than grew.
            self.usage_anchor = None;
        }

        // Spend a recovery retry only when the pass made progress, and only
        // for threshold-triggered compaction — see the note on this function.
        //
        // A pass that puts the context back under the line ends the recovery
        // episode, and the budget belongs to the episode: it bounds the passes
        // spent getting back under the line, not the number of times a long
        // session may compact. Charging every successful pass instead made a
        // headless run — one engine turn for the whole task — die after its
        // third compaction with "the compaction retry budget is spent", with
        // work still to do and thousands of tokens of room under the window.
        // The forced last-resort pass is free: it is what the exhausted budget
        // leaves available, not another paid retry.
        if trigger == CompactTrigger::TokenThreshold && made_progress && !last_resort {
            if tokens_after <= line {
                turn.recovery_count = 0;
            } else {
                turn.recovery_count += 1;
            }
        }

        // Fire PostCompact hook
        let mut ctx = std::collections::HashMap::new();
        ctx.insert(
            "tokens_removed".to_string(),
            result.tokens_removed.to_string(),
        );
        ctx.insert(
            "messages_remaining".to_string(),
            messages_remaining.to_string(),
        );
        ctx.insert("strategy".to_string(), format!("{strategy:?}"));
        ctx.insert("effective".to_string(), format!("{effective:?}"));
        ctx.insert("trigger".to_string(), format!("{trigger:?}"));
        ctx.insert("summary".to_string(), format!("{summary:?}"));
        ctx.insert("tokens_before".to_string(), tokens_before.to_string());
        ctx.insert("tokens_after".to_string(), tokens_after.to_string());
        ctx.insert("target_tokens".to_string(), line.to_string());
        ctx.insert("prefix_tokens".to_string(), prefix_tokens.to_string());
        let post_compact_result = self
            .fire_hook(hook_events::POST_COMPACT, "compact_complete", &ctx)
            .await;
        if !post_compact_result.errors.is_empty() {
            tracing::warn!("PostCompact hook errors: {:?}", post_compact_result.errors);
        }

        let outcome = if made_progress {
            CompactOutcome::Compacted {
                strategy,
                effective,
                trigger,
                tokens_removed: result.tokens_removed,
                messages_remaining,
                summary,
                tokens_before,
                tokens_after,
                target_tokens: line,
                prefix_tokens,
                measured_tokens: measured_before,
                fixed_tokens,
            }
        } else {
            CompactOutcome::NoProgress {
                strategy,
                effective,
                trigger,
                tokens_before,
                tokens_after,
                summary,
            }
        };
        tracing::debug!(
            trigger = ?trigger,
            requested = ?strategy,
            effective = ?effective,
            tokens_before,
            tokens_after,
            prefix_tokens,
            target_tokens = line,
            tokens_removed = result.tokens_removed,
            compact_failures = turn.compact_failures,
            recovery_count = turn.recovery_count,
            maintenance,
            "compaction pass"
        );
        tracing::debug!(
            "CompactRecovery: {outcome:?} ({} tokens removed, {} messages remaining)",
            result.tokens_removed,
            turn.messages.len(),
        );
        outcome
    }

    /// Copy the transcript aside before compaction overwrites it.
    ///
    /// Returns the path written, or `None` when there is no transcript on disk
    /// yet. The name is timestamped so a second compaction does not overwrite
    /// the first backup with an already-compacted transcript — the point of a
    /// backup is to keep what the *first* one had.
    fn write_transcript_backup(path: &PathBuf) -> std::io::Result<Option<PathBuf>> {
        if !path.exists() {
            return Ok(None);
        }
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("transcript.jsonl");
        let backup_path = path.with_file_name(format!("{name}.backup.{stamp}"));
        std::fs::copy(path, &backup_path)?;
        Ok(Some(backup_path))
    }

    /// Whether the prompt cache has gone cold since the last request.
    fn cache_ttl_elapsed(&self) -> bool {
        let Some(last) = self.last_request_at() else {
            return false;
        };
        compact::should_compact_on_cache_ttl(
            last.elapsed(),
            std::time::Duration::from_secs(self.compact_config.cache_ttl_secs),
            std::time::Duration::from_secs(self.compact_config.cache_ttl_early_exit_secs),
        )
    }

    /// Note that a provider request has just started: the prompt cache is
    /// fresh from this instant.
    fn note_request_started(&self) {
        *self
            .last_request_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Instant::now());
    }

    /// The instant of the last provider request, if there has been one.
    fn last_request_at(&self) -> Option<Instant> {
        *self
            .last_request_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Set the last-request instant directly (tests only).
    #[cfg(test)]
    fn set_last_request_at(&self, at: Instant) {
        *self
            .last_request_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(at);
    }

    /// Report a compaction lifecycle event to the event stream.
    fn emit_compaction(&self, event: CompactionEvent) {
        let _ = self.event_tx.send(EngineEvent::Compaction(event));
    }

    /// Start the idle cache-TTL timer on the session's first turn.
    ///
    /// Started lazily rather than in [`new`](Self::new) because spawning needs
    /// a tokio runtime, and an engine may be constructed outside one.
    fn ensure_cache_ttl_timer(&mut self) {
        if self.cache_ttl_timer.is_none() {
            self.cache_ttl_timer = self.spawn_cache_ttl_timer();
        }
    }

    /// Start the background prompt-cache TTL timer, or `None` when no cache
    /// TTL is configured.
    ///
    /// The reference schedules the cache-TTL trigger on a timer, so a session
    /// that sits idle still compacts before its next request. The engine's
    /// turn loop already checks the same condition at the turn boundary, but a
    /// session that never sends another request never reaches that check —
    /// this is the path that reaches it anyway.
    ///
    /// The engine starts it on the session's first turn (in
    /// `ensure_cache_ttl_timer`), so a session that runs one turn and then
    /// sits idle is covered.
    ///
    /// The timer cannot compact by itself: compaction rewrites the live
    /// conversation and needs `&mut self`. What it can do is *request* it, by
    /// emitting [`CompactionEvent::Requested`] on the event stream, which the
    /// session that owns the engine observes. It fires once per idle window —
    /// after a request refreshes the cache, it arms again.
    ///
    /// With `cache_ttl_secs` at zero there is no cache to keep warm and no
    /// timer is started.
    #[must_use]
    pub fn spawn_cache_ttl_timer(&self) -> Option<CacheTtlTimer> {
        let ttl = Duration::from_secs(self.compact_config.cache_ttl_secs);
        if ttl.is_zero() {
            return None;
        }
        let early_exit = Duration::from_secs(self.compact_config.cache_ttl_early_exit_secs);
        let wake_after = compact::cache_ttl_wake_after(ttl, early_exit);

        let last_request = Arc::clone(&self.last_request_at);
        let events = self.event_tx.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_task = Arc::clone(&cancel);
        let handle = tokio::spawn(async move {
            let mut last_fired: Option<Instant> = None;
            loop {
                tokio::time::sleep(wake_after).await;
                if cancel_task.load(Ordering::SeqCst) {
                    break;
                }
                let last = last_request
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .to_owned();
                if !cache_ttl_timer_should_request(last, last_fired, wake_after) {
                    continue;
                }
                last_fired = Some(Instant::now());
                let _ = events.send(EngineEvent::Compaction(CompactionEvent::Requested {
                    trigger: compact_trigger_name(CompactTrigger::CacheTtl).to_string(),
                }));
            }
        });
        Some(CacheTtlTimer { cancel, handle })
    }

    /// Start generating the next compaction's summary in the background.
    ///
    /// Compaction's visible cost is latency: the turn that crosses the line
    /// cannot send until a summarization round-trip finishes. Once the context
    /// is past the warning line, the interval the summary would cover is
    /// already known, so the work can be done while the turn continues and
    /// spent later if the interval still matches.
    ///
    /// Nothing here may fail the turn: a pre-generation error is recorded and
    /// the synchronous path runs exactly as it did before.
    fn prefetch_summary_if_useful(&self, turn: &TurnRuntime) {
        if !self.compact_config.background_summary {
            return;
        }
        if self
            .summary_prefetch_failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
        {
            return;
        }
        if self
            .pending_summary
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
        {
            return;
        }
        if self.summary_prefetch_in_flight.swap(true, Ordering::SeqCst) {
            return;
        }

        let history = turn.history().to_vec();
        let window = self.context_window().tokens;
        let summary_budget = self.compact_config.resolve_summary_input_budget(window);
        let tail_tokens = self.compact_config.resolve_summary_tail_tokens(window);
        let split = compact::summary_split_bounded(
            &history,
            3,
            self.compact_config.keep_recent_messages,
            tail_tokens,
        );
        let input = compact::build_summary_input(&history, &split, summary_budget);
        if input.len() < 2 {
            // Too little between the preserved head and tail to summarize;
            // the compaction would fall through to the rule-based path, so
            // there is nothing to compute early.
            self.summary_prefetch_in_flight
                .store(false, Ordering::SeqCst);
            return;
        }

        let slot = Arc::clone(&self.pending_summary);
        let failure = Arc::clone(&self.summary_prefetch_failure);
        let in_flight = Arc::clone(&self.summary_prefetch_in_flight);
        let client = self.api_client.clone_box();
        let backend = self.compaction_backend.clone();
        let max_output = self.compact_config.summary_output_max_tokens;
        let summarize_from = split.summarize_from;
        let summarize_len = split.summarize_to.saturating_sub(split.summarize_from);

        // The task holds its own client, backend and budget: the engine keeps
        // running turns meanwhile, and the summary belongs to the interval it
        // was computed from, not to whatever the context is by the time it
        // arrives.
        tokio::spawn(async move {
            let result = summarize_conversation(
                client.as_ref(),
                backend.as_ref(),
                &input.join("\n\n"),
                max_output,
            )
            .await;
            let mut slot = slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match result {
                Ok(summary) if summary.trim().is_empty() => {
                    tracing::debug!("background summary came back empty; ignoring it");
                }
                Ok(summary) => {
                    tracing::debug!("background summary ready for the next compaction");
                    *slot = Some(compact::SummaryDigest::new(
                        summarize_from,
                        summarize_len,
                        summary,
                    ));
                }
                Err(error) => {
                    tracing::debug!(
                        "background summary failed ({error}); compaction will retry it"
                    );
                    *failure
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(error);
                }
            }
            in_flight.store(false, Ordering::SeqCst);
        });
    }

    /// A pre-generated summary, if one describes exactly this interval.
    ///
    /// The returned split is the digest's own, not the one just computed: the
    /// digest never read the messages past its end, so they must be kept
    /// verbatim rather than elided.
    fn take_matching_summary(
        &self,
        history: &[Message],
        split: &compact::SummarySplit,
    ) -> Option<(String, compact::SummarySplit)> {
        let mut slot = self
            .pending_summary
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let digest = slot.take()?;
        if digest.covers(history, split) {
            Some((digest.summary.clone(), digest.split()))
        } else {
            // The interval moved on: what was computed is not a summary of
            // these messages, and using it would elide something it never saw.
            // Dropping it also lets the next warning line start a fresh one.
            tracing::debug!("the pre-generated summary no longer matches; discarding it");
            None
        }
    }

    /// Where a context that has stopped yielding to compaction becomes worth
    /// another attempt.
    ///
    /// `None` while no pass has failed. Once one has, the next attempt has to
    /// wait for measured growth worth a twentieth of the compaction line (at
    /// least 2 000 tokens): a context that has barely moved would meet the
    /// same target with the same result, and repeating that is the failure
    /// mode this guards against. The blocked line bypasses the wait in the
    /// caller — there a request genuinely cannot be sent.
    fn no_progress_retry_at(
        turn: &TurnRuntime,
        thresholds: &compact::CompactThresholds,
    ) -> Option<usize> {
        (turn.compact_no_progress > 0).then(|| {
            turn.compact_no_progress_tokens
                .saturating_add((thresholds.auto / 20).max(2_000))
        })
    }

    /// Compact before a request when the estimated context has crossed a line.
    ///
    /// `CompactThresholds` answers "how full is the window"; until this call
    /// existed, only the output-token budget could trigger compaction, so a
    /// large input could overflow a small-window model before anything ran.
    ///
    /// Returns `Some(reason)` only when the context is still blocked after a
    /// compaction attempt; at the compact line the turn continues.
    async fn maybe_compact_on_threshold(
        &mut self,
        turn: &mut TurnRuntime,
        options: &SubmitOptions,
        turn_number: u32,
    ) -> Option<TerminalReason> {
        let estimated = self.turn_context_tokens(turn);
        let thresholds = self.compact_thresholds();
        let level = thresholds.level(estimated);

        match level {
            compact::ContextLevel::Ok => return None,
            compact::ContextLevel::Warn => {
                tracing::debug!(
                    estimated_tokens = estimated,
                    warn_at = thresholds.warn,
                    window_source = %thresholds.window_source,
                    "context is approaching the compaction line"
                );
                // The warning is the only early signal before compaction is
                // unavoidable, and it is exactly the moment a summary can be
                // made in the background: its interval is known and only
                // grows, and paying the round-trip now takes it off the
                // critical path of the turn that later has to compact.
                self.prefetch_summary_if_useful(turn);
                return None;
            }
            compact::ContextLevel::Compact => {
                // A pass that removed nothing will remove nothing again at
                // the same size: the arithmetic that set its target has not
                // changed. Retrying it every turn is how a two-hour run spent
                // 241 passes on compaction instead of work. Only a context
                // that has really moved — or one that is now blocked and
                // cannot be sent at all — is worth another attempt.
                if let Some(retry_at) = Self::no_progress_retry_at(turn, &thresholds)
                    && estimated < retry_at
                {
                    let outcome = CompactOutcome::BackedOff {
                        consecutive_no_progress: turn.compact_no_progress,
                        estimated_tokens: estimated,
                        last_tokens: turn.compact_no_progress_tokens,
                        retry_at,
                    };
                    tracing::debug!(
                        estimated_tokens = estimated,
                        last_tokens = turn.compact_no_progress_tokens,
                        retry_at,
                        consecutive_no_progress = turn.compact_no_progress,
                        "a recent compaction removed nothing; not retrying at this size"
                    );
                    self.emit_compaction(compaction_event_from_outcome(&outcome));
                    return None;
                }
            }
            compact::ContextLevel::Blocked => {}
        }

        tracing::debug!(
            estimated_tokens = estimated,
            auto = thresholds.auto,
            blocked = thresholds.blocked,
            window_source = %thresholds.window_source,
            level = ?level,
            "context crossed the compaction line"
        );

        // A spent budget or an open breaker does not end the attempt here. It
        // selects the last-resort pass inside `run_compact_recovery`: the
        // named strategies may have stopped working, but a local snip plus
        // truncation of the largest remaining tool results costs nothing and
        // may still clear the line. Refusing to try turned a context 357
        // tokens over the blocked line into a failed two-hour run.

        // This is compaction, so the turn is in `CompactRecovery` while it
        // runs. Entering the state here does two things the pre-request path
        // needs: it records what actually happened, and it leaves the turn in
        // the state from which either the retry's `StartRequest` or the
        // closing path's `StopHooks` is a legal move. The cache-TTL pass may
        // already have entered it, hence the check.
        if !matches!(turn.state, TurnState::CompactRecovery) {
            turn.transition_to(TurnState::CompactRecovery);
        }
        let outcome = self
            .run_compact_recovery(turn, options, turn_number, CompactTrigger::TokenThreshold)
            .await;
        if let Some(reason) = outcome.failure_message() {
            if outcome.is_circuit_breaker_open()
                && thresholds.level(self.turn_context_tokens(turn))
                    != compact::ContextLevel::Blocked
            {
                tracing::warn!(
                    "compaction circuit breaker open; continuing with the context as-is"
                );
                return None;
            }
            return Some(TerminalReason::Error(reason));
        }

        // Re-check at the compaction line too, not only at the blocked one: a
        // pass that left the context above the line means the next turn starts
        // from a worse place, and the retry budget is finite. Only the blocked
        // line is fatal — above the compaction line the request can still be
        // sent, so the turn continues and the next attempt escalates.
        let after = self.turn_context_tokens(turn);
        if thresholds.level(after) == compact::ContextLevel::Blocked {
            return Some(TerminalReason::Error(format!(
                "context window is still full after compaction ({after} estimated tokens, \
                 blocked line {} from a {} window)",
                thresholds.blocked, thresholds.window_source,
            )));
        }

        None
    }

    /// The estimated size of the request this turn would send.
    ///
    /// The whole request counts against the window — preamble, system prompt,
    /// history and per-turn injections — not just the history. Measuring only
    /// the compactable part made the numbers look comfortable while the
    /// request was already at the limit. Once the provider has reported a
    /// usage count, [`TurnRuntime::context_tokens`] anchors on it instead of
    /// the character heuristic.
    fn turn_context_tokens(&self, turn: &TurnRuntime) -> usize {
        turn.context_tokens(self.usage_anchor)
    }

    /// Capture the shape of the request about to be sent.
    ///
    /// Paired with [`UsageAnchor::measured`] once the provider answers. The
    /// prompt is assembled before the request, so this has to run there, not
    /// when the response arrives — by then the response messages are already
    /// in the list, and the measurement would be anchored past them.
    fn record_request_shape(&mut self, turn: &TurnRuntime) {
        self.usage_anchor = Some(UsageAnchor {
            history_messages: turn.history().len(),
            prefix_tokens: turn.context_prefix_tokens(),
            prompt_tokens: 0,
            fixed_tokens: 0,
        });
    }

    /// The provider-measured size of everything compaction cannot touch.
    ///
    /// `None` until the provider has answered a request: the message-level
    /// prefix is a heuristic, and an unmeasured overhead is better left to
    /// `rule_based_targets`' fallback than presented as a measurement.
    fn measured_fixed_tokens(&self) -> Option<usize> {
        self.usage_anchor
            .filter(|anchor| anchor.prompt_tokens > 0 && anchor.fixed_tokens > 0)
            .map(|anchor| anchor.fixed_tokens)
    }

    /// Anchor the estimate on a provider's measurement of the last request.
    ///
    /// Two measurements teach nothing and are ignored, so neither can drag the
    /// estimate to an unreachable number: a zero count (a provider that
    /// reports no usage), and a count smaller than the prompt's own prefix —
    /// a request that carries a multi-thousand-token system prompt cannot have
    /// been 100 tokens, so that report is not about the whole request.
    fn record_prompt_tokens(&mut self, turn: &TurnRuntime, usage: &UsageSummary) {
        let prompt_tokens = usage.prompt_tokens();
        if prompt_tokens == 0 {
            return;
        }
        let prefix_tokens = turn.context_prefix_tokens();
        // With no history range (an embedder that assembled its own list)
        // there is no separately counted prefix; the system prompt is still a
        // floor no honest count can be below.
        let floor = if prefix_tokens > 0 {
            prefix_tokens
        } else {
            PromptCompiler::estimate_message_tokens(
                &turn
                    .messages
                    .iter()
                    .filter(|m| matches!(m.role, crate::types::Role::System))
                    .cloned()
                    .collect::<Vec<_>>(),
            )
        };
        if prompt_tokens < floor {
            tracing::debug!(
                prompt_tokens,
                floor,
                "provider usage is smaller than the system prompt; not anchoring on it"
            );
            return;
        }
        // What the provider charged beyond the history is the only measurement
        // of the request's fixed cost there is: the system prompt, the
        // per-turn injections and the serialized tools together. Targets must
        // leave this much of the budget alone, or they ask a history-only
        // strategy for room that does not exist.
        let (history_messages, fixed_tokens) = match self.usage_anchor {
            // The anchor's shape describes the request that was measured; the
            // history may have grown with the response since then.
            Some(anchor) if anchor.history_messages <= turn.history().len() => {
                let measured_history = PromptCompiler::estimate_message_tokens(
                    &turn.history()[..anchor.history_messages],
                );
                (
                    anchor.history_messages,
                    prompt_tokens.saturating_sub(measured_history),
                )
            }
            _ => {
                let measured_history = PromptCompiler::estimate_message_tokens(turn.history());
                (
                    turn.history().len(),
                    prompt_tokens.saturating_sub(measured_history),
                )
            }
        };
        self.usage_anchor = Some(UsageAnchor {
            history_messages,
            prefix_tokens,
            prompt_tokens,
            fixed_tokens,
        });

        // The request was accepted, which is a statement about the window:
        // input plus the requested output fit inside it. Unknown models
        // otherwise run against [`UNKNOWN_CONTEXT_WINDOW`], a guess that can
        // be four times too small.
        let lower_bound = prompt_tokens.saturating_add(self.max_tokens as usize);
        self.raise_context_window_lower_bound(lower_bound);
    }

    /// Compact proactively when the prompt cache has gone cold.
    ///
    /// Every request refreshes the cache, so a session that sat idle past the
    /// TTL pays a cache miss on its next request whatever happens — which is
    /// the cheapest moment to rewrite the context. This runs *before* the
    /// request, and only while the context is below the token line: at or
    /// above it the threshold path owns compaction, and running both would
    /// compact twice for one reason.
    ///
    /// The reference implementation schedules this on a timer, as does
    /// [`spawn_cache_ttl_timer`](Self::spawn_cache_ttl_timer); this is the
    /// turn-boundary check that catches the case where the timer is not
    /// running, and it is what actually compacts when the next turn arrives.
    ///
    /// On the compacting path the turn is left in `CompactRecovery` — the
    /// caller's next move to `StartRequest` is the one the graph already
    /// allows from there.
    async fn maybe_compact_on_cache_ttl(
        &mut self,
        turn: &mut TurnRuntime,
        options: &SubmitOptions,
        turn_number: u32,
    ) -> Option<CompactOutcome> {
        if !self.cache_ttl_elapsed() {
            return None;
        }

        let estimated_tokens = self.turn_context_tokens(turn);
        let thresholds = self.compact_thresholds();
        if matches!(
            thresholds.level(estimated_tokens),
            compact::ContextLevel::Compact | compact::ContextLevel::Blocked
        ) {
            let outcome = CompactOutcome::DeferredToThresholdPath { estimated_tokens };
            self.emit_compaction(compaction_event_from_outcome(&outcome));
            return Some(outcome);
        }

        if turn.recovery_count >= self.compact_config.max_recovery_retries {
            let outcome = CompactOutcome::SkippedRecoveryBudget {
                recovery_count: turn.recovery_count,
                max: self.compact_config.max_recovery_retries,
            };
            self.emit_compaction(compaction_event_from_outcome(&outcome));
            return Some(outcome);
        }

        turn.transition_to(TurnState::CompactRecovery);
        let outcome = self
            .run_compact_recovery(turn, options, turn_number, CompactTrigger::CacheTtl)
            .await;
        // The caller moves on to StartRequest; the graph allows it from here.
        Some(outcome)
    }

    /// Write a full replacement to the transcript file with the given messages.
    ///
    /// Uses a temp file + atomic rename for crash safety.
    fn write_transcript_replacement(path: &PathBuf, messages: &[Message]) -> std::io::Result<()> {
        use std::io::Write;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let temp_path = path.with_extension("jsonl.tmp");
        let mut file = std::fs::File::create(&temp_path)?;
        for msg in messages {
            let line = serde_json::to_string(msg).map_err(std::io::Error::other)?;
            writeln!(file, "{line}")?;
        }
        std::fs::rename(&temp_path, path)
    }

    /// Append a compact boundary marker to the transcript file.
    fn append_transcript_boundary(path: &PathBuf) -> std::io::Result<()> {
        use std::io::Write;

        let boundary = compact::compact_boundary_message();
        let line = serde_json::to_string(&boundary).map_err(std::io::Error::other)?;

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        writeln!(file, "{line}")
    }

    /// Format denied tool calls into a system prompt summary.
    fn format_denied_tools_summary(&self) -> String {
        use std::fmt::Write;
        let mut summary =
            String::from("The following tool calls from the previous turn were denied:\n");
        for denied in &self.denied_tools {
            let _ = writeln!(summary, "- `{}`: {}", denied.name, denied.reason);
        }
        summary.push_str("Please do not attempt these tool calls again in the same way. Adjust your approach accordingly.");
        summary
    }

    /// Send the request to the API.
    async fn send_request(
        &mut self,
        turn: &TurnRuntime,
        options: &SubmitOptions,
    ) -> Result<ApiResponse, TurnError> {
        let system = turn
            .messages
            .iter()
            .filter(|m| matches!(m.role, crate::types::Role::System))
            .filter_map(|m| match &m.content {
                crate::types::MessageContent::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n\n");

        let messages = Self::provider_messages(&turn.messages);

        let tools = self.tools.list_tools();
        let request_client = self
            .api_client
            .with_request_context(self.build_request_context(
                self.current_turn.load(std::sync::atomic::Ordering::SeqCst),
                options,
            ));
        // Mark the request time: a successful response refreshes the provider's
        // prompt cache, and the TTL trigger measures idleness from here.
        self.note_request_started();
        // Record what this request covers before it is sent: the anchor pairs
        // with the provider's measurement of exactly these messages.
        self.record_request_shape(turn);
        let response = match request_client
            .create_message(&system, &messages, &self.model, self.max_tokens, &tools)
            .await
        {
            Ok(resp) => resp,
            Err(error) => {
                let Some(fallback) = self.fallback_for_api_error(&error) else {
                    return Err(self.turn_error_from_api_error(error).await);
                };
                // One retry of the same request. Switching the engine's model
                // means this turn and every later request stay on the fallback.
                self.switch_to_fallback_model(fallback);
                match request_client
                    .create_message(&system, &messages, &self.model, self.max_tokens, &tools)
                    .await
                {
                    Ok(resp) => resp,
                    Err(error) => return Err(self.turn_error_from_api_error(error).await),
                }
            }
        };

        self.record_prompt_tokens(turn, &response.usage);

        // Cache successful response for graceful degradation
        self.last_successful_response = Some((response.content.clone(), response.usage));

        // Emit text delta event
        let _ = self
            .event_tx
            .send(EngineEvent::TextDelta(response.content.clone()));

        Ok(response)
    }

    /// Send the request using streaming API.
    ///
    /// Returns the accumulated text content, deferred tool calls (for batch
    /// execution after the stream), early-executed tool results, and usage.
    async fn send_request_streaming(
        &mut self,
        turn: &mut TurnRuntime,
        options: &SubmitOptions,
    ) -> Result<StreamedResponse, TurnError> {
        use futures::StreamExt;
        use std::collections::HashSet;

        /// Maximum total attempts for a streaming request:
        /// 1 initial + `MAX_STREAM_ATTEMPTS - 1` auto-retries.
        const MAX_STREAM_ATTEMPTS: u32 = 3;

        let system = turn
            .messages
            .iter()
            .filter(|m| matches!(m.role, crate::types::Role::System))
            .filter_map(|m| match &m.content {
                crate::types::MessageContent::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n\n");

        let messages = Self::provider_messages(&turn.messages);
        let tools = self.tools.list_tools();

        // Text survives a retry. A stream cannot be resumed at an offset —
        // the retry re-requests the turn — but what the first attempt already
        // delivered is kept and merged with what the retry produces, so an
        // interruption costs the reader nothing that had already arrived.
        // `StreamTextMerger` emits only the growth, once per delta.
        let mut stream_text = StreamTextMerger::new();

        // The stream's usage report describes the request below, so the anchor
        // has to be taken before the first byte arrives.
        self.record_request_shape(turn);

        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            // A retry regenerates the answer from the top; start comparing
            // again from the first character.
            stream_text.begin_attempt();
            let request_client = self
                .api_client
                .with_request_context(self.build_request_context(
                    self.current_turn.load(std::sync::atomic::Ordering::SeqCst),
                    options,
                ));
            let mut stream = request_client
                .create_message_streaming(&system, &messages, &self.model, self.max_tokens, &tools)
                .await;
            self.note_request_started();

            // Per-attempt state. Text is merged into `stream_text` as it
            // arrives; tool calls are the attempt's own — a retry re-asks the
            // model, and executing a call twice because two attempts both
            // proposed it would double its side effects.
            let mut assembler = StreamingToolCallAssembler::new();
            let mut usage = UsageSummary {
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            };
            let mut early_calls: Vec<ToolCall> = Vec::new();
            let mut early_results: Vec<ToolResult> = Vec::new();
            let mut deferred_calls: Vec<ToolCall> = Vec::new();
            let mut stream_failed = false;
            // The message is what turns a failed stream into a reported error
            // instead of an empty, apparently successful turn.
            let mut stream_error: Option<String> = None;
            let mut stop_reason: Option<String> = None;
            let mut retry_requested = false;

            while let Some(event) = stream.next().await {
                // Stop reading a stream the person has asked to abandon: the
                // rest of it is tokens nobody is waiting for.
                if self.interrupt.is_interrupted() {
                    stream_failed = true;
                    break;
                }
                match event.clone() {
                    StreamEvent::TextDelta(ref delta) => {
                        if let Some(new_text) = stream_text.push_delta(delta) {
                            let _ = self.event_tx.send(EngineEvent::TextDelta(new_text));
                        }
                    }
                    // Tool-call blocks are assembled elsewhere: the start is
                    // ignored and completion is handled below via
                    // assembler.process_event. Pending tool calls are
                    // finalized after the loop.
                    // Thinking deltas are accumulated by the assembler below.
                    StreamEvent::ToolCallBlockStart { .. }
                    | StreamEvent::ToolCallBlockEnd { .. }
                    | StreamEvent::StreamEnd
                    | StreamEvent::ThinkingDelta(_)
                    | StreamEvent::SignatureDelta(_) => {}
                    StreamEvent::ToolCallInputDelta { ref id, ref delta } => {
                        tracing::trace!("tool call input delta for {id}: {} bytes", delta.len());
                    }
                    StreamEvent::UsageSummary(ref u) => {
                        usage = *u;
                    }
                    StreamEvent::StopReason(ref r) => {
                        stop_reason = Some(r.clone());
                    }
                    StreamEvent::Error(ref e) => {
                        // A retry re-asks the model and re-runs what it
                        // proposes. If this attempt already executed a tool,
                        // retrying can repeat its side effects, so the partial
                        // result is returned instead.
                        //
                        // Model unavailability gets one dedicated retry with
                        // the fallback model, independent of the transient
                        // retry budget below. Switching the engine's model
                        // makes a second fallback attempt impossible.
                        if early_calls.is_empty()
                            && let Some(fallback) = self.fallback_for_api_error(e)
                        {
                            self.switch_to_fallback_model(fallback);
                            let _ = self.event_tx.send(EngineEvent::StreamInterrupted {
                                reason: "error".into(),
                                partial_len: stream_text.delivered().len(),
                                will_retry: true,
                            });
                            retry_requested = true;
                            break;
                        }
                        if early_calls.is_empty()
                            && attempt < MAX_STREAM_ATTEMPTS
                            && crate::api_client::is_retryable(e)
                        {
                            tracing::warn!(
                                "streaming network error on attempt {attempt} ({} bytes received): {e} — re-requesting",
                                stream_text.delivered().len()
                            );
                            let _ = self.event_tx.send(EngineEvent::StreamInterrupted {
                                reason: "error".into(),
                                partial_len: stream_text.delivered().len(),
                                will_retry: true,
                            });
                            tokio::time::sleep(std::time::Duration::from_millis(
                                500u64 << (attempt - 1),
                            ))
                            .await;
                            retry_requested = true;
                            // Text carries into the retry; the attempt's tool
                            // calls do not (see the note above the loop).
                            break;
                        }
                        tracing::warn!(
                            "streaming network error ({} bytes received so far): {e}",
                            stream_text.delivered().len()
                        );
                        stream_error = Some(e.to_string());
                        stream_failed = true;
                        // Fail-open: stop consuming the stream but return
                        // whatever partial text and tool calls we have.
                        break;
                    }
                }

                // Process event through assembler and, when
                // `streaming_tool_execution` is on, execute completed tool
                // calls immediately while the stream is still running. With
                // it off the assembler still records the calls; they flow
                // through `take_completed` below and execute once the stream
                // has finished.
                for call in assembler.process_event(event) {
                    if !self.streaming_tool_execution {
                        continue;
                    }
                    let recorded = call.clone();
                    match self.execute_early_tool_call(turn, call, options).await {
                        Ok(Some(result)) => {
                            early_calls.push(recorded);
                            early_results.push(result);
                        }
                        Ok(None) => { /* deferred — call already tracked */ }
                        Err(e) => {
                            // An error here is not a reason to drop the call:
                            // the model still has to see a result for the
                            // tool_use, or the history is an invalid pair.
                            tracing::warn!("early tool execution failed: {e}");
                            let id = recorded.id.clone();
                            early_calls.push(recorded);
                            early_results.push(ToolResult {
                                tool_call_id: id,
                                output: format!("tool execution failed: {e}"),
                                is_error: true,
                                images: Vec::new(),
                            });
                        }
                    }
                }
            }

            if !retry_requested {
                // Stream ended — finalize any remaining pending calls. The
                // assembler must see `StreamEnd` either way; whether the
                // finalized calls execute now or after the stream is what
                // `streaming_tool_execution` decides.
                let finalized = assembler.process_event(StreamEvent::StreamEnd);
                for call in finalized {
                    if !self.streaming_tool_execution {
                        continue;
                    }
                    let recorded = call.clone();
                    match self.execute_early_tool_call(turn, call, options).await {
                        Ok(Some(result)) => {
                            early_calls.push(recorded);
                            early_results.push(result);
                        }
                        Ok(None) => { /* deferred */ }
                        Err(e) => {
                            tracing::warn!("early tool execution failed: {e}");
                            let id = recorded.id.clone();
                            early_calls.push(recorded);
                            early_results.push(ToolResult {
                                tool_call_id: id,
                                output: format!("tool execution failed: {e}"),
                                is_error: true,
                                images: Vec::new(),
                            });
                        }
                    }
                }
            }

            // A retryable mid-stream error with retry attempts left:
            // restart the request from scratch.
            if retry_requested {
                continue;
            }

            self.record_prompt_tokens(turn, &usage);

            // Partition completed calls: early-executed vs deferred.
            let early_ids: HashSet<String> = early_results
                .iter()
                .map(|r| r.tool_call_id.clone())
                .collect();
            let all_completed = assembler.take_completed();
            for call in all_completed {
                if !early_ids.contains(&call.id) {
                    deferred_calls.push(call);
                }
            }

            if stream_failed
                && stream_text.delivered().is_empty()
                && early_results.is_empty()
                && deferred_calls.is_empty()
            {
                tracing::info!(
                    "streaming request yielded no usable events; retrying same turn non-streaming"
                );
                if let Ok(resp) = self.send_request(turn, options).await {
                    // The fallback keeps the response's own stop reason: a
                    // truncated answer is still truncated when it arrives this
                    // way.
                    return Ok(StreamedResponse {
                        content: resp.content,
                        tool_calls: resp.tool_calls,
                        early_calls: Vec::new(),
                        early_results: Vec::new(),
                        usage: resp.usage,
                        stop_reason: resp.stop_reason,
                        thinking: resp.thinking,
                    });
                }
            }

            // An interrupt is not a failure: nothing is wrong, the person
            // asked for this to stop, and saying "error" would send whoever
            // reads the event looking for a fault.
            if self.interrupt.is_interrupted() {
                let _ = self.event_tx.send(EngineEvent::StreamInterrupted {
                    reason: "user".into(),
                    partial_len: stream_text.delivered().len(),
                    will_retry: false,
                });
                // Whatever arrived before the interrupt is kept: a partial
                // answer is what the person asked to stop at, and throwing it
                // away would lose the tokens they already paid for.
                return Ok(StreamedResponse {
                    content: stream_text.into_delivered(),
                    tool_calls: deferred_calls,
                    early_calls,
                    early_results,
                    usage,
                    stop_reason: stop_reason.unwrap_or_else(|| "interrupted".to_string()),
                    thinking: assembler.thinking(),
                });
            }

            if stream_failed {
                let _ = self.event_tx.send(EngineEvent::StreamInterrupted {
                    reason: "error".into(),
                    partial_len: stream_text.delivered().len(),
                    will_retry: false,
                });
            } else if matches!(stop_reason.as_deref(), Some("max_tokens" | "length")) {
                // Legitimate provider-completed truncation: not an error and
                // not retryable (re-requesting would loop and double cost).
                let _ = self.event_tx.send(EngineEvent::StreamInterrupted {
                    reason: stop_reason.clone().unwrap_or_default(),
                    partial_len: stream_text.delivered().len(),
                    will_retry: false,
                });
            }

            let content = stream_text.into_delivered();
            // A failed stream that produced nothing usable is an error, not an
            // empty "end_turn". Reporting it as a finished turn let the caller
            // start another request, which is why a non-retryable HTTP 400
            // looked like a hang instead of failing the turn.
            if stream_failed
                && content.is_empty()
                && deferred_calls.is_empty()
                && early_results.is_empty()
            {
                let message =
                    stream_error.unwrap_or_else(|| "the provider returned no response".to_string());
                // A stream that failed because the prompt does not fit is not
                // a malformed request: the provider stated its limit, and the
                // turn can retry after compacting.
                if let Some(overflow) = crate::context_overflow::detect(&message) {
                    return Err(TurnError::ContextOverflow {
                        message,
                        actual: overflow.actual,
                        limit: overflow.limit,
                    });
                }
                return Err(TurnError::Fatal { message });
            }

            // A clean stream can still carry no answer: a provider that ends
            // after a thinking block, or a gateway that closes the body early,
            // produces neither text nor a tool call. That used to fall through
            // as `end_turn`, and because the thinking block is stored as an
            // assistant message the turn loop saw "the last message is not a
            // tool result" and broke with `Completed` — a headless run that
            // wrote nothing exited 0 with an empty result (gpt2-codegolf in
            // terminal-bench). An empty answer is not a completion: ask once
            // more, then fail the turn.
            //
            // A truncated (`max_tokens`) empty answer used to be excluded from
            // this, on the grounds that retrying would spend the same budget
            // again. That is right when the ceiling cannot move — but a
            // reasoning model that spent 32 768 tokens thinking and emitted
            // nothing is exactly the case where a larger ceiling is the fix,
            // and where reporting "your answer was cut off" describes an
            // answer that never existed. So the distinction is not
            // truncated-or-not but *whether there is anywhere to go*:
            // [`retry_ceiling`] offers a bigger ceiling only when the request
            // can carry one, and a turn with no room left is reported as the
            // no-op it was (see `TokenLimitStop { produced: false }`).
            let empty_answer = content.trim().is_empty()
                && deferred_calls.is_empty()
                && early_calls.is_empty()
                && early_results.is_empty();
            if empty_answer {
                let truncated = matches!(stop_reason.as_deref(), Some("max_tokens" | "length"));
                // Only a truncated turn has something to gain from a larger
                // ceiling; an `end_turn` that produced nothing is the
                // provider's answer, not a limit.
                let retry = truncated
                    .then(|| {
                        retry_ceiling(
                            self.max_tokens,
                            self.turn_context_tokens(turn),
                            self.context_window(),
                        )
                    })
                    .flatten()
                    .filter(|_| turn.empty_answer_retries < MAX_EMPTY_ANSWER_RETRIES);
                if let Some(ceiling) = retry {
                    turn.empty_answer_retries += 1;
                    let previous = self.max_tokens;
                    tracing::warn!(
                        previous,
                        ceiling,
                        "the turn produced nothing and was cut off at {previous}; re-asking with {ceiling}"
                    );
                    self.max_tokens = ceiling;
                    let retried = self.send_request(turn, options).await;
                    // The raised ceiling is a retry, not a decision: leaving it
                    // in place would make the next request carry a number
                    // chosen for a failure that has already been resolved.
                    self.max_tokens = previous;
                    let retried = retried?;
                    if produced_output(&retried.content, retried.tool_calls.len()) {
                        return Ok(StreamedResponse {
                            content: retried.content,
                            tool_calls: retried.tool_calls,
                            early_calls: Vec::new(),
                            early_results: Vec::new(),
                            usage: retried.usage,
                            stop_reason: retried.stop_reason,
                            thinking: retried.thinking,
                        });
                    }
                    // Still nothing, at a ceiling this request could not have
                    // carried a second time. Report what actually happened
                    // rather than the generic empty-response error: the
                    // provider answered, and the budget is where it went.
                    //
                    // The stop reason stays the original one when there is one.
                    // A provider that answers `end_turn` with no content would
                    // otherwise report `Completed`, which is the empty-answer
                    // exit this whole branch exists to prevent.
                    let stop_reason = stop_reason.unwrap_or(retried.stop_reason);
                    return Ok(StreamedResponse {
                        content: retried.content,
                        tool_calls: retried.tool_calls,
                        early_calls: Vec::new(),
                        early_results: Vec::new(),
                        usage: retried.usage,
                        stop_reason,
                        thinking: retried.thinking,
                    });
                }
                if truncated {
                    // No room for a larger ceiling: end the turn as the no-op
                    // it was. The response is returned unchanged so the caller
                    // reports `TokenLimitStop { produced: false }` and the host
                    // can tell this apart from a truncated answer.
                    return Ok(StreamedResponse {
                        content,
                        tool_calls: deferred_calls,
                        early_calls,
                        early_results,
                        usage,
                        stop_reason: stop_reason.unwrap_or_else(|| "end_turn".to_string()),
                        thinking: assembler.thinking(),
                    });
                }
                tracing::warn!(
                    "provider ended the turn with no content and no tool call; retrying non-streaming"
                );
                let retried = self.send_request(turn, options).await?;
                if produced_output(&retried.content, retried.tool_calls.len()) {
                    return Ok(StreamedResponse {
                        content: retried.content,
                        tool_calls: retried.tool_calls,
                        early_calls: Vec::new(),
                        early_results: Vec::new(),
                        usage: retried.usage,
                        stop_reason: retried.stop_reason,
                        thinking: retried.thinking,
                    });
                }
                return Err(TurnError::Fatal {
                    message: "the provider returned an empty response".to_string(),
                });
            }

            return Ok(StreamedResponse {
                content,
                tool_calls: deferred_calls,
                early_calls,
                early_results,
                usage,
                stop_reason: stop_reason.unwrap_or_else(|| "end_turn".to_string()),
                thinking: assembler.thinking(),
            });
        }
    }

    /// Execute a single tool call immediately during streaming.
    ///
    /// Runs the full validation pipeline (allowed_tools_override, PreToolUse
    /// hook, PermissionEngine) and executes the tool if allowed.
    ///
    /// - `Ok(Some(result))` — the tool was executed, denied, or resolved after
    ///   user interaction.
    /// - `Ok(None)` — reserved for a deferred call with no immediate result.
    async fn execute_early_tool_call(
        &mut self,
        turn: &mut TurnRuntime,
        call: ToolCall,
        options: &SubmitOptions,
    ) -> Result<Option<ToolResult>, String> {
        // 1. allowed_tools_override
        if let Some(ref allowed) = options.allowed_tools_override
            && !allowed.iter().any(|t| t.eq_ignore_ascii_case(&call.name))
        {
            self.denied_tools.push(DeniedToolCall {
                name: call.name.clone(),
                reason: "tool is not in the allowed tools override".to_string(),
            });
            let result = ToolResult {
                tool_call_id: call.id.clone(),
                output: format!(
                    "tool '{}' blocked: not in allowed tools override",
                    call.name
                ),
                is_error: true,
                images: Vec::new(),
            };
            let _ = self.event_tx.send(EngineEvent::ToolCallEnd {
                id: call.id.clone(),
                result: result.clone(),
            });
            return Ok(Some(result));
        }

        // 2. PreToolUse hook
        let mut ctx = std::collections::HashMap::new();
        ctx.insert("tool_call_id".to_string(), call.id.clone());
        ctx.insert("tool_input".to_string(), call.input.to_string());
        let pre_hook = self
            .fire_hook(hook_events::PRE_TOOL_USE, &call.name, &ctx)
            .await;
        if pre_hook.stop {
            self.denied_tools.push(DeniedToolCall {
                name: call.name.clone(),
                reason: "pre-use hook stopped execution".to_string(),
            });
            let result = ToolResult {
                tool_call_id: call.id.clone(),
                output: "tool execution stopped by hook".to_string(),
                is_error: true,
                images: Vec::new(),
            };
            let _ = self.event_tx.send(EngineEvent::ToolCallEnd {
                id: call.id.clone(),
                result: result.clone(),
            });
            return Ok(Some(result));
        }

        // `tool_use_summaries` asks the lightweight model for a one-line
        // label. Failure (including no usable client) falls back to the
        // deterministic activity summary, so a turn never fails over a label.
        let summary = self.tool_use_summary(&call.name, &call.input).await;
        if self.tool_use_summaries {
            let _ = self.event_tx.send(EngineEvent::ToolUseSummary {
                id: call.id.clone(),
                name: call.name.clone(),
                summary: summary.clone(),
            });
        }
        let _ = self.event_tx.send(EngineEvent::ToolCallStart {
            id: call.id.clone(),
            name: call.name.clone(),
            summary,
        });

        // 3. Permission check
        let subjects = self.tools.rule_subjects(&call.name, &call.input);
        self.tools.note_permission_subjects(&call.id, &subjects);
        let (_request, perm_result) = self
            .permission_engine
            .evaluate_with_subjects(
                &call.name,
                call.input.to_string(),
                None,
                crate::tool_scheduler::permission_context_for(turn.agent_id.as_deref(), &call.id),
                subjects,
            )
            .await
            .map_err(|e| format!("permission check failed: {e}"))?;

        match perm_result {
            PermissionResult::Allow | PermissionResult::AllowForPath { .. } => {
                let outcome = self.tools.execute(&call.id, &call.name, &call.input).await;
                let mut result = outcome.result;

                let mut ctx = std::collections::HashMap::new();
                ctx.insert("tool_call_id".to_string(), call.id.clone());
                ctx.insert("tool_input".to_string(), call.input.to_string());
                ctx.insert(
                    "tool_output_len".to_string(),
                    result.output.len().to_string(),
                );
                let post_result = self
                    .fire_hook(hook_events::POST_TOOL_USE, &call.name, &ctx)
                    .await;
                if !post_result.errors.is_empty() {
                    tracing::warn!(
                        "PostToolUse hook errors for {}: {:?}",
                        call.name,
                        post_result.errors
                    );
                }
                // A PostToolUse hook's `updatedToolOutput` replaces what the
                // model sees.
                if let Some(updated) = post_result.updated_tool_output {
                    result.output = updated;
                }

                let _ = self.event_tx.send(EngineEvent::ToolCallEnd {
                    id: call.id.clone(),
                    result: result.clone(),
                });
                Ok(Some(result))
            }
            PermissionResult::Ask { reason } => {
                let _ = self.event_tx.send(EngineEvent::PermissionRequest {
                    id: call.id.clone(),
                    tool: call.name.clone(),
                    args: call.input.clone(),
                });
                self.denied_tools.push(DeniedToolCall {
                    name: call.name.clone(),
                    reason: reason.clone(),
                });
                let result = ToolResult {
                    tool_call_id: call.id.clone(),
                    output: crate::permissions::PermissionResult::unapproved_tool_message(
                        &call.name, &reason,
                    ),
                    is_error: true,
                    images: Vec::new(),
                };
                let mut ctx = std::collections::HashMap::new();
                ctx.insert("tool_call_id".to_string(), call.id.clone());
                ctx.insert("tool_input".to_string(), call.input.to_string());
                ctx.insert("reason".to_string(), reason.clone());
                let fail_result = self
                    .fire_hook(hook_events::POST_TOOL_USE_FAILURE, &call.name, &ctx)
                    .await;
                if !fail_result.errors.is_empty() {
                    tracing::warn!(
                        "PostToolUseFailure hook errors for {}: {:?}",
                        call.name,
                        fail_result.errors
                    );
                }
                let _ = self.event_tx.send(EngineEvent::ToolCallEnd {
                    id: call.id.clone(),
                    result: result.clone(),
                });
                Ok(Some(result))
            }
            PermissionResult::Deny { reason } => {
                self.denied_tools.push(DeniedToolCall {
                    name: call.name.clone(),
                    reason: reason.clone(),
                });
                // The reason is the only thing that tells the model what to
                // do differently.
                let result = ToolResult {
                    tool_call_id: call.id.clone(),
                    output: format!("tool '{}' denied: {reason}", call.name),
                    is_error: true,
                    images: Vec::new(),
                };
                let mut ctx = std::collections::HashMap::new();
                ctx.insert("tool_call_id".to_string(), call.id.clone());
                ctx.insert("tool_input".to_string(), call.input.to_string());
                ctx.insert("reason".to_string(), reason.clone());
                let fail_result = self
                    .fire_hook(hook_events::POST_TOOL_USE_FAILURE, &call.name, &ctx)
                    .await;
                if !fail_result.errors.is_empty() {
                    tracing::warn!(
                        "PostToolUseFailure hook errors for {}: {:?}",
                        call.name,
                        fail_result.errors
                    );
                }
                let _ = self.event_tx.send(EngineEvent::ToolCallEnd {
                    id: call.id.clone(),
                    result: result.clone(),
                });
                Ok(Some(result))
            }
            PermissionResult::Elevated { reason } => {
                let elevation_id = format!("elev_{}", call.id);
                let _ = self.event_tx.send(EngineEvent::ElevationRequest {
                    id: elevation_id.clone(),
                    tool: call.name.clone(),
                    reason: reason.clone(),
                });
                turn.pending_elevation = Some(PendingElevation {
                    tool_call_id: call.id,
                    tool_name: call.name,
                    input: call.input,
                    request_id: elevation_id,
                    reason,
                });
                self.resolve_pending_elevation(turn).await
            }
        }
    }

    /// Produce a display label for one tool call.
    ///
    /// With `tool_use_summaries` on, asks the lightweight model for a
    /// one-line description. Any failure — an unavailable client, an API
    /// error, or an empty response — falls back to
    /// [`ToolCall::activity_summary`](crate::types::ToolCall::activity_summary).
    /// This method never returns an error: a summary is a display nicety and
    /// must not fail the turn.
    async fn tool_use_summary(&self, name: &str, input: &serde_json::Value) -> String {
        let fallback = crate::types::ToolCall::summarize_activity(name, input);
        if !self.tool_use_summaries {
            return fallback;
        }
        match crate::prompts::generate_tool_use_summary(self.api_client.as_ref(), name, input).await
        {
            Ok(summary) if !summary.is_empty() => summary,
            Ok(_) => fallback,
            Err(error) => {
                tracing::debug!(
                    %error,
                    tool = name,
                    "tool use summary unavailable; using activity summary"
                );
                fallback
            }
        }
    }

    /// Execute pending tool calls with permission checks.
    async fn execute_tool_calls(
        &mut self,
        turn: &mut TurnRuntime,
        options: &SubmitOptions,
    ) -> Result<Vec<ToolResult>, String> {
        // An interrupt that arrived while the model was answering stops the
        // tools as well: the person asked for this to end, not for one more
        // batch of edits to land.
        if self.interrupt.is_interrupted() {
            return Ok(turn
                .pending_tool_calls
                .iter()
                .map(|call| ToolResult {
                    tool_call_id: call.id.clone(),
                    output: "the turn was interrupted".to_string(),
                    is_error: true,
                    images: Vec::new(),
                })
                .collect());
        }

        // Generate labels before the scheduler emits `ToolCallStart`, so the
        // start event and the preceding `ToolUseSummary` agree. When the flag
        // is off this loop is skipped and the scheduler falls back to its
        // deterministic activity summary.
        let mut summaries = std::collections::HashMap::new();
        if self.tool_use_summaries {
            for call in &turn.pending_tool_calls {
                let summary = self.tool_use_summary(&call.name, &call.input).await;
                let _ = self.event_tx.send(EngineEvent::ToolUseSummary {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    summary: summary.clone(),
                });
                summaries.insert(call.id.clone(), summary);
            }
        }

        let outcome = tool_scheduler::run_tool_calls(
            &turn.pending_tool_calls,
            self.tools.as_ref(),
            &mut self.permission_engine,
            self.hooks.as_ref(),
            options,
            &self.event_tx,
            &mut self.denied_tools,
            &summaries,
        )
        .await?;

        let mut results = outcome.results;
        for elevation in outcome.pending_elevations {
            let tool_call_id = elevation.tool_call_id.clone();
            turn.pending_elevation = Some(elevation);
            if let Some(result) = self.resolve_pending_elevation(turn).await?
                && let Some(slot) = results
                    .iter_mut()
                    .find(|result| result.tool_call_id == tool_call_id)
            {
                *slot = result;
            }
        }
        turn.pending_tool_calls.clear();
        self.tools.clear_permission_subjects();
        Ok(results)
    }

    /// Ask the interactive frontend for an elevation decision, then resolve it.
    ///
    /// The confirmation bus is the same channel used by ordinary permission
    /// prompts. Keeping the wait inside the engine is important: `submit` and
    /// `submit_streaming` own the [`TurnRuntime`], so an external UI cannot call
    /// [`Self::resolve_elevation`] while a query is running.
    async fn resolve_pending_elevation(
        &mut self,
        turn: &mut TurnRuntime,
    ) -> Result<Option<ToolResult>, String> {
        let Some(elevation) = turn.pending_elevation.clone() else {
            return Ok(None);
        };

        let response = if let Some(bus) = self.permission_engine.confirmation_bus() {
            let previous_state = turn.state;
            turn.transition_to(TurnState::AwaitPermission);
            let response = bus
                .request(ConfirmationRequest {
                    id: elevation.request_id.clone(),
                    tool_name: elevation.tool_name.clone(),
                    arguments: elevation.input.to_string(),
                    path: None,
                    reason: elevation.reason.clone(),
                    allow_session: false,
                    context: HashMap::new(),
                })
                .await;
            turn.transition_to(previous_state);
            response
        } else {
            None
        };

        match response {
            Some(ConfirmationResponse::Approved | ConfirmationResponse::ApproveSession) => {
                self.resolve_elevation(turn, true).await
            }
            Some(ConfirmationResponse::Denied) => self.resolve_elevation(turn, false).await,
            None => Ok(self.abandon_pending_elevation(turn)),
        }
    }

    /// Finish an elevation that could not be presented to a user.
    ///
    /// A missing or closed confirmation channel is not a denial by the user.
    /// Return that distinction to the model instead of leaving the tool call
    /// pending forever or recording a fabricated refusal.
    fn abandon_pending_elevation(&self, turn: &mut TurnRuntime) -> Option<ToolResult> {
        let elevation = turn.pending_elevation.take()?;
        let result = ToolResult {
            tool_call_id: elevation.tool_call_id.clone(),
            output: PermissionResult::unapproved_tool_message(
                &elevation.tool_name,
                &elevation.reason,
            ),
            is_error: true,
            images: Vec::new(),
        };
        let _ = self.event_tx.send(EngineEvent::ToolCallEnd {
            id: elevation.tool_call_id,
            result: result.clone(),
        });
        Some(result)
    }

    /// Resolve a pending elevation request and retry the tool call with elevated permissions.
    ///
    /// After the user approves elevation, this temporarily grants bypass-level
    /// permissions for the specific tool call and re-executes it.
    pub async fn resolve_elevation(
        &mut self,
        turn: &mut TurnRuntime,
        approved: bool,
    ) -> Result<Option<ToolResult>, String> {
        let Some(elevation) = turn.pending_elevation.take() else {
            return Ok(None);
        };

        if !approved {
            // User denied elevation — treat as permanent denial
            self.denied_tools.push(DeniedToolCall {
                name: elevation.tool_name.clone(),
                reason: "elevation denied by user".to_string(),
            });
            let result = ToolResult {
                tool_call_id: elevation.tool_call_id.clone(),
                output: format!("tool '{}' elevation denied", elevation.tool_name),
                is_error: true,
                images: Vec::new(),
            };
            let _ = self.event_tx.send(EngineEvent::ToolCallEnd {
                id: elevation.tool_call_id,
                result: result.clone(),
            });
            return Ok(Some(result));
        }

        // Temporarily switch to BypassPermissions for the elevated call
        let checker = self.permission_engine.checker();
        let prev_mode = {
            let mut c = checker.lock().unwrap();
            let prev = c.mode();
            c.set_mode(PermissionMode::BypassPermissions);
            prev
        };

        // Fire PreToolUse hook
        let mut ctx = std::collections::HashMap::new();
        ctx.insert("tool_call_id".to_string(), elevation.tool_call_id.clone());
        ctx.insert("tool_input".to_string(), elevation.input.to_string());
        let pre_hook = self
            .fire_hook(hook_events::PRE_TOOL_USE, &elevation.tool_name, &ctx)
            .await;
        if pre_hook.stop {
            // Restore mode
            {
                let mut c = checker.lock().unwrap();
                c.set_mode(prev_mode);
            }
            let result = ToolResult {
                tool_call_id: elevation.tool_call_id.clone(),
                output: "elevated tool execution stopped by hook".to_string(),
                is_error: true,
                images: Vec::new(),
            };
            let _ = self.event_tx.send(EngineEvent::ToolCallEnd {
                id: elevation.tool_call_id,
                result: result.clone(),
            });
            return Ok(Some(result));
        }

        // Re-run permission check (should now pass with BypassPermissions).
        // Capture the result before `?` so an evaluation error cannot leave the
        // shared checker stuck in BypassPermissions.
        let subjects = self
            .tools
            .rule_subjects(&elevation.tool_name, &elevation.input);
        self.tools
            .note_permission_subjects(&elevation.tool_call_id, &subjects);
        let permission_result = self
            .permission_engine
            .evaluate_with_subjects(
                &elevation.tool_name,
                elevation.input.to_string(),
                None,
                permission_context_for(turn.agent_id.as_deref(), &elevation.tool_call_id),
                subjects,
            )
            .await;

        // Restore original mode
        {
            let mut c = checker.lock().unwrap();
            c.set_mode(prev_mode);
        }

        let (_request, perm_result) =
            permission_result.map_err(|e| format!("permission check failed: {e}"))?;

        let result = match perm_result {
            PermissionResult::Allow | PermissionResult::AllowForPath { .. } => {
                let outcome = self
                    .tools
                    .execute(
                        &elevation.tool_call_id,
                        &elevation.tool_name,
                        &elevation.input,
                    )
                    .await;
                outcome.result
            }
            _ => {
                self.denied_tools.push(DeniedToolCall {
                    name: elevation.tool_name.clone(),
                    reason: "elevated permission check still denied".to_string(),
                });
                ToolResult {
                    tool_call_id: elevation.tool_call_id.clone(),
                    output: format!("tool '{}' elevated execution denied", elevation.tool_name),
                    is_error: true,
                    images: Vec::new(),
                }
            }
        };

        let _ = self.event_tx.send(EngineEvent::ToolCallEnd {
            id: elevation.tool_call_id.clone(),
            result: result.clone(),
        });
        Ok(Some(result))
    }

    /// Get the current cost tracker (shared reference).
    pub const fn cost_tracker(&self) -> &CostTracker {
        &self.cost_tracker
    }

    /// Get the current turn number.
    #[must_use]
    pub fn current_turn(&self) -> u32 {
        self.current_turn.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// A handle that keeps tracking the turn count after the engine is moved.
    ///
    /// The headless turn task owns the engine while it runs, so the REPL
    /// cannot call [`Self::current_turn`] at the moment it writes the closing
    /// `result` frame — and a SIGTERM can cut the turn short while the task
    /// still owns it. Both paths need the number.
    #[must_use]
    pub fn turn_counter(&self) -> TurnCounter {
        TurnCounter(Arc::clone(&self.current_turn))
    }

    /// Interrupt the current turn (e.g., from Ctrl+C).
    ///
    /// Sets the flag the turn loop reads *and* emits the event: the event
    /// tells a UI the session is ending, the flag is what actually stops the
    /// work.
    pub fn interrupt(&self) {
        self.interrupt_handle().interrupt();
        let _ = self.event_tx.send(EngineEvent::SessionEnd);
    }

    /// A handle for interrupting this engine from outside it.
    ///
    /// A caller that has moved the engine into a task — the TUI does — keeps
    /// one of these instead, because it can no longer call `interrupt` on an
    /// engine it does not own.
    #[must_use]
    pub fn interrupt_handle(&self) -> InterruptHandle {
        self.interrupt.clone()
    }
}

/// The context a permission decision is made in.
///
/// `BackgroundAgentDenyHook` reads `execution_mode` to refuse tools in a
/// background run, and every call site passed an empty map — so a hook that
/// was written, registered, and documented could never fire. The mode comes
/// from the run this call belongs to: one with an agent id is a forked or
/// background flow, one without is the person's own turn.
fn permission_context_for(
    agent_id: Option<&str>,
    tool_call_id: &str,
) -> std::collections::HashMap<String, String> {
    let mut context = std::collections::HashMap::new();
    context.insert(
        "execution_mode".to_string(),
        if agent_id.is_some() {
            "background".to_string()
        } else {
            "interactive".to_string()
        },
    );
    context.insert("tool_call_id".to_string(), tool_call_id.to_string());
    if let Some(agent_id) = agent_id {
        context.insert("agent_id".to_string(), agent_id.to_string());
    }
    context
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_client::StreamEvent;
    use crate::permission_engine::PermissionEngine;
    use crate::permissions::{PermissionChecker, PermissionMode};

    /// The handle keeps reading the engine's counter after the engine moves.
    ///
    /// This is the shape the headless REPL depends on: it captures the handle
    /// before handing the engine to the turn task, then reports the difference
    /// in the closing `result` frame. Hardcoding that frame's `num_turns` to 1
    /// is what made a 30-call benchmark trial report a single turn.
    #[test]
    fn a_turn_counter_handle_tracks_the_engine_after_the_handoff() {
        let (engine, _rx, _session) = make_test_engine();
        let counter = engine.turn_counter();
        assert_eq!(counter.value(), 0);

        // Advance the engine, as the turn task does while the caller holds
        // only the handle.
        engine
            .current_turn
            .store(4, std::sync::atomic::Ordering::SeqCst);

        assert_eq!(counter.value(), 4);
        assert_eq!(counter.consumed_since(0), 4);
        assert_eq!(counter.consumed_since(3), 1);
        // A later submit in the same session starts from where the last one
        // ended, so a baseline from the future must not underflow.
        assert_eq!(counter.consumed_since(9), 0);
    }

    /// A history micro compaction can actually shrink: `count` whitelisted
    /// tool results, each large enough for truncation, paired with their
    /// calls so the tool identity survives.
    fn read_result_history(count: usize) -> Vec<Message> {
        let mut messages = vec![Message::user("read the files")];
        for i in 0..count {
            let id = format!("call-{i}");
            messages.push(Message {
                role: crate::types::Role::Assistant,
                content: crate::types::MessageContent::ToolCall(ToolCall {
                    id: id.clone(),
                    name: "Read".into(),
                    input: serde_json::json!({"file_path": format!("src/file{i}.rs")}),
                }),
            });
            messages.push(Message::tool(ToolResult::text(id, "x".repeat(20_000))));
        }
        messages
    }

    fn make_test_engine() -> (
        QueryEngine,
        mpsc::UnboundedReceiver<EngineEvent>,
        SessionState,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        let checker = PermissionChecker::new(PermissionMode::BypassPermissions);
        let perm_engine = PermissionEngine::new(checker);
        let session = SessionState::new(std::path::PathBuf::from("/tmp/test"));
        let api = MockApiClient;
        let mut engine = QueryEngine::new(
            Box::new(api),
            ModelId::sonnet(),
            session.clone(),
            perm_engine,
            tx,
        );
        engine.set_tool_executor(Box::new(StubOkToolExecutor));
        (engine, rx, session)
    }

    /// A mod's UI is forwarded as an engine event, but only when the hook
    /// actually contributed something. An empty tree would make every
    /// ordinary hook look like it wanted to draw.
    #[tokio::test]
    async fn a_hook_that_returns_ui_emits_mod_ui_only_when_non_empty() {
        struct UiHooks;

        #[async_trait::async_trait]
        impl HookExecutor for UiHooks {
            async fn fire_hook(
                &self,
                event_name: &str,
                _input: &str,
                _context: &HashMap<String, String>,
            ) -> HookFireResult {
                if event_name == "WithUi" {
                    HookFireResult {
                        ui: vec![crate::ui_protocol::UiElement::Text {
                            text: "hello".to_string(),
                            style: crate::ui_protocol::UiStyle::Bold,
                        }],
                        ..HookFireResult::default()
                    }
                } else {
                    HookFireResult::default()
                }
            }
        }

        let (mut engine, mut events, _session) = make_test_engine();
        engine.set_hooks(Arc::new(UiHooks));

        let with_ui = engine.fire_hook("WithUi", "input", &HashMap::new()).await;
        let without_ui = engine
            .fire_hook("WithoutUi", "input", &HashMap::new())
            .await;

        assert_eq!(with_ui.ui.len(), 1);
        assert!(without_ui.ui.is_empty());

        let mut mod_ui = Vec::new();
        while let Ok(event) = events.try_recv() {
            if let EngineEvent::ModUi(elements) = event {
                mod_ui.push(elements);
            }
        }
        assert_eq!(mod_ui.len(), 1, "only the non-empty result may emit");
        assert_eq!(mod_ui[0], with_ui.ui);
    }

    /// A hook executor that records the events the engine fires and their
    /// context, so tests can assert that lifecycle hooks really ran.
    struct RecordingHooks {
        events: Arc<std::sync::Mutex<Vec<(String, HashMap<String, String>)>>>,
    }

    impl RecordingHooks {
        fn new() -> (
            Self,
            Arc<std::sync::Mutex<Vec<(String, HashMap<String, String>)>>>,
        ) {
            let events = Arc::new(std::sync::Mutex::new(Vec::new()));
            (
                Self {
                    events: Arc::clone(&events),
                },
                events,
            )
        }
    }

    #[async_trait::async_trait]
    impl HookExecutor for RecordingHooks {
        async fn fire_hook(
            &self,
            event_name: &str,
            _input: &str,
            context: &HashMap<String, String>,
        ) -> HookFireResult {
            self.events
                .lock()
                .expect("the recording hook lock is not poisoned")
                .push((event_name.to_string(), context.clone()));
            HookFireResult::default()
        }
    }

    fn recorded_event_names(
        events: &Arc<std::sync::Mutex<Vec<(String, HashMap<String, String>)>>>,
    ) -> Vec<String> {
        events
            .lock()
            .expect("the recording hook lock is not poisoned")
            .iter()
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// A streaming client whose response consumes more than a one-token
    /// budget, so the turn loop reaches its budget-exhausted exits.
    struct BudgetStreamMock;

    #[async_trait::async_trait]
    impl ApiClient for BudgetStreamMock {
        fn clone_box(&self) -> Box<dyn ApiClient> {
            Box::new(Self)
        }

        async fn create_message(
            &self,
            _system: &str,
            _messages: &[serde_json::Value],
            _model: &ModelId,
            _max_tokens: u32,
            _tools: &[crate::tool_executor::ToolInfo],
        ) -> Result<ApiResponse, crate::api_client::ApiError> {
            Ok(ApiResponse {
                content: "budgeted answer".into(),
                tool_calls: Vec::new(),
                usage: UsageSummary {
                    input_tokens: 10,
                    output_tokens: 50,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                },
                stop_reason: "end_turn".into(),
                thinking: None,
            })
        }

        async fn create_message_streaming(
            &self,
            _system: &str,
            _messages: &[serde_json::Value],
            _model: &ModelId,
            _max_tokens: u32,
            _tools: &[crate::tool_executor::ToolInfo],
        ) -> std::pin::Pin<Box<dyn futures::Stream<Item = StreamEvent> + Send + 'static>> {
            Box::pin(futures::stream::iter(vec![
                StreamEvent::UsageSummary(UsageSummary {
                    input_tokens: 10,
                    output_tokens: 50,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                }),
                StreamEvent::StopReason("end_turn".into()),
                StreamEvent::StreamEnd,
            ]))
        }
    }

    /// Test-only tool executor that echoes back a successful result so
    /// permission/elevation flow tests can observe the allow path.
    struct StubOkToolExecutor;

    #[async_trait::async_trait]
    impl crate::tool_executor::ToolExecutor for StubOkToolExecutor {
        async fn execute(
            &self,
            tool_call_id: &str,
            tool_name: &str,
            _input: &serde_json::Value,
        ) -> crate::tool_executor::ToolDispatchOutcome {
            crate::tool_executor::ToolDispatchOutcome {
                result: ToolResult {
                    tool_call_id: tool_call_id.to_string(),
                    output: format!("stub ok: {tool_name}"),
                    is_error: false,
                    images: Vec::new(),
                },
            }
        }
    }

    // ─── Mock API Client ─────────────────────────────────────────────

    struct MockApiClient;

    #[async_trait::async_trait]
    impl ApiClient for MockApiClient {
        fn clone_box(&self) -> Box<dyn ApiClient> {
            Box::new(Self)
        }

        async fn create_message(
            &self,
            _system: &str,
            _messages: &[serde_json::Value],
            _model: &ModelId,
            _max_tokens: u32,
            _tools: &[crate::tool_executor::ToolInfo],
        ) -> Result<ApiResponse, crate::api_client::ApiError> {
            Ok(ApiResponse {
                content: "Hello from the mock assistant.".into(),
                tool_calls: Vec::new(),
                usage: UsageSummary {
                    input_tokens: 100,
                    output_tokens: 50,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                },
                stop_reason: "end_turn".into(),
                thinking: None,
            })
        }

        async fn create_message_streaming(
            &self,
            _system: &str,
            _messages: &[serde_json::Value],
            _model: &ModelId,
            _max_tokens: u32,
            _tools: &[crate::tool_executor::ToolInfo],
        ) -> std::pin::Pin<Box<dyn futures::Stream<Item = StreamEvent> + Send + 'static>> {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let _ = tx.send(StreamEvent::StreamEnd);
            Box::pin(futures::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|item| (item, rx))
            }))
        }
    }

    struct EmptyStreamingApiClient;

    #[async_trait::async_trait]
    impl ApiClient for EmptyStreamingApiClient {
        fn clone_box(&self) -> Box<dyn ApiClient> {
            Box::new(Self)
        }

        async fn create_message(
            &self,
            _system: &str,
            _messages: &[serde_json::Value],
            _model: &ModelId,
            _max_tokens: u32,
            _tools: &[crate::tool_executor::ToolInfo],
        ) -> Result<ApiResponse, crate::api_client::ApiError> {
            Ok(ApiResponse {
                content: String::new(),
                tool_calls: Vec::new(),
                usage: UsageSummary {
                    input_tokens: 1,
                    output_tokens: 0,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                },
                stop_reason: "end_turn".into(),
                thinking: None,
            })
        }

        async fn create_message_streaming(
            &self,
            _system: &str,
            _messages: &[serde_json::Value],
            _model: &ModelId,
            _max_tokens: u32,
            _tools: &[crate::tool_executor::ToolInfo],
        ) -> std::pin::Pin<Box<dyn futures::Stream<Item = StreamEvent> + Send + 'static>> {
            Box::pin(futures::stream::iter([StreamEvent::StreamEnd]))
        }
    }

    #[test]
    fn turn_state_user_waiting_only_on_specific_states() {
        assert!(TurnState::AwaitPermission.is_user_waiting());
        assert!(TurnState::AwaitElicitation.is_user_waiting());
        assert!(!TurnState::Initialize.is_user_waiting());
        assert!(!TurnState::StreamAssistant.is_user_waiting());
    }

    #[test]
    fn turn_state_terminal_only_on_terminated() {
        assert!(TurnState::Terminated.is_terminal());
        assert!(!TurnState::Finalize.is_terminal());
        assert!(!TurnState::CompactRecovery.is_terminal());
    }

    #[test]
    fn terminal_reason_display_formats() {
        assert_eq!(format!("{}", TerminalReason::Completed), "completed");
        assert_eq!(
            format!("{}", TerminalReason::Error("something broke".into())),
            "error: something broke"
        );
        assert_eq!(
            format!("{}", TerminalReason::MaxTurnsReached),
            "max turns reached"
        );
    }

    #[test]
    fn turn_runtime_new_starts_at_initialize() {
        let budget = TokenBudget::with_defaults();
        let turn = TurnRuntime::new(budget);
        assert_eq!(turn.state, TurnState::Initialize);
        assert!(turn.messages.is_empty());
        assert!(turn.pending_tool_calls.is_empty());
        assert!(turn.pending_permission.is_none());
        assert!(turn.terminal_reason.is_none());
    }

    #[test]
    fn turn_runtime_transition_to_works() {
        let budget = TokenBudget::with_defaults();
        let mut turn = TurnRuntime::new(budget);
        turn.transition_to(TurnState::BuildContext);
        assert_eq!(turn.state, TurnState::BuildContext);
    }

    #[test]
    fn turn_runtime_terminate_sets_reason() {
        let budget = TokenBudget::with_defaults();
        let mut turn = TurnRuntime::new(budget);
        turn.terminate(TerminalReason::UserInterrupted);
        assert_eq!(turn.state, TurnState::Terminated);
        assert!(matches!(
            turn.terminal_reason,
            Some(TerminalReason::UserInterrupted)
        ));
    }

    #[test]
    fn turn_runtime_reset_clears_turn_data() {
        let budget = TokenBudget::with_defaults();
        let mut turn = TurnRuntime::new(budget);
        turn.messages.push(Message::user("hello"));
        turn.pending_tool_calls.push(PendingToolCall {
            id: "tc_1".into(),
            name: "bash".into(),
            input: serde_json::json!({}),
        });
        turn.budget.record_tokens(&UsageSummary {
            input_tokens: 0,
            output_tokens: 500,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        });

        turn.reset();

        assert_eq!(turn.state, TurnState::Initialize);
        assert!(turn.pending_tool_calls.is_empty());
        assert_eq!(turn.budget.turn_tokens, 0);
        // `turn.messages` is the per-request list, not the session history —
        // the session's messages live in `session.messages` and are re-extended
        // by `build_context`. Leaving them here duplicated the preamble, system
        // prompt and history once per turn.
        assert!(
            turn.messages.is_empty(),
            "reset must clear the per-request message list"
        );
    }

    /// Regression: the turn runtime is reused across the whole submit loop, so
    /// a second `build_context` for the same runtime must not see the first
    /// one's messages.
    #[test]
    fn rebuild_context_does_not_accumulate_messages() {
        let (mut engine, _rx, mut session) = make_test_engine();
        session.messages.push(Message::user("first"));

        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());

        engine.build_context(&mut turn, &SubmitOptions::default());
        let after_first = turn.messages.len();

        // Same turn runtime reused for the next turn, as the submit loop does.
        turn.reset();
        engine.build_context(&mut turn, &SubmitOptions::default());
        let after_second = turn.messages.len();

        assert_eq!(
            after_first, after_second,
            "rebuilding context must be idempotent per turn, not append"
        );
    }

    #[test]
    fn fallback_is_used_only_for_model_unavailability() {
        let primary = ModelId("claude-opus-4-6".into());
        let fallback = ModelId("claude-haiku-4-5-20251001".into());

        assert_eq!(
            fallback_after_model_error(
                &primary,
                Some(&fallback),
                "404 model_not_found: claude-opus-4-6"
            )
            .map(|model| model.0),
            Some(fallback.0.clone())
        );
        assert!(
            fallback_after_model_error(&primary, Some(&fallback), "connection reset by peer")
                .is_none(),
            "a transport error is not evidence the model is unavailable"
        );
        assert!(fallback_after_model_error(&primary, Some(&primary), "model_not_found").is_none());
        assert!(fallback_after_model_error(&primary, None, "model_not_found").is_none());
    }

    /// The engine retries the same request exactly once when the primary model
    /// is unavailable, then continues on the fallback.
    #[tokio::test]
    async fn unavailable_primary_retries_once_with_the_fallback_model() {
        struct FallbackMock {
            primary: String,
            models: Arc<std::sync::Mutex<Vec<String>>>,
        }

        #[async_trait::async_trait]
        impl ApiClient for FallbackMock {
            fn clone_box(&self) -> Box<dyn ApiClient> {
                Box::new(Self {
                    primary: self.primary.clone(),
                    models: Arc::clone(&self.models),
                })
            }

            async fn create_message(
                &self,
                _system: &str,
                _messages: &[serde_json::Value],
                model: &ModelId,
                _max_tokens: u32,
                _tools: &[crate::tool_executor::ToolInfo],
            ) -> Result<ApiResponse, crate::api_client::ApiError> {
                self.models
                    .lock()
                    .expect("the request log lock is not poisoned")
                    .push(model.0.clone());
                if model.0 == self.primary {
                    Err(crate::api_client::ApiError::InvalidModel(
                        "model_not_found".into(),
                    ))
                } else {
                    Ok(ApiResponse {
                        content: "fallback answer".into(),
                        tool_calls: Vec::new(),
                        usage: UsageSummary {
                            input_tokens: 10,
                            output_tokens: 5,
                            cache_read_tokens: 0,
                            cache_creation_tokens: 0,
                        },
                        stop_reason: "end_turn".into(),
                        thinking: None,
                    })
                }
            }

            async fn create_message_streaming(
                &self,
                _system: &str,
                _messages: &[serde_json::Value],
                _model: &ModelId,
                _max_tokens: u32,
                _tools: &[crate::tool_executor::ToolInfo],
            ) -> std::pin::Pin<Box<dyn futures::Stream<Item = StreamEvent> + Send + 'static>>
            {
                Box::pin(futures::stream::iter(vec![StreamEvent::StreamEnd]))
            }
        }

        let models = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (tx, _rx) = mpsc::unbounded_channel();
        let checker = PermissionChecker::new(PermissionMode::BypassPermissions);
        let permission_engine = PermissionEngine::new(checker);
        let session = SessionState::new(std::path::PathBuf::from("/tmp/test"));
        let mut engine = QueryEngine::new(
            Box::new(FallbackMock {
                primary: "primary-model".into(),
                models: Arc::clone(&models),
            }),
            ModelId("primary-model".into()),
            session,
            permission_engine,
            tx,
        );
        engine.set_tool_executor(Box::new(StubOkToolExecutor));
        engine.set_fallback_model(Some(ModelId("fallback-model".into())));

        let options = SubmitOptions {
            handle_elicitation: false,
            ..SubmitOptions::default()
        };
        let result = engine.submit("hello".into(), options).await;
        assert!(
            matches!(result, Ok(TerminalReason::Completed)),
            "expected the fallback response to complete the turn, got {result:?}"
        );
        assert_eq!(
            *models.lock().expect("the request log lock is not poisoned"),
            vec!["primary-model".to_string(), "fallback-model".to_string()],
            "the request is retried once, with the fallback"
        );
        assert_eq!(engine.model().0, "fallback-model");
    }

    #[test]
    fn submit_options_defaults_are_sane() {
        let opts = SubmitOptions::default();
        assert_eq!(opts.max_turns, 0);
        assert_eq!(opts.max_budget_usd, 0.0);
        assert!(opts.json_schema.is_none());
        assert!(opts.custom_system_prompt.is_none());
        assert!(opts.handle_elicitation);
    }

    /// `coordinator_mode` installs a `CoordinatorState` on the session engine;
    /// the engine's per-turn advance then moves the phase. Without an
    /// installed coordinator nothing runs, which is what made the flag inert.
    #[tokio::test]
    async fn coordinator_advances_a_phase_per_turn_when_installed() {
        let (mut engine, _rx, _session) = make_test_engine();
        assert!(
            engine.coordinator().is_none(),
            "no coordinator is installed by default"
        );

        let mut coordinator = crate::coordinator::CoordinatorState::new();
        coordinator.start();
        engine.set_coordinator(coordinator);

        let reason = engine
            .submit("hello".into(), SubmitOptions::default())
            .await
            .expect("the mock turn completes");
        assert!(matches!(reason, TerminalReason::Completed), "{reason:?}");

        let coordinator = engine
            .coordinator()
            .expect("the coordinator stays installed after the turn");
        assert_eq!(
            coordinator.current_phase,
            crate::coordinator::CoordinatorPhase::Synthesis,
            "one completed turn advances Research → Synthesis"
        );
        assert_eq!(coordinator.phases_completed(), 1);
    }

    #[test]
    fn history_snip_setter_toggles_the_compact_config() {
        let (mut engine, _rx, _session) = make_test_engine();
        assert!(!engine.history_snip(), "snip preference is off by default");
        engine.set_history_snip(true);
        assert!(engine.history_snip());
        engine.set_history_snip(false);
        assert!(!engine.history_snip());
    }

    #[test]
    fn build_context_loads_session_messages() {
        let (engine, _rx, _session) = make_test_engine();
        // Verify that session() returns a reference to the session
        let session = engine.session();
        assert!(session.messages.is_empty());
        // New sessions start with no messages, build_context would copy them
    }

    #[tokio::test]
    async fn send_request_returns_mock_response() {
        let (mut engine, _rx, _session) = make_test_engine();
        let budget = TokenBudget::with_defaults();
        let mut turn = TurnRuntime::new(budget);
        turn.messages.push(Message::user("hello"));
        let opts = SubmitOptions::default();

        let result = engine.send_request(&turn, &opts).await;
        assert!(result.is_ok());
        let resp = result.unwrap();
        assert!(!resp.content.is_empty());
        assert!(resp.usage.output_tokens > 0);
    }

    /// An interrupt stops the work rather than only announcing itself. The
    /// engine used to emit an event nothing listened for, so the turn ran to
    /// completion while the caller believed it had stopped.
    /// A model that interrupts the engine while it is answering — which is
    /// when a person actually presses Ctrl+C.
    struct InterruptingMock {
        handle: InterruptHandle,
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl ApiClient for InterruptingMock {
        fn clone_box(&self) -> Box<dyn ApiClient> {
            Box::new(Self {
                handle: self.handle.clone(),
                calls: std::sync::Arc::clone(&self.calls),
            })
        }

        async fn create_message(
            &self,
            _system: &str,
            _messages: &[serde_json::Value],
            _model: &ModelId,
            _max_tokens: u32,
            _tools: &[crate::tool_executor::ToolInfo],
        ) -> Result<ApiResponse, crate::api_client::ApiError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            // The interrupt lands while the model is answering.
            self.handle.interrupt();
            Ok(ApiResponse {
                content: "a first answer".into(),
                tool_calls: Vec::new(),
                usage: UsageSummary {
                    input_tokens: 10,
                    output_tokens: 5,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                },
                stop_reason: "end_turn".into(),
                thinking: None,
            })
        }

        async fn create_message_streaming(
            &self,
            system: &str,
            messages: &[serde_json::Value],
            model: &ModelId,
            max_tokens: u32,
            tools: &[crate::tool_executor::ToolInfo],
        ) -> std::pin::Pin<
            Box<dyn futures::Stream<Item = crate::api_client::StreamEvent> + Send + 'static>,
        > {
            let response = self
                .create_message(system, messages, model, max_tokens, tools)
                .await
                .expect("the interrupting mock answers");
            Box::pin(futures::stream::iter(vec![
                crate::api_client::StreamEvent::TextDelta(response.content),
                crate::api_client::StreamEvent::StopReason(response.stop_reason),
                crate::api_client::StreamEvent::StreamEnd,
            ]))
        }
    }

    /// A truncated answer ends the turn as `TokenLimitStop` — on both paths.
    /// The streaming path lost this: it passed a hardcoded `"end_turn"` as the
    /// stop reason and never checked for truncation, so a response the model
    /// ran out of room for looked exactly like a finished turn.
    #[tokio::test]
    async fn a_truncated_answer_stops_the_turn_on_the_streaming_path() {
        /// Answers, and says it ran out of room.
        struct TruncatingMock;

        #[async_trait::async_trait]
        impl ApiClient for TruncatingMock {
            fn clone_box(&self) -> Box<dyn ApiClient> {
                Box::new(Self)
            }

            async fn create_message(
                &self,
                _system: &str,
                _messages: &[serde_json::Value],
                _model: &ModelId,
                _max_tokens: u32,
                _tools: &[crate::tool_executor::ToolInfo],
            ) -> Result<ApiResponse, crate::api_client::ApiError> {
                Ok(ApiResponse {
                    content: "a half-finished ans".into(),
                    tool_calls: Vec::new(),
                    usage: UsageSummary {
                        input_tokens: 10,
                        output_tokens: 5,
                        cache_read_tokens: 0,
                        cache_creation_tokens: 0,
                    },
                    stop_reason: "max_tokens".into(),
                    thinking: None,
                })
            }

            async fn create_message_streaming(
                &self,
                system: &str,
                messages: &[serde_json::Value],
                model: &ModelId,
                max_tokens: u32,
                tools: &[crate::tool_executor::ToolInfo],
            ) -> std::pin::Pin<
                Box<dyn futures::Stream<Item = crate::api_client::StreamEvent> + Send + 'static>,
            > {
                let response = self
                    .create_message(system, messages, model, max_tokens, tools)
                    .await
                    .expect("the mock answers");
                Box::pin(futures::stream::iter(vec![
                    crate::api_client::StreamEvent::TextDelta(response.content),
                    crate::api_client::StreamEvent::StopReason(response.stop_reason),
                    crate::api_client::StreamEvent::StreamEnd,
                ]))
            }
        }

        let (tx, _rx) = mpsc::unbounded_channel();
        let session = SessionState::new(std::path::PathBuf::from("/tmp/test"));
        let checker = PermissionChecker::new(PermissionMode::BypassPermissions);
        let mut engine = QueryEngine::new(
            Box::new(TruncatingMock),
            ModelId::sonnet(),
            session,
            PermissionEngine::new(checker),
            tx,
        );

        let streamed = engine
            .submit_streaming("write something long".into(), SubmitOptions::default())
            .await
            .unwrap();
        assert!(
            matches!(streamed, TerminalReason::TokenLimitStop { produced: true }),
            "streaming reported {streamed:?}"
        );

        // And the blocking path agrees, which is the property that was lost.
        let blocked = engine
            .submit("write something long".into(), SubmitOptions::default())
            .await
            .unwrap();
        assert!(
            matches!(blocked, TerminalReason::TokenLimitStop { produced: true }),
            "blocking reported {blocked:?}"
        );
    }

    /// Providers spell "cut off" two ways; both count.
    #[test]
    fn a_cut_off_answer_is_recognised_whichever_way_it_is_spelled() {
        assert!(stop_reason_eq("max_tokens", "max_tokens"));
        assert!(stop_reason_eq("length", "max_tokens"));
        assert!(!stop_reason_eq("end_turn", "max_tokens"));
        assert!(!stop_reason_eq("", "max_tokens"));

        assert!(matches!(
            QueryEngine::reason_for("max_tokens", true),
            TerminalReason::TokenLimitStop { produced: true }
        ));
        assert!(matches!(
            QueryEngine::reason_for("length", true),
            TerminalReason::TokenLimitStop { produced: true }
        ));
        assert!(matches!(
            QueryEngine::reason_for("end_turn", true),
            TerminalReason::Completed
        ));
    }

    /// A turn that spent its whole ceiling thinking is not a truncated answer.
    ///
    /// Both arrive as `max_tokens`; only one of them produced something. The
    /// reason carried this bit nowhere before, so a headless run that emitted
    /// nothing exited looking like a task with an answer that was merely cut
    /// short.
    #[test]
    fn a_cut_off_turn_reports_whether_it_produced_anything() {
        assert!(matches!(
            QueryEngine::reason_for("max_tokens", true),
            TerminalReason::TokenLimitStop { produced: true }
        ));
        assert!(matches!(
            QueryEngine::reason_for("max_tokens", false),
            TerminalReason::TokenLimitStop { produced: false }
        ));
        // The bit means nothing for any other stop reason.
        assert!(matches!(
            QueryEngine::reason_for("end_turn", false),
            TerminalReason::Completed
        ));
        assert!(matches!(
            QueryEngine::reason_for("pause_turn", false),
            TerminalReason::Resume
        ));
    }

    #[test]
    fn only_text_or_a_tool_call_counts_as_output() {
        assert!(!produced_output("", 0));
        assert!(!produced_output("   \n\t ", 0));
        assert!(produced_output("an answer", 0));
        assert!(produced_output("", 1));
        assert!(produced_output("an answer", 2));
    }

    /// A no-op turn is only worth re-asking when the request can carry a
    /// larger ceiling. The default configuration never can, which is what
    /// keeps behaviour byte-identical unless somebody asked for more room.
    #[test]
    fn a_retry_ceiling_needs_room_the_request_does_not_have() {
        use crate::types::{ContextWindowSource, ResolvedContextWindow};

        let known = |tokens| ResolvedContextWindow::new(tokens, ContextWindowSource::Env);

        // A 20 000-token input on a 64 000 window leaves 44 000, but only
        // half the window (32 000) is a ceiling compaction can recover from,
        // so a doubling from 8 192 stops there.
        assert_eq!(retry_ceiling(8_192, 20_000, known(64_000)), Some(16_384));
        assert_eq!(retry_ceiling(20_000, 20_000, known(64_000)), Some(32_000));
        // The static clamp: nothing crosses half the window even with all of
        // it free.
        assert_eq!(retry_ceiling(20_000, 0, known(64_000)), Some(32_000));

        // The default engine ceiling against a full half-window is the
        // configuration a DeepSeek session actually runs: no room to grow, so
        // nothing is retried and the turn reports the no-op it was.
        assert_eq!(retry_ceiling(32_768, 40_000, known(64_000)), None);

        // A window nobody established is not evidence of room.
        assert_eq!(retry_ceiling(32_768, 1_000, known(0)), None);
        assert_eq!(
            retry_ceiling(
                32_768,
                1_000,
                ResolvedContextWindow::new(64_000, ContextWindowSource::Unknown)
            ),
            None
        );

        // The candidate is never the same number twice: re-asking at an
        // unchanged ceiling spends the same budget the same way.
        assert_eq!(retry_ceiling(u32::MAX, 0, known(200_000)), None);
        // Room for only a token or two is not worth a request.
        assert_eq!(retry_ceiling(31_999, 1_000, known(64_000)), Some(32_000));
        assert_eq!(retry_ceiling(32_768, 1_000, known(64_000)), None);
    }

    /// A provider outage ends the turn as `ApiUnavailable`, whatever the
    /// message says. The turn loop used to decide by reading the message —
    /// `e.starts_with("The AI service is temporarily unavailable")` — so the
    /// two halves of the engine were joined by a sentence.
    #[tokio::test]
    async fn an_unreachable_provider_is_reported_as_temporarily_unavailable() {
        /// Fails every request the way no network does.
        struct UnreachableMock;

        #[async_trait::async_trait]
        impl ApiClient for UnreachableMock {
            fn clone_box(&self) -> Box<dyn ApiClient> {
                Box::new(Self)
            }

            async fn create_message(
                &self,
                _system: &str,
                _messages: &[serde_json::Value],
                _model: &ModelId,
                _max_tokens: u32,
                _tools: &[crate::tool_executor::ToolInfo],
            ) -> Result<ApiResponse, crate::api_client::ApiError> {
                Err(crate::api_client::ApiError::Transport {
                    kind: crate::api_client::TransportError::Connect,
                    class: crate::api_client::ErrorClass::Connection,
                    // Deliberately not the sentence the loop used to look for.
                    message: "the far side is not answering".to_string(),
                })
            }

            async fn create_message_streaming(
                &self,
                system: &str,
                messages: &[serde_json::Value],
                model: &ModelId,
                max_tokens: u32,
                tools: &[crate::tool_executor::ToolInfo],
            ) -> std::pin::Pin<
                Box<dyn futures::Stream<Item = crate::api_client::StreamEvent> + Send + 'static>,
            > {
                match self
                    .create_message(system, messages, model, max_tokens, tools)
                    .await
                {
                    Ok(response) => Box::pin(futures::stream::iter(vec![
                        crate::api_client::StreamEvent::TextDelta(response.content),
                        crate::api_client::StreamEvent::StreamEnd,
                    ])),
                    Err(error) => Box::pin(futures::stream::iter(vec![
                        crate::api_client::StreamEvent::Error(error),
                    ])),
                }
            }
        }

        let (tx, _rx) = mpsc::unbounded_channel();
        let session = SessionState::new(std::path::PathBuf::from("/tmp/test"));
        let checker = PermissionChecker::new(PermissionMode::BypassPermissions);
        let mut engine = QueryEngine::new(
            Box::new(UnreachableMock),
            ModelId::sonnet(),
            session,
            PermissionEngine::new(checker),
            tx,
        );

        let reason = engine
            .submit("say something".into(), SubmitOptions::default())
            .await
            .unwrap();

        assert!(
            matches!(reason, TerminalReason::ApiUnavailable { .. }),
            "an unreachable provider came back as {reason:?}"
        );
    }

    /// An engine with the supplied API client and a hook executor that records
    /// lifecycle events.
    fn engine_with_recording_hooks(api: Box<dyn ApiClient>) -> (QueryEngine, RecordedEvents) {
        let (tx, _rx) = mpsc::unbounded_channel();
        let checker = PermissionChecker::new(PermissionMode::BypassPermissions);
        let mut engine = QueryEngine::new(
            api,
            ModelId::sonnet(),
            SessionState::new(std::path::PathBuf::from("/tmp/test")),
            PermissionEngine::new(checker),
            tx,
        );
        engine.set_tool_executor(Box::new(StubOkToolExecutor));
        let (hooks, recorded) = RecordingHooks::new();
        engine.set_hooks(Arc::new(hooks));
        (engine, recorded)
    }

    /// Both hooks the closing path owns must have run, with the reason the
    /// turn closed.
    fn assert_closing_hooks_ran(recorded: &RecordedEvents, stop_reason: &str) {
        let events = recorded
            .lock()
            .expect("the recording hook lock is not poisoned");
        let stop = events
            .iter()
            .find(|(name, _)| name == hook_events::STOP)
            .unwrap_or_else(|| panic!("the Stop hook did not fire: {events:?}"));
        let turn_end = events
            .iter()
            .find(|(name, _)| name == hook_events::TURN_END)
            .unwrap_or_else(|| panic!("the TurnEnd hook did not fire: {events:?}"));
        assert_eq!(
            stop.1.get("stop_reason").map(String::as_str),
            Some(stop_reason),
            "Stop saw the wrong stop reason: {events:?}"
        );
        assert_eq!(
            turn_end.1.get("stop_reason").map(String::as_str),
            Some(stop_reason),
            "TurnEnd saw the wrong stop reason: {events:?}"
        );
    }

    /// A fatal request error used to return straight out of `submit`, past the
    /// Stop/TurnEnd hooks and the cost and loop bookkeeping.
    #[tokio::test]
    async fn a_fatal_error_runs_stop_and_turn_end_hooks() {
        struct FatalMock;

        fn failure() -> crate::api_client::ApiError {
            crate::api_client::ApiError::Response {
                status: 400,
                message: "messages: field required".into(),
            }
        }

        #[async_trait::async_trait]
        impl ApiClient for FatalMock {
            fn clone_box(&self) -> Box<dyn ApiClient> {
                Box::new(Self)
            }

            async fn create_message(
                &self,
                _system: &str,
                _messages: &[serde_json::Value],
                _model: &ModelId,
                _max_tokens: u32,
                _tools: &[crate::tool_executor::ToolInfo],
            ) -> Result<ApiResponse, crate::api_client::ApiError> {
                Err(failure())
            }

            async fn create_message_streaming(
                &self,
                _system: &str,
                _messages: &[serde_json::Value],
                _model: &ModelId,
                _max_tokens: u32,
                _tools: &[crate::tool_executor::ToolInfo],
            ) -> std::pin::Pin<Box<dyn futures::Stream<Item = StreamEvent> + Send + 'static>>
            {
                Box::pin(futures::stream::iter(vec![StreamEvent::Error(failure())]))
            }
        }

        let (mut engine, recorded) = engine_with_recording_hooks(Box::new(FatalMock));
        let outcome = engine
            .submit("hello".into(), SubmitOptions::default())
            .await;

        assert!(
            outcome.is_err(),
            "a fatal error was reported as: {outcome:?}"
        );
        assert_closing_hooks_ran(&recorded, "error");
    }

    /// An unavailable provider is still an error the caller may recover from,
    /// but the turn it ended closes like any other.
    #[tokio::test]
    async fn an_api_unavailable_error_runs_stop_and_turn_end_hooks() {
        struct UnavailableMock;

        fn failure() -> crate::api_client::ApiError {
            crate::api_client::ApiError::RetryExhausted {
                attempts: 3,
                last_error_message: "connection refused".into(),
            }
        }

        #[async_trait::async_trait]
        impl ApiClient for UnavailableMock {
            fn clone_box(&self) -> Box<dyn ApiClient> {
                Box::new(Self)
            }

            async fn create_message(
                &self,
                _system: &str,
                _messages: &[serde_json::Value],
                _model: &ModelId,
                _max_tokens: u32,
                _tools: &[crate::tool_executor::ToolInfo],
            ) -> Result<ApiResponse, crate::api_client::ApiError> {
                Err(failure())
            }

            async fn create_message_streaming(
                &self,
                _system: &str,
                _messages: &[serde_json::Value],
                _model: &ModelId,
                _max_tokens: u32,
                _tools: &[crate::tool_executor::ToolInfo],
            ) -> std::pin::Pin<Box<dyn futures::Stream<Item = StreamEvent> + Send + 'static>>
            {
                Box::pin(futures::stream::iter(vec![StreamEvent::Error(failure())]))
            }
        }

        let (mut engine, recorded) = engine_with_recording_hooks(Box::new(UnavailableMock));
        let reason = engine
            .submit("hello".into(), SubmitOptions::default())
            .await
            .expect("provider unavailability remains a recoverable outcome");

        assert!(
            matches!(reason, TerminalReason::ApiUnavailable { .. }),
            "an unavailable provider came back as {reason:?}"
        );
        assert_closing_hooks_ran(&recorded, "api_unavailable");

        // The user is told the provider is unavailable, and the Notification
        // hook runs with the message that was shown.
        let events = recorded
            .lock()
            .expect("the recording hook lock is not poisoned");
        let notification = events
            .iter()
            .find(|(name, _)| name == hook_events::NOTIFICATION)
            .unwrap_or_else(|| panic!("the Notification hook did not fire: {events:?}"));
        assert!(
            notification
                .1
                .get("notification")
                .is_some_and(|message| message.contains("temporarily unavailable")),
            "the Notification hook saw the wrong message: {notification:?}"
        );
    }

    /// And a failure that is ours is not dressed up as a provider outage.
    #[tokio::test]
    async fn a_bad_request_is_not_reported_as_a_provider_outage() {
        struct RejectingMock;

        #[async_trait::async_trait]
        impl ApiClient for RejectingMock {
            fn clone_box(&self) -> Box<dyn ApiClient> {
                Box::new(Self)
            }

            async fn create_message(
                &self,
                _system: &str,
                _messages: &[serde_json::Value],
                _model: &ModelId,
                _max_tokens: u32,
                _tools: &[crate::tool_executor::ToolInfo],
            ) -> Result<ApiResponse, crate::api_client::ApiError> {
                Err(crate::api_client::ApiError::Response {
                    status: 400,
                    message: "messages: field required".to_string(),
                })
            }

            async fn create_message_streaming(
                &self,
                system: &str,
                messages: &[serde_json::Value],
                model: &ModelId,
                max_tokens: u32,
                tools: &[crate::tool_executor::ToolInfo],
            ) -> std::pin::Pin<
                Box<dyn futures::Stream<Item = crate::api_client::StreamEvent> + Send + 'static>,
            > {
                match self
                    .create_message(system, messages, model, max_tokens, tools)
                    .await
                {
                    Ok(response) => Box::pin(futures::stream::iter(vec![
                        crate::api_client::StreamEvent::TextDelta(response.content),
                        crate::api_client::StreamEvent::StreamEnd,
                    ])),
                    Err(error) => Box::pin(futures::stream::iter(vec![
                        crate::api_client::StreamEvent::Error(error),
                    ])),
                }
            }
        }

        let (tx, _rx) = mpsc::unbounded_channel();
        let session = SessionState::new(std::path::PathBuf::from("/tmp/test"));
        let checker = PermissionChecker::new(PermissionMode::BypassPermissions);
        let mut engine = QueryEngine::new(
            Box::new(RejectingMock),
            ModelId::sonnet(),
            session,
            PermissionEngine::new(checker),
            tx,
        );

        let outcome = engine
            .submit("say something".into(), SubmitOptions::default())
            .await;

        assert!(
            outcome.is_err(),
            "a bad request was reported as recoverable: {outcome:?}"
        );
    }

    #[test]
    fn a_turn_error_says_which_kind_it_is() {
        let unavailable = TurnError::ApiUnavailable {
            message: "later".into(),
        };
        assert!(matches!(
            unavailable.terminal_reason(),
            TerminalReason::ApiUnavailable { .. }
        ));
        assert_eq!(unavailable.message(), "later");

        let fatal = TurnError::Fatal {
            message: "our bug".into(),
        };
        assert!(matches!(fatal.terminal_reason(), TerminalReason::Error(_)));
        // A plain message from the engine's own layers is fatal.
        assert!(matches!(
            TurnError::from("tool failed".to_string()),
            TurnError::Fatal { .. }
        ));
    }

    /// The transition graph describes what a turn does: every step of a real
    /// turn is a legal move.
    #[test]
    fn the_legal_transitions_are_the_ones_a_turn_makes() {
        use TurnState as S;

        // The happy path, in order, twice round the tool loop.
        let path = [
            S::Initialize,
            S::BuildContext,
            S::RunPreSubmitHooks,
            S::StartRequest,
            S::StreamAssistant,
            S::ExecuteTools,
            S::ApplyToolResults,
            S::EvaluateContinuation,
            S::CompactRecovery,
            S::StartRequest,
            S::StreamAssistant,
            S::EvaluateContinuation,
            S::StopHooks,
            S::Finalize,
            S::Terminated,
        ];
        for pair in path.windows(2) {
            assert!(
                pair[0].can_transition_to(pair[1]),
                "{} → {} is not allowed",
                pair[0].as_str(),
                pair[1].as_str()
            );
        }

        // Waiting for a decision is a detour, not a dead end.
        assert!(S::ExecuteTools.can_transition_to(S::AwaitPermission));
        assert!(S::AwaitPermission.can_transition_to(S::ExecuteTools));
        assert!(S::StartRequest.can_transition_to(S::AwaitElicitation));
        assert!(S::AwaitElicitation.can_transition_to(S::StreamAssistant));

        // A turn can always end.
        for state in [
            S::BuildContext,
            S::StartRequest,
            S::StreamAssistant,
            S::ExecuteTools,
            S::AwaitPermission,
            S::AwaitElicitation,
            S::ApplyToolResults,
            S::EvaluateContinuation,
            S::CompactRecovery,
            S::StopHooks,
            S::Finalize,
        ] {
            assert!(
                state.can_transition_to(S::Terminated),
                "{} cannot end",
                state.as_str()
            );
        }
    }

    /// The graph is checked against a turn that really ran, not against a path
    /// written by hand to match it.
    #[tokio::test]
    async fn a_real_turn_only_makes_legal_moves() {
        struct ToolThenText {
            calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        }

        #[async_trait::async_trait]
        impl ApiClient for ToolThenText {
            fn clone_box(&self) -> Box<dyn ApiClient> {
                Box::new(Self {
                    calls: std::sync::Arc::clone(&self.calls),
                })
            }

            async fn create_message(
                &self,
                _system: &str,
                _messages: &[serde_json::Value],
                _model: &ModelId,
                _max_tokens: u32,
                _tools: &[crate::tool_executor::ToolInfo],
            ) -> Result<ApiResponse, crate::api_client::ApiError> {
                let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(ApiResponse {
                    content: if call == 0 {
                        String::new()
                    } else {
                        "done".into()
                    },
                    tool_calls: if call == 0 {
                        vec![crate::types::ToolCall {
                            id: "tc_1".into(),
                            name: "bash".into(),
                            input: serde_json::json!({"command": "true"}),
                        }]
                    } else {
                        Vec::new()
                    },
                    usage: UsageSummary {
                        input_tokens: 10,
                        output_tokens: 5,
                        cache_read_tokens: 0,
                        cache_creation_tokens: 0,
                    },
                    stop_reason: if call == 0 { "tool_use" } else { "end_turn" }.into(),
                    thinking: None,
                })
            }

            async fn create_message_streaming(
                &self,
                system: &str,
                messages: &[serde_json::Value],
                model: &ModelId,
                max_tokens: u32,
                tools: &[crate::tool_executor::ToolInfo],
            ) -> std::pin::Pin<
                Box<dyn futures::Stream<Item = crate::api_client::StreamEvent> + Send + 'static>,
            > {
                let response = self
                    .create_message(system, messages, model, max_tokens, tools)
                    .await
                    .expect("the mock answers");
                let mut events = Vec::new();
                if !response.content.is_empty() {
                    events.push(crate::api_client::StreamEvent::TextDelta(
                        response.content.clone(),
                    ));
                }
                events.push(crate::api_client::StreamEvent::StopReason(
                    response.stop_reason.clone(),
                ));
                for call in &response.tool_calls {
                    events.insert(
                        0,
                        crate::api_client::StreamEvent::ToolCallBlockStart {
                            id: call.id.clone(),
                            name: call.name.clone(),
                        },
                    );
                    events.insert(
                        1,
                        crate::api_client::StreamEvent::ToolCallBlockEnd {
                            id: call.id.clone(),
                            input: call.input.clone(),
                        },
                    );
                }
                events.push(crate::api_client::StreamEvent::StreamEnd);
                Box::pin(futures::stream::iter(events))
            }
        }

        let (tx, _rx) = mpsc::unbounded_channel();
        let session = SessionState::new(std::path::PathBuf::from("/tmp/test"));
        let checker = PermissionChecker::new(PermissionMode::BypassPermissions);
        let mut engine = QueryEngine::new(
            Box::new(ToolThenText {
                calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }),
            ModelId::sonnet(),
            session,
            PermissionEngine::new(checker),
            tx,
        );

        let _ = take_recorded_transitions();
        engine
            .submit_streaming("run something".into(), SubmitOptions::default())
            .await
            .unwrap();

        let recorded = take_recorded_transitions();
        assert!(recorded.len() > 5, "the turn barely moved: {recorded:?}");
        for (from, to) in &recorded {
            assert!(
                from.can_transition_to(*to),
                "a real turn went {} → {}, which the graph refuses",
                from.as_str(),
                to.as_str()
            );
        }
    }

    /// A real turn that goes through `CompactRecovery` also only makes legal
    /// moves. The recorder test above only exercised the tool loop; the
    /// compaction edges (`CompactRecovery -> StartRequest`,
    /// `StartRequest -> StopHooks`) are what this covers.
    #[tokio::test]
    async fn a_real_turn_through_compaction_only_makes_legal_moves() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let session = SessionState::new(std::path::PathBuf::from("/tmp/test"));
        let checker = PermissionChecker::new(PermissionMode::BypassPermissions);
        let mut engine = QueryEngine::new(
            Box::new(BudgetStreamMock),
            ModelId::sonnet(),
            session,
            PermissionEngine::new(checker),
            tx,
        );

        let mut budget = TokenBudget::with_defaults();
        budget.thresholds.max_tokens_per_session = 1;
        let mut turn = TurnRuntime::new(budget);

        let _ = take_recorded_transitions();
        let reason = engine
            .execute_turn_streaming(&mut turn, &SubmitOptions::default(), 1)
            .await
            .unwrap();
        assert_eq!(reason, TerminalReason::BudgetExhausted);

        let recorded = take_recorded_transitions();
        assert!(
            recorded
                .iter()
                .any(|(from, to)| matches!(from, TurnState::CompactRecovery)
                    && matches!(to, TurnState::StartRequest)),
            "the turn never took the compaction-retry edge: {recorded:?}"
        );
        assert!(
            recorded
                .iter()
                .any(|(_, to)| matches!(to, TurnState::StopHooks)),
            "the turn never entered the stop hooks: {recorded:?}"
        );
        for (from, to) in &recorded {
            assert!(
                from.can_transition_to(*to),
                "a real turn went {} → {}, which the graph refuses",
                from.as_str(),
                to.as_str()
            );
        }
    }

    /// A budget-exhausted turn still runs the Stop and TurnEnd hooks: it used
    /// to return straight out of the loop, past every closing step.
    #[tokio::test]
    async fn a_budget_exhausted_turn_runs_stop_and_turn_end_hooks() {
        let (mut engine, _rx, _session) = make_test_engine();
        let (hooks, recorded) = RecordingHooks::new();
        engine.set_hooks(Arc::new(hooks));
        // No recovery budget, so the turn takes the direct budget exit.
        engine.compact_config.max_recovery_retries = 0;

        let mut budget = TokenBudget::with_defaults();
        budget.thresholds.max_tokens_per_session = 1;
        let mut turn = TurnRuntime::new(budget);

        let reason = engine
            .execute_turn(&mut turn, &SubmitOptions::default(), 1)
            .await
            .unwrap();

        assert_eq!(reason, TerminalReason::BudgetExhausted);
        let names = recorded_event_names(&recorded);
        assert!(
            names.iter().any(|name| name == hook_events::STOP),
            "the Stop hook did not fire: {names:?}"
        );
        assert!(
            names.iter().any(|name| name == hook_events::TURN_END),
            "the TurnEnd hook did not fire: {names:?}"
        );
        assert!(matches!(turn.state, TurnState::Terminated));
        assert_eq!(turn.terminal_reason, Some(TerminalReason::BudgetExhausted));
    }

    /// The streaming path's post-recovery over-budget exit closes through the
    /// same hooks as a normal end of turn.
    #[tokio::test]
    async fn a_budget_exhausted_streaming_turn_runs_stop_and_turn_end_hooks() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let session = SessionState::new(std::path::PathBuf::from("/tmp/test"));
        let checker = PermissionChecker::new(PermissionMode::BypassPermissions);
        let mut engine = QueryEngine::new(
            Box::new(BudgetStreamMock),
            ModelId::sonnet(),
            session,
            PermissionEngine::new(checker),
            tx,
        );
        let (hooks, recorded) = RecordingHooks::new();
        engine.set_hooks(Arc::new(hooks));

        let mut budget = TokenBudget::with_defaults();
        budget.thresholds.max_tokens_per_session = 1;
        let mut turn = TurnRuntime::new(budget);

        let reason = engine
            .execute_turn_streaming(&mut turn, &SubmitOptions::default(), 1)
            .await
            .unwrap();

        assert_eq!(reason, TerminalReason::BudgetExhausted);
        let names = recorded_event_names(&recorded);
        assert!(
            names.iter().any(|name| name == hook_events::STOP),
            "the Stop hook did not fire: {names:?}"
        );
        assert!(
            names.iter().any(|name| name == hook_events::TURN_END),
            "the TurnEnd hook did not fire: {names:?}"
        );
        assert_eq!(turn.terminal_reason, Some(TerminalReason::BudgetExhausted));
    }

    /// A paused compaction still closes the turn through the hooks, from
    /// `CompactRecovery` (an edge the graph did not have). The budget is what
    /// ended the turn, so that is the reason reported; compaction's own
    /// message is not the turn's story.
    #[tokio::test]
    async fn a_recovery_failure_runs_stop_and_turn_end_hooks() {
        let (mut engine, _rx, _session) = make_test_engine();
        let (hooks, recorded) = RecordingHooks::new();
        engine.set_hooks(Arc::new(hooks));

        let mut budget = TokenBudget::with_defaults();
        budget.thresholds.max_tokens_per_session = 1;
        let mut turn = TurnRuntime::new(budget);
        turn.compact_failures = engine.compact_config.max_consecutive_failures;

        let reason = engine
            .execute_turn(&mut turn, &SubmitOptions::default(), 1)
            .await
            .unwrap();

        assert_eq!(reason, TerminalReason::BudgetExhausted, "{reason:?}");
        let names = recorded_event_names(&recorded);
        assert!(
            names.iter().any(|name| name == hook_events::STOP),
            "the Stop hook did not fire: {names:?}"
        );
        assert!(
            names.iter().any(|name| name == hook_events::TURN_END),
            "the TurnEnd hook did not fire: {names:?}"
        );
    }

    /// The exact graph, state by state.
    ///
    /// Written as the whole set rather than a few `assert!(!...)` lines: a
    /// spot check passes whether the graph is right or merely missing an
    /// entry, which is how `evaluate_continuation → stop_hooks` went unnoticed
    /// until a real turn was recorded.
    #[test]
    fn the_legal_moves_are_exactly_these() {
        use TurnState as S;

        let expected: &[(S, &[S])] = &[
            (S::Initialize, &[S::BuildContext, S::Terminated]),
            (S::BuildContext, &[S::RunPreSubmitHooks, S::Terminated]),
            (
                S::RunPreSubmitHooks,
                &[S::StartRequest, S::CompactRecovery, S::Terminated],
            ),
            (
                S::StartRequest,
                &[
                    S::StreamAssistant,
                    S::AwaitElicitation,
                    S::StopHooks,
                    S::Terminated,
                ],
            ),
            (
                S::StreamAssistant,
                &[
                    S::ExecuteTools,
                    S::EvaluateContinuation,
                    S::AwaitElicitation,
                    // The provider refused the request as too large: the one
                    // recovery not decided by this turn's own accounting.
                    S::CompactRecovery,
                    S::Terminated,
                ],
            ),
            (
                S::ExecuteTools,
                &[S::ApplyToolResults, S::AwaitPermission, S::Terminated],
            ),
            (
                S::AwaitPermission,
                &[S::ExecuteTools, S::ApplyToolResults, S::Terminated],
            ),
            (
                S::AwaitElicitation,
                &[S::StreamAssistant, S::StartRequest, S::Terminated],
            ),
            (
                S::ApplyToolResults,
                &[S::EvaluateContinuation, S::Terminated],
            ),
            (
                S::EvaluateContinuation,
                &[S::CompactRecovery, S::StopHooks, S::Terminated],
            ),
            (
                S::CompactRecovery,
                &[S::StartRequest, S::StopHooks, S::Finalize, S::Terminated],
            ),
            (S::StopHooks, &[S::Finalize, S::Terminated]),
            (S::Finalize, &[S::Terminated]),
            (S::Terminated, &[S::Terminated]),
        ];

        let all = [
            S::Initialize,
            S::BuildContext,
            S::RunPreSubmitHooks,
            S::StartRequest,
            S::StreamAssistant,
            S::ExecuteTools,
            S::AwaitPermission,
            S::AwaitElicitation,
            S::ApplyToolResults,
            S::EvaluateContinuation,
            S::CompactRecovery,
            S::StopHooks,
            S::Finalize,
            S::Terminated,
        ];

        for (from, allowed) in expected {
            for to in all {
                assert_eq!(
                    from.can_transition_to(to),
                    allowed.contains(&to),
                    "{} → {} should be {}",
                    from.as_str(),
                    to.as_str(),
                    if allowed.contains(&to) {
                        "allowed"
                    } else {
                        "refused"
                    }
                );
            }
        }
    }

    #[test]
    fn an_illegal_transition_is_reported_and_does_not_stop_the_turn() {
        let budget = TokenBudget::with_defaults();
        let mut turn = TurnRuntime::new(budget);

        // Nothing legitimate goes straight from a fresh turn to the end.
        assert!(!turn.state.can_transition_to(TurnState::Finalize));
        turn.transition_to(TurnState::Finalize);

        // It moved, and the turn is not poisoned by the report.
        assert!(matches!(turn.state, TurnState::Finalize));
        turn.transition_to(TurnState::Terminated);
        assert!(matches!(turn.state, TurnState::Terminated));
    }

    /// `interrupt()` sets the flag the turn loop reads, not only the event.
    /// The event is what a UI listens for; the flag is what stops the work.
    #[tokio::test]
    async fn interrupting_the_engine_sets_the_flag_it_reads() {
        let (engine, _rx, _session) = make_test_engine();
        assert!(!engine.interrupt_handle().is_interrupted());

        engine.interrupt();

        assert!(engine.interrupt_handle().is_interrupted());
    }

    /// A poll whose answer keeps changing is progress, not a loop.
    ///
    /// Waiting for a long install is done by asking the same question
    /// repeatedly — `sleep 55; tail -3 install.log` — and the answer moves on
    /// each time as the download progresses. The detector already knew that
    /// (`from_tool_call_and_result`), but the engine never handed it the
    /// answers: `close_turn` reads results from `turn.messages`, and the
    /// streaming path wrote them only to `session.messages`. Every turn's
    /// fingerprint was therefore built from its calls alone, and three
    /// identical-looking polls ended the run.
    #[tokio::test]
    async fn polling_a_changing_answer_is_not_treated_as_a_loop() {
        /// Answers with a poll call whose result differs every time, then
        /// finishes.
        struct PollThenFinish {
            calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        }

        #[async_trait::async_trait]
        impl ApiClient for PollThenFinish {
            fn clone_box(&self) -> Box<dyn ApiClient> {
                Box::new(Self {
                    calls: std::sync::Arc::clone(&self.calls),
                })
            }

            async fn create_message(
                &self,
                _system: &str,
                _messages: &[serde_json::Value],
                _model: &ModelId,
                _max_tokens: u32,
                _tools: &[crate::tool_executor::ToolInfo],
            ) -> Result<ApiResponse, crate::api_client::ApiError> {
                let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(ApiResponse {
                    // The final answer must carry text: an empty one leaves the
                    // last message a tool result, which the loop reads as "the
                    // model has not seen it yet" and keeps running.
                    content: if call < 4 {
                        String::new()
                    } else {
                        "installed".into()
                    },
                    tool_calls: if call < 4 {
                        vec![crate::types::ToolCall {
                            id: format!("tc_{call}"),
                            name: "bash".into(),
                            // The model literally repeats the same command:
                            // the input carries no turn number.
                            input: serde_json::json!({"command": "sleep 55; tail -3 install.log"}),
                        }]
                    } else {
                        Vec::new()
                    },
                    usage: UsageSummary {
                        input_tokens: 10,
                        output_tokens: 5,
                        cache_read_tokens: 0,
                        cache_creation_tokens: 0,
                    },
                    stop_reason: if call < 4 { "tool_use" } else { "end_turn" }.into(),
                    thinking: None,
                })
            }

            async fn create_message_streaming(
                &self,
                system: &str,
                messages: &[serde_json::Value],
                model: &ModelId,
                max_tokens: u32,
                tools: &[crate::tool_executor::ToolInfo],
            ) -> std::pin::Pin<
                Box<dyn futures::Stream<Item = crate::api_client::StreamEvent> + Send + 'static>,
            > {
                let response = self
                    .create_message(system, messages, model, max_tokens, tools)
                    .await
                    .expect("the mock answers");
                let mut events = Vec::new();
                if !response.content.is_empty() {
                    events.push(crate::api_client::StreamEvent::TextDelta(
                        response.content.clone(),
                    ));
                }
                events.push(crate::api_client::StreamEvent::StopReason(
                    response.stop_reason.clone(),
                ));
                for call in &response.tool_calls {
                    events.insert(
                        0,
                        crate::api_client::StreamEvent::ToolCallBlockStart {
                            id: call.id.clone(),
                            name: call.name.clone(),
                        },
                    );
                    events.insert(
                        1,
                        crate::api_client::StreamEvent::ToolCallBlockEnd {
                            id: call.id.clone(),
                            input: call.input.clone(),
                        },
                    );
                }
                events.push(crate::api_client::StreamEvent::StreamEnd);
                Box::pin(futures::stream::iter(events))
            }
        }

        /// Returns a different progress line on every call.
        struct ProgressingInstall(std::sync::Arc<std::sync::atomic::AtomicUsize>);

        #[async_trait::async_trait]
        impl crate::tool_executor::ToolExecutor for ProgressingInstall {
            async fn execute(
                &self,
                tool_call_id: &str,
                _tool_name: &str,
                _input: &serde_json::Value,
            ) -> crate::tool_executor::ToolDispatchOutcome {
                let step = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                crate::tool_executor::ToolDispatchOutcome {
                    result: ToolResult {
                        tool_call_id: tool_call_id.to_string(),
                        output: format!("downloaded {step} of 40 MB"),
                        is_error: false,
                        images: Vec::new(),
                    },
                }
            }

            fn list_tools(&self) -> Vec<crate::tool_executor::ToolInfo> {
                Vec::new()
            }
        }

        let (tx, _rx) = mpsc::unbounded_channel();
        let session = SessionState::new(std::path::PathBuf::from("/tmp/test"));
        let checker = PermissionChecker::new(PermissionMode::BypassPermissions);
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut engine = QueryEngine::new(
            Box::new(PollThenFinish {
                calls: std::sync::Arc::clone(&calls),
            }),
            ModelId::sonnet(),
            session,
            PermissionEngine::new(checker),
            tx,
        );
        engine.set_tool_executor(Box::new(ProgressingInstall(std::sync::Arc::new(
            std::sync::atomic::AtomicUsize::new(0),
        ))));

        let reason = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            engine.submit_streaming("install it".into(), SubmitOptions::default()),
        )
        .await
        .expect("the run must finish; a hang here is the bug this test exists for")
        .expect("the run finishes");

        assert!(
            !matches!(reason, TerminalReason::LoopDetected { .. }),
            "a poll whose answer changes was called a loop: {reason:?}"
        );
        assert_eq!(
            reason,
            TerminalReason::Completed,
            "the run did not finish normally: {reason:?}"
        );
    }

    /// An interrupt that lands *between* turns — while a tool is running —
    /// stops the run before the next request is sent. This is the window the
    /// loop-top check exists for: without it the engine would ask the model
    /// for another answer after the person had asked it to stop.
    #[tokio::test]
    async fn an_interrupt_between_turns_stops_before_the_next_request() {
        /// Answers with a tool call once, then with text.
        struct ToolThenText {
            calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        }

        #[async_trait::async_trait]
        impl ApiClient for ToolThenText {
            fn clone_box(&self) -> Box<dyn ApiClient> {
                Box::new(Self {
                    calls: std::sync::Arc::clone(&self.calls),
                })
            }

            async fn create_message(
                &self,
                _system: &str,
                _messages: &[serde_json::Value],
                _model: &ModelId,
                _max_tokens: u32,
                _tools: &[crate::tool_executor::ToolInfo],
            ) -> Result<ApiResponse, crate::api_client::ApiError> {
                let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(ApiResponse {
                    content: if call == 0 {
                        String::new()
                    } else {
                        "done".into()
                    },
                    tool_calls: if call == 0 {
                        vec![crate::types::ToolCall {
                            id: "tc_1".into(),
                            name: "bash".into(),
                            input: serde_json::json!({"command": "true"}),
                        }]
                    } else {
                        Vec::new()
                    },
                    usage: UsageSummary {
                        input_tokens: 10,
                        output_tokens: 5,
                        cache_read_tokens: 0,
                        cache_creation_tokens: 0,
                    },
                    stop_reason: if call == 0 { "tool_use" } else { "end_turn" }.into(),
                    thinking: None,
                })
            }

            async fn create_message_streaming(
                &self,
                system: &str,
                messages: &[serde_json::Value],
                model: &ModelId,
                max_tokens: u32,
                tools: &[crate::tool_executor::ToolInfo],
            ) -> std::pin::Pin<
                Box<dyn futures::Stream<Item = crate::api_client::StreamEvent> + Send + 'static>,
            > {
                let response = self
                    .create_message(system, messages, model, max_tokens, tools)
                    .await
                    .expect("the mock answers");
                let mut events = Vec::new();
                if !response.content.is_empty() {
                    events.push(crate::api_client::StreamEvent::TextDelta(
                        response.content.clone(),
                    ));
                }
                events.push(crate::api_client::StreamEvent::StopReason(
                    response.stop_reason.clone(),
                ));
                for call in &response.tool_calls {
                    // A call needs its start, its input, and its end: the
                    // assembler builds the call from the whole sequence.
                    events.insert(
                        0,
                        crate::api_client::StreamEvent::ToolCallBlockStart {
                            id: call.id.clone(),
                            name: call.name.clone(),
                        },
                    );
                    events.insert(
                        1,
                        crate::api_client::StreamEvent::ToolCallBlockEnd {
                            id: call.id.clone(),
                            input: call.input.clone(),
                        },
                    );
                }
                events.push(crate::api_client::StreamEvent::StreamEnd);
                Box::pin(futures::stream::iter(events))
            }
        }

        /// Interrupts the engine when the tool runs — the person pressing
        /// Ctrl+C while something is executing.
        struct InterruptingTools(InterruptHandle);

        #[async_trait::async_trait]
        impl crate::tool_executor::ToolExecutor for InterruptingTools {
            async fn execute(
                &self,
                tool_call_id: &str,
                _tool_name: &str,
                _input: &serde_json::Value,
            ) -> crate::tool_executor::ToolDispatchOutcome {
                self.0.interrupt();
                crate::tool_executor::ToolDispatchOutcome {
                    result: ToolResult {
                        tool_call_id: tool_call_id.to_string(),
                        output: "the tool ran".to_string(),
                        is_error: false,
                        images: Vec::new(),
                    },
                }
            }

            fn list_tools(&self) -> Vec<crate::tool_executor::ToolInfo> {
                Vec::new()
            }
        }

        let (tx, _rx) = mpsc::unbounded_channel();
        let session = SessionState::new(std::path::PathBuf::from("/tmp/test"));
        let checker = PermissionChecker::new(PermissionMode::BypassPermissions);
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut engine = QueryEngine::new(
            Box::new(ToolThenText {
                calls: std::sync::Arc::clone(&calls),
            }),
            ModelId::sonnet(),
            session,
            PermissionEngine::new(checker),
            tx,
        );
        engine.set_tool_executor(Box::new(InterruptingTools(engine.interrupt_handle())));

        let reason = engine
            .submit_streaming("run something".into(), SubmitOptions::default())
            .await
            .unwrap();

        assert!(
            matches!(reason, TerminalReason::UserInterrupted),
            "expected an interrupt, got {reason:?}"
        );
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the model was asked for another answer after the interrupt"
        );
    }

    #[tokio::test]
    async fn an_interrupt_ends_the_turn_it_arrives_during() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let session = SessionState::new(std::path::PathBuf::from("/tmp/test"));
        let checker = PermissionChecker::new(PermissionMode::BypassPermissions);
        let mut engine = QueryEngine::new(
            Box::new(MockApiClient),
            ModelId::sonnet(),
            session,
            PermissionEngine::new(checker),
            tx,
        );

        let handle = engine.interrupt_handle();
        engine.set_api_client(Box::new(InterruptingMock {
            handle,
            calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }));

        let reason = engine
            .submit_streaming("say something".into(), SubmitOptions::default())
            .await
            .unwrap();

        assert!(
            matches!(reason, TerminalReason::UserInterrupted),
            "expected an interrupt, got {reason:?}"
        );
    }

    /// And the next turn runs: a request that arrives after an interrupt is a
    /// fresh one, not a continuation of the stopped one.
    #[tokio::test]
    async fn a_turn_after_an_interrupt_still_runs() {
        let (mut engine, _rx, _session) = make_test_engine();
        engine.interrupt();
        let _ = engine
            .submit_streaming("first".into(), SubmitOptions::default())
            .await;

        let reason = engine
            .submit_streaming("second".into(), SubmitOptions::default())
            .await
            .unwrap();

        assert!(
            !matches!(reason, TerminalReason::UserInterrupted),
            "the interrupt outlived the turn it was meant for: {reason:?}"
        );
    }

    /// An interrupt that lands before the tools run stops them: the person
    /// asked for the turn to end, not for one more batch of edits.
    #[tokio::test]
    async fn an_interrupt_stops_the_tools_from_running() {
        let (mut engine, _rx, _session) = make_test_engine();
        let budget = TokenBudget::with_defaults();
        let mut turn = TurnRuntime::new(budget);
        turn.pending_tool_calls.push(PendingToolCall {
            id: "tc_1".into(),
            name: "bash".into(),
            input: serde_json::json!({"command": "touch /tmp/should-not-exist"}),
        });

        engine.interrupt_handle().interrupt();
        let results = engine
            .execute_tool_calls(&mut turn, &SubmitOptions::default())
            .await
            .unwrap();

        assert_eq!(results.len(), 1);
        assert!(results[0].is_error);
        assert!(
            results[0].output.contains("interrupted"),
            "{}",
            results[0].output
        );
    }

    /// `TurnState::StopHooks` is entered before a turn closes, and the Stop
    /// hook fires in it — the state and the event were both declared, and
    /// neither happened.
    #[tokio::test]
    async fn a_finished_turn_runs_the_stop_hooks() {
        let (engine, _rx, _session) = make_test_engine();
        let budget = TokenBudget::with_defaults();
        let mut turn = TurnRuntime::new(budget);

        engine.run_stop_hooks(&mut turn, 1, "end_turn").await;

        assert!(
            matches!(turn.state, TurnState::StopHooks),
            "{:?}",
            turn.state
        );
    }

    #[tokio::test]
    async fn execute_tool_calls_with_bypass_permission() {
        let (mut engine, _rx, _session) = make_test_engine();
        let budget = TokenBudget::with_defaults();
        let mut turn = TurnRuntime::new(budget);
        turn.pending_tool_calls.push(PendingToolCall {
            id: "tc_mock".into(),
            name: "read".into(),
            input: serde_json::json!({"path": "test.txt"}),
        });
        let opts = SubmitOptions::default();

        let results = engine.execute_tool_calls(&mut turn, &opts).await;
        assert!(results.is_ok());
        let results = results.unwrap();
        assert_eq!(results.len(), 1);
        assert!(!results[0].is_error);
    }

    #[tokio::test]
    async fn api_unavailable_returns_graceful_reason() {
        struct FailingMock;

        #[async_trait::async_trait]
        impl ApiClient for FailingMock {
            fn clone_box(&self) -> Box<dyn ApiClient> {
                Box::new(Self)
            }

            async fn create_message(
                &self,
                _system: &str,
                _messages: &[serde_json::Value],
                _model: &ModelId,
                _max_tokens: u32,
                _tools: &[crate::tool_executor::ToolInfo],
            ) -> Result<ApiResponse, crate::api_client::ApiError> {
                Err(crate::api_client::ApiError::RetryExhausted {
                    attempts: 3,
                    last_error_message: "connection refused".into(),
                })
            }

            async fn create_message_streaming(
                &self,
                _system: &str,
                _messages: &[serde_json::Value],
                _model: &ModelId,
                _max_tokens: u32,
                _tools: &[crate::tool_executor::ToolInfo],
            ) -> std::pin::Pin<Box<dyn futures::Stream<Item = StreamEvent> + Send + 'static>>
            {
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                let _ = tx.send(StreamEvent::StreamEnd);
                Box::pin(futures::stream::unfold(rx, |mut rx| async move {
                    rx.recv().await.map(|item| (item, rx))
                }))
            }
        }

        let (tx, mut rx) = mpsc::unbounded_channel();
        let checker = PermissionChecker::new(PermissionMode::BypassPermissions);
        let perm_engine = PermissionEngine::new(checker);
        let session = SessionState::new(std::path::PathBuf::from("/tmp/test"));
        let mut engine = QueryEngine::new(
            Box::new(FailingMock),
            ModelId::sonnet(),
            session,
            perm_engine,
            tx,
        );
        engine.set_tool_executor(Box::new(StubOkToolExecutor));

        let options = SubmitOptions {
            handle_elicitation: false,
            ..Default::default()
        };

        let result = engine.submit("hello".into(), options).await;

        assert!(
            matches!(result, Ok(TerminalReason::ApiUnavailable { .. })),
            "expected ApiUnavailable, got {result:?}"
        );

        // Verify a Notification event was emitted
        let mut found_notification = false;
        while let Ok(event) = rx.try_recv() {
            if matches!(event, EngineEvent::Notification { .. }) {
                found_notification = true;
                break;
            }
        }
        assert!(
            found_notification,
            "expected Notification event to be emitted"
        );
    }

    #[tokio::test]
    async fn execute_tool_calls_respects_allowed_tools_override() {
        let (mut engine, _rx, _session) = make_test_engine();
        let budget = TokenBudget::with_defaults();
        let mut turn = TurnRuntime::new(budget);
        turn.pending_tool_calls.push(PendingToolCall {
            id: "tc_blocked".into(),
            name: "write".into(),
            input: serde_json::json!({"path": "blocked.txt"}),
        });

        let opts = SubmitOptions {
            allowed_tools_override: Some(vec!["read".into()]),
            ..SubmitOptions::default()
        };

        let results = engine.execute_tool_calls(&mut turn, &opts).await.unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].is_error);
        assert!(results[0].output.contains("blocked"));
    }

    #[tokio::test]
    async fn empty_streaming_response_fails_without_persisting_a_message() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let checker = PermissionChecker::new(PermissionMode::BypassPermissions);
        let perm_engine = PermissionEngine::new(checker);
        let session = SessionState::new(std::path::PathBuf::from("/tmp/test"));
        let mut engine = QueryEngine::new(
            Box::new(EmptyStreamingApiClient),
            ModelId::sonnet(),
            session,
            perm_engine,
            tx,
        );
        engine.set_tool_executor(Box::new(StubOkToolExecutor));

        // An empty answer is a failed turn (it is not a finished one), and it
        // must not leave an assistant message behind: the next request starts
        // from the user's text, not from an empty reply.
        let first = engine
            .submit_streaming("first".into(), SubmitOptions::default())
            .await;
        assert!(
            first.is_err(),
            "an empty answer must not succeed: {first:?}"
        );
        let second = engine
            .submit_streaming("second".into(), SubmitOptions::default())
            .await;
        assert!(
            second.is_err(),
            "an empty answer must not succeed: {second:?}"
        );

        let roles = engine
            .session
            .messages
            .iter()
            .map(|msg| msg.role)
            .collect::<Vec<_>>();
        assert_eq!(
            roles,
            vec![crate::types::Role::User, crate::types::Role::User]
        );
    }

    #[test]
    fn pending_tool_call_from_tool_call() {
        let tc = ToolCall {
            id: "tc_x".into(),
            name: "write".into(),
            input: serde_json::json!({"path": "out.txt", "content": "hi"}),
        };
        let pending = PendingToolCall::from(&tc);
        assert_eq!(pending.id, "tc_x");
        assert_eq!(pending.name, "write");
        assert_eq!(pending.input["path"], "out.txt");
    }

    #[test]
    fn turn_runtime_recovery_count_starts_at_zero() {
        let budget = TokenBudget::with_defaults();
        let turn = TurnRuntime::new(budget);
        assert_eq!(turn.recovery_count, 0);
        assert!(!turn.stop_hook_active);
    }

    // ─── Structured Output Validation ─────────────────────────────────────

    #[test]
    fn structured_output_validator_validates_valid_json() {
        let schema = serde_json::json!({
            "type": "object",
            "required": ["name", "age"],
            "properties": {
                "name": { "type": "string" },
                "age": { "type": "number" }
            }
        });
        let validator = StructuredOutputValidator::new(schema);
        let output = r#"{"name": "Alice", "age": 30}"#;
        let result = validator.validate(output, 0);
        assert!(matches!(result, ValidationResult::Valid(_)));
    }

    #[test]
    fn structured_output_validator_rejects_invalid_json() {
        let schema = serde_json::json!({
            "type": "object",
            "required": ["name"],
            "properties": {
                "name": { "type": "string" }
            }
        });
        let validator = StructuredOutputValidator::new(schema);
        let result = validator.validate("not json at all", 0);
        assert!(matches!(result, ValidationResult::Invalid { .. }));
    }

    #[test]
    fn structured_output_validator_rejects_missing_required_field() {
        let schema = serde_json::json!({
            "type": "object",
            "required": ["name", "email"],
            "properties": {
                "name": { "type": "string" },
                "email": { "type": "string" }
            }
        });
        let validator = StructuredOutputValidator::new(schema);
        let result = validator.validate(r#"{"name": "Alice"}"#, 0);
        assert!(matches!(result, ValidationResult::Invalid { .. }));
    }

    #[test]
    fn structured_output_validator_rejects_wrong_type() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "count": { "type": "number" }
            }
        });
        let validator = StructuredOutputValidator::new(schema);
        let result = validator.validate(r#"{"count": "not a number"}"#, 0);
        assert!(matches!(result, ValidationResult::Invalid { .. }));
    }

    #[test]
    fn structured_output_validator_respects_max_retries() {
        let schema = serde_json::json!({ "type": "object" });
        let validator = StructuredOutputValidator::new(schema).with_max_retries(2);
        let result = validator.validate("bad", 2);
        assert!(matches!(result, ValidationResult::Invalid { retries, .. } if retries == 2));
    }

    #[test]
    fn structured_output_validator_retry_instruction() {
        let schema = serde_json::json!({ "type": "object" });
        let validator = StructuredOutputValidator::new(schema);
        let instruction = validator.retry_instruction("missing required field: name");
        assert!(instruction.contains("missing required field: name"));
        assert!(instruction.contains("JSON schema"));
    }

    // ─── Permission Denial Tracking ─────────────────────────────────────

    #[test]
    fn query_engine_tracks_denied_tools() {
        let (mut engine, _rx, _session) = make_test_engine();
        engine.denied_tools.push(DeniedToolCall {
            name: "bash".into(),
            reason: "requires approval".into(),
        });
        assert_eq!(engine.denied_tools.len(), 1);
        assert_eq!(engine.denied_tools[0].name, "bash");
    }

    #[test]
    fn query_engine_denied_tools_summary_format() {
        let (engine, _rx, _session) = make_test_engine();
        let summary = engine.format_denied_tools_summary();
        assert!(summary.contains("The following tool calls"));
        assert!(summary.contains("denied"));

        // With denied tools
        let (mut engine, _rx, _session) = make_test_engine();
        engine.denied_tools.push(DeniedToolCall {
            name: "write".into(),
            reason: "permission denied".into(),
        });
        let summary = engine.format_denied_tools_summary();
        assert!(summary.contains("`write`"));
        assert!(summary.contains("permission denied"));
    }

    #[test]
    fn query_engine_clear_denied_tools() {
        let (mut engine, _rx, _session) = make_test_engine();
        engine.denied_tools.push(DeniedToolCall {
            name: "test".into(),
            reason: "test".into(),
        });
        engine.clear_denied_tools();
        assert!(engine.denied_tools().is_empty());
    }

    // ─── Elevation Flow ────────────────────────────────────────────────────

    /// The wiring, not just the helper: a turn that belongs to an agent runs
    /// its tools through the permission engine with a background context, and
    /// the hook that refuses tools in a background run refuses this one.
    /// The wiring, not just the helper: a turn that belongs to an agent runs
    /// its tools with a background context, and the hook that refuses tools in
    /// a background run refuses this one. The same call in the person's own
    /// turn is allowed.
    #[tokio::test]
    async fn a_background_turn_has_its_tools_denied_by_the_hook() {
        let (mut engine, _rx, _session) = make_test_engine();
        // As the session does.
        engine.permission_engine.add_hook(std::sync::Arc::new(
            crate::permission_engine::BackgroundAgentDenyHook,
        ));
        let budget = TokenBudget::with_defaults();

        let background = SubmitOptions {
            agent_id: Some("task_7".into()),
            ..SubmitOptions::default()
        };
        let mut turn = TurnRuntime::new(budget.clone());
        turn.agent_id = background.agent_id.clone();
        turn.pending_tool_calls.push(PendingToolCall {
            id: "tc_1".into(),
            name: "bash".into(),
            input: serde_json::json!({"command": "whoami"}),
        });

        let denied = engine
            .execute_tool_calls(&mut turn, &background)
            .await
            .unwrap();
        assert_eq!(denied.len(), 1);
        assert!(denied[0].is_error, "{:?}", denied[0]);
        assert!(
            denied[0].output.contains("background"),
            "expected a denial naming the background run, got {}",
            denied[0].output
        );

        // The same call in the person's own turn runs.
        let interactive = SubmitOptions::default();
        let mut turn = TurnRuntime::new(budget);
        turn.pending_tool_calls.push(PendingToolCall {
            id: "tc_2".into(),
            name: "bash".into(),
            input: serde_json::json!({"command": "whoami"}),
        });

        let allowed = engine
            .execute_tool_calls(&mut turn, &interactive)
            .await
            .unwrap();
        assert_eq!(allowed.len(), 1);
        assert!(!allowed[0].is_error, "{:?}", allowed[0]);
    }

    /// The hook that refuses tools in a background run reads `execution_mode`
    /// from the permission context. Every call site used to hand it an empty
    /// map, so it could never fire; these pin what it is handed now.
    #[test]
    fn a_permission_context_says_which_run_it_belongs_to() {
        let interactive = permission_context_for(None, "tc_1");
        assert_eq!(interactive["execution_mode"], "interactive");
        assert_eq!(interactive["tool_call_id"], "tc_1");
        assert!(!interactive.contains_key("agent_id"));

        let background = permission_context_for(Some("task_7"), "tc_2");
        assert_eq!(background["execution_mode"], "background");
        assert_eq!(background["agent_id"], "task_7");
    }

    /// And the hook acts on it.
    #[tokio::test]
    async fn the_background_deny_hook_fires_when_the_run_is_a_background_one() {
        use crate::permission_engine::PermissionRequest;
        use crate::permission_engine::{BackgroundAgentDenyHook, PermissionHook};
        use crate::permissions::PermissionMode;

        let hook = BackgroundAgentDenyHook;
        let request = |context: std::collections::HashMap<String, String>| PermissionRequest {
            id: "req_1".into(),
            tool_name: "Bash".into(),
            arguments: "{}".into(),
            path: None,
            context,
            created_at: chrono::Utc::now(),
        };

        let denied = hook
            .evaluate(
                &request(permission_context_for(Some("task_7"), "tc_1")),
                PermissionMode::Default,
            )
            .await
            .unwrap();
        assert!(
            matches!(
                denied,
                Some(crate::permissions::PermissionResult::Deny { .. })
            ),
            "{denied:?}"
        );

        let allowed = hook
            .evaluate(
                &request(permission_context_for(None, "tc_1")),
                PermissionMode::Default,
            )
            .await
            .unwrap();
        assert!(allowed.is_none(), "{allowed:?}");
    }

    #[tokio::test]
    async fn resolve_elevation_approved_succeeds_with_bypass() {
        let (mut engine, _rx, _session) = make_test_engine();
        let budget = TokenBudget::with_defaults();
        let mut turn = TurnRuntime::new(budget);
        turn.pending_elevation = Some(PendingElevation {
            tool_call_id: "tc_elev".into(),
            tool_name: "bash".into(),
            input: serde_json::json!({"command": "whoami"}),
            request_id: "elev_tc_elev".into(),
            reason: "test elevation".into(),
        });

        let result = engine.resolve_elevation(&mut turn, true).await;
        assert!(result.is_ok());
        let result = result.unwrap();
        assert!(result.is_some());
        let r = result.unwrap();
        assert!(!r.is_error);
    }

    #[tokio::test]
    async fn resolve_elevation_denied_returns_error() {
        let (mut engine, _rx, _session) = make_test_engine();
        let budget = TokenBudget::with_defaults();
        let mut turn = TurnRuntime::new(budget);
        turn.pending_elevation = Some(PendingElevation {
            tool_call_id: "tc_elev".into(),
            tool_name: "write".into(),
            input: serde_json::json!({}),
            request_id: "elev_tc_elev".into(),
            reason: "test elevation".into(),
        });

        let result = engine.resolve_elevation(&mut turn, false).await;
        assert!(result.is_ok());
        let result = result.unwrap().unwrap();
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn resolve_elevation_no_pending_returns_none() {
        let (mut engine, _rx, _session) = make_test_engine();
        let budget = TokenBudget::with_defaults();
        let mut turn = TurnRuntime::new(budget);

        let result = engine.resolve_elevation(&mut turn, true).await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[tokio::test]
    async fn execute_tool_calls_drives_every_elevation_through_the_confirmation_bus() {
        let (mut engine, mut rx, _session) = make_test_engine();
        engine
            .permission_checker()
            .lock()
            .unwrap()
            .set_mode(PermissionMode::Default);
        let bus = ConfirmationBus::new(4);
        engine.set_confirmation_bus(bus.clone());

        let responder = tokio::spawn(async move {
            for expected_id in ["elev_tc_one", "elev_tc_two"] {
                let request = bus.next_request().await.expect("elevation request");
                assert_eq!(request.id, expected_id);
                assert!(!request.allow_session);
                assert!(request.reason.contains("sudo"));
                assert!(
                    bus.respond(&request.id, ConfirmationResponse::Approved)
                        .await
                );
            }
        });

        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        turn.pending_tool_calls = vec![
            PendingToolCall {
                id: "tc_one".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "sudo whoami"}),
            },
            PendingToolCall {
                id: "tc_two".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "sudo id"}),
            },
        ];

        let results = engine
            .execute_tool_calls(&mut turn, &SubmitOptions::default())
            .await
            .unwrap();
        responder.await.unwrap();

        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|result| !result.is_error));
        assert!(turn.pending_elevation.is_none());
        assert_eq!(
            engine.permission_checker().lock().unwrap().mode(),
            PermissionMode::Default
        );

        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, EngineEvent::ToolCallStart { .. }))
                .count(),
            2,
            "retries must not duplicate ToolCallStart: {events:?}"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, EngineEvent::ElevationRequest { .. }))
                .count(),
            2
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, EngineEvent::ToolCallEnd { .. }))
                .count(),
            2,
            "each elevated call must finish exactly once: {events:?}"
        );
    }

    #[tokio::test]
    async fn elevation_without_a_confirmation_channel_is_not_a_user_denial() {
        let (mut engine, mut rx, _session) = make_test_engine();
        engine
            .permission_checker()
            .lock()
            .unwrap()
            .set_mode(PermissionMode::Default);
        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        turn.pending_tool_calls = vec![PendingToolCall {
            id: "tc_no_bus".into(),
            name: "bash".into(),
            input: serde_json::json!({"command": "sudo whoami"}),
        }];

        let results = engine
            .execute_tool_calls(&mut turn, &SubmitOptions::default())
            .await
            .unwrap();

        assert_eq!(results.len(), 1);
        assert!(results[0].is_error);
        assert!(results[0].output.contains("no confirmation channel"));
        assert!(!results[0].output.contains("denied by user"));
        assert!(engine.denied_tools().is_empty());
        assert!(turn.pending_elevation.is_none());

        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, EngineEvent::ToolCallEnd { .. }))
                .count(),
            1,
            "an unresolved call still needs a terminal event: {events:?}"
        );
    }

    #[tokio::test]
    async fn early_streaming_elevation_waits_for_approval_and_executes() {
        let (mut engine, _rx, _session) = make_test_engine();
        engine
            .permission_checker()
            .lock()
            .unwrap()
            .set_mode(PermissionMode::Default);
        let bus = ConfirmationBus::new(1);
        engine.set_confirmation_bus(bus.clone());
        let responder = tokio::spawn(async move {
            let request = bus.next_request().await.expect("elevation request");
            assert_eq!(request.id, "elev_tc_early");
            bus.respond(&request.id, ConfirmationResponse::Approved)
                .await;
        });
        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());

        let result = engine
            .execute_early_tool_call(
                &mut turn,
                ToolCall {
                    id: "tc_early".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "sudo whoami"}),
                },
                &SubmitOptions::default(),
            )
            .await
            .unwrap()
            .expect("resolved tool result");
        responder.await.unwrap();

        assert!(!result.is_error, "{}", result.output);
        assert_eq!(result.output, "stub ok: bash");
        assert!(turn.pending_elevation.is_none());
    }

    #[test]
    fn pending_elevation_struct_fields() {
        let elev = PendingElevation {
            tool_call_id: "tc_1".into(),
            tool_name: "bash".into(),
            input: serde_json::json!({}),
            request_id: "elev_1".into(),
            reason: "test elevation".into(),
        };
        assert_eq!(elev.tool_call_id, "tc_1");
        assert_eq!(elev.tool_name, "bash");
    }

    // ─── Streaming Tool Call Assembler ─────────────────────────────────────

    #[test]
    fn streaming_assembler_accumulates_text() {
        let mut assembler = StreamingToolCallAssembler::new();
        assembler.process_event(StreamEvent::TextDelta("Hello".into()));
        assembler.process_event(StreamEvent::TextDelta(" world".into()));
        assert_eq!(assembler.text(), "Hello world");
    }

    #[test]
    fn streaming_assembler_accumulates_thinking_and_signature() {
        let mut assembler = StreamingToolCallAssembler::new();
        assembler.process_event(StreamEvent::ThinkingDelta("Let me ".into()));
        assembler.process_event(StreamEvent::ThinkingDelta("think.".into()));
        assembler.process_event(StreamEvent::SignatureDelta("sig-".into()));
        assembler.process_event(StreamEvent::SignatureDelta("abc".into()));

        let thinking = assembler.thinking().expect("a thinking block");
        assert_eq!(thinking.thinking, "Let me think.");
        assert_eq!(thinking.signature, "sig-abc");
    }

    #[test]
    fn streaming_assembler_assembles_tool_call_from_deltas() {
        let mut assembler = StreamingToolCallAssembler::new();
        assembler.process_event(StreamEvent::ToolCallBlockStart {
            id: "tc_1".into(),
            name: "read".into(),
        });
        assembler.process_event(StreamEvent::ToolCallInputDelta {
            id: "tc_1".into(),
            delta: r#"{"path":"test.txt"}"#.into(),
        });
        let completed = assembler.process_event(StreamEvent::ToolCallBlockEnd {
            id: "tc_1".into(),
            input: serde_json::json!({"path": "test.txt"}),
        });
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].name, "read");
        assert!(assembler.has_tool_calls());
    }

    #[test]
    fn streaming_assembler_finalizes_pending_on_stream_end() {
        let mut assembler = StreamingToolCallAssembler::new();
        assembler.process_event(StreamEvent::ToolCallBlockStart {
            id: "tc_2".into(),
            name: "bash".into(),
        });
        assembler.process_event(StreamEvent::ToolCallInputDelta {
            id: "tc_2".into(),
            delta: r#"{"command":"ls"}"#.into(),
        });
        // StreamEnd finalizes pending tool calls
        let finalized = assembler.process_event(StreamEvent::StreamEnd);
        assert_eq!(finalized.len(), 1);
        assert_eq!(finalized[0].name, "bash");
    }

    #[test]
    fn streaming_assembler_no_tool_calls_for_text_only() {
        let mut assembler = StreamingToolCallAssembler::new();
        assembler.process_event(StreamEvent::TextDelta("text only".into()));
        assert!(!assembler.has_tool_calls());
        assert!(assembler.take_completed().is_empty());
    }

    #[test]
    fn streaming_assembler_takes_completed() {
        let mut assembler = StreamingToolCallAssembler::new();
        assembler.process_event(StreamEvent::ToolCallBlockStart {
            id: "tc_1".into(),
            name: "read".into(),
        });
        let _c = assembler.process_event(StreamEvent::ToolCallBlockEnd {
            id: "tc_1".into(),
            input: serde_json::json!({}),
        });
        // take_completed returns already-completed calls
        let taken = assembler.take_completed();
        assert_eq!(taken.len(), 1);
        // Second take is empty
        assert!(assembler.take_completed().is_empty());
    }

    #[test]
    fn provider_messages_prepend_thinking_and_merge_the_assistant_turn() {
        let thinking = ThinkingBlock {
            thinking: "I should read the file.".into(),
            signature: "sig-abc".into(),
        };
        let tool_call = ToolCall {
            id: "tc_1".into(),
            name: "Read".into(),
            input: serde_json::json!({"file_path": "src/lib.rs"}),
        };
        let messages = QueryEngine::provider_messages(&[
            Message::user("Read src/lib.rs"),
            Message {
                role: crate::types::Role::Assistant,
                content: crate::types::MessageContent::Thinking(thinking),
            },
            Message::assistant("I'll read it."),
            Message {
                role: crate::types::Role::Assistant,
                content: crate::types::MessageContent::ToolCall(tool_call),
            },
        ]);

        assert_eq!(messages.len(), 2, "one user turn, one assistant turn");
        let content = messages[1]["content"]
            .as_array()
            .expect("the assistant turn must be a block array");
        assert_eq!(
            content
                .iter()
                .map(|b| b["type"].as_str())
                .collect::<Vec<_>>(),
            vec![Some("thinking"), Some("text"), Some("tool_use")],
            "thinking, text, and tool_use are one assistant turn in order"
        );
        assert_eq!(content[0]["thinking"], "I should read the file.");
        assert_eq!(content[0]["signature"], "sig-abc");
        assert_eq!(content[1]["text"], "I'll read it.");
        assert_eq!(content[2]["id"], "tc_1");
    }

    #[test]
    fn provider_messages_drop_pending_thinking_without_a_following_assistant() {
        let messages = QueryEngine::provider_messages(&[
            Message {
                role: crate::types::Role::Assistant,
                content: crate::types::MessageContent::Thinking(ThinkingBlock {
                    thinking: "unfinished thought".into(),
                    signature: "sig-abc".into(),
                }),
            },
            Message::user("next request"),
        ]);

        assert_eq!(messages.len(), 1, "the thinking block is not a message");
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"], "next request");
    }

    #[test]
    fn provider_messages_map_tool_result_to_user_role() {
        let tool_call = ToolCall {
            id: "tc_1".into(),
            name: "Read".into(),
            input: serde_json::json!({"file_path": "src/lib.rs"}),
        };
        let tool_result = ToolResult {
            tool_call_id: "tc_1".into(),
            output: "file body".into(),
            is_error: false,
            images: Vec::new(),
        };
        let messages = QueryEngine::provider_messages(&[
            Message::user("Read src/lib.rs"),
            Message {
                role: crate::types::Role::Assistant,
                content: crate::types::MessageContent::ToolCall(tool_call),
            },
            Message::tool(tool_result),
        ]);

        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"][0]["type"], "tool_use");
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(messages[2]["content"][0]["type"], "tool_result");
        assert_eq!(messages[2]["content"][0]["tool_use_id"], "tc_1");
    }

    /// Two calls in one assistant turn must arrive as two `tool_use` blocks
    /// answered by two `tool_result` blocks in *one* following message.
    #[test]
    fn provider_messages_merge_parallel_tool_results() {
        let call = |id: &str| ToolCall {
            id: id.into(),
            name: "Read".into(),
            input: serde_json::json!({"file_path": "src/lib.rs"}),
        };
        let result = |id: &str| ToolResult {
            tool_call_id: id.into(),
            output: "denied".into(),
            is_error: true,
            images: Vec::new(),
        };

        let messages = QueryEngine::provider_messages(&[
            Message::user("read two files"),
            Message {
                role: crate::types::Role::Assistant,
                content: crate::types::MessageContent::ToolCall(call("tc_a")),
            },
            Message {
                role: crate::types::Role::Assistant,
                content: crate::types::MessageContent::ToolCall(call("tc_b")),
            },
            Message::tool(result("tc_a")),
            Message::tool(result("tc_b")),
        ]);

        assert_eq!(messages.len(), 3, "{messages:#?}");
        let tool_uses = messages[1]["content"].as_array().unwrap();
        assert_eq!(tool_uses.len(), 2);
        let tool_results = messages[2]["content"].as_array().unwrap();
        assert_eq!(tool_results.len(), 2);
        assert_eq!(tool_results[0]["tool_use_id"], "tc_a");
        assert_eq!(tool_results[1]["tool_use_id"], "tc_b");
    }

    #[test]
    fn provider_messages_keep_consecutive_user_text_messages_separate() {
        let messages =
            QueryEngine::provider_messages(&[Message::user("one"), Message::user("two")]);
        assert_eq!(messages.len(), 2);
    }

    #[test]
    fn provider_messages_serialize_tool_result_images_as_image_blocks() {
        let tool_result = ToolResult {
            tool_call_id: "tc_img".into(),
            output: "screenshot taken".into(),
            is_error: false,
            images: vec![crate::types::ImageBlock {
                media_type: "image/png".into(),
                data: "aGVsbG8=".into(),
            }],
        };
        let messages = QueryEngine::provider_messages(&[Message::tool(tool_result)]);

        let block = &messages[0]["content"][0];
        assert_eq!(block["type"], "tool_result");
        assert_eq!(block["tool_use_id"], "tc_img");
        let content = block["content"]
            .as_array()
            .expect("image results must use content blocks");
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "screenshot taken");
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["source"]["type"], "base64");
        assert_eq!(content[1]["source"]["media_type"], "image/png");
        assert_eq!(content[1]["source"]["data"], "aGVsbG8=");
    }

    #[test]
    fn provider_messages_omit_empty_text_before_image_blocks() {
        let tool_result = ToolResult {
            tool_call_id: "tc_img".into(),
            output: String::new(),
            is_error: false,
            images: vec![crate::types::ImageBlock {
                media_type: "image/jpeg".into(),
                data: "aGVsbG8=".into(),
            }],
        };
        let messages = QueryEngine::provider_messages(&[Message::tool(tool_result)]);

        let content = messages[0]["content"][0]["content"]
            .as_array()
            .expect("image results must use content blocks");
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "image");
    }

    /// The context-size line is checked before a request, not only after an
    /// output-token budget trips. A small-window model with a large input must
    /// compact before sending.
    #[tokio::test]
    async fn the_context_threshold_compacts_before_a_request() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let checker = PermissionChecker::new(PermissionMode::BypassPermissions);
        let mut engine = QueryEngine::new(
            Box::new(MockApiClient),
            ModelId("unknown-small-window".into()),
            SessionState::new(std::path::PathBuf::from("/tmp/test")),
            PermissionEngine::new(checker),
            tx,
        );
        engine.set_tool_executor(Box::new(StubOkToolExecutor));

        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        turn.messages
            .push(Message::user("the original request that must be preserved"));
        for i in 0..10 {
            turn.messages.push(Message {
                role: crate::types::Role::Assistant,
                content: crate::types::MessageContent::Text(format!(
                    "assistant turn {i}: {}",
                    "x".repeat(10_000)
                )),
            });
        }

        let before = PromptCompiler::estimate_message_tokens(&turn.messages);
        assert!(before > 16_000, "the test context is too small: {before}");

        let reason = engine
            .maybe_compact_on_threshold(&mut turn, &SubmitOptions::default(), 1)
            .await;
        assert!(
            reason.is_none(),
            "an over-line context was refused: {reason:?}"
        );

        let after = PromptCompiler::estimate_message_tokens(&turn.messages);
        assert!(
            after < before,
            "the context was not reduced: {before} -> {after}"
        );
        // This fixture was never sent, so there is no provider measurement and
        // the pass is judged on the unanchored line: it removes more than a
        // tenth but the request still sits just over the line, which is an
        // in-episode pass and spends a retry. Reaching the line ends the
        // episode and restores the budget — see
        // `a_pass_back_under_the_line_replenishes_the_recovery_budget`.
        assert_eq!(turn.recovery_count, 1, "the compaction was not recorded");
        assert_eq!(turn.compact_failures, 0);
    }

    /// A pass that brings the context back under the line ends the recovery
    /// episode, and the retry budget belongs to the episode. A long headless
    /// run is one engine turn for the whole task, so without this it could
    /// compact only `max_recovery_retries` times in its entire life and then
    /// died mid-work with "the compaction retry budget is spent" while its
    /// context was still growing.
    #[tokio::test]
    async fn a_pass_back_under_the_line_replenishes_the_recovery_budget() {
        let (mut engine, _rx, _session) = make_test_engine();
        engine.learn_context_window(32_000);
        let max = engine.compact_config.max_recovery_retries;
        assert!(max > 0, "the fixture needs a real budget");

        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        turn.messages = vec![Message::user("x".repeat(400_000))];
        turn.set_history_range(0, turn.messages.len());
        turn.recovery_count = max;

        let outcome = engine
            .finish_compaction(
                &mut turn,
                CompactResult {
                    strategy: CompactStrategy::Auto,
                    effective: CompactStrategy::Auto,
                    messages: vec![Message::user("x".repeat(8_000))],
                    tokens_removed: 0,
                    boundary_inserted: false,
                    boundary_index: None,
                },
                CompactTrigger::TokenThreshold,
                SummaryOutcome::NotRequested,
            )
            .await;

        assert!(
            matches!(outcome, CompactOutcome::Compacted { .. }),
            "{outcome:?}"
        );
        assert_eq!(
            turn.recovery_count, 0,
            "the episode ended under the line; the budget was not restored"
        );
    }

    /// A pass that removes a meaningful share but leaves the context over the
    /// line is still inside the episode and spends a retry.
    #[tokio::test]
    async fn a_pass_that_stays_over_the_line_spends_a_recovery_retry() {
        let (mut engine, _rx, _session) = make_test_engine();
        engine.learn_context_window(32_000);

        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        turn.messages = vec![Message::user("x".repeat(400_000))];
        turn.set_history_range(0, turn.messages.len());
        turn.recovery_count = 0;

        let outcome = engine
            .finish_compaction(
                &mut turn,
                CompactResult {
                    strategy: CompactStrategy::Micro,
                    effective: CompactStrategy::Micro,
                    messages: vec![Message::user("x".repeat(320_000))],
                    tokens_removed: 0,
                    boundary_inserted: false,
                    boundary_index: None,
                },
                CompactTrigger::TokenThreshold,
                SummaryOutcome::NotRequested,
            )
            .await;

        assert!(
            matches!(outcome, CompactOutcome::Compacted { .. }),
            "{outcome:?}"
        );
        assert_eq!(
            turn.recovery_count, 1,
            "a pass that stayed over the line must spend a retry"
        );
    }

    /// `Reactive` compaction is reachable from the engine, not dead code: a
    /// response that stopped on `max_tokens` while the context is over the
    /// compact line selects it.
    #[tokio::test]
    async fn a_max_tokens_stop_over_the_line_selects_reactive_compaction() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let checker = PermissionChecker::new(PermissionMode::BypassPermissions);
        let mut engine = QueryEngine::new(
            Box::new(MockApiClient),
            ModelId("unknown-small-window".into()),
            SessionState::new(std::path::PathBuf::from("/tmp/test")),
            PermissionEngine::new(checker),
            tx,
        );
        let (hooks, recorded) = RecordingHooks::new();
        engine.set_hooks(Arc::new(hooks));

        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        turn.last_stop_reason = Some("max_tokens".into());
        turn.messages
            .push(Message::user("the original request that must be preserved"));
        for i in 0..10 {
            turn.messages.push(Message {
                role: crate::types::Role::Assistant,
                content: crate::types::MessageContent::Text(format!(
                    "assistant turn {i}: {}",
                    "x".repeat(10_000)
                )),
            });
        }

        let before = PromptCompiler::estimate_message_tokens(&turn.messages);
        assert!(before > 16_000, "the test context is too small: {before}");

        let reason = engine
            .maybe_compact_on_threshold(&mut turn, &SubmitOptions::default(), 1)
            .await;
        assert!(reason.is_none(), "the turn was blocked: {reason:?}");

        let events = recorded
            .lock()
            .expect("the recording hook lock is not poisoned");
        let strategy = events
            .iter()
            .find(|(name, _)| name == hook_events::PRE_COMPACT)
            .and_then(|(_, context)| context.get("strategy"))
            .cloned();
        assert_eq!(
            strategy.as_deref(),
            Some("Reactive"),
            "max_tokens did not select reactive compaction: {events:?}"
        );
    }

    #[test]
    fn persist_assistant_tool_calls_records_tool_use_messages() {
        let (mut engine, _rx, _session) = make_test_engine();
        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        let tool_call = ToolCall {
            id: "tc_1".into(),
            name: "Read".into(),
            input: serde_json::json!({"file_path": "src/lib.rs"}),
        };

        engine.persist_assistant_tool_calls(&mut turn, std::slice::from_ref(&tool_call));

        assert!(matches!(
            turn.messages.first(),
            Some(Message {
                role: crate::types::Role::Assistant,
                content: crate::types::MessageContent::ToolCall(tc),
            }) if tc.id == "tc_1"
        ));
        assert!(matches!(
            engine.session.messages.last(),
            Some(Message {
                role: crate::types::Role::Assistant,
                content: crate::types::MessageContent::ToolCall(tc),
            }) if tc.id == "tc_1"
        ));
    }

    // ─── Elicitation Integration ───────────────────────────────────────────

    #[tokio::test]
    async fn handle_elicitation_stores_response() {
        let (mut engine, _rx, _session) = make_test_engine();
        engine.set_elicitation_handler(Box::new(
            crate::elicitation::SdkElicitationHandler::default(),
        ));
        let budget = TokenBudget::with_defaults();
        let mut turn = TurnRuntime::new(budget);

        let result = engine
            .handle_elicitation(&mut turn, "elic_1".into(), "Which approach?".into())
            .await;
        // Default handler returns Cancelled
        assert!(matches!(result, ElicitationResult::Cancelled));
        // Turn state should be AwaitElicitation
        assert_eq!(turn.state, TurnState::AwaitElicitation);
    }

    /// An elicitation is bracketed by its two hooks: the ask, then the outcome
    /// that came back.
    #[tokio::test]
    async fn elicitation_hooks_bracket_the_request() {
        let (mut engine, _rx, _session) = make_test_engine();
        // The default handler reads stdin, which never reaches EOF under a CI
        // runner. Use the SDK handler, which answers without a console.
        engine.set_elicitation_handler(Box::new(
            crate::elicitation::SdkElicitationHandler::default(),
        ));
        let (hooks, recorded) = RecordingHooks::new();
        engine.set_hooks(Arc::new(hooks));
        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());

        engine
            .handle_elicitation(&mut turn, "elic_1".into(), "Which approach?".into())
            .await;

        let events = recorded
            .lock()
            .expect("the recording hook lock is not poisoned");
        let ask = events
            .iter()
            .find(|(name, _)| name == hook_events::ELICITATION)
            .unwrap_or_else(|| panic!("the Elicitation hook did not fire: {events:?}"));
        assert_eq!(
            ask.1.get("elicitation_id").map(String::as_str),
            Some("elic_1")
        );
        assert_eq!(
            ask.1.get("message").map(String::as_str),
            Some("Which approach?")
        );

        let outcome = events
            .iter()
            .find(|(name, _)| name == hook_events::ELICITATION_RESULT)
            .unwrap_or_else(|| panic!("the ElicitationResult hook did not fire: {events:?}"));
        assert_eq!(
            outcome.1.get("elicitation_id").map(String::as_str),
            Some("elic_1")
        );
        assert_eq!(
            outcome.1.get("outcome").map(String::as_str),
            Some("cancelled")
        );
    }

    /// A session runs its `SessionStart` hook once, on its first turn, with the
    /// session it is starting.
    #[tokio::test]
    async fn session_start_fires_once_with_the_session_it_starts() {
        let (mut engine, recorded) = engine_with_recording_hooks(Box::new(MockApiClient));
        let session_id = engine.session.session_id.to_string();

        engine.fire_session_start_hook().await;
        engine.fire_session_start_hook().await;

        let events = recorded
            .lock()
            .expect("the recording hook lock is not poisoned");
        let starts: Vec<_> = events
            .iter()
            .filter(|(name, _)| name == hook_events::SESSION_START)
            .collect();
        assert_eq!(starts.len(), 1, "SessionStart must fire once: {events:?}");
        assert_eq!(
            starts[0].1.get("session_id").map(String::as_str),
            Some(session_id.as_str())
        );
        assert!(
            starts[0].1.contains_key("cwd"),
            "SessionStart should see the working directory: {starts:?}"
        );
    }

    /// A `SessionStart` hook's `watchPaths` join the static `watch`
    /// configuration, in that order, with duplicates removed. The merged list
    /// is what the session hands to a file watcher.
    #[tokio::test]
    async fn session_start_watch_paths_merge_with_static_configuration() {
        struct WatchHooks {
            session: Vec<String>,
            statically_configured: Vec<String>,
        }

        #[async_trait::async_trait]
        impl HookExecutor for WatchHooks {
            async fn fire_hook(
                &self,
                event_name: &str,
                _input: &str,
                _context: &HashMap<String, String>,
            ) -> HookFireResult {
                if event_name == hook_events::SESSION_START {
                    HookFireResult {
                        watch_paths: self.session.clone(),
                        ..HookFireResult::default()
                    }
                } else {
                    HookFireResult::default()
                }
            }

            fn watch_paths(&self) -> Vec<String> {
                self.statically_configured.clone()
            }
        }

        let temp = std::env::temp_dir();
        let static_path = temp
            .join("codape-static-watch")
            .to_string_lossy()
            .into_owned();
        let session_path = temp
            .join("codape-session-watch")
            .to_string_lossy()
            .into_owned();
        let duplicate = static_path.clone();

        let (mut engine, _recorded) = engine_with_recording_hooks(Box::new(MockApiClient));
        engine.set_hooks(Arc::new(WatchHooks {
            session: vec![session_path.clone(), duplicate],
            statically_configured: vec![static_path.clone()],
        }));

        // Static configuration is visible before the session starts; the
        // session's own paths appear once `SessionStart` has run.
        assert_eq!(engine.watch_paths(), vec![static_path.clone()]);
        engine.ensure_session_started().await;
        assert_eq!(
            engine.watch_paths(),
            vec![static_path, session_path],
            "the watched paths must merge the static and session lists without duplicates"
        );

        // The accessor hands out a clone of the same executor.
        let executor = engine.hook_executor();
        let fired = executor
            .fire_hook("FileChanged", "{}", &HashMap::new())
            .await;
        assert!(!fired.failed);
    }

    #[test]
    fn query_engine_has_default_elicitation_handler() {
        let (mut engine, _rx, _session) = make_test_engine();
        // Can set a custom handler
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        engine
            .set_elicitation_handler(Box::new(crate::elicitation::SdkElicitationHandler::new(tx)));
        // Handler was set without error
    }

    #[tokio::test]
    async fn elicitation_heuristic_triggers_in_execute_turn() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };

        struct StatefulMock {
            call_count: Arc<AtomicUsize>,
        }

        #[async_trait::async_trait]
        impl ApiClient for StatefulMock {
            fn clone_box(&self) -> Box<dyn ApiClient> {
                Box::new(Self {
                    call_count: Arc::clone(&self.call_count),
                })
            }

            async fn create_message(
                &self,
                _system: &str,
                _messages: &[serde_json::Value],
                _model: &ModelId,
                _max_tokens: u32,
                _tools: &[crate::tool_executor::ToolInfo],
            ) -> Result<ApiResponse, crate::api_client::ApiError> {
                let count = self.call_count.fetch_add(1, Ordering::SeqCst);
                if count == 0 {
                    Ok(ApiResponse {
                        content: "Which approach would you prefer?".into(),
                        tool_calls: Vec::new(),
                        usage: UsageSummary {
                            input_tokens: 100,
                            output_tokens: 50,
                            cache_read_tokens: 0,
                            cache_creation_tokens: 0,
                        },
                        stop_reason: "end_turn".into(),
                        thinking: None,
                    })
                } else {
                    Ok(ApiResponse {
                        content: "Using approach A.".into(),
                        tool_calls: Vec::new(),
                        usage: UsageSummary {
                            input_tokens: 100,
                            output_tokens: 50,
                            cache_read_tokens: 0,
                            cache_creation_tokens: 0,
                        },
                        stop_reason: "end_turn".into(),
                        thinking: None,
                    })
                }
            }

            async fn create_message_streaming(
                &self,
                _system: &str,
                _messages: &[serde_json::Value],
                _model: &ModelId,
                _max_tokens: u32,
                _tools: &[crate::tool_executor::ToolInfo],
            ) -> std::pin::Pin<Box<dyn futures::Stream<Item = StreamEvent> + Send + 'static>>
            {
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                let _ = tx.send(StreamEvent::StreamEnd);
                Box::pin(futures::stream::unfold(rx, |mut rx| async move {
                    rx.recv().await.map(|item| (item, rx))
                }))
            }
        }

        struct TestElicitationHandler;

        #[async_trait::async_trait]
        impl ElicitationHandler for TestElicitationHandler {
            async fn request(
                &self,
                _questions: Vec<crate::elicitation::ElicitationQuestion>,
            ) -> ElicitationResult {
                ElicitationResult::Responded(crate::elicitation::ElicitationResponse {
                    request_id: "elic_1".into(),
                    selected: vec!["Approach A".into()],
                    other_text: None,
                })
            }
        }

        let (tx, _rx) = mpsc::unbounded_channel();
        let checker = PermissionChecker::new(PermissionMode::BypassPermissions);
        let perm_engine = PermissionEngine::new(checker);
        let session = SessionState::new(std::path::PathBuf::from("/tmp/test"));
        let api = StatefulMock {
            call_count: Arc::new(AtomicUsize::new(0)),
        };
        let mut engine =
            QueryEngine::new(Box::new(api), ModelId::sonnet(), session, perm_engine, tx);
        engine.set_tool_executor(Box::new(StubOkToolExecutor));
        engine.set_elicitation_handler(Box::new(TestElicitationHandler));

        let budget = TokenBudget::with_defaults();
        let mut turn = TurnRuntime::new(budget);
        let options = SubmitOptions {
            handle_elicitation: true,
            ..Default::default()
        };

        let reason = engine.execute_turn(&mut turn, &options, 1).await.unwrap();

        assert_eq!(reason, TerminalReason::Completed);

        // turn.messages should contain the user clarification and second assistant response
        let user_texts: Vec<String> = turn
            .messages
            .iter()
            .filter_map(|m| match (&m.role, &m.content) {
                (crate::types::Role::User, crate::types::MessageContent::Text(t)) => {
                    Some(t.clone())
                }
                _ => None,
            })
            .collect();
        assert!(
            user_texts.iter().any(|t| t == "Approach A"),
            "expected user clarification in turn messages"
        );

        let assistant_texts: Vec<String> = turn
            .messages
            .iter()
            .filter_map(|m| match (&m.role, &m.content) {
                (crate::types::Role::Assistant, crate::types::MessageContent::Text(t)) => {
                    Some(t.clone())
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            assistant_texts.len(),
            2,
            "expected two assistant messages: question and answer"
        );
        assert_eq!(assistant_texts[1], "Using approach A.");

        // session.messages should contain the user clarification
        assert!(
            engine.session.messages.iter().any(|m| {
                matches!(&m.content, crate::types::MessageContent::Text(t) if t == "Approach A")
            }),
            "expected user clarification in session messages"
        );
    }

    // ─── Transcript Persistence ────────────────────────────────────────────

    #[test]
    fn set_transcript_path_stores_path() {
        let (mut engine, _rx, _session) = make_test_engine();
        engine.set_transcript_path(PathBuf::from("/tmp/transcripts"));
        // Path was set without error
        assert!(engine.transcript_path.is_some());
    }

    #[test]
    fn transcript_path_defaults_to_none() {
        let (engine, _rx, _session) = make_test_engine();
        assert!(engine.transcript_path.is_none());
    }

    #[test]
    fn write_transcript_replacement_creates_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.jsonl");

        let messages = vec![Message::user("hello"), Message::assistant("world")];

        QueryEngine::write_transcript_replacement(&path, &messages).unwrap();
        assert!(path.exists());

        // Verify content
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content.lines().count(), 2);
    }

    #[test]
    fn write_transcript_replacement_overwrites() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.jsonl");

        // Write initial messages
        QueryEngine::write_transcript_replacement(
            &path,
            &[
                Message::user("old1"),
                Message::user("old2"),
                Message::user("old3"),
            ],
        )
        .unwrap();

        // Replace with fewer messages
        QueryEngine::write_transcript_replacement(&path, &[Message::user("new")]).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content.lines().count(), 1);
    }

    #[test]
    fn append_transcript_boundary_adds_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.jsonl");

        QueryEngine::write_transcript_replacement(&path, &[Message::user("before")]).unwrap();

        QueryEngine::append_transcript_boundary(&path).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[1].contains("codape_compact_boundary"));
    }

    // ─── Provider stop reasons ─────────────────────────────────────────────

    /// A stop reason is read for what it asks the loop to do.
    #[test]
    fn a_stop_reason_says_what_the_loop_does_next() {
        assert_eq!(classify_stop_reason("tool_use"), StopDisposition::RunTools);
        assert_eq!(
            classify_stop_reason("tool_calls"),
            StopDisposition::RunTools
        );
        assert_eq!(classify_stop_reason("pause_turn"), StopDisposition::Resume);
        assert_eq!(classify_stop_reason("compaction"), StopDisposition::Resume);

        // Anything not named is the end of the turn — including a reason a
        // provider invents next, which must not make the loop re-request.
        for ending in ["end_turn", "max_tokens", "length", "refusal", "stop", ""] {
            assert_eq!(
                classify_stop_reason(ending),
                StopDisposition::Stop,
                "{ending:?}"
            );
        }
    }

    /// A paused turn is reported as one to resume, not as a finished answer.
    /// `reason_for` used to special-case only `max_tokens`; every other reason
    /// became `Completed`, so a `pause_turn` ended the run mid-answer.
    #[test]
    fn a_paused_turn_is_not_reported_as_finished() {
        for paused in ["pause_turn", "compaction"] {
            assert!(
                matches!(
                    QueryEngine::reason_for(paused, true),
                    TerminalReason::Resume
                ),
                "{paused} should resume"
            );
        }
        for finished in ["end_turn", "refusal", "stop", ""] {
            assert!(
                matches!(
                    QueryEngine::reason_for(finished, true),
                    TerminalReason::Completed
                ),
                "{finished} should finish"
            );
        }
        assert!(matches!(
            QueryEngine::reason_for("max_tokens", true),
            TerminalReason::TokenLimitStop { .. }
        ));
    }

    /// A resumed turn runs again, but a provider that only ever pauses cannot
    /// spin the loop forever.
    #[test]
    fn a_paused_turn_runs_again_until_the_bound() {
        assert!(matches!(
            next_loop_step(&TerminalReason::Resume, false, 0),
            LoopStep::Continue
        ));
        assert!(matches!(
            next_loop_step(&TerminalReason::Resume, false, MAX_PROVIDER_RESUMES - 1),
            LoopStep::Continue
        ));
        assert!(matches!(
            next_loop_step(&TerminalReason::Resume, false, MAX_PROVIDER_RESUMES),
            LoopStep::Break(TerminalReason::Completed)
        ));

        // The older rule still holds: a turn that ended on a tool result is
        // not finished, the model has not seen the result yet.
        assert!(matches!(
            next_loop_step(&TerminalReason::Completed, true, 0),
            LoopStep::Continue
        ));
        assert!(matches!(
            next_loop_step(&TerminalReason::Completed, false, 0),
            LoopStep::Break(TerminalReason::Completed)
        ));
    }

    /// The provider-resume cap bounds consecutive pauses, not every pause in
    /// the run: a normal turn between two pauses resets the count.
    #[test]
    fn provider_resumes_are_consecutive() {
        let mut resumes = 0;
        for _ in 0..3 {
            track_provider_resumes(&TerminalReason::Resume, &mut resumes);
        }
        assert_eq!(resumes, 3);

        track_provider_resumes(&TerminalReason::Completed, &mut resumes);
        assert_eq!(resumes, 0, "a completed turn resets the pause count");

        track_provider_resumes(&TerminalReason::Resume, &mut resumes);
        assert_eq!(resumes, 1);
    }

    /// An error reason ends the run.
    ///
    /// The loop used to have no `Error` arm, so it fell through to
    /// `LoopStep::Continue`: a turn blocked on context re-ran forever because
    /// `turn.reset()` cleared the state and `max_turns` defaults to 0.
    #[test]
    fn an_error_reason_breaks_the_turn_loop() {
        assert!(matches!(
            next_loop_step(
                &TerminalReason::Error("context window is full".into()),
                false,
                0
            ),
            LoopStep::Break(TerminalReason::Error(_))
        ));
        // The `Completed`-on-a-tool-result rule must not rescue an error: the
        // model has not seen the result only when there is a next request.
        assert!(matches!(
            next_loop_step(&TerminalReason::Error("bad request".into()), true, 0),
            LoopStep::Break(TerminalReason::Error(_))
        ));
    }

    /// Answers with the next stop reason from a fixed list, one per request,
    /// and counts the requests.
    struct StopReasonSequenceMock {
        reasons: Vec<String>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl StopReasonSequenceMock {
        fn next_reason(&self) -> String {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.reasons
                .get(n)
                .or_else(|| self.reasons.last())
                .cloned()
                .unwrap_or_else(|| "end_turn".to_string())
        }
    }

    #[async_trait::async_trait]
    impl ApiClient for StopReasonSequenceMock {
        fn clone_box(&self) -> Box<dyn ApiClient> {
            Box::new(Self {
                reasons: self.reasons.clone(),
                calls: Arc::clone(&self.calls),
            })
        }

        async fn create_message(
            &self,
            _system: &str,
            _messages: &[serde_json::Value],
            _model: &ModelId,
            _max_tokens: u32,
            _tools: &[crate::tool_executor::ToolInfo],
        ) -> Result<ApiResponse, crate::api_client::ApiError> {
            let n = self.calls.load(std::sync::atomic::Ordering::SeqCst);
            Ok(ApiResponse {
                content: format!("answer {n}"),
                tool_calls: Vec::new(),
                usage: UsageSummary {
                    input_tokens: 10,
                    output_tokens: 5,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                },
                stop_reason: self.next_reason(),
                thinking: None,
            })
        }

        async fn create_message_streaming(
            &self,
            system: &str,
            messages: &[serde_json::Value],
            model: &ModelId,
            max_tokens: u32,
            tools: &[crate::tool_executor::ToolInfo],
        ) -> std::pin::Pin<
            Box<dyn futures::Stream<Item = crate::api_client::StreamEvent> + Send + 'static>,
        > {
            let response = self
                .create_message(system, messages, model, max_tokens, tools)
                .await
                .expect("the sequence mock answers");
            Box::pin(futures::stream::iter(vec![
                crate::api_client::StreamEvent::TextDelta(response.content),
                crate::api_client::StreamEvent::StopReason(response.stop_reason),
                crate::api_client::StreamEvent::StreamEnd,
            ]))
        }
    }

    fn sequence_engine(reasons: &[&str]) -> (QueryEngine, Arc<std::sync::atomic::AtomicUsize>) {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (tx, _rx) = mpsc::unbounded_channel();
        let session = SessionState::new(std::path::PathBuf::from("/tmp/test"));
        let checker = PermissionChecker::new(PermissionMode::BypassPermissions);
        let engine = QueryEngine::new(
            Box::new(StopReasonSequenceMock {
                reasons: reasons.iter().map(|r| (*r).to_string()).collect(),
                calls: Arc::clone(&calls),
            }),
            ModelId::sonnet(),
            session,
            PermissionEngine::new(checker),
            tx,
        );
        (engine, calls)
    }

    /// A `pause_turn` is a pause, not an ending: the request is sent again and
    /// the run reports the reason that really ended it.
    #[tokio::test]
    async fn a_paused_turn_is_resumed_rather_than_reported_as_an_answer() {
        let (mut engine, calls) = sequence_engine(&["pause_turn", "end_turn"]);
        let reason = engine
            .submit("go".into(), SubmitOptions::default())
            .await
            .unwrap();
        assert!(matches!(reason, TerminalReason::Completed), "{reason:?}");
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "the paused turn should have been requested again"
        );
    }

    /// The streaming path resumes a paused turn too.
    #[tokio::test]
    async fn a_paused_turn_is_resumed_on_the_streaming_path() {
        let (mut engine, calls) = sequence_engine(&["compaction", "end_turn"]);
        let reason = engine
            .submit_streaming("go".into(), SubmitOptions::default())
            .await
            .unwrap();
        assert!(matches!(reason, TerminalReason::Completed), "{reason:?}");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    /// A provider that only ever pauses stops at the bound rather than
    /// spinning.
    #[tokio::test]
    async fn a_provider_that_only_pauses_does_not_loop_forever() {
        let (mut engine, calls) = sequence_engine(&["pause_turn"]);
        let reason = engine
            .submit("go".into(), SubmitOptions::default())
            .await
            .unwrap();
        assert!(matches!(reason, TerminalReason::Completed), "{reason:?}");
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            MAX_PROVIDER_RESUMES as usize + 1
        );
    }

    /// Hook events recorded by [`RecordingHooks`].
    type RecordedEvents = Arc<std::sync::Mutex<Vec<(String, HashMap<String, String>)>>>;

    /// An engine on the conservative 32K unknown-model window whose compaction
    /// retry budget is spent, so the blocked line is reported immediately
    /// without attempting a compaction.
    fn blocked_context_engine() -> (
        QueryEngine,
        Arc<std::sync::atomic::AtomicUsize>,
        RecordedEvents,
    ) {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (tx, _rx) = mpsc::unbounded_channel();
        let checker = PermissionChecker::new(PermissionMode::BypassPermissions);
        let mut engine = QueryEngine::new(
            Box::new(StopReasonSequenceMock {
                reasons: vec!["end_turn".into()],
                calls: Arc::clone(&calls),
            }),
            ModelId("unknown-small-window".into()),
            SessionState::new(std::path::PathBuf::from("/tmp/test")),
            PermissionEngine::new(checker),
            tx,
        );
        // Ignore ambient context-window overrides, then state the window as a
        // provider would. A guessed window never enforces the blocked line, so
        // the test has to hand the engine a real one to exercise that path.
        engine.set_context_window_resolver(ContextWindowResolver::new(
            crate::types::ContextWindowOverrides::default(),
            crate::types::ContextWindowOverrides::default(),
            crate::types::ContextWindowOverrides::default(),
            crate::types::ContextWindowOverrides::default(),
        ));
        engine.learn_context_window(32_000);
        engine.compact_config.max_recovery_retries = 0;

        let (hooks, recorded) = RecordingHooks::new();
        engine.set_hooks(Arc::new(hooks));
        (engine, calls, recorded)
    }

    /// A turn blocked on context ends after one turn: before `Error` was a
    /// breaking reason, the loop reset the turn and re-ran the same
    /// un-sendable request forever.
    #[tokio::test]
    async fn a_context_blocked_turn_does_not_send_a_request_or_run_again() {
        let (mut engine, calls, recorded) = blocked_context_engine();
        // ~50K estimated tokens, past the 29K blocked line of a 32K window.
        engine
            .session
            .messages
            .push(Message::user("x".repeat(200_000)));

        // Bounds the pre-fix fall-through at turn 2 instead of hanging: the
        // old behaviour reported `MaxTurnsReached`, not the context error.
        let options = SubmitOptions {
            max_turns: 1,
            ..SubmitOptions::default()
        };

        let reason = engine.submit("go".into(), options).await.unwrap();

        assert!(
            matches!(reason, TerminalReason::Error(_)),
            "the blocked turn did not terminate with the context error: {reason:?}"
        );
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a blocked context was sent to the provider"
        );

        let stop_ran = {
            let events = recorded
                .lock()
                .expect("the recording hook lock is not poisoned");
            events.iter().any(|(name, _)| name == hook_events::STOP)
        };
        let turn_end_ran = {
            let events = recorded
                .lock()
                .expect("the recording hook lock is not poisoned");
            events.iter().any(|(name, _)| name == hook_events::TURN_END)
        };
        assert!(stop_ran, "Stop hooks did not run for the blocked turn");
        assert!(
            turn_end_ran,
            "TurnEnd hooks did not run for the blocked turn"
        );
    }

    /// The blocked turn leaves the runtime terminal rather than in a state the
    /// loop can reset and run again.
    #[tokio::test]
    async fn a_context_blocked_turn_is_left_terminal() {
        let (mut engine, calls, _recorded) = blocked_context_engine();
        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        turn.messages.push(Message::user("x".repeat(200_000)));

        let reason = engine
            .execute_turn(&mut turn, &SubmitOptions::default(), 1)
            .await
            .expect("a blocked context returns a reason, not a fatal error");

        assert!(matches!(reason, TerminalReason::Error(_)), "{reason:?}");
        assert!(
            turn.state.is_terminal(),
            "the blocked turn was left in {:?}",
            turn.state
        );
        assert!(
            matches!(turn.terminal_reason, Some(TerminalReason::Error(_))),
            "the blocked turn has no terminal reason: {:?}",
            turn.terminal_reason
        );
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a blocked context was sent to the provider"
        );
    }

    /// A blocked context closes through `finish_turn`, and the move into the
    /// closing state is one the graph allows. Before the fix this recorded
    /// `RunPreSubmitHooks -> StopHooks`, which `can_transition_to` refuses.
    #[tokio::test]
    async fn a_context_blocked_turn_only_makes_legal_moves() {
        let (mut engine, _calls, _recorded) = blocked_context_engine();
        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        turn.messages.push(Message::user("x".repeat(200_000)));

        let _ = take_recorded_transitions();
        let reason = engine
            .execute_turn(&mut turn, &SubmitOptions::default(), 1)
            .await
            .expect("a blocked context returns a reason, not a fatal error");

        assert!(matches!(reason, TerminalReason::Error(_)), "{reason:?}");
        let recorded = take_recorded_transitions();
        assert!(
            recorded
                .iter()
                .any(|(from, to)| matches!(from, TurnState::RunPreSubmitHooks)
                    && matches!(to, TurnState::CompactRecovery)),
            "the blocked turn did not route through CompactRecovery: {recorded:?}"
        );
        assert!(
            recorded
                .iter()
                .any(|(_, to)| matches!(to, TurnState::StopHooks)),
            "the blocked turn never entered StopHooks: {recorded:?}"
        );
        for (from, to) in &recorded {
            assert!(
                from.can_transition_to(*to),
                "a blocked turn went {} -> {}, which the graph refuses",
                from.as_str(),
                to.as_str()
            );
        }
    }

    // ─── Mid-stream interruption recovery ──────────────────────────────────

    /// Merge keeps a prefix from being delivered twice and never loses text.
    #[test]
    fn a_retry_that_repeats_the_opening_does_not_repeat_it_to_the_reader() {
        assert_eq!(
            merge_stream_text("The answer is ", "The answer is 42."),
            "The answer is 42."
        );
        assert_eq!(merge_stream_text("abc", "ab"), "abc");
        assert_eq!(merge_stream_text("", "abc"), "abc");
        // A retry that picks up past the cut, or diverges, keeps both — text
        // already delivered is never dropped.
        assert_eq!(merge_stream_text("abc", "def"), "abcdef");
    }

    /// The incremental merger skips an opening the retry repeats, then emits
    /// the rest once — it does not re-append the attempt's prefix per delta.
    #[test]
    fn an_identical_retry_opening_is_merged_incrementally() {
        let mut merger = StreamTextMerger::new();
        assert_eq!(
            merger.push_delta("The answer is ").as_deref(),
            Some("The answer is ")
        );

        merger.begin_attempt();
        assert_eq!(
            merger.push_delta("The answer is 42.").as_deref(),
            Some("42.")
        );
        assert_eq!(merger.delivered(), "The answer is 42.");
    }

    /// A retry that diverges immediately hands back only each newly arrived
    /// fragment: `x`, then `y`, then `z`, never `x`, `xy`, `xyz`.
    #[test]
    fn a_divergent_retry_is_merged_incrementally() {
        let mut merger = StreamTextMerger::new();
        assert_eq!(merger.push_delta("abc").as_deref(), Some("abc"));

        merger.begin_attempt();
        assert_eq!(merger.push_delta("x").as_deref(), Some("x"));
        assert_eq!(merger.push_delta("y").as_deref(), Some("y"));
        assert_eq!(merger.push_delta("z").as_deref(), Some("z"));

        assert_eq!(merger.delivered(), "abcxyz");
    }

    /// A stream that dies mid-answer is re-requested, and what it already
    /// delivered reaches the reader once. The old recovery re-requested from
    /// scratch and threw the partial text away.
    #[tokio::test]
    async fn a_stream_that_dies_mid_answer_keeps_what_it_delivered() {
        struct InterruptedStreamMock {
            attempts: Arc<std::sync::atomic::AtomicUsize>,
        }

        #[async_trait::async_trait]
        impl ApiClient for InterruptedStreamMock {
            fn clone_box(&self) -> Box<dyn ApiClient> {
                Box::new(Self {
                    attempts: Arc::clone(&self.attempts),
                })
            }

            async fn create_message(
                &self,
                _system: &str,
                _messages: &[serde_json::Value],
                _model: &ModelId,
                _max_tokens: u32,
                _tools: &[crate::tool_executor::ToolInfo],
            ) -> Result<ApiResponse, crate::api_client::ApiError> {
                Ok(ApiResponse {
                    content: "fallback".into(),
                    tool_calls: Vec::new(),
                    usage: UsageSummary {
                        input_tokens: 1,
                        output_tokens: 1,
                        cache_read_tokens: 0,
                        cache_creation_tokens: 0,
                    },
                    stop_reason: "end_turn".into(),
                    thinking: None,
                })
            }

            async fn create_message_streaming(
                &self,
                _system: &str,
                _messages: &[serde_json::Value],
                _model: &ModelId,
                _max_tokens: u32,
                _tools: &[crate::tool_executor::ToolInfo],
            ) -> std::pin::Pin<
                Box<dyn futures::Stream<Item = crate::api_client::StreamEvent> + Send + 'static>,
            > {
                let n = self
                    .attempts
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n == 0 {
                    // The connection dies with the answer half-delivered.
                    Box::pin(futures::stream::iter(vec![
                        StreamEvent::TextDelta("The answer is ".into()),
                        StreamEvent::Error(crate::api_client::ApiError::Transport {
                            kind: crate::api_client::TransportError::Other,
                            class: crate::api_client::ErrorClass::Stream,
                            message: "connection reset mid-body".into(),
                        }),
                    ]))
                } else {
                    Box::pin(futures::stream::iter(vec![
                        StreamEvent::TextDelta("The answer is 42.".into()),
                        StreamEvent::StopReason("end_turn".into()),
                        StreamEvent::StreamEnd,
                    ]))
                }
            }
        }

        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let session = SessionState::new(std::path::PathBuf::from("/tmp/test"));
        let checker = PermissionChecker::new(PermissionMode::BypassPermissions);
        let mut engine = QueryEngine::new(
            Box::new(InterruptedStreamMock {
                attempts: Arc::clone(&attempts),
            }),
            ModelId::sonnet(),
            session,
            PermissionEngine::new(checker),
            tx,
        );

        let reason = engine
            .submit_streaming("go".into(), SubmitOptions::default())
            .await
            .unwrap();
        assert!(matches!(reason, TerminalReason::Completed), "{reason:?}");
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "the cut stream should have been re-requested"
        );

        let mut delivered = String::new();
        let mut interrupts: Vec<(bool, usize)> = Vec::new();
        while let Ok(event) = rx.try_recv() {
            match event {
                EngineEvent::TextDelta(text) => delivered.push_str(&text),
                EngineEvent::StreamInterrupted {
                    will_retry,
                    partial_len,
                    ..
                } => interrupts.push((will_retry, partial_len)),
                _ => {}
            }
        }

        assert_eq!(
            delivered, "The answer is 42.",
            "the reader should see the answer once, not twice"
        );
        assert_eq!(
            interrupts,
            vec![(true, "The answer is ".len())],
            "the interruption should be reported as one that will retry"
        );
    }

    /// A retry that diverges after a partial answer emits each new fragment
    /// once. The old per-delta merge re-applied the accumulated attempt, so
    /// the reader saw `x`, then `xy`, then `xyz` — and the output grew
    /// quadratically with the retry's length.
    #[tokio::test]
    async fn a_divergent_retry_emits_each_fragment_once() {
        struct DivergingRetryMock {
            attempts: Arc<std::sync::atomic::AtomicUsize>,
        }

        #[async_trait::async_trait]
        impl ApiClient for DivergingRetryMock {
            fn clone_box(&self) -> Box<dyn ApiClient> {
                Box::new(Self {
                    attempts: Arc::clone(&self.attempts),
                })
            }

            async fn create_message(
                &self,
                _system: &str,
                _messages: &[serde_json::Value],
                _model: &ModelId,
                _max_tokens: u32,
                _tools: &[crate::tool_executor::ToolInfo],
            ) -> Result<ApiResponse, crate::api_client::ApiError> {
                Ok(ApiResponse {
                    content: "fallback".into(),
                    tool_calls: Vec::new(),
                    usage: UsageSummary {
                        input_tokens: 1,
                        output_tokens: 1,
                        cache_read_tokens: 0,
                        cache_creation_tokens: 0,
                    },
                    stop_reason: "end_turn".into(),
                    thinking: None,
                })
            }

            async fn create_message_streaming(
                &self,
                _system: &str,
                _messages: &[serde_json::Value],
                _model: &ModelId,
                _max_tokens: u32,
                _tools: &[crate::tool_executor::ToolInfo],
            ) -> std::pin::Pin<
                Box<dyn futures::Stream<Item = crate::api_client::StreamEvent> + Send + 'static>,
            > {
                let n = self
                    .attempts
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n == 0 {
                    Box::pin(futures::stream::iter(vec![
                        StreamEvent::TextDelta("abc".into()),
                        StreamEvent::Error(crate::api_client::ApiError::Transport {
                            kind: crate::api_client::TransportError::Other,
                            class: crate::api_client::ErrorClass::Stream,
                            message: "connection reset mid-body".into(),
                        }),
                    ]))
                } else {
                    Box::pin(futures::stream::iter(vec![
                        StreamEvent::TextDelta("x".into()),
                        StreamEvent::TextDelta("y".into()),
                        StreamEvent::TextDelta("z".into()),
                        StreamEvent::StopReason("end_turn".into()),
                        StreamEvent::StreamEnd,
                    ]))
                }
            }
        }

        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let session = SessionState::new(std::path::PathBuf::from("/tmp/test"));
        let checker = PermissionChecker::new(PermissionMode::BypassPermissions);
        let mut engine = QueryEngine::new(
            Box::new(DivergingRetryMock {
                attempts: Arc::clone(&attempts),
            }),
            ModelId::sonnet(),
            session,
            PermissionEngine::new(checker),
            tx,
        );

        let reason = engine
            .submit_streaming("go".into(), SubmitOptions::default())
            .await
            .unwrap();
        assert!(matches!(reason, TerminalReason::Completed), "{reason:?}");
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);

        let mut deltas: Vec<String> = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let EngineEvent::TextDelta(text) = event {
                deltas.push(text);
            }
        }

        assert_eq!(
            deltas,
            vec!["abc".to_string(), "x".into(), "y".into(), "z".into()],
            "the retry's fragments were replayed: {deltas:?}"
        );
        assert_eq!(
            deltas.concat(),
            "abcxyz",
            "the answer grew by more than the retry's text"
        );
    }

    // ─── Compaction outcomes ───────────────────────────────────────────────

    /// Compaction replaces the messages with a summary, so the turns the loop
    /// detector judged are no longer in the context. A repeat the model can no
    /// longer see must not count towards ending the run as a loop.
    #[tokio::test]
    async fn compaction_clears_the_loop_history() {
        let (mut engine, _rx, _session) = make_test_engine();
        engine.record_and_check_loop(
            &[PendingToolCall {
                id: "call-1".into(),
                name: "Read".into(),
                input: serde_json::json!({"file_path": "src/lib.rs"}),
            }],
            &[],
        );
        assert_eq!(
            engine.loop_history_len(),
            Some(1),
            "the call was not recorded"
        );

        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        // Something micro compaction actually shrinks: seven whitelisted tool
        // results, two more than `keep_recent`, so the pass changes the list.
        // A no-op is deliberately not a compaction and must not clear the
        // detector's history.
        turn.messages = read_result_history(7);

        let outcome = engine
            .run_compact_recovery(
                &mut turn,
                &SubmitOptions::default(),
                1,
                CompactTrigger::TokenThreshold,
            )
            .await;

        assert!(
            !matches!(outcome, CompactOutcome::CircuitBreakerOpen { .. }),
            "{outcome:?}"
        );
        assert!(
            matches!(outcome, CompactOutcome::Compacted { .. }),
            "the fixture must actually compact: {outcome:?}"
        );
        assert_eq!(
            engine.loop_history_len(),
            Some(0),
            "compaction kept the pre-compaction loop history"
        );
    }

    /// Why compaction did not happen is returned, not only logged.
    #[tokio::test]
    async fn compaction_says_why_it_did_not_happen() {
        let (mut engine, _rx, _session) = make_test_engine();
        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        turn.messages = vec![Message::user("hello")];
        turn.compact_failures = engine.compact_config.max_consecutive_failures;

        // The cache-TTL pass is the one the breaker still refuses: it is a
        // named strategy and a maintenance call, and a run of failures says
        // the summarizer is not working. The token-threshold path no longer
        // refuses — see the last-resort test below.
        let outcome = engine
            .run_compact_recovery(
                &mut turn,
                &SubmitOptions::default(),
                1,
                CompactTrigger::CacheTtl,
            )
            .await;

        assert!(
            matches!(outcome, CompactOutcome::CircuitBreakerOpen { .. }),
            "{outcome:?}"
        );
        assert!(outcome.failure_message().is_some());
    }

    /// An open breaker does not stop compaction at the token line: it stops
    /// the named strategies and selects the local last resort, which spends no
    /// model call and may still clear the line.
    #[tokio::test]
    async fn an_open_breaker_still_attempts_the_last_resort() {
        let (mut engine, _rx, _session) = make_test_engine();
        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        turn.messages = vec![Message::user("hello")];
        turn.compact_failures = engine.compact_config.max_consecutive_failures;

        let outcome = engine
            .run_compact_recovery(
                &mut turn,
                &SubmitOptions::default(),
                1,
                CompactTrigger::TokenThreshold,
            )
            .await;

        match outcome {
            CompactOutcome::NoProgress {
                strategy,
                effective,
                trigger,
                ..
            } => {
                assert_eq!(strategy, CompactStrategy::LastResort);
                assert_eq!(effective, CompactStrategy::LastResort);
                assert_eq!(trigger, CompactTrigger::TokenThreshold);
            }
            other => panic!("the breaker should have selected the last resort: {other:?}"),
        }
        assert_eq!(
            turn.compact_failures, engine.compact_config.max_consecutive_failures,
            "the last resort must not add to the count it is trying to clear"
        );
    }

    /// A spent recovery budget is the other stop signal for the named
    /// strategies, and it selects the same last resort.
    #[tokio::test]
    async fn a_spent_recovery_budget_still_attempts_the_last_resort() {
        let (mut engine, _rx, _session) = make_test_engine();
        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        turn.messages = vec![Message::user("hello")];
        turn.recovery_count = engine.compact_config.max_recovery_retries;

        let outcome = engine
            .run_compact_recovery(
                &mut turn,
                &SubmitOptions::default(),
                1,
                CompactTrigger::TokenThreshold,
            )
            .await;

        assert!(
            matches!(
                outcome,
                CompactOutcome::NoProgress {
                    effective: CompactStrategy::LastResort,
                    trigger: CompactTrigger::TokenThreshold,
                    ..
                }
            ),
            "a spent budget must not end the attempt: {outcome:?}"
        );
    }

    /// A blocked context with an open breaker still gets the pass before the
    /// turn fails. The figlet run died 357 tokens over the blocked line
    /// without trying, after two hours of productive work.
    #[tokio::test]
    async fn a_blocked_context_is_not_fatal_before_a_last_resort_pass() {
        let (mut engine, mut rx, _session) = make_test_engine();
        engine.learn_context_window(32_000);
        let thresholds = engine.compact_thresholds();

        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        // Far past the blocked line and unshrinkable: one user message, no
        // tool result for the last resort to trim. The pass must still run.
        turn.messages = vec![Message::user("x".repeat(400_000))];
        turn.compact_failures = engine.compact_config.max_consecutive_failures;
        assert!(
            thresholds.level(engine.turn_context_tokens(&turn)) == compact::ContextLevel::Blocked,
            "the fixture must be past the blocked line"
        );

        let reason = engine
            .maybe_compact_on_threshold(&mut turn, &SubmitOptions::default(), 1)
            .await;

        let mut attempts = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let EngineEvent::Compaction(CompactionEvent::NoProgress {
                effective, trigger, ..
            }) = event
            {
                attempts.push((effective, trigger));
            }
        }
        assert_eq!(
            attempts,
            vec![("LastResort".to_string(), "token_threshold".to_string())],
            "the blocked context must be attempted before it is fatal"
        );
        // The pass cannot shrink a single user message, so the line is still
        // full and the turn ends — but only after trying.
        assert!(
            matches!(reason, Some(TerminalReason::Error(_))),
            "{reason:?}"
        );
    }

    /// The ladder from `Auto` reaches `Snip` even when this pass's own
    /// selection would have chosen `Micro` again.
    #[tokio::test]
    async fn the_ladder_reaches_snip_from_auto() {
        let (mut engine, _rx, _session) = make_test_engine();
        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        // One message: the summary path has no interval, so the rule ladder
        // is what runs. The configured selection would be `Micro`.
        turn.messages = vec![Message::user("x".repeat(200_000))];
        turn.last_compact_strategy = Some(CompactStrategy::Auto);

        let outcome = engine
            .run_compact_recovery(
                &mut turn,
                &SubmitOptions::default(),
                1,
                CompactTrigger::TokenThreshold,
            )
            .await;

        assert!(
            matches!(
                outcome,
                CompactOutcome::Compacted {
                    strategy: CompactStrategy::Snip,
                    ..
                } | CompactOutcome::NoProgress {
                    strategy: CompactStrategy::Snip,
                    ..
                }
            ),
            "the ladder stayed on Auto: {outcome:?}"
        );
    }

    /// A failed maintenance pass is not a compaction failure: it spends no
    /// model call, removed nothing, and must not arm the breaker that exists
    /// to stop retrying a failing summarizer.
    #[tokio::test]
    async fn a_cache_ttl_no_op_does_not_open_the_breaker() {
        let (mut engine, _rx, _session) = make_test_engine();
        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        turn.messages = vec![Message::user("hello")];

        let outcome = engine
            .run_compact_recovery(
                &mut turn,
                &SubmitOptions::default(),
                1,
                CompactTrigger::CacheTtl,
            )
            .await;

        assert!(
            matches!(outcome, CompactOutcome::NoProgress { .. }),
            "{outcome:?}"
        );
        assert_eq!(turn.compact_failures, 0, "a no-op charged the breaker");
        assert_eq!(
            turn.last_compact_strategy, None,
            "a maintenance no-op entered the escalation ladder"
        );
    }

    /// The progress line is the landing line, not the trigger: a pass that
    /// lands exactly on `auto` has bought no headroom.
    #[test]
    fn a_pass_that_lands_on_the_trigger_line_is_not_progress() {
        let (mut engine, _rx, _session) = make_test_engine();
        engine.learn_context_window(131_072);
        let thresholds = engine.compact_thresholds();

        let landing = engine.heuristic_landing_line(thresholds.auto, thresholds.auto);
        assert_eq!(landing, thresholds.landing());

        // Over the trigger, shrunk to exactly the trigger: still a failure.
        assert!(!QueryEngine::context_back_below_line(
            thresholds.auto + 1_000,
            thresholds.auto,
            landing
        ));
        // Shrunk to the landing line: a recovery.
        assert!(QueryEngine::context_back_below_line(
            thresholds.auto + 1_000,
            thresholds.landing(),
            landing
        ));
    }

    /// The event carries the rung, the effective pass, the trigger, both
    /// sizes, the landing target and the non-compactable prefix — the fields
    /// a replay needs to explain a run without the engine's logs.
    #[test]
    fn compact_event_records_the_rung_and_targets() {
        let compacted = CompactOutcome::Compacted {
            strategy: CompactStrategy::Auto,
            effective: CompactStrategy::Micro,
            trigger: CompactTrigger::TokenThreshold,
            tokens_removed: 1_000,
            messages_remaining: 4,
            summary: SummaryOutcome::NotRequested,
            tokens_before: 100_000,
            tokens_after: 99_000,
            target_tokens: 78_072,
            prefix_tokens: 800,
            measured_tokens: Some(97_500),
            fixed_tokens: 8_100,
        };
        match compaction_event_from_outcome(&compacted) {
            CompactionEvent::Compacted {
                strategy,
                effective,
                trigger,
                tokens_removed,
                messages_remaining,
                tokens_before,
                tokens_after,
                target_tokens,
                prefix_tokens,
                summary,
                measured_tokens,
                fixed_tokens,
            } => {
                assert_eq!(summary, "not_requested");
                assert_eq!(measured_tokens, Some(97_500));
                assert_eq!(fixed_tokens, 8_100);
                assert_eq!(strategy, "Auto");
                assert_eq!(effective, "Micro");
                assert_eq!(trigger, "token_threshold");
                assert_eq!(tokens_removed, 1_000);
                assert_eq!(messages_remaining, 4);
                assert_eq!(tokens_before, 100_000);
                assert_eq!(tokens_after, 99_000);
                assert_eq!(target_tokens, 78_072);
                assert_eq!(prefix_tokens, 800);
            }
            other => panic!("wrong event: {other:?}"),
        }

        let no_progress = CompactOutcome::NoProgress {
            strategy: CompactStrategy::LastResort,
            effective: CompactStrategy::LastResort,
            trigger: CompactTrigger::TokenThreshold,
            tokens_before: 130_000,
            tokens_after: 130_000,
            summary: SummaryOutcome::NotRequested,
        };
        assert!(matches!(
            compaction_event_from_outcome(&no_progress),
            CompactionEvent::NoProgress {
                ref strategy,
                ref effective,
                ref trigger,
                ..
            } if strategy == "LastResort"
                && effective == "LastResort"
                && trigger == "token_threshold"
        ));
    }

    /// Only the outcomes that end the turn carry a message.
    #[test]
    fn only_a_fatal_compaction_outcome_ends_the_turn() {
        let compacted = CompactOutcome::Compacted {
            strategy: CompactStrategy::Micro,
            effective: CompactStrategy::Micro,
            trigger: CompactTrigger::TokenThreshold,
            tokens_removed: 10,
            messages_remaining: 2,
            summary: SummaryOutcome::Summarized,
            tokens_before: 100,
            tokens_after: 90,
            target_tokens: 80,
            prefix_tokens: 10,
            measured_tokens: None,
            fixed_tokens: 10,
        };
        assert!(compacted.failure_message().is_none());

        let deferred = CompactOutcome::DeferredToThresholdPath {
            estimated_tokens: 200_000,
        };
        assert!(deferred.failure_message().is_none());

        let stopped = CompactOutcome::StopHookTerminated {
            reason: "stop".into(),
        };
        assert_eq!(stopped.failure_message().as_deref(), Some("stop"));
    }

    // ─── Pre-compaction backup ─────────────────────────────────────────────

    /// The transcript is copied aside before compaction overwrites it, and a
    /// second compaction does not overwrite the first backup.
    #[test]
    fn a_backup_keeps_what_compaction_is_about_to_discard() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("session.jsonl");
        QueryEngine::write_transcript_replacement(&path, &[Message::user("the history")]).unwrap();

        let first = QueryEngine::write_transcript_backup(&path)
            .unwrap()
            .expect("a backup");
        assert!(first.exists());
        assert_eq!(
            std::fs::read_to_string(&first).unwrap(),
            std::fs::read_to_string(&path).unwrap()
        );

        std::thread::sleep(std::time::Duration::from_millis(2));
        QueryEngine::write_transcript_replacement(&path, &[Message::user("compacted")]).unwrap();
        let second = QueryEngine::write_transcript_backup(&path)
            .unwrap()
            .expect("a second backup");

        assert_ne!(first, second, "the first backup must survive a second");
        assert!(
            std::fs::read_to_string(&first)
                .unwrap()
                .contains("the history")
        );
    }

    /// There is nothing to back up before the first write.
    #[test]
    fn there_is_nothing_to_back_up_before_the_first_write() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("session.jsonl");
        assert!(
            QueryEngine::write_transcript_backup(&path)
                .unwrap()
                .is_none()
        );
    }

    /// Compacting a session with a transcript on disk leaves a backup holding
    /// the pre-compaction history.
    #[tokio::test]
    async fn compaction_backs_up_the_transcript_before_overwriting_it() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut engine, _rx, session) = make_test_engine();
        engine.set_transcript_path(tmp.path().to_path_buf());

        let transcript = tmp.path().join(format!("{}.jsonl", session.session_id));
        QueryEngine::write_transcript_replacement(&transcript, &[Message::user("the history")])
            .unwrap();

        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        turn.messages = vec![Message::user("a much longer history than this")];

        let outcome = engine
            .finish_compaction(
                &mut turn,
                CompactResult {
                    strategy: CompactStrategy::Micro,
                    effective: CompactStrategy::Micro,
                    messages: vec![Message::user("short")],
                    tokens_removed: 0,
                    boundary_inserted: false,
                    boundary_index: None,
                },
                CompactTrigger::TokenThreshold,
                SummaryOutcome::Summarized,
            )
            .await;

        assert!(
            matches!(outcome, CompactOutcome::Compacted { .. }),
            "{outcome:?}"
        );
        assert!(
            std::fs::read_to_string(&transcript)
                .unwrap()
                .contains("short"),
            "the live transcript should hold the compacted messages"
        );

        let backups: Vec<PathBuf> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.to_string_lossy().contains(".backup."))
            .collect();
        assert_eq!(backups.len(), 1, "one backup, got {backups:?}");
        assert!(
            std::fs::read_to_string(&backups[0])
                .unwrap()
                .contains("the history"),
            "the backup should hold the pre-compaction transcript"
        );
    }

    // ─── Compaction writes back ────────────────────────────────────────────

    /// Compaction replaces the history, not the request. The system prompt
    /// sits outside the history range — it is rebuilt every turn — and a
    /// compaction that dropped it would send the next turn with no
    /// instructions at all.
    #[tokio::test]
    async fn compaction_keeps_the_system_prompt() {
        let (mut engine, _rx, _session) = make_test_engine();
        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        turn.messages = vec![
            Message::system("You are Codape. Follow the repository conventions."),
            Message::user("the request"),
        ];
        turn.set_history_range(1, 2);
        for i in 0..10 {
            turn.messages.push(Message::assistant(format!(
                "step {i}: {}",
                "x".repeat(2_000)
            )));
        }
        turn.set_history_range(1, turn.messages.len());

        engine
            .run_compact_recovery(
                &mut turn,
                &SubmitOptions::default(),
                1,
                CompactTrigger::TokenThreshold,
            )
            .await;

        assert_eq!(
            turn.messages[0].role,
            crate::types::Role::System,
            "the system prompt was compacted away: {:?}",
            turn.messages.first()
        );
        assert!(
            matches!(
                &turn.messages[0].content,
                crate::types::MessageContent::Text(text) if text.starts_with("You are Codape")
            ),
            "the first message is no longer the system prompt"
        );
    }

    /// Compaction is only durable if the session — what the next turn builds
    /// from — is updated too. Writing only to the turn made an automatic
    /// compaction last exactly one request.
    #[tokio::test]
    async fn compaction_writes_back_to_the_session() {
        let (mut engine, _rx, _session) = make_test_engine();
        engine.session.messages = vec![Message::user("the request")];
        for i in 0..20 {
            engine.session.messages.push(Message::assistant(format!(
                "step {i}: {}",
                "x".repeat(2_000)
            )));
        }
        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        turn.messages = engine.session.messages.clone();
        turn.set_history_range(0, turn.messages.len());
        let before = engine.session.messages.len();

        engine
            .run_compact_recovery(
                &mut turn,
                &SubmitOptions::default(),
                1,
                CompactTrigger::TokenThreshold,
            )
            .await;

        assert!(
            engine.session.messages.len() < before,
            "the session still holds the pre-compaction history: {} vs {}",
            engine.session.messages.len(),
            before
        );
        assert_eq!(
            engine.session.messages.len(),
            turn.messages.len(),
            "the session and the compacted turn disagree"
        );
    }

    /// The preserved anchor is the first user message — the task prompt — not
    /// whatever the compiler put before it. CLAUDE.md arrives as a user
    /// message too, and preserving that instead loses the request.
    #[tokio::test]
    async fn compaction_preserves_the_task_prompt_as_anchor() {
        let (mut engine, _rx, _session) = make_test_engine();
        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        // Preamble injected by `build_context`, then the system prompt.
        turn.messages = vec![
            Message::user("<system-reminder>CLAUDE.md contents</system-reminder>"),
            Message::system("You are Codape."),
            Message::user("Fix the failing parser test"),
        ];
        turn.set_history_range(2, 3);
        for i in 0..10 {
            turn.messages.push(Message::assistant(format!(
                "step {i}: {}",
                "x".repeat(2_000)
            )));
        }
        turn.set_history_range(2, turn.messages.len());

        engine
            .run_compact_recovery(
                &mut turn,
                &SubmitOptions::default(),
                1,
                CompactTrigger::TokenThreshold,
            )
            .await;

        let history = turn.history();
        assert!(
            matches!(
                &history[0].content,
                crate::types::MessageContent::Text(text)
                    if text == "Fix the failing parser test"
            ),
            "the task prompt was not preserved: {:?}",
            history.first()
        );
    }

    // ─── Progress judgement ────────────────────────────────────────────────

    /// A pass that removes 35 tokens of 20 000 has not recovered anything: it
    /// must not be reported as progress, must not reset the circuit breaker,
    /// and must not spend a recovery retry.
    #[tokio::test]
    async fn a_35_token_pass_is_not_reported_as_progress() {
        let (mut engine, _rx, _session) = make_test_engine();
        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        turn.messages = vec![Message::user("the request")];
        for i in 0..4 {
            turn.messages.push(Message::assistant(format!(
                "step {i}: {}",
                "x".repeat(10_000)
            )));
        }
        turn.set_history_range(0, turn.messages.len());
        turn.compact_failures = 2;

        // What the rules would do on their own: a small, ineffective trim.
        let mut trimmed = turn.messages.clone();
        trimmed.push(Message::assistant("x".repeat(140)));

        let outcome = engine
            .finish_compaction(
                &mut turn,
                CompactResult {
                    strategy: CompactStrategy::Micro,
                    effective: CompactStrategy::Micro,
                    messages: trimmed,
                    tokens_removed: 0,
                    boundary_inserted: false,
                    boundary_index: None,
                },
                CompactTrigger::TokenThreshold,
                SummaryOutcome::NotRequested,
            )
            .await;

        assert!(
            matches!(outcome, CompactOutcome::NoProgress { .. }),
            "{outcome:?}"
        );
        assert_eq!(
            turn.compact_failures, 3,
            "an ineffective pass must count toward the breaker"
        );
        assert_eq!(
            turn.recovery_count, 0,
            "an ineffective pass must not spend a recovery retry"
        );
    }

    /// The strategy that failed is kept for the next attempt, and it survives
    /// the turn boundary: three turns used to retry the same `Auto` pass until
    /// the breaker ended a run that was still productive.
    #[tokio::test]
    async fn an_ineffective_pass_leaves_its_strategy_for_escalation() {
        let (mut engine, _rx, _session) = make_test_engine();
        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        turn.messages = vec![
            Message::user("the request"),
            Message::assistant("x".repeat(10_000)),
        ];

        let unchanged = turn.messages.clone();
        let outcome = engine
            .finish_compaction(
                &mut turn,
                CompactResult {
                    strategy: CompactStrategy::Auto,
                    effective: CompactStrategy::Auto,
                    messages: unchanged,
                    tokens_removed: 0,
                    boundary_inserted: false,
                    boundary_index: None,
                },
                CompactTrigger::TokenThreshold,
                SummaryOutcome::NotRequested,
            )
            .await;

        assert!(
            matches!(outcome, CompactOutcome::NoProgress { .. }),
            "{outcome:?}"
        );
        assert_eq!(turn.last_compact_strategy, Some(CompactStrategy::Auto));

        turn.reset();
        assert_eq!(
            turn.last_compact_strategy,
            Some(CompactStrategy::Auto),
            "the turn reset dropped the escalation ladder"
        );
    }

    /// A pass that recovered the context starts the next crossing from the
    /// lightest strategy the line allows, not from the heavy one that worked
    /// once.
    #[tokio::test]
    async fn a_progressing_pass_clears_the_escalation_ladder() {
        let (mut engine, _rx, _session) = make_test_engine();
        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        turn.messages = vec![
            Message::user("the request"),
            Message::assistant("x".repeat(10_000)),
        ];
        turn.last_compact_strategy = Some(CompactStrategy::Snip);

        let outcome = engine
            .finish_compaction(
                &mut turn,
                CompactResult {
                    strategy: CompactStrategy::Snip,
                    effective: CompactStrategy::Snip,
                    messages: vec![Message::user("the request")],
                    tokens_removed: 0,
                    boundary_inserted: false,
                    boundary_index: None,
                },
                CompactTrigger::TokenThreshold,
                SummaryOutcome::NotRequested,
            )
            .await;

        assert!(
            matches!(outcome, CompactOutcome::Compacted { .. }),
            "{outcome:?}"
        );
        assert_eq!(turn.last_compact_strategy, None);
        assert_eq!(turn.compact_failures, 0);
    }

    /// The provider's count is the ruler the line is stated in; the strategies
    /// count `chars / 4`. On tool-heavy traffic those differ by a third, and a
    /// budget copied unchanged is one the history already meets on the
    /// strategy's own ruler — so the pass removes nothing.
    #[test]
    fn a_provider_measurement_scales_the_rule_targets() {
        let (mut engine, _rx, _session) = make_test_engine();
        engine.learn_context_window(131_072);
        let thresholds = engine.compact_thresholds();
        assert!(
            engine.compact_config.snip_target_tokens > thresholds.landing(),
            "the fixture expects the landing line to cap Snip's configured target"
        );

        // 90 000 heuristic tokens that the provider measured at 123 000. Both
        // rule targets aim at the landing line, one buffer below the trigger.
        let (auto_target, snip_target) = engine.rule_based_targets(123_000, 90_000, 0);
        assert_eq!(auto_target, thresholds.landing() * 90_000 / 123_000);
        assert_eq!(snip_target, thresholds.landing() * 90_000 / 123_000);
        assert!(auto_target < thresholds.landing());

        // The non-compactable prefix is subtracted after the translation: the
        // strategies trim history, and the target is a whole-request budget.
        let (with_prefix, _) = engine.rule_based_targets(123_000, 90_000, 20_000);
        assert_eq!(with_prefix, auto_target - 20_000);

        // A heuristic that already over-counts, or an equal count, is not
        // evidence of a larger real context: the budget stays as stated.
        assert_eq!(
            engine.rule_based_targets(90_000, 90_000, 0).0,
            thresholds.landing()
        );
        assert_eq!(
            engine.rule_based_targets(60_000, 90_000, 0).0,
            thresholds.landing()
        );
    }

    /// Under the scaled target Snip actually trims a history that the
    /// configured target would have found already small enough.
    #[test]
    fn a_scaled_snip_target_trims_a_history_the_unscaled_one_would_not() {
        let (mut engine, _rx, _session) = make_test_engine();
        engine.learn_context_window(131_072);

        // 7 500 tokens per message × 10 ≈ 75 000 heuristic tokens, measured at
        // 103 000 by the provider.
        let mut messages = vec![Message::user("the request")];
        for i in 0..10 {
            messages.push(Message::assistant(format!("{i}: {}", "x".repeat(30_000))));
        }
        let heuristic = PromptCompiler::estimate_message_tokens(&messages);
        assert!(
            heuristic > 74_000 && heuristic < 76_000,
            "the fixture is off: {heuristic}"
        );

        let configured_snip = engine.compact_config.snip_target_tokens;
        let unscaled = compact::snip_compact(&messages, configured_snip);
        assert!(
            unscaled.tokens_removed == 0,
            "the configured target should already be met on the heuristic ruler"
        );

        let (_, snip_target) = engine.rule_based_targets(103_000, heuristic, 0);
        let scaled = compact::snip_compact(&messages, snip_target);
        let after = PromptCompiler::estimate_message_tokens(&scaled.messages);
        assert!(
            after < heuristic,
            "the scaled target removed nothing: {heuristic} -> {after}"
        );
        assert!(
            after <= snip_target,
            "Snip trimmed to {after}, above its own target {snip_target}"
        );
    }

    /// The tool schemas are sent with every request and are not messages; the
    /// request estimate has to carry them, or it describes a request several
    /// thousand tokens smaller than the one the provider measures.
    #[test]
    fn tool_schemas_count_toward_the_request_estimate() {
        let tools = vec![ToolInfo {
            name: "bash".into(),
            description: "Run a shell command".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {"command": {"type": "string"}},
            }),
        }];
        let tokens = QueryEngine::tool_schema_tokens(&tools);
        assert!(tokens > 0, "a serialized schema costs tokens");
        assert_eq!(
            QueryEngine::tool_schema_tokens(&[]),
            0,
            "no tools, no overhead"
        );

        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        turn.messages = vec![Message::system("system prompt"), Message::user("hi")];
        turn.set_history_range(1, 2);
        let without = turn.context_tokens(None);
        turn.tool_schema_tokens = tokens;
        assert_eq!(turn.context_tokens(None), without + tokens);
        assert_eq!(
            turn.context_prefix_tokens(),
            PromptCompiler::estimate_message_tokens(&turn.messages[..1]) + tokens
        );

        // An anchor's measurement already covers the schemas; counting them
        // again would shrink every target by their size.
        let anchor = UsageAnchor {
            history_messages: 1,
            prefix_tokens: turn.context_prefix_tokens(),
            prompt_tokens: 500,
            fixed_tokens: 480,
        };
        assert_eq!(turn.context_tokens(Some(anchor)), 500);
    }

    /// The provider measures everything the request contains, including the
    /// tool schemas and the scaffolding the message estimator cannot see.
    /// What it charged beyond the history is a measurement of the part
    /// compaction cannot touch, and targets translated against it leave that
    /// part alone.
    #[test]
    fn a_measurement_learns_the_fixed_overhead_targets_subtract() {
        let (mut engine, _rx, _session) = make_test_engine();
        engine.learn_context_window(131_072);
        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        turn.messages = vec![Message::system("s"), Message::user("x".repeat(40_000))];
        turn.set_history_range(1, 2);
        turn.tool_schema_tokens = 2_000;

        let heuristic =
            PromptCompiler::estimate_message_tokens(&turn.messages) + turn.tool_schema_tokens;
        let history = PromptCompiler::estimate_message_tokens(turn.history());
        let prefix = turn.context_prefix_tokens();

        engine.record_request_shape(&turn);
        engine.record_prompt_tokens(
            &turn,
            &UsageSummary {
                input_tokens: history + 12_000,
                output_tokens: 30,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            },
        );
        let anchor = engine.usage_anchor.expect("the measurement is anchored");
        assert_eq!(
            anchor.fixed_tokens, 12_000,
            "the measured overhead is prompt minus the measured request's history"
        );

        let measured_target = engine
            .rule_based_targets(history + 12_000, heuristic, prefix)
            .0;
        let (mut unmeasured, _rx2, _session2) = make_test_engine();
        unmeasured.learn_context_window(131_072);
        let prefix_target = unmeasured
            .rule_based_targets(history + 12_000, heuristic, prefix)
            .0;
        assert!(
            measured_target < prefix_target,
            "the measured overhead ({}) must be subtracted, not the message prefix ({prefix}): \
             {measured_target} vs {prefix_target}",
            anchor.fixed_tokens
        );
    }

    /// Nothing the engine does to the history can put the request under the
    /// landing line when the fixed part alone fills it. Running a pass anyway
    /// destroys working context and then repeats next turn; the outcome says
    /// so instead.
    #[tokio::test]
    async fn a_request_whose_fixed_part_fills_the_landing_line_is_uncompactable() {
        let (mut engine, _rx, _session) = make_test_engine();
        // An unknown model: the window is a 32k guess, exactly the setup in
        // which a 128k provider thrashed through 241 compaction passes.
        engine.model = ModelId("mystery-model".into());
        engine.set_context_window_resolver(ContextWindowResolver::new(
            crate::types::ContextWindowOverrides::default(),
            crate::types::ContextWindowOverrides::default(),
            crate::types::ContextWindowOverrides::default(),
            crate::types::ContextWindowOverrides::default(),
        ));

        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        turn.messages = vec![Message::system("s"), Message::user("x".repeat(4_000))];
        turn.set_history_range(1, 2);
        let prefix = turn.context_prefix_tokens();
        // The provider measured the small request at 16 600 tokens, of which
        // 8 600 are not history: system prompt, tool schemas, scaffolding.
        engine.usage_anchor = Some(UsageAnchor {
            history_messages: 1,
            prefix_tokens: prefix,
            prompt_tokens: 16_600,
            fixed_tokens: 8_600,
        });

        let outcome = engine
            .run_compact_recovery(
                &mut turn,
                &SubmitOptions::default(),
                1,
                CompactTrigger::TokenThreshold,
            )
            .await;

        assert!(
            matches!(outcome, CompactOutcome::Uncompactable { .. }),
            "{outcome:?}"
        );
        if let CompactOutcome::Uncompactable {
            fixed_tokens,
            landing_tokens,
            minimum_tokens,
        } = outcome
        {
            assert!(
                landing_tokens <= minimum_tokens,
                "the report must show why no pass can reach the line: {fixed_tokens}, \
                 {landing_tokens}, {minimum_tokens}"
            );
        }
    }

    /// The r2 pathology end to end: an unknown model, the 32k guess, and a
    /// request the provider measured almost entirely as fixed overhead. The
    /// pass stands down without touching the history, the request goes out as
    /// it is, and the acceptance teaches the window a lower bound — so the
    /// next check is not the same arithmetic with the same outcome. Before
    /// these guards, this shape ran 241 passes and submitted nothing.
    #[tokio::test]
    async fn an_unknown_window_that_cannot_compact_learns_from_the_request_it_sends() {
        let (mut engine, mut rx, _session) = make_test_engine();
        engine.model = ModelId("mystery-model".into());
        engine.set_context_window_resolver(ContextWindowResolver::new(
            crate::types::ContextWindowOverrides::default(),
            crate::types::ContextWindowOverrides::default(),
            crate::types::ContextWindowOverrides::default(),
            crate::types::ContextWindowOverrides::default(),
        ));
        assert_eq!(
            engine.context_window().tokens,
            crate::types::UNKNOWN_CONTEXT_WINDOW
        );

        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        turn.messages = vec![Message::system("s"), Message::user("x".repeat(40_000))];
        turn.set_history_range(1, 2);
        let history_before = turn.history().to_vec();
        let prefix = turn.context_prefix_tokens();
        // The provider measured the request at 19 000 tokens, 8 600 of them
        // not history. Against the 32k guess the landing line is a few
        // thousand tokens: the fixed part alone fills it.
        engine.usage_anchor = Some(UsageAnchor {
            history_messages: 1,
            prefix_tokens: prefix,
            prompt_tokens: 19_000,
            fixed_tokens: 8_600,
        });

        let outcome = engine
            .run_compact_recovery(
                &mut turn,
                &SubmitOptions::default(),
                1,
                CompactTrigger::TokenThreshold,
            )
            .await;
        assert!(
            matches!(outcome, CompactOutcome::Uncompactable { .. }),
            "{outcome:?}"
        );
        assert_eq!(
            turn.history().len(),
            history_before.len(),
            "no pass may delete history"
        );
        assert_eq!(
            PromptCompiler::estimate_message_tokens(turn.history()),
            PromptCompiler::estimate_message_tokens(&history_before),
            "no message may be rewritten"
        );
        assert_eq!(turn.compact_no_progress, 0, "no pass may have run");
        assert_eq!(turn.recovery_count, 0, "no retry may have been spent");

        let mut saw_uncompactable = false;
        while let Ok(event) = rx.try_recv() {
            match event {
                EngineEvent::Compaction(CompactionEvent::Uncompactable { .. }) => {
                    saw_uncompactable = true;
                }
                EngineEvent::Compaction(CompactionEvent::Compacted { .. }) => {
                    panic!("a pass ran: {event:?}");
                }
                _ => {}
            }
        }
        assert!(saw_uncompactable, "the stand-down must be observable");

        // The request is sent as it is and the provider accepts it. Its input
        // plus the requested output is a proven lower bound on the window.
        engine.record_request_shape(&turn);
        engine.record_prompt_tokens(
            &turn,
            &UsageSummary {
                input_tokens: 19_000,
                output_tokens: 20,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            },
        );
        let expected = 19_000 + QueryEngine::DEFAULT_MAX_TOKENS as usize;
        assert_eq!(
            engine.context_window().tokens,
            expected,
            "the accepted request must raise the guess"
        );
        assert!(engine.context_window().tokens > crate::types::UNKNOWN_CONTEXT_WINDOW);
    }

    /// An accepted request proves the window was at least its input plus the
    /// requested output. That is the only evidence about an unknown model's
    /// window before an overflow, and it moves the guess in the safe direction.
    #[test]
    fn an_accepted_request_raises_the_window_lower_bound() {
        let (mut engine, _rx, _session) = make_test_engine();
        engine.model = ModelId("mystery-model".into());
        engine.set_context_window_resolver(ContextWindowResolver::new(
            crate::types::ContextWindowOverrides::default(),
            crate::types::ContextWindowOverrides::default(),
            crate::types::ContextWindowOverrides::default(),
            crate::types::ContextWindowOverrides::default(),
        ));
        assert_eq!(
            engine.context_window().tokens,
            crate::types::UNKNOWN_CONTEXT_WINDOW
        );

        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        turn.messages = vec![Message::user("x".repeat(240_000))];
        engine.record_request_shape(&turn);
        engine.record_prompt_tokens(
            &turn,
            &UsageSummary {
                input_tokens: 60_000,
                output_tokens: 10,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            },
        );
        let expected = 60_000 + QueryEngine::DEFAULT_MAX_TOKENS as usize;
        assert_eq!(engine.context_window().tokens, expected);

        // A smaller accepted request teaches nothing new, and nothing lowers
        // a bound that a larger request already proved.
        let mut smaller = TurnRuntime::new(TokenBudget::with_defaults());
        smaller.messages = vec![Message::user("x".repeat(40_000))];
        engine.record_request_shape(&smaller);
        engine.record_prompt_tokens(
            &smaller,
            &UsageSummary {
                input_tokens: 10_000,
                output_tokens: 10,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            },
        );
        assert_eq!(engine.context_window().tokens, expected);
    }

    /// A pass that removed nothing will remove nothing again at the same size.
    /// The retry waits for measured growth worth a twentieth of the line.
    #[test]
    fn a_no_progress_pass_sets_the_size_at_which_a_retry_is_worth_it() {
        let (mut engine, _rx, _session) = make_test_engine();
        engine.learn_context_window(131_072);
        let thresholds = engine.compact_thresholds();
        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        assert_eq!(
            QueryEngine::no_progress_retry_at(&turn, &thresholds),
            None,
            "nothing has failed yet"
        );

        turn.compact_no_progress = 2;
        turn.compact_no_progress_tokens = 90_000;
        let margin = (thresholds.auto / 20).max(2_000);
        assert_eq!(
            QueryEngine::no_progress_retry_at(&turn, &thresholds),
            Some(90_000 + margin)
        );
    }

    /// The caller-side effect: above the compact line, with a recent pass that
    /// removed nothing at this size, the turn sends the request instead of
    /// compacting again — and says on the event stream why.
    #[tokio::test]
    async fn a_context_that_stopped_yielding_to_compaction_is_not_retried() {
        let (mut engine, mut rx, _session) = make_test_engine();
        engine.learn_context_window(131_072);
        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        // ~110 000 heuristic tokens: over the auto line, below blocked.
        turn.messages = vec![Message::user("x".repeat(440_000))];
        let measured = engine.turn_context_tokens(&turn);
        turn.compact_no_progress = 1;
        turn.compact_no_progress_tokens = measured;

        let reason = engine
            .maybe_compact_on_threshold(&mut turn, &SubmitOptions::default(), 1)
            .await;
        assert!(reason.is_none(), "{reason:?}");
        assert_eq!(turn.compact_failures, 0, "no pass may have run");
        assert_eq!(turn.recovery_count, 0, "no pass may have run");

        let mut backed_off = None;
        while let Ok(event) = rx.try_recv() {
            if let EngineEvent::Compaction(CompactionEvent::BackedOff {
                estimated_tokens,
                retry_at,
                ..
            }) = event
            {
                backed_off = Some((estimated_tokens, retry_at));
            }
        }
        let (estimated_tokens, retry_at) =
            backed_off.expect("the backoff must be observable on the event stream");
        assert_eq!(estimated_tokens, measured);
        assert!(retry_at > measured);
    }

    /// The line a pass is judged against is the provider's, translated. A
    /// result that looks under `auto` to the heuristic but is still over it to
    /// the provider — and removed less than a tenth — is not a recovery.
    #[tokio::test]
    async fn a_pass_between_the_measured_and_heuristic_lines_is_not_progress() {
        let (mut engine, _rx, _session) = make_test_engine();
        engine.learn_context_window(131_072);
        let auto = engine.compact_thresholds().auto;

        // 25 × 10 000 chars ≈ 62 500 heuristic tokens, measured at 90 000.
        // The measurement is under the window minus the requested output, so
        // accepting it is consistent with the window this test states.
        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        turn.messages = vec![Message::user("the request")];
        for _ in 0..25 {
            turn.messages.push(Message::assistant("x".repeat(10_000)));
        }
        let before = PromptCompiler::estimate_message_tokens(&turn.messages);
        engine.record_request_shape(&turn);
        engine.record_prompt_tokens(
            &turn,
            &UsageSummary {
                input_tokens: 90_000,
                output_tokens: 30,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            },
        );
        assert_eq!(
            engine.context_window().tokens,
            131_072,
            "a measurement that fits must not move the window"
        );

        // Drop two messages: the heuristic result lands under the raw line,
        // and the pass removed under a tenth of the context.
        let mut trimmed = turn.messages.clone();
        trimmed.remove(1);
        trimmed.remove(1);
        let after = PromptCompiler::estimate_message_tokens(&trimmed);
        assert!(after <= auto, "fixture: {after} vs the raw line {auto}");
        assert!(
            (before - after) * 10 < before,
            "fixture: the pass has to remove under a tenth"
        );

        let outcome = engine
            .finish_compaction(
                &mut turn,
                CompactResult {
                    strategy: CompactStrategy::Auto,
                    effective: CompactStrategy::Auto,
                    messages: trimmed,
                    tokens_removed: 0,
                    boundary_inserted: false,
                    boundary_index: None,
                },
                CompactTrigger::TokenThreshold,
                SummaryOutcome::NotRequested,
            )
            .await;

        assert!(
            matches!(outcome, CompactOutcome::NoProgress { .. }),
            "{outcome:?}"
        );
        assert_eq!(
            turn.compact_failures, 1,
            "a pass judged over the line by the provider must count as a failure"
        );
    }

    /// An open breaker stops compaction, not the turn. Below the blocked line
    /// the provider still accepts the request, so the turn continues; only a
    /// full window is terminal.
    #[tokio::test]
    async fn an_open_breaker_does_not_end_a_turn_below_the_blocked_line() {
        let (mut engine, _rx, _session) = make_test_engine();
        engine.learn_context_window(131_072);

        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        turn.messages = vec![Message::user("x".repeat(400_000))];
        turn.compact_failures = engine.compact_config.max_consecutive_failures;

        let reason = engine
            .maybe_compact_on_threshold(&mut turn, &SubmitOptions::default(), 1)
            .await;
        assert!(reason.is_none(), "{reason:?}");

        // The same breaker with a context past the blocked line is terminal.
        turn.messages = vec![Message::user("x".repeat(600_000))];
        let reason = engine
            .maybe_compact_on_threshold(&mut turn, &SubmitOptions::default(), 1)
            .await;
        assert!(
            matches!(reason, Some(TerminalReason::Error(_))),
            "{reason:?}"
        );
    }

    /// A `Compact`-level pass that did not reach the line is checked again:
    /// the turn must not proceed while still past it.
    #[tokio::test]
    async fn a_compact_level_pass_is_rechecked() {
        let (mut engine, _rx, _session) = make_test_engine();
        // A stated window, so the blocked line is enforced.
        engine.learn_context_window(32_000);
        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        // Absurd for a 32K window, and unshrinkable: one message.
        turn.messages = vec![Message::user("x".repeat(400_000))];

        let reason = engine
            .maybe_compact_on_threshold(&mut turn, &SubmitOptions::default(), 1)
            .await;

        let reason = reason.expect("a context still past the line after compaction must stop");
        assert!(
            matches!(reason, TerminalReason::Error(ref message) if message.contains("context")),
            "the reason does not name the context: {reason:?}"
        );
    }

    // ─── Provider-reported overflow ────────────────────────────────────────

    /// An overflowing mock: the first request is refused with the provider's
    /// real window, later ones succeed.
    struct OverflowThenOk {
        remaining_overflow: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl OverflowThenOk {
        fn new(overflows: usize) -> Self {
            Self {
                remaining_overflow: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(
                    overflows,
                )),
            }
        }
    }

    #[async_trait::async_trait]
    impl ApiClient for OverflowThenOk {
        fn clone_box(&self) -> Box<dyn ApiClient> {
            Box::new(Self {
                remaining_overflow: std::sync::Arc::clone(&self.remaining_overflow),
            })
        }

        async fn create_message(
            &self,
            _system: &str,
            _messages: &[serde_json::Value],
            _model: &ModelId,
            _max_tokens: u32,
            _tools: &[crate::tool_executor::ToolInfo],
        ) -> Result<ApiResponse, crate::api_client::ApiError> {
            let left = self
                .remaining_overflow
                .try_update(
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                    |n| n.checked_sub(1),
                )
                .is_ok();
            if left {
                return Err(crate::api_client::ApiError::Response {
                    status: 400,
                    message: "prompt is too long: 213462 tokens > 200000 maximum".into(),
                });
            }
            Ok(ApiResponse {
                content: "done".into(),
                tool_calls: Vec::new(),
                usage: UsageSummary {
                    input_tokens: 100,
                    output_tokens: 10,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                },
                stop_reason: "end_turn".into(),
                thinking: None,
            })
        }

        async fn create_message_streaming(
            &self,
            _system: &str,
            _messages: &[serde_json::Value],
            _model: &ModelId,
            _max_tokens: u32,
            _tools: &[crate::tool_executor::ToolInfo],
        ) -> std::pin::Pin<
            Box<dyn futures::Stream<Item = crate::api_client::StreamEvent> + Send + 'static>,
        > {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let _ = tx.send(crate::api_client::StreamEvent::StreamEnd);
            Box::pin(futures::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|item| (item, rx))
            }))
        }
    }

    #[tokio::test]
    async fn a_prompt_too_long_error_teaches_the_real_window() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let checker = PermissionChecker::new(PermissionMode::BypassPermissions);
        let mut engine = QueryEngine::new(
            Box::new(OverflowThenOk::new(1)),
            ModelId::sonnet(),
            SessionState::new(std::path::PathBuf::from("/tmp/test")),
            PermissionEngine::new(checker),
            tx,
        );
        engine.set_tool_executor(Box::new(StubOkToolExecutor));
        engine.compact_config.max_recovery_retries = 3;
        assert_eq!(engine.context_window().tokens, 200_000);

        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        turn.messages = vec![Message::user("the request")];
        for i in 0..10 {
            turn.messages.push(Message::assistant(format!(
                "step {i}: {}",
                "x".repeat(1_000)
            )));
        }
        turn.set_history_range(0, turn.messages.len());

        let turn_error = engine
            .send_request(&turn, &SubmitOptions::default())
            .await
            .expect_err("the mock refuses the first request");
        assert!(
            matches!(turn_error, TurnError::ContextOverflow { limit, .. } if limit == 200_000),
            "{turn_error:?}"
        );

        engine
            .recover_from_context_overflow(&mut turn, turn_error, &SubmitOptions::default(), 1)
            .await
            .expect("compacting makes room for the retry");

        // The provider measured 213 462; that is the window this session uses
        // from now on, not the table's 200 000.
        assert_eq!(engine.context_window().tokens, 213_462);
        assert!(!engine.context_window().source.is_guess());
    }

    #[tokio::test]
    async fn a_learned_window_survives_the_next_turn() {
        let (mut engine, _rx, _session) = make_test_engine();
        let before = engine.compact_thresholds();
        engine.learn_context_window(64_000);
        assert_eq!(
            engine.context_window().tokens,
            64_000,
            "the learned window was not adopted"
        );
        let after = engine.compact_thresholds();
        assert_ne!(
            before, after,
            "the learned window did not move the compaction lines"
        );
        assert_eq!(
            after.blocked,
            CompactConfig::default().resolve_blocked_threshold(64_000)
        );
    }

    // ─── Provider-usage estimation ─────────────────────────────────────────

    /// The provider's own count of the last request beats the character
    /// heuristic: CJK is about one token per character where the heuristic
    /// assumes four, so a Chinese prompt is underestimated three-fold and the
    /// compaction line is crossed far too late.
    #[tokio::test]
    async fn estimation_prefers_provider_usage() {
        let (mut engine, _rx, _session) = make_test_engine();
        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());

        // 400 Chinese characters: the heuristic calls this 300 tokens, the
        // provider would report about 400.
        let cjk = "压缩上下文窗口的正确做法是先把真实大小量出来，而不是靠字符数估算。".repeat(10);
        turn.messages = vec![Message::user(cjk)];
        let heuristic = PromptCompiler::estimate_message_tokens(&turn.messages);
        assert!(
            heuristic < 400,
            "the fixture must be underestimated by the heuristic: {heuristic}"
        );

        assert_eq!(
            engine.turn_context_tokens(&turn),
            heuristic,
            "with no measurement the estimate is the heuristic"
        );

        engine.record_request_shape(&turn);
        engine.record_prompt_tokens(
            &turn,
            &UsageSummary {
                input_tokens: 400,
                output_tokens: 12,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            },
        );
        assert_eq!(
            engine.turn_context_tokens(&turn),
            400,
            "the provider's measurement must replace the heuristic"
        );

        // What arrives after the measured request is estimated on top of it.
        turn.messages.push(Message::assistant("x".repeat(400)));
        assert_eq!(
            engine.turn_context_tokens(&turn),
            500,
            "the appended message must be added to the measured prefix"
        );
    }

    /// The measurement describes the request that produced it. The next turn
    /// rebuilds its history from the same session messages and appends to it,
    /// so the anchor still applies — this is the case that matters, because
    /// the request that overflows is the one after the measurement.
    #[tokio::test]
    async fn a_measurement_carries_into_the_next_turn() {
        let (mut engine, _rx, mut session) = make_test_engine();
        session.messages = vec![Message::user("the task"), Message::assistant("step one")];
        engine.set_session(session);

        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        turn.messages = vec![Message::user("the task"), Message::assistant("step one")];
        turn.set_history_range(0, 2);
        engine.record_request_shape(&turn);
        engine.record_prompt_tokens(
            &turn,
            &UsageSummary {
                input_tokens: 2_000,
                output_tokens: 20,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            },
        );

        // The next turn: a fresh runtime, the same history plus new messages.
        let mut next = TurnRuntime::new(TokenBudget::with_defaults());
        next.messages = vec![
            Message::user("the task"),
            Message::assistant("step one"),
            Message::assistant("step two: a new message the provider has not seen"),
        ];
        next.set_history_range(0, 3);

        let appended = PromptCompiler::estimate_message_tokens(&next.messages[2..]);
        assert_eq!(
            engine.turn_context_tokens(&next),
            2_000 + appended,
            "the previous turn's measurement must still anchor this turn"
        );
    }

    /// Cached input is part of the request. Anthropic reports the uncached
    /// remainder in `input_tokens`; measuring only that would make a
    /// cache-heavy session look almost empty.
    #[tokio::test]
    async fn cached_input_counts_toward_the_measurement() {
        let (mut engine, _rx, _session) = make_test_engine();
        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        turn.messages = vec![Message::user("x".repeat(4_000))];
        engine.record_request_shape(&turn);
        engine.record_prompt_tokens(
            &turn,
            &UsageSummary {
                input_tokens: 200,
                output_tokens: 40,
                cache_read_tokens: 900,
                cache_creation_tokens: 100,
            },
        );
        assert_eq!(engine.turn_context_tokens(&turn), 1_200);
    }

    /// A measurement the request could not have produced — fewer tokens than
    /// the system prompt alone — is not about this request; anchoring on it
    /// would make the context look permanently tiny.
    #[tokio::test]
    async fn a_usage_report_below_the_system_prompt_is_ignored() {
        let (mut engine, _rx, _session) = make_test_engine();
        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        turn.messages = vec![Message::system("s".repeat(8_000)), Message::user("hi")];
        engine.record_request_shape(&turn);
        let before = engine.turn_context_tokens(&turn);

        engine.record_prompt_tokens(
            &turn,
            &UsageSummary {
                input_tokens: 100,
                output_tokens: 5,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            },
        );

        assert_eq!(
            engine.turn_context_tokens(&turn),
            before,
            "a 100-token report cannot describe a request with a 2 000-token prompt"
        );
    }

    /// Compaction replaces the messages the measurement described, so the
    /// anchor must stop applying rather than undercount the new history.
    #[tokio::test]
    async fn compaction_invalidates_the_usage_anchor() {
        let (mut engine, _rx, _session) = make_test_engine();
        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        turn.messages = vec![
            Message::user("start"),
            Message::assistant("y".repeat(8_000)),
            Message::assistant("z".repeat(8_000)),
        ];
        turn.set_history_range(0, 3);
        engine.record_request_shape(&turn);
        engine.record_prompt_tokens(
            &turn,
            &UsageSummary {
                input_tokens: 4_000,
                output_tokens: 10,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            },
        );
        assert_eq!(engine.turn_context_tokens(&turn), 4_000);

        // Compaction folds three messages into one short summary.
        turn.replace_history(vec![Message::user("a much shorter summary")]);

        let heuristic = PromptCompiler::estimate_message_tokens(&turn.messages);
        assert_eq!(
            engine.turn_context_tokens(&turn),
            heuristic,
            "the anchor described messages that are gone; the estimate must fall \
             back to the history as it is now"
        );
    }

    // ─── Background summary pre-generation ─────────────────────────────────

    /// Records how many summaries were requested and serves a canned one.
    struct CountingSummaryBackend {
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl crate::compaction_backend::CompactionBackend for CountingSummaryBackend {
        fn kind(&self) -> crate::compaction_backend::CompactionKind {
            crate::compaction_backend::CompactionKind::Remote
        }

        fn name(&self) -> &'static str {
            "counting"
        }

        async fn summarize(
            &self,
            _request: crate::compaction_backend::CompactionRequest,
        ) -> Result<String, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok("a summary of the middle of the conversation".into())
        }
    }

    /// A conversation big enough to sit between the warning and compaction
    /// lines of a 32K window.
    fn warned_turn() -> TurnRuntime {
        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        turn.messages.push(Message::user("the task for this run"));
        for i in 0..14 {
            turn.messages.push(Message::assistant(format!(
                "step {i}: {}",
                "x".repeat(3_000)
            )));
        }
        turn.set_history_range(0, turn.messages.len());
        turn
    }

    /// Crossing the warning line starts the summary in the background, and
    /// the compaction that follows uses it instead of paying for a
    /// round-trip on the critical path.
    #[tokio::test]
    async fn the_warning_line_starts_a_background_summary() {
        let (mut engine, _rx, _session) = make_test_engine();
        engine.learn_context_window(32_000);
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        engine.set_compaction_backend(Arc::new(CountingSummaryBackend {
            calls: Arc::clone(&calls),
        }));

        let mut turn = warned_turn();
        let tokens = turn.context_tokens(None);
        let thresholds = engine.compact_thresholds();
        assert_eq!(
            thresholds.level(tokens),
            compact::ContextLevel::Warn,
            "the fixture must sit between the lines: {tokens} tokens, {thresholds:?}"
        );

        engine
            .maybe_compact_on_threshold(&mut turn, &SubmitOptions::default(), 1)
            .await;

        // The pre-generation runs on its own task: wait for it rather than
        // assuming a schedule.
        for _ in 0..200 {
            if calls.load(Ordering::SeqCst) > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the warning line did not start a background summary"
        );

        // Now cross the compaction line, and let compaction use the digest.
        for i in 14..24 {
            turn.messages.push(Message::assistant(format!(
                "step {i}: {}",
                "y".repeat(3_000)
            )));
        }
        turn.set_history_range(0, turn.messages.len());
        assert!(matches!(
            engine.compact_thresholds().level(turn.context_tokens(None)),
            compact::ContextLevel::Compact | compact::ContextLevel::Blocked
        ));

        let reason = engine
            .maybe_compact_on_threshold(&mut turn, &SubmitOptions::default(), 1)
            .await;
        assert!(reason.is_none(), "compaction failed: {reason:?}");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "compaction re-requested a summary it already had"
        );
    }

    /// With pre-generation off, the summary is made when compaction needs it
    /// — the behaviour before this existed, kept intact behind the switch.
    #[tokio::test]
    async fn a_disabled_prefetch_summarizes_only_at_compaction() {
        let (mut engine, _rx, _session) = make_test_engine();
        engine.learn_context_window(32_000);
        engine.set_compact_config(CompactConfig {
            background_summary: false,
            ..CompactConfig::default()
        });
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        engine.set_compaction_backend(Arc::new(CountingSummaryBackend {
            calls: Arc::clone(&calls),
        }));

        let mut turn = warned_turn();
        engine
            .maybe_compact_on_threshold(&mut turn, &SubmitOptions::default(), 1)
            .await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a disabled prefetch must not call the summarizer"
        );

        for i in 14..24 {
            turn.messages.push(Message::assistant(format!(
                "step {i}: {}",
                "y".repeat(3_000)
            )));
        }
        turn.set_history_range(0, turn.messages.len());
        let reason = engine
            .maybe_compact_on_threshold(&mut turn, &SubmitOptions::default(), 1)
            .await;
        assert!(reason.is_none(), "compaction failed: {reason:?}");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the synchronous path must still summarize"
        );
    }

    // ─── Cache-TTL compaction ──────────────────────────────────────────────

    /// A cold cache is not by itself a reason to compact a small context:
    /// nothing has gone wrong, and the pass is skipped without a model call.
    #[tokio::test]
    async fn a_warm_cache_is_not_a_reason_to_compact() {
        let (mut engine, _rx, _session) = make_test_engine();
        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        turn.messages = vec![Message::user("hello")];

        // Never sent a request — no TTL to have elapsed.
        assert!(
            engine
                .maybe_compact_on_cache_ttl(&mut turn, &SubmitOptions::default(), 1)
                .await
                .is_none()
        );

        // A request a moment ago leaves the cache warm.
        engine.set_last_request_at(Instant::now());
        assert!(
            engine
                .maybe_compact_on_cache_ttl(&mut turn, &SubmitOptions::default(), 1)
                .await
                .is_none()
        );
    }

    /// Once the token line is crossed the threshold path owns compaction, and
    /// the TTL pass stands down rather than compacting twice for one reason.
    #[tokio::test]
    async fn the_cache_ttl_pass_stands_down_at_the_token_line() {
        let (mut engine, _rx, _session) = make_test_engine();
        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());

        // A context past the compaction line for this model.
        let auto = compact::CompactThresholds::resolve(&engine.model, &engine.compact_config).auto;
        turn.messages = vec![Message::user("x".repeat(auto * 4 + 4_000))];
        assert!(
            PromptCompiler::estimate_message_tokens(&turn.messages) >= auto,
            "the fixture must cross the compaction line"
        );

        // The cache went cold while the session sat idle.
        engine.set_last_request_at(
            Instant::now()
                - std::time::Duration::from_secs(engine.compact_config.cache_ttl_secs + 1),
        );

        let outcome = engine
            .maybe_compact_on_cache_ttl(&mut turn, &SubmitOptions::default(), 1)
            .await;
        assert!(
            matches!(
                outcome,
                Some(CompactOutcome::DeferredToThresholdPath { .. })
            ),
            "{outcome:?}"
        );
    }

    /// A cold cache below the token line compacts, and the pass is charged to
    /// nobody's recovery budget — tidying up must not starve a real recovery.
    #[tokio::test]
    async fn a_cold_cache_below_the_token_line_compacts_without_spending_a_retry() {
        let (mut engine, _rx, _session) = make_test_engine();
        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        turn.messages = vec![Message::user("hello")];
        turn.recovery_count = 0;

        engine.set_last_request_at(
            Instant::now()
                - std::time::Duration::from_secs(engine.compact_config.cache_ttl_secs + 1),
        );

        let outcome = engine
            .maybe_compact_on_cache_ttl(&mut turn, &SubmitOptions::default(), 1)
            .await;

        assert!(outcome.is_some(), "a cold cache should be acted on");
        assert_eq!(
            turn.recovery_count, 0,
            "a maintenance pass must not spend a recovery retry"
        );
    }

    /// The turn loop reaches the TTL pass: a cold cache is compacted before
    /// the request goes out, and the turn still completes.
    #[tokio::test]
    async fn the_turn_loop_compacts_a_cold_cache_before_the_request() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut engine, _rx, session) = make_test_engine();
        engine.set_transcript_path(tmp.path().to_path_buf());
        let transcript = tmp.path().join(format!("{}.jsonl", session.session_id));
        QueryEngine::write_transcript_replacement(&transcript, &[Message::user("earlier history")])
            .unwrap();

        // A cold cache below the line runs the lightest pass. Give it
        // something to shrink: a no-op would (correctly) leave the transcript
        // alone, and this test is about the transcript being rewritten.
        engine.session.messages = read_result_history(7);

        engine.set_last_request_at(
            Instant::now()
                - std::time::Duration::from_secs(engine.compact_config.cache_ttl_secs + 1),
        );

        let reason = engine
            .submit("go".into(), SubmitOptions::default())
            .await
            .unwrap();
        assert!(matches!(reason, TerminalReason::Completed), "{reason:?}");

        // The pass ran: the transcript was rewritten, and a backup of what it
        // held before is on disk.
        let backups: Vec<PathBuf> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.to_string_lossy().contains(".backup."))
            .collect();
        assert_eq!(backups.len(), 1, "the cold cache should have compacted");
    }

    /// The idle timer's rule, without a clock: nothing before a request has
    /// been sent, nothing while the cache is warm, and nothing twice for the
    /// same idle window.
    #[test]
    fn the_cache_ttl_timer_rule_fires_once_per_idle_window() {
        let wake_after = Duration::from_secs(270);
        let now = Instant::now();
        let cold = now - Duration::from_secs(300);

        assert!(
            !cache_ttl_timer_should_request(None, None, wake_after),
            "no request has been sent, so there is no cache to keep warm"
        );
        assert!(
            !cache_ttl_timer_should_request(Some(now), None, wake_after),
            "a request a moment ago leaves the cache warm"
        );
        assert!(
            cache_ttl_timer_should_request(Some(cold), None, wake_after),
            "a request past the TTL has left a cold cache behind it"
        );
        assert!(
            cache_ttl_timer_should_request(
                Some(cold),
                Some(now - Duration::from_secs(400)),
                wake_after
            ),
            "the request came after the last fire, so this is a new idle window"
        );
        assert!(
            !cache_ttl_timer_should_request(
                Some(cold),
                Some(now - Duration::from_secs(200)),
                wake_after
            ),
            "the timer already asked for this idle window"
        );
    }

    /// With no cache TTL configured there is no cache to keep warm, so no
    /// timer is started; with one, the timer starts.
    #[tokio::test]
    async fn no_cache_ttl_means_no_timer() {
        let (mut engine, _rx, _session) = make_test_engine();
        engine.compact_config.cache_ttl_secs = 0;
        assert!(engine.spawn_cache_ttl_timer().is_none());

        engine.compact_config.cache_ttl_secs = 1;
        assert!(
            engine.spawn_cache_ttl_timer().is_some(),
            "a configured TTL should arm the timer"
        );
    }

    /// The session's first turn starts the idle timer, so a session that goes
    /// quiet is still watched.
    #[tokio::test]
    async fn the_first_turn_starts_the_idle_timer() {
        let (mut engine, _rx, _session) = make_test_engine();
        assert!(engine.cache_ttl_timer.is_none());

        engine
            .submit("hello".into(), SubmitOptions::default())
            .await
            .expect("the mock provider answers");

        assert!(
            engine.cache_ttl_timer.is_some(),
            "the session should be watched for idleness after its first turn"
        );
    }

    /// A session that sits idle past the cache TTL asks for compaction on its
    /// own — no next request is needed to reach the turn-boundary check.
    #[tokio::test]
    async fn an_idle_session_requests_compaction_when_its_cache_goes_cold() {
        let (mut engine, mut rx, _session) = make_test_engine();
        // The shortest honest window: a 1s TTL wakes at 90% of it.
        engine.compact_config.cache_ttl_secs = 1;
        engine.set_last_request_at(Instant::now());

        let _timer = engine.spawn_cache_ttl_timer().expect("a TTL is configured");

        let event = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("the idle timer should ask for compaction")
            .expect("the event channel is open");

        assert!(
            matches!(
                event,
                EngineEvent::Compaction(CompactionEvent::Requested { ref trigger })
                    if trigger == "cache_ttl"
            ),
            "got {event:?}"
        );
    }

    /// The outcome of a compaction pass reaches the event stream, so a caller
    /// can see that compaction ran and what it did.
    #[tokio::test]
    async fn a_compaction_outcome_reaches_the_event_stream() {
        let (mut engine, mut rx, _session) = make_test_engine();
        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        turn.messages = vec![Message::user("hello")];

        engine.set_last_request_at(
            Instant::now() - Duration::from_secs(engine.compact_config.cache_ttl_secs + 1),
        );
        let outcome = engine
            .maybe_compact_on_cache_ttl(&mut turn, &SubmitOptions::default(), 1)
            .await
            .expect("a cold cache is acted on");

        let expected = compaction_event_from_outcome(&outcome);
        let mut seen = Vec::new();
        while let Ok(event) = rx.try_recv() {
            seen.push(event);
        }
        assert!(
            seen.iter().any(
                |event| matches!(event, EngineEvent::Compaction(reported) if *reported == expected)
            ),
            "the outcome {expected:?} should be on the stream, got {seen:?}"
        );
    }

    /// The pass standing down is also reported: a caller that sees nothing
    /// cannot tell "not needed" from "not reached".
    #[tokio::test]
    async fn a_compaction_pass_that_stands_down_is_reported() {
        let (mut engine, mut rx, _session) = make_test_engine();
        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());

        let auto = compact::CompactThresholds::resolve(&engine.model, &engine.compact_config).auto;
        turn.messages = vec![Message::user("x".repeat(auto * 4 + 4_000))];
        engine.set_last_request_at(
            Instant::now() - Duration::from_secs(engine.compact_config.cache_ttl_secs + 1),
        );

        let outcome = engine
            .maybe_compact_on_cache_ttl(&mut turn, &SubmitOptions::default(), 1)
            .await;
        assert!(
            matches!(
                outcome,
                Some(CompactOutcome::DeferredToThresholdPath { .. })
            ),
            "{outcome:?}"
        );

        let mut seen = Vec::new();
        while let Ok(event) = rx.try_recv() {
            seen.push(event);
        }
        assert!(
            seen.iter().any(|event| matches!(
                event,
                EngineEvent::Compaction(CompactionEvent::DeferredToThresholdPath { .. })
            )),
            "standing down should be on the stream, got {seen:?}"
        );
    }

    // ─── Wired feature flags ───────────────────────────────────────────────

    fn test_engine_with_api(
        api: Box<dyn ApiClient>,
    ) -> (QueryEngine, mpsc::UnboundedReceiver<EngineEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let checker = PermissionChecker::new(PermissionMode::BypassPermissions);
        let session = SessionState::new(std::path::PathBuf::from("/tmp/test"));
        let mut engine = QueryEngine::new(
            api,
            ModelId::sonnet(),
            session,
            PermissionEngine::new(checker),
            tx,
        );
        engine.set_tool_executor(Box::new(StubOkToolExecutor));
        (engine, rx)
    }

    fn drain_events(rx: &mut mpsc::UnboundedReceiver<EngineEvent>) -> Vec<EngineEvent> {
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    }

    #[test]
    fn team_context_block_reads_the_installed_context() {
        let (mut engine, _rx, _session) = make_test_engine();
        assert!(
            engine.team_context_block().is_none(),
            "no installed context must leave the prompt unchanged"
        );

        let mut context = crate::team_context::TeamContext::new("team_alpha".into());
        context.add_note(
            "Storage".into(),
            "Use Postgres for durable state".into(),
            "alice".into(),
        );
        context.add_work_item(crate::team_context::WorkItem {
            id: "w1".into(),
            description: "Wire the remaining flags".into(),
            worker_id: "bob".into(),
            remaining_steps: 2,
        });
        context.record_activity("carol".into(), "opened a pull request".into());
        engine.set_team_context(std::sync::Arc::new(std::sync::Mutex::new(context)));

        let block = engine.team_context_block().expect("the installed context");
        assert!(block.contains("team_alpha"), "{block}");
        assert!(block.contains("Use Postgres for durable state"), "{block}");
        assert!(block.contains("Wire the remaining flags"), "{block}");
        assert!(block.contains("opened a pull request"), "{block}");
    }

    struct StreamingToolCallApi;

    #[async_trait::async_trait]
    impl ApiClient for StreamingToolCallApi {
        fn clone_box(&self) -> Box<dyn ApiClient> {
            Box::new(Self)
        }

        async fn create_message(
            &self,
            _system: &str,
            _messages: &[serde_json::Value],
            _model: &ModelId,
            _max_tokens: u32,
            _tools: &[crate::tool_executor::ToolInfo],
        ) -> Result<ApiResponse, ApiError> {
            Ok(ApiResponse {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "tc_stream".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "true"}),
                }],
                usage: UsageSummary {
                    input_tokens: 10,
                    output_tokens: 5,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                },
                stop_reason: "tool_use".into(),
                thinking: None,
            })
        }

        async fn create_message_streaming(
            &self,
            _system: &str,
            _messages: &[serde_json::Value],
            _model: &ModelId,
            _max_tokens: u32,
            _tools: &[crate::tool_executor::ToolInfo],
        ) -> std::pin::Pin<Box<dyn futures::Stream<Item = StreamEvent> + Send + 'static>> {
            Box::pin(futures::stream::iter(vec![
                StreamEvent::ToolCallBlockStart {
                    id: "tc_stream".into(),
                    name: "bash".into(),
                },
                StreamEvent::ToolCallBlockEnd {
                    id: "tc_stream".into(),
                    input: serde_json::json!({"command": "true"}),
                },
                StreamEvent::StopReason("tool_use".into()),
                StreamEvent::StreamEnd,
            ]))
        }
    }

    #[tokio::test]
    async fn streaming_tool_execution_flag_gates_early_execution() {
        let (mut engine, _rx) = test_engine_with_api(Box::new(StreamingToolCallApi));
        let options = SubmitOptions::default();

        // Default: off, matching the reference. The call is assembled during
        // the stream but returned for execution after it completes.
        assert!(!engine.streaming_tool_execution());
        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        let deferred = engine
            .send_request_streaming(&mut turn, &options)
            .await
            .unwrap();
        assert!(deferred.early_calls.is_empty());
        assert_eq!(deferred.tool_calls.len(), 1);
        assert_eq!(deferred.tool_calls[0].id, "tc_stream");

        // On: the call executes while the stream is still open and comes
        // back as an early result rather than a deferred call.
        engine.set_streaming_tool_execution(true);
        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        let early = engine
            .send_request_streaming(&mut turn, &options)
            .await
            .unwrap();
        assert_eq!(early.early_calls.len(), 1);
        assert_eq!(early.early_calls[0].id, "tc_stream");
        assert_eq!(early.early_results.len(), 1);
        assert_eq!(early.early_results[0].tool_call_id, "tc_stream");
        assert!(early.tool_calls.is_empty());
    }

    /// A provider that rejects the request with a non-retryable 400 in both
    /// the streaming and non-streaming paths.
    struct NonRetryableApi;

    #[async_trait::async_trait]
    impl ApiClient for NonRetryableApi {
        fn clone_box(&self) -> Box<dyn ApiClient> {
            Box::new(Self)
        }

        async fn create_message(
            &self,
            _system: &str,
            _messages: &[serde_json::Value],
            _model: &ModelId,
            _max_tokens: u32,
            _tools: &[crate::tool_executor::ToolInfo],
        ) -> Result<ApiResponse, crate::api_client::ApiError> {
            Err(crate::api_client::ApiError::Response {
                status: 400,
                message: "bad request".into(),
            })
        }

        async fn create_message_streaming(
            &self,
            _system: &str,
            _messages: &[serde_json::Value],
            _model: &ModelId,
            _max_tokens: u32,
            _tools: &[crate::tool_executor::ToolInfo],
        ) -> std::pin::Pin<Box<dyn futures::Stream<Item = StreamEvent> + Send + 'static>> {
            Box::pin(futures::stream::iter(vec![
                StreamEvent::Error(crate::api_client::ApiError::Response {
                    status: 400,
                    message: "bad request".into(),
                }),
                StreamEvent::StreamEnd,
            ]))
        }
    }

    /// A non-retryable failure that produced nothing usable fails the turn.
    ///
    /// It used to fall through as `end_turn` with empty content, so the caller
    /// started another request — which is why a 400 looked like a hang.
    #[tokio::test]
    async fn a_non_retryable_stream_failure_fails_the_turn() {
        let (mut engine, _rx) = test_engine_with_api(Box::new(NonRetryableApi));
        let options = SubmitOptions::default();
        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        let error = engine
            .send_request_streaming(&mut turn, &options)
            .await
            .expect_err("a 400 with no content must fail the turn");
        assert!(matches!(error, TurnError::Fatal { .. }), "{error:?}");
        assert!(
            error.message().contains("bad request"),
            "the provider message should reach the caller: {}",
            error.message()
        );
    }

    /// A provider whose stream ends cleanly with nothing in it, plus the
    /// non-streaming answer the retry should pick up.
    struct EmptyStreamApi {
        fallback: Option<&'static str>,
        fallback_calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl ApiClient for EmptyStreamApi {
        fn clone_box(&self) -> Box<dyn ApiClient> {
            Box::new(Self {
                fallback: self.fallback,
                fallback_calls: std::sync::Arc::clone(&self.fallback_calls),
            })
        }

        async fn create_message(
            &self,
            _system: &str,
            _messages: &[serde_json::Value],
            _model: &ModelId,
            _max_tokens: u32,
            _tools: &[crate::tool_executor::ToolInfo],
        ) -> Result<ApiResponse, ApiError> {
            self.fallback_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(ApiResponse {
                content: self.fallback.unwrap_or_default().into(),
                tool_calls: Vec::new(),
                usage: UsageSummary::default(),
                stop_reason: "end_turn".into(),
                thinking: None,
            })
        }

        async fn create_message_streaming(
            &self,
            _system: &str,
            _messages: &[serde_json::Value],
            _model: &ModelId,
            _max_tokens: u32,
            _tools: &[crate::tool_executor::ToolInfo],
        ) -> std::pin::Pin<Box<dyn futures::Stream<Item = StreamEvent> + Send + 'static>> {
            Box::pin(futures::stream::iter(vec![
                StreamEvent::StopReason("end_turn".into()),
                StreamEvent::StreamEnd,
            ]))
        }
    }

    fn empty_stream_api(
        fallback: Option<&'static str>,
    ) -> (
        Box<dyn ApiClient>,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        (
            Box::new(EmptyStreamApi {
                fallback,
                fallback_calls: std::sync::Arc::clone(&calls),
            }),
            calls,
        )
    }

    /// A clean end with no text and no tool call is not a finished answer.
    ///
    /// It used to become `end_turn` with empty content, which the loop
    /// reported as success — a headless run could stop early with nothing.
    #[tokio::test]
    async fn an_empty_stream_retries_the_turn_non_streaming() {
        let (api, calls) = empty_stream_api(Some("the answer"));
        let (mut engine, _rx) = test_engine_with_api(api);
        let options = SubmitOptions::default();
        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());

        let response = engine
            .send_request_streaming(&mut turn, &options)
            .await
            .expect("the retry produced an answer");
        assert_eq!(response.content, "the answer");
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the empty stream must be retried exactly once"
        );
    }

    /// If the retry is empty too, the turn fails instead of reporting a
    /// success with nothing in it.
    #[tokio::test]
    async fn an_empty_stream_with_an_empty_retry_fails_the_turn() {
        let (api, calls) = empty_stream_api(None);
        let (mut engine, _rx) = test_engine_with_api(api);
        let options = SubmitOptions::default();
        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());

        let error = engine
            .send_request_streaming(&mut turn, &options)
            .await
            .expect_err("an empty answer must not complete the turn");
        assert!(matches!(error, TurnError::Fatal { .. }), "{error:?}");
        assert!(
            error.message().contains("empty response"),
            "{}",
            error.message()
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// Returns a fixed summary on `create_message`, or an error when
    /// `summary` is `None`, to exercise the fallback path.
    struct SummaryApi {
        summary: Option<&'static str>,
    }

    #[async_trait::async_trait]
    impl ApiClient for SummaryApi {
        fn clone_box(&self) -> Box<dyn ApiClient> {
            Box::new(Self {
                summary: self.summary,
            })
        }

        async fn create_message(
            &self,
            _system: &str,
            _messages: &[serde_json::Value],
            _model: &ModelId,
            _max_tokens: u32,
            _tools: &[crate::tool_executor::ToolInfo],
        ) -> Result<ApiResponse, ApiError> {
            match self.summary {
                Some(summary) => Ok(ApiResponse {
                    content: summary.into(),
                    tool_calls: Vec::new(),
                    usage: UsageSummary::default(),
                    stop_reason: "end_turn".into(),
                    thinking: None,
                }),
                None => Err(ApiError::Http("no lightweight client available".into())),
            }
        }

        async fn create_message_streaming(
            &self,
            _system: &str,
            _messages: &[serde_json::Value],
            _model: &ModelId,
            _max_tokens: u32,
            _tools: &[crate::tool_executor::ToolInfo],
        ) -> std::pin::Pin<Box<dyn futures::Stream<Item = StreamEvent> + Send + 'static>> {
            Box::pin(futures::stream::empty())
        }
    }

    #[tokio::test]
    async fn tool_use_summaries_flag_emits_a_model_written_label() {
        let (mut engine, mut rx) = test_engine_with_api(Box::new(SummaryApi {
            summary: Some("Running a shell command"),
        }));
        engine.set_tool_use_summaries(true);
        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        let call = ToolCall {
            id: "tc_summary".into(),
            name: "bash".into(),
            input: serde_json::json!({"command": "true"}),
        };
        let result = engine
            .execute_early_tool_call(&mut turn, call, &SubmitOptions::default())
            .await
            .unwrap();
        assert!(result.is_some(), "the tool call still executed");

        let events = drain_events(&mut rx);
        let summary = events
            .iter()
            .find_map(|event| match event {
                EngineEvent::ToolUseSummary { summary, .. } => Some(summary.clone()),
                _ => None,
            })
            .expect("a ToolUseSummary event");
        assert_eq!(summary, "Running a shell command");
        let start = events
            .iter()
            .find_map(|event| match event {
                EngineEvent::ToolCallStart { summary, .. } => Some(summary.clone()),
                _ => None,
            })
            .expect("a ToolCallStart event");
        assert_eq!(
            start, summary,
            "the start label must be the model-written summary"
        );
    }

    #[tokio::test]
    async fn tool_use_summaries_flag_off_keeps_the_deterministic_label() {
        let (mut engine, mut rx) = test_engine_with_api(Box::new(SummaryApi {
            summary: Some("this text must never be requested"),
        }));
        assert!(!engine.tool_use_summaries());
        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        let call = ToolCall {
            id: "tc_summary".into(),
            name: "bash".into(),
            input: serde_json::json!({"command": "true"}),
        };
        engine
            .execute_early_tool_call(&mut turn, call, &SubmitOptions::default())
            .await
            .unwrap();

        let events = drain_events(&mut rx);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, EngineEvent::ToolUseSummary { .. })),
            "the summary event is gated by the flag: {events:?}"
        );
        let start = events
            .iter()
            .find_map(|event| match event {
                EngineEvent::ToolCallStart { summary, .. } => Some(summary.clone()),
                _ => None,
            })
            .expect("a ToolCallStart event");
        assert_eq!(
            start,
            ToolCall::summarize_activity("bash", &serde_json::json!({"command": "true"}))
        );
    }

    #[tokio::test]
    async fn tool_use_summary_failure_falls_back_without_failing_the_turn() {
        let (mut engine, mut rx) = test_engine_with_api(Box::new(SummaryApi { summary: None }));
        engine.set_tool_use_summaries(true);
        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        let call = ToolCall {
            id: "tc_summary".into(),
            name: "bash".into(),
            input: serde_json::json!({"command": "true"}),
        };
        let result = engine
            .execute_early_tool_call(&mut turn, call, &SubmitOptions::default())
            .await;
        assert!(
            result.unwrap().is_some(),
            "a failed summary must not fail the tool call"
        );

        let events = drain_events(&mut rx);
        let summary = events
            .iter()
            .find_map(|event| match event {
                EngineEvent::ToolUseSummary { summary, .. } => Some(summary.clone()),
                _ => None,
            })
            .expect("a ToolUseSummary event with the fallback label");
        assert_eq!(
            summary,
            ToolCall::summarize_activity("bash", &serde_json::json!({"command": "true"}))
        );
    }

    /// The engine installs a verifier that re-checks a path after a tool has
    /// opened it; an unexpected path must not inherit the pre-check verdict.
    #[tokio::test]
    async fn the_engine_verifier_rechecks_an_opened_path() {
        use crate::permissions::{PermissionRuleSpec, RuleCompileContext};
        use std::sync::Mutex;

        #[derive(Clone, Default)]
        struct Capture(std::sync::Arc<Mutex<Option<crate::tool_executor::PathVerifier>>>);

        #[async_trait::async_trait]
        impl ToolExecutor for Capture {
            async fn execute(
                &self,
                _tool_call_id: &str,
                _tool_name: &str,
                _input: &serde_json::Value,
            ) -> crate::tool_executor::ToolDispatchOutcome {
                crate::tool_executor::ToolDispatchOutcome {
                    result: ToolResult {
                        tool_call_id: "x".into(),
                        output: String::new(),
                        is_error: false,
                        images: Vec::new(),
                    },
                }
            }

            fn set_path_verifier(&self, verifier: Option<crate::tool_executor::PathVerifier>) {
                *self.0.lock().unwrap() = verifier;
            }
        }

        let (mut engine, _rx, _session) = make_test_engine();
        let checker = engine.permission_checker();
        {
            let mut checker = checker.lock().unwrap();
            checker.set_mode(PermissionMode::Default);
            checker
                .add_specs(
                    &[PermissionRuleSpec::parse(
                        "Read(./secret/**)",
                        crate::permissions::PermissionAction::Deny,
                    )
                    .unwrap()],
                    &RuleCompileContext {
                        cwd: std::path::Path::new("/tmp"),
                        home: None,
                    },
                )
                .unwrap();
        }
        let capture = Capture::default();
        engine.set_tool_executor(Box::new(capture.clone()));
        let verifier = capture
            .0
            .lock()
            .unwrap()
            .clone()
            .expect("verifier installed");

        assert!(verifier("Read", &[std::path::PathBuf::from("/tmp/secret/x")]).is_err());
        assert!(verifier("Read", &[std::path::PathBuf::from("/tmp/public/x")]).is_ok());
    }
}
