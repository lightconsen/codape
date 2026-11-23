//! Cost tracking — model pricing, per-turn/per-session cost accumulation.

use crate::types::{ModelId, UsageSummary};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

/// Per-model pricing configuration.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ModelPricing {
    /// Price per 1M input tokens (USD).
    pub input_per_million: f64,
    /// Price per 1M output tokens (USD).
    pub output_per_million: f64,
    /// Price per 1M cache read tokens (USD).
    pub cache_read_per_million: f64,
    /// Price per 1M cache creation tokens (USD).
    pub cache_creation_per_million: f64,
}

impl ModelPricing {
    /// Calculate cost for a usage summary.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn calculate(&self, usage: &UsageSummary) -> f64 {
        let input_cost = usage.input_tokens as f64 / 1_000_000.0 * self.input_per_million;
        let output_cost = usage.output_tokens as f64 / 1_000_000.0 * self.output_per_million;
        let cache_read_cost =
            usage.cache_read_tokens as f64 / 1_000_000.0 * self.cache_read_per_million;
        let cache_creation_cost =
            usage.cache_creation_tokens as f64 / 1_000_000.0 * self.cache_creation_per_million;
        input_cost + output_cost + cache_read_cost + cache_creation_cost
    }
}

/// Known model pricing.
impl ModelPricing {
    /// Claude Sonnet 4.6 pricing; also correct for Sonnet 4.5 and 4.
    #[must_use]
    pub const fn sonnet() -> Self {
        Self {
            input_per_million: 3.0,
            output_per_million: 15.0,
            cache_read_per_million: 0.3,
            cache_creation_per_million: 3.75,
        }
    }

    /// Claude Fable 5.2 and Mythos 5.2 pricing.
    #[must_use]
    pub const fn fable_5_2() -> Self {
        Self {
            input_per_million: 8.0,
            output_per_million: 40.0,
            cache_read_per_million: 0.2,
            cache_creation_per_million: 10.0,
        }
    }

    /// Claude Fable 5.1 and Mythos 5.1 pricing.
    #[must_use]
    pub const fn fable_5_1() -> Self {
        Self {
            input_per_million: 10.0,
            output_per_million: 50.0,
            cache_read_per_million: 0.25,
            cache_creation_per_million: 12.5,
        }
    }

    /// Claude Fable 5 and Mythos 5 pricing.
    ///
    /// Same headline rates as 5.1, but a cache read costs four times as much.
    #[must_use]
    pub const fn fable_5() -> Self {
        Self {
            input_per_million: 10.0,
            output_per_million: 50.0,
            cache_read_per_million: 1.0,
            cache_creation_per_million: 12.5,
        }
    }

    /// Claude Opus 5.5 pricing.
    #[must_use]
    pub const fn opus_5() -> Self {
        Self {
            input_per_million: 4.0,
            output_per_million: 20.0,
            cache_read_per_million: 0.2,
            cache_creation_per_million: 5.0,
        }
    }

    /// Claude Opus 4.6 pricing; also correct for Opus 4.5, 4.7 and 4.8.
    ///
    /// Those four sit at the same $5 / $25 rates.
    #[must_use]
    pub const fn opus() -> Self {
        Self {
            input_per_million: 5.0,
            output_per_million: 25.0,
            cache_read_per_million: 0.5,
            cache_creation_per_million: 6.25,
        }
    }

    /// Claude Sonnet 5.5 pricing; also correct for Sonnet 5.
    #[must_use]
    pub const fn sonnet_5() -> Self {
        Self {
            input_per_million: 2.0,
            output_per_million: 10.0,
            cache_read_per_million: 0.1,
            cache_creation_per_million: 2.5,
        }
    }

    /// Claude Haiku 5.5 pricing at the ≤100k-prompt tier.
    ///
    /// Haiku 5.5 raises its rates on prompts over 100k tokens. A per-turn
    /// estimate does not know which tier a request landed in, so this uses the
    /// cheaper tier: the figure under-reports rather than inventing a
    /// surcharge that may not apply.
    #[must_use]
    pub const fn haiku_5() -> Self {
        Self {
            input_per_million: 0.1,
            output_per_million: 0.5,
            cache_read_per_million: 0.01,
            cache_creation_per_million: 0.125,
        }
    }

    /// Claude Haiku 4.5 pricing.
    #[must_use]
    pub const fn haiku() -> Self {
        Self {
            input_per_million: 1.0,
            output_per_million: 5.0,
            cache_read_per_million: 0.1,
            cache_creation_per_million: 1.25,
        }
    }
}

/// Built-in pricing for a model Codape knows, if it knows one.
///
/// `None` is the honest answer for anything else: a gateway model name
/// (`deepseek-v4.1-flash`) has no Anthropic price, and the difference between
/// "this costs Sonnet money" and "nobody has told me what this costs" is the
/// difference between a cost estimate and an invented number. Callers that
/// report a dollar figure check this first; [`pricing_for_model`] keeps the
/// lenient behaviour for internal budgeting.
///
/// Matching is by family prefix, so a dated release
/// (`claude-opus-5-5-20270101`) and a provider-prefixed alias
/// (`anthropic/claude-sonnet-5-5`) resolve to their family's rates. The order
/// below is specific-before-general for the same reason the context-window
/// table is: `claude-opus-5-5` must win over `claude-opus-5`.
#[must_use]
pub fn known_model_pricing(model: &str) -> Option<ModelPricing> {
    let name = crate::types::normalized_model_name(model);
    // The first family that matches wins.
    let families: &[(&str, ModelPricing)] = &[
        // Fable and Mythos: 5.2 is cheaper than 5.1/5, and 5's cache read is
        // four times 5.1's, so the two are separate entries.
        ("claude-fable-5-2", ModelPricing::fable_5_2()),
        ("claude-mythos-5-2", ModelPricing::fable_5_2()),
        ("claude-fable-5-1", ModelPricing::fable_5_1()),
        ("claude-mythos-5-1", ModelPricing::fable_5_1()),
        ("claude-fable-5", ModelPricing::fable_5()),
        ("claude-mythos-5", ModelPricing::fable_5()),
        // Opus.
        ("claude-opus-5-5", ModelPricing::opus_5()),
        ("claude-opus-5", ModelPricing::opus()),
        ("claude-opus-4-8", ModelPricing::opus()),
        ("claude-opus-4-7", ModelPricing::opus()),
        ("claude-opus-4-6", ModelPricing::opus()),
        ("claude-opus-4-5", ModelPricing::opus()),
        // Sonnet.
        ("claude-sonnet-5-5", ModelPricing::sonnet_5()),
        ("claude-sonnet-5", ModelPricing::sonnet_5()),
        ("claude-sonnet-4-6", ModelPricing::sonnet()),
        ("claude-sonnet-4-5", ModelPricing::sonnet()),
        ("claude-sonnet-4", ModelPricing::sonnet()),
        // Haiku.
        ("claude-haiku-5-5", ModelPricing::haiku_5()),
        ("claude-haiku-4-5", ModelPricing::haiku()),
    ];
    families
        .iter()
        .find(|(prefix, _)| crate::types::family_matches(&name, prefix))
        .map(|(_, pricing)| *pricing)
}

/// Resolve pricing for a given model, falling back to Sonnet's.
///
/// The fallback is a budgeting heuristic, not a price: use
/// [`known_model_pricing`] when the number will be shown to someone as money.
#[must_use]
pub fn pricing_for_model(model: &ModelId) -> ModelPricing {
    known_model_pricing(model.0.as_str()).unwrap_or_else(ModelPricing::sonnet)
}

/// Load custom pricing from a JSON file.
///
/// The JSON format is a map of model name to [`ModelPricing`]:
/// ```json
/// {
///   "claude-sonnet-4-6": {
///     "input_per_million": 3.0,
///     "output_per_million": 15.0,
///     "cache_read_per_million": 0.3,
///     "cache_creation_per_million": 3.75
///   }
/// }
/// ```
///
/// Returns an empty map if the file does not exist or cannot be parsed.
#[must_use]
pub fn load_pricing_from_path(path: &Path) -> HashMap<String, ModelPricing> {
    if !path.exists() {
        return HashMap::new();
    }
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("failed to read pricing file {}: {}", path.display(), e);
            return HashMap::new();
        }
    };
    match serde_json::from_str(&content) {
        Ok(pricing) => pricing,
        Err(e) => {
            tracing::warn!("failed to parse pricing file {}: {}", path.display(), e);
            HashMap::new()
        }
    }
}

/// Load pricing from the config home (`pricing.json`).
///
/// `CLAUDE_CONFIG_DIR` when set, otherwise `$HOME/.codape`, matching
/// [`crate::types::context_window_client_data_path`] and every other config
/// path in the tree. Honouring the override matters beyond tidiness: a test or
/// a second profile that points the config home somewhere else would otherwise
/// still read the real user's prices.
#[must_use]
pub fn user_pricing_path() -> std::path::PathBuf {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return std::path::PathBuf::from(dir).join("pricing.json");
    }
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        return std::path::PathBuf::from(home)
            .join(".codape")
            .join("pricing.json");
    }
    std::path::PathBuf::from(".codape").join("pricing.json")
}

/// Load pricing from the config home.
#[must_use]
pub fn load_user_pricing() -> HashMap<String, ModelPricing> {
    load_pricing_from_path(&user_pricing_path())
}

/// Tracks costs per turn, session, and agent.
#[derive(Debug, Clone)]
pub struct CostTracker {
    /// Pricing overrides for specific models.
    pricing: HashMap<String, ModelPricing>,
    /// Cost accumulated in the current turn.
    turn_cost: f64,
    /// Total cost for the entire session.
    session_cost: f64,
    /// Tokens billed for the entire session.
    ///
    /// Accumulated at [`record_usage`](Self::record_usage), not summed from
    /// [`TurnCost`] afterwards: a turn that re-sends its request (an
    /// elicitation answer, a compaction recovery) bills both requests but
    /// records only the last one in the turn history, so a sum over
    /// `turn_history` silently undercounts a long run.
    session_usage: UsageSummary,
    /// Some usage was recorded for a model whose price nobody knows.
    ///
    /// The cost of that usage is in `session_cost` at Sonnet's rate, because
    /// that is what budgeting needs; a dollar figure shown to a person must
    /// not repeat the guess, so [`known_total_cost`](Self::known_total_cost)
    /// reports `None` once this is set.
    unpriced_usage: bool,
    /// Provider time accumulated in the current turn, in milliseconds.
    turn_model_ms: u64,
    /// Tool time accumulated in the current turn, in milliseconds.
    turn_tool_ms: u64,
    /// Whether the turn those two counters belong to has already been closed.
    ///
    /// [`end_turn`](Self::end_turn) copies them into `turn_history` but does
    /// not zero them, because the turn may still be reported in other ways
    /// before the next one starts. That leaves them double-representable: a
    /// caller adding [`current_turn_timing`](Self::current_turn_timing) to
    /// [`timing_since`](Self::timing_since) would count a finished turn twice.
    /// This flag is what makes the in-progress half mean what it says.
    turn_closed: bool,
    /// Costs by agent/agent_id.
    agent_costs: HashMap<String, f64>,
    /// Costs by task/task_id.
    task_costs: HashMap<String, f64>,
    /// Cost history per turn (for reporting).
    turn_history: Vec<TurnCost>,
    /// Optional team context to broadcast costs to.
    team_context: Option<Arc<std::sync::Mutex<crate::team_context::TeamContext>>>,
}

/// Cost information for a single turn.
#[derive(Debug, Clone)]
pub struct TurnCost {
    /// Turn number (1-indexed).
    pub turn_number: u32,
    /// Cost in USD for this turn.
    pub cost_usd: f64,
    /// Token usage for this turn.
    pub usage: UsageSummary,
    /// Model used for this turn.
    pub model: String,
    /// Time spent waiting for the provider, across every request the turn
    /// made, in milliseconds.
    ///
    /// Measured around each request rather than around the turn: a turn that
    /// re-sends (an elicitation answer, a compaction recovery, a stream retry)
    /// spent that time too, and leaving it out would attribute it to the tools.
    /// Where a tool executes inside the stream, its time is subtracted before
    /// the call is recorded, so the same seconds are not in both figures.
    pub model_ms: u64,
    /// Time spent executing tool calls, in milliseconds.
    ///
    /// With early execution on, a tool runs while the provider is still
    /// delivering the rest of the answer, so its wall clock is inside the
    /// stream's. The engine takes that overlap out of `model_ms` rather than
    /// letting one call's time appear in both figures.
    ///
    /// Neither figure is the turn: context building, hooks, compaction and the
    /// gap between turns belong to neither, so `model_ms + tool_ms` is at most
    /// the turn's wall clock and usually less.
    pub tool_ms: u64,
}

/// A [`CostTracker`] shared with the engine that owns it.
///
/// The engine hands itself to a task to run a turn, and the REPL cannot ask
/// that task anything — including how much the turn has spent so far. A plain
/// field put the numbers out of reach of the one frame that needs them most:
/// the `result` a SIGTERM writes for a turn that never finished. Sharing the
/// tracker keeps them reachable, in the same way
/// [`current_turn`](crate::engine::QueryEngine) lets a caller read the turn
/// counter after the hand-off.
///
/// Every method takes the lock for the duration of one call and returns a
/// value, never a reference into the tracker, so a holder cannot keep the lock
/// alive across an `await` by accident.
#[derive(Debug, Clone)]
pub struct SharedCostTracker(std::sync::Arc<std::sync::Mutex<CostTracker>>);

impl SharedCostTracker {
    /// Wrap a tracker for sharing.
    #[must_use]
    pub fn new(tracker: CostTracker) -> Self {
        Self(std::sync::Arc::new(std::sync::Mutex::new(tracker)))
    }

    /// Run `f` against the tracker.
    ///
    /// A poisoned lock is recovered rather than propagated: the numbers a
    /// panic left behind are still the best estimate available, and a caller
    /// reporting why a run ended must not fail because of it.
    fn with<T>(&self, f: impl FnOnce(&mut CostTracker) -> T) -> T {
        let mut guard = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&mut guard)
    }

    /// End the current turn and record it in history.
    pub fn end_turn(
        &self,
        turn_number: u32,
        usage: &crate::types::UsageSummary,
        model: &crate::types::ModelId,
    ) {
        self.with(|tracker| tracker.end_turn(turn_number, usage, model));
    }

    /// Start a new turn, resetting the turn-level cost and timings.
    pub fn start_turn(&self) {
        self.with(CostTracker::start_turn);
    }

    /// Add one request's usage to the session and turn totals.
    pub fn record_usage(
        &self,
        usage: &crate::types::UsageSummary,
        model: &crate::types::ModelId,
    ) -> f64 {
        self.with(|tracker| tracker.record_usage(usage, model))
    }

    /// Add one request's provider time to the current turn.
    pub fn record_model_time(&self, elapsed: std::time::Duration) {
        self.with(|tracker| tracker.record_model_time(elapsed));
    }

    /// Add one batch of tool execution's time to the current turn.
    pub fn record_tool_time(&self, elapsed: std::time::Duration) {
        self.with(|tracker| tracker.record_tool_time(elapsed));
    }

    /// Provider and tool time for `from_turn` onwards, in milliseconds.
    ///
    /// Includes the turn in progress *if there is one*. A turn that already
    /// reached [`end_turn`](Self::end_turn) is in the history and is not added
    /// again — which is what makes the completed-turn closing frame report the
    /// same numbers the finished turn does.
    #[must_use]
    pub fn timing(&self, from_turn: u32) -> (u64, u64) {
        self.with(|tracker| {
            let (completed_model, completed_tool) = tracker.timing_since(from_turn);
            let (turn_model, turn_tool) = tracker.in_progress_timing();
            (
                completed_model.saturating_add(turn_model),
                completed_tool.saturating_add(turn_tool),
            )
        })
    }

    /// Provider and tool time for the turn in progress, in milliseconds.
    ///
    /// Unlike [`timing`](Self::timing) this does not add the completed turns'
    /// history, so it is what a caller wants at a point that has already
    /// recorded the turn. A turn that has closed reports zero here.
    #[must_use]
    pub fn current_turn_timing(&self) -> (u64, u64) {
        self.with(|tracker| tracker.current_turn_timing())
    }

    /// Tokens billed for the session, summed as they were recorded.
    #[must_use]
    pub fn session_usage(&self) -> crate::types::UsageSummary {
        self.with(|tracker| tracker.session_usage())
    }

    /// The session's total cost when every model it used has a known price.
    #[must_use]
    pub fn known_total_cost(&self) -> Option<f64> {
        self.with(|tracker| tracker.known_total_cost())
    }

    /// Format a cost summary for display.
    #[must_use]
    pub fn format_summary(&self) -> String {
        self.with(|tracker| tracker.format_summary())
    }
}

impl CostTracker {
    /// Create a new cost tracker.
    ///
    /// Attempts to load custom pricing from `~/.codape/pricing.json`.
    /// Falls back to built-in defaults if the file is missing or invalid.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pricing: load_user_pricing(),
            turn_cost: 0.0,
            session_cost: 0.0,
            session_usage: UsageSummary::default(),
            unpriced_usage: false,
            turn_model_ms: 0,
            turn_tool_ms: 0,
            turn_closed: false,
            agent_costs: HashMap::new(),
            task_costs: HashMap::new(),
            turn_history: Vec::new(),
            team_context: None,
        }
    }

    /// Reload pricing from `~/.codape/pricing.json` at runtime.
    pub fn reload_pricing(&mut self) {
        self.pricing = load_user_pricing();
    }

    /// Set the team context for broadcasting turn costs.
    pub fn set_team_context(
        &mut self,
        context: Arc<std::sync::Mutex<crate::team_context::TeamContext>>,
    ) {
        self.team_context = Some(context);
    }

    /// Register a custom pricing for a model name.
    pub fn register_pricing(&mut self, model_name: String, pricing: ModelPricing) {
        self.pricing.insert(model_name, pricing);
    }

    /// Record usage and calculate cost for the current turn.
    pub fn record_usage(&mut self, usage: &UsageSummary, model: &ModelId) -> f64 {
        let known = self
            .pricing
            .get(&model.0)
            .copied()
            .or_else(|| known_model_pricing(model.0.as_str()));
        if known.is_none() {
            self.unpriced_usage = true;
        }
        let pricing = known.unwrap_or_else(ModelPricing::sonnet);

        let cost = pricing.calculate(usage);
        self.turn_cost += cost;
        self.session_cost += cost;
        self.session_usage = self.session_usage.saturating_add(usage);
        cost
    }

    /// End the current turn and record it in history.
    pub fn end_turn(&mut self, turn_number: u32, usage: &UsageSummary, model: &ModelId) {
        let cost = if self.turn_cost > 0.0 {
            self.turn_cost
        } else {
            pricing_for_model(model).calculate(usage)
        };

        self.turn_history.push(TurnCost {
            turn_number,
            cost_usd: cost,
            usage: *usage,
            model: model.0.clone(),
            model_ms: self.turn_model_ms,
            tool_ms: self.turn_tool_ms,
        });
        // The numbers above are now in history. Leaving them also readable as
        // "the turn in progress" would put the same turn in both books.
        self.turn_closed = true;

        // Broadcast to team context if present
        if let Some(ref team_ctx) = self.team_context
            && let Ok(mut ctx) = team_ctx.lock()
        {
            ctx.record_activity(
                "cost_tracker".into(),
                format!("Turn {turn_number} cost: ${cost:.4} ({})", model.0),
            );
        }
    }

    /// Start a new turn (reset turn-level cost and timings).
    ///
    /// The session totals survive: a caller reading them after a turn was cut
    /// short still gets what the session spent up to that point. Only the
    /// *turn* counters, which `end_turn` moves into `turn_history` and
    /// `timing_since` sums, go back to zero. That is also why an interrupted
    /// turn's numbers do not appear in `timing_since`: the turn never reached
    /// `end_turn`, so there is no history entry to sum.
    pub const fn start_turn(&mut self) {
        self.turn_cost = 0.0;
        self.turn_model_ms = 0;
        self.turn_tool_ms = 0;
        self.turn_closed = false;
    }

    /// Add one request's provider time to the current turn.
    ///
    /// Called once per request rather than once per turn, so a re-send, a
    /// compaction retry and a stream retry all land in the turn that paid for
    /// them.
    pub fn record_model_time(&mut self, elapsed: std::time::Duration) {
        self.turn_model_ms = self
            .turn_model_ms
            .saturating_add(u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX));
    }

    /// Add one batch of tool execution's time to the current turn.
    pub fn record_tool_time(&mut self, elapsed: std::time::Duration) {
        self.turn_tool_ms = self
            .turn_tool_ms
            .saturating_add(u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX));
    }

    /// Provider time and tool time for the turn in progress, in milliseconds.
    #[must_use]
    pub const fn current_turn_timing(&self) -> (u64, u64) {
        (self.turn_model_ms, self.turn_tool_ms)
    }

    /// The same pair, but zero once the turn has been closed.
    ///
    /// This is the in-progress half of [`SharedCostTracker::timing`]. It is a
    /// separate method because `current_turn_timing` has a second reader — the
    /// `turn timing` debug line, which closes the turn and then logs its
    /// numbers — and that reader wants them after the close.
    #[must_use]
    const fn in_progress_timing(&self) -> (u64, u64) {
        if self.turn_closed {
            (0, 0)
        } else {
            (self.turn_model_ms, self.turn_tool_ms)
        }
    }

    /// Add cost to an agent.
    pub fn record_agent_cost(&mut self, agent_id: &str, cost: f64) {
        *self.agent_costs.entry(agent_id.into()).or_default() += cost;
    }

    /// Add cost to a task.
    pub fn record_task_cost(&mut self, task_id: &str, cost: f64) {
        *self.task_costs.entry(task_id.into()).or_default() += cost;
    }

    /// Get the total session cost.
    ///
    /// This is the budgeting number: it includes usage priced at Sonnet's rate
    /// because no real price was known. Use [`known_total_cost`](Self::known_total_cost)
    /// for a figure that will be reported as money.
    #[must_use]
    pub const fn get_total_cost(&self) -> f64 {
        self.session_cost
    }

    /// The session cost, or `None` once any of it was priced by guesswork.
    ///
    /// A dollar figure derived from `pricing_for_model`'s Sonnet fallback is
    /// not an estimate of what a gateway model costs — it is Sonnet's price
    /// wearing another model's name. Reporting it is worse than reporting
    /// nothing, because nothing prompts the reader to look for the real
    /// number. A pricing entry in the config home's `pricing.json` clears this.
    ///
    /// A session that has billed nothing yet reports `$0.00`, not `None`.
    /// Zero is a known number: the session really has spent nothing so far,
    /// and the statusline before the first turn would otherwise read `$n/a`
    /// on a perfectly priced model.
    #[must_use]
    pub const fn known_total_cost(&self) -> Option<f64> {
        if self.unpriced_usage {
            None
        } else {
            Some(self.session_cost)
        }
    }

    /// Tokens billed for the session, summed as they were recorded.
    #[must_use]
    pub const fn session_usage(&self) -> UsageSummary {
        self.session_usage
    }

    /// Get the current turn cost.
    #[must_use]
    pub const fn get_turn_cost(&self) -> f64 {
        self.turn_cost
    }

    /// Get the cost for a specific agent.
    #[must_use]
    pub fn get_agent_cost(&self, agent_id: &str) -> f64 {
        self.agent_costs.get(agent_id).copied().unwrap_or(0.0)
    }

    /// Get the cost for a specific task.
    #[must_use]
    pub fn get_task_cost(&self, task_id: &str) -> f64 {
        self.task_costs.get(task_id).copied().unwrap_or(0.0)
    }

    /// Get all turn costs.
    #[must_use]
    pub fn get_turn_history(&self) -> &[TurnCost] {
        &self.turn_history
    }

    /// Provider time and tool time summed over every turn after `from_turn`.
    ///
    /// `from_turn` is exclusive and 1-indexed, so a caller that captured the
    /// turn number before a submit gets the submits' own turns. Summing the
    /// history rather than keeping a second running total keeps one number
    /// with one source.
    #[must_use]
    pub fn timing_since(&self, from_turn: u32) -> (u64, u64) {
        self.turn_history
            .iter()
            .filter(|turn| turn.turn_number > from_turn)
            .fold((0, 0), |(model, tool), turn| {
                (
                    model.saturating_add(turn.model_ms),
                    tool.saturating_add(turn.tool_ms),
                )
            })
    }

    /// Get the number of turns so far.
    #[must_use]
    pub const fn turn_count(&self) -> usize {
        self.turn_history.len()
    }

    /// Format a cost summary for display (e.g., for /cost command).
    #[must_use]
    pub fn format_summary(&self) -> String {
        let usage = self.session_usage;
        let (model_ms, tool_ms) = self.timing_since(0);
        format!(
            "Session cost: {} ({} turns, {} tokens)\nTime: {} waiting on the provider, {} in tools",
            self.known_total_cost()
                .map_or_else(|| "unpriced".to_string(), |cost| format!("${cost:.2}")),
            self.turn_history.len(),
            usage.prompt_tokens().saturating_add(usage.output_tokens),
            format_millis(model_ms),
            format_millis(tool_ms),
        )
    }
}

/// A millisecond count as a human-readable duration.
///
/// Integer arithmetic rather than a cast to `f64` seconds: the figures are
/// milliseconds and a tenth of a second is not worth a precision-loss lint.
fn format_millis(ms: u64) -> String {
    let secs = ms / 1000;
    let tenths = (ms % 1000) / 100;
    if secs >= 60 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{secs}.{tenths}s")
    }
}

impl Default for CostTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_pricing_calculation_correct() {
        let pricing = ModelPricing::sonnet();
        let usage = UsageSummary {
            input_tokens: 1000,
            output_tokens: 500,
            cache_read_tokens: 200,
            cache_creation_tokens: 100,
        };
        let cost = pricing.calculate(&usage);
        // input: 1000/1M * 3.0 = 0.003
        // output: 500/1M * 15.0 = 0.0075
        // cache_read: 200/1M * 0.3 = 0.00006
        // cache_creation: 100/1M * 3.75 = 0.000375
        let expected = 0.003 + 0.0075 + 0.000_06 + 0.000_375;
        assert!((cost - expected).abs() < 1e-9);
    }

    #[test]
    fn different_models_have_different_pricing() {
        let usage = UsageSummary {
            input_tokens: 1000,
            output_tokens: 1000,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        };
        let sonnet_cost = ModelPricing::sonnet().calculate(&usage);
        let opus_cost = ModelPricing::opus().calculate(&usage);
        let haiku_cost = ModelPricing::haiku().calculate(&usage);

        assert!(opus_cost > sonnet_cost);
        assert!(sonnet_cost > haiku_cost);
    }

    #[test]
    fn pricing_for_model_resolves_correctly() {
        // Current-generation rates.
        assert_eq!(pricing_for_model(&ModelId::sonnet()).input_per_million, 2.0);
        assert_eq!(pricing_for_model(&ModelId::opus()).input_per_million, 4.0);
        assert_eq!(pricing_for_model(&ModelId::haiku()).input_per_million, 0.1);

        // The previous generation keeps its own rates rather than inheriting
        // the newer (cheaper) numbers.
        assert_eq!(
            pricing_for_model(&ModelId("claude-sonnet-4-6".into())).input_per_million,
            3.0
        );
        assert_eq!(
            pricing_for_model(&ModelId("claude-opus-4-6".into())).input_per_million,
            5.0
        );
        assert_eq!(
            pricing_for_model(&ModelId("claude-haiku-4-5-20251001".into())).input_per_million,
            1.0
        );
    }

    /// A dated release and a provider-prefixed alias are the same model.
    #[test]
    fn pricing_matches_by_family_not_exact_string() {
        for name in [
            "claude-opus-5-5-20270101",
            "anthropic/claude-opus-5-5",
            "CLAUDE-OPUS-5-5",
        ] {
            assert_eq!(
                known_model_pricing(name).map(|p| p.input_per_million),
                Some(4.0),
                "{name}"
            );
        }
        // 5.5 must win over the bare `claude-opus-5` entry.
        assert_eq!(
            known_model_pricing("claude-opus-5").map(|p| p.input_per_million),
            Some(5.0)
        );
        assert!(known_model_pricing("claude-opus-4-50").is_none());
    }

    #[test]
    fn pricing_for_model_defaults_to_sonnet() {
        let unknown = ModelId("unknown-model".into());
        let p = pricing_for_model(&unknown);
        assert_eq!(p.input_per_million, 3.0);
    }

    /// The lenient fallback and the honest answer are different questions.
    ///
    /// `pricing_for_model` has to return *something* so budgeting can run; a
    /// dollar figure built on that answer is Sonnet's price wearing another
    /// model's name, which is what `known_model_pricing` exists to prevent.
    #[test]
    fn known_model_pricing_refuses_to_name_a_price_it_does_not_know() {
        assert!(known_model_pricing("claude-sonnet-4-6").is_some());
        assert!(known_model_pricing("claude-opus-4-6").is_some());
        assert!(known_model_pricing("claude-haiku-4-5-20251001").is_some());
        assert!(
            known_model_pricing("deepseek-v4.1-flash").is_none(),
            "a gateway model has no Anthropic price; none may be invented"
        );
    }

    /// A fresh session has spent nothing, and that is a number.
    ///
    /// "No price is known" and "no tokens have been billed" are different
    /// facts, and collapsing them made the statusline read `$n/a` on a known
    /// model before its first turn.
    #[test]
    fn a_session_that_has_billed_nothing_reports_zero_not_unknown() {
        let tracker = CostTracker::new();
        assert_eq!(tracker.known_total_cost(), Some(0.0));
    }

    #[test]
    fn a_session_that_touched_an_unpriced_model_reports_no_cost() {
        let mut tracker = CostTracker::new();
        let usage = UsageSummary {
            input_tokens: 1000,
            output_tokens: 500,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        };

        tracker.record_usage(&usage, &ModelId::sonnet());
        let priced = tracker.known_total_cost();
        assert!(
            priced.is_some_and(|cost| cost > 0.0),
            "a priced model still has a total: {priced:?}"
        );

        // One request through an unpriced model is enough: the total is now a
        // mix of a real price and a guess, and no arithmetic makes that a cost.
        tracker.record_usage(&usage, &ModelId("deepseek-v4.1-flash".into()));
        assert_eq!(
            tracker.known_total_cost(),
            None,
            "the total included guessed pricing and was still reported as money"
        );
        // Budgeting still gets a number, which is what the fallback is for.
        assert!(tracker.get_total_cost() > 0.0);
    }

    /// Session tokens are accumulated as billed, not summed from the history.
    ///
    /// A turn that re-sends its request (an elicitation answer, a compaction
    /// recovery) bills both requests but records only the last in
    /// `turn_history`, so the two counts diverge exactly where a long run
    /// needs them to agree.
    #[test]
    fn session_usage_counts_every_billed_request() {
        let mut tracker = CostTracker::new();
        let first = UsageSummary {
            input_tokens: 100,
            output_tokens: 10,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        };
        let resend = UsageSummary {
            input_tokens: 200,
            output_tokens: 20,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        };

        tracker.start_turn();
        tracker.record_usage(&first, &ModelId::sonnet());
        tracker.record_usage(&resend, &ModelId::sonnet());
        tracker.end_turn(1, &resend, &ModelId::sonnet());

        let session = tracker.session_usage();
        assert_eq!(
            session.input_tokens, 300,
            "the re-sent request was not counted"
        );
        assert_eq!(session.output_tokens, 30);
        assert_eq!(
            tracker.get_turn_history()[0].usage.input_tokens,
            200,
            "the turn history records the turn's last request, unchanged"
        );
    }

    /// A turn records the provider time and tool time it spent, and a re-send
    /// adds to the same turn rather than starting a second one.
    #[test]
    fn a_turn_accumulates_provider_and_tool_time() {
        let mut tracker = CostTracker::new();
        let usage = UsageSummary::default();

        tracker.start_turn();
        tracker.record_model_time(std::time::Duration::from_millis(120));
        tracker.record_tool_time(std::time::Duration::from_millis(80));
        // An elicitation re-send within the same turn.
        tracker.record_model_time(std::time::Duration::from_millis(30));
        assert_eq!(tracker.current_turn_timing(), (150, 80));

        tracker.end_turn(1, &usage, &ModelId::sonnet());
        let turn = &tracker.get_turn_history()[0];
        assert_eq!(turn.model_ms, 150, "the re-send was not attributed");
        assert_eq!(turn.tool_ms, 80);
    }

    /// A new turn starts from zero rather than carrying the last one's time.
    #[test]
    fn starting_a_turn_clears_the_previous_turn_timing() {
        let mut tracker = CostTracker::new();
        tracker.start_turn();
        tracker.record_model_time(std::time::Duration::from_millis(500));
        tracker.record_tool_time(std::time::Duration::from_millis(400));

        tracker.start_turn();
        assert_eq!(tracker.current_turn_timing(), (0, 0));
    }

    /// `timing_since` excludes the turn the caller captured.
    ///
    /// The submit boundary is "turns after the one that was already running",
    /// so an off-by-one here would report the previous submit's time as this
    /// one's — every single time, and never by a little.
    #[test]
    fn timing_since_reports_only_the_turns_after_the_captured_one() {
        let mut tracker = CostTracker::new();
        let usage = UsageSummary::default();

        for (turn, model_ms, tool_ms) in [(1u32, 100u64, 10u64), (2, 200, 20), (3, 300, 30)] {
            tracker.start_turn();
            tracker.record_model_time(std::time::Duration::from_millis(model_ms));
            tracker.record_tool_time(std::time::Duration::from_millis(tool_ms));
            tracker.end_turn(turn, &usage, &ModelId::sonnet());
        }

        assert_eq!(tracker.timing_since(1), (500, 50));
        assert_eq!(tracker.timing_since(3), (0, 0));
        assert_eq!(tracker.timing_since(0), (600, 60));
    }

    /// A finished turn is in the history and must not be counted again.
    ///
    /// `timing` sums `timing_since` (closed turns) and the in-progress
    /// counters. `end_turn` does not zero those counters, so without the
    /// closed flag every closing frame would report its last turn twice: a
    /// one-turn run that spent 100 ms at the provider would say 200 ms, and
    /// nothing about the frame would look wrong.
    #[test]
    fn a_closed_turn_is_not_counted_as_in_progress() {
        let tracker = SharedCostTracker::new(CostTracker::new());
        tracker.start_turn();
        tracker.record_model_time(std::time::Duration::from_millis(100));
        tracker.record_tool_time(std::time::Duration::from_millis(40));
        tracker.end_turn(1, &UsageSummary::default(), &ModelId::sonnet());

        assert_eq!(
            tracker.timing(0),
            (100, 40),
            "the closed turn was counted in both books"
        );
        // The in-progress pair is still there for the debug line, which reads
        // it after the close on purpose.
        assert_eq!(tracker.current_turn_timing(), (100, 40));

        // The next turn starts clean, and its progress is what `timing` adds.
        tracker.start_turn();
        tracker.record_model_time(std::time::Duration::from_millis(7));
        assert_eq!(tracker.timing(0), (107, 40));
    }

    /// A turn that never reached `end_turn` is only in the in-progress book.
    ///
    /// This is the SIGTERM case: no history entry exists, so adding
    /// `current_turn_timing` is the only way the frame can report anything.
    #[test]
    fn a_turn_that_never_closed_reports_its_progress() {
        let tracker = SharedCostTracker::new(CostTracker::new());
        tracker.start_turn();
        tracker.record_model_time(std::time::Duration::from_millis(250));
        tracker.record_tool_time(std::time::Duration::from_millis(90));

        assert_eq!(
            tracker.timing(0),
            (250, 90),
            "an interrupted turn has no history, so only the progress counted"
        );
    }

    #[test]
    fn usage_summaries_add_saturating() {
        let a = UsageSummary {
            input_tokens: usize::MAX,
            output_tokens: 1,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        };
        let b = UsageSummary {
            input_tokens: 5,
            output_tokens: 2,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        };
        let sum = a.saturating_add(&b);
        assert_eq!(sum.input_tokens, usize::MAX, "addition wrapped or panicked");
        assert_eq!(sum.output_tokens, 3);
    }

    // A `mul_add` rewrite of the tolerance check below would be harder to
    // read for no measurable gain in a test.
    #[allow(clippy::suboptimal_flops)]
    #[test]
    fn cost_tracker_accumulates_per_turn() {
        let mut tracker = CostTracker::new();
        let usage = UsageSummary {
            input_tokens: 1000,
            output_tokens: 500,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        };

        tracker.start_turn();
        tracker.record_usage(&usage, &ModelId::sonnet());
        let turn1_cost = tracker.get_turn_cost();
        tracker.end_turn(1, &usage, &ModelId::sonnet());

        tracker.start_turn();
        tracker.record_usage(&usage, &ModelId::sonnet());
        let turn2_cost = tracker.get_turn_cost();

        assert!((turn1_cost - turn2_cost).abs() < 1e-9);
        assert!((tracker.get_total_cost() - turn1_cost * 2.0).abs() < 1e-9);
    }

    #[test]
    fn cost_tracker_session_cost_accumulates() {
        let mut tracker = CostTracker::new();
        let usage = UsageSummary {
            input_tokens: 1000,
            output_tokens: 500,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        };

        tracker.start_turn();
        tracker.record_usage(&usage, &ModelId::sonnet());
        tracker.end_turn(1, &usage, &ModelId::sonnet());

        tracker.start_turn();
        tracker.record_usage(&usage, &ModelId::sonnet());
        tracker.end_turn(2, &usage, &ModelId::sonnet());

        assert!(tracker.get_total_cost() > 0.0);
        assert_eq!(tracker.turn_count(), 2);
    }

    #[test]
    fn cost_tracker_agent_costs() {
        let mut tracker = CostTracker::new();
        tracker.record_agent_cost("agent_1", 0.50);
        tracker.record_agent_cost("agent_1", 0.25);
        tracker.record_agent_cost("agent_2", 1.00);

        assert!((tracker.get_agent_cost("agent_1") - 0.75).abs() < 1e-9);
        assert!((tracker.get_agent_cost("agent_2") - 1.00).abs() < 1e-9);
        assert!(tracker.get_agent_cost("agent_3") < 1e-9);
    }

    #[test]
    fn cost_tracker_task_costs() {
        let mut tracker = CostTracker::new();
        tracker.record_task_cost("task_a", 0.10);
        tracker.record_task_cost("task_a", 0.20);

        assert!((tracker.get_task_cost("task_a") - 0.30).abs() < 1e-9);
    }

    #[test]
    fn cost_tracker_format_summary() {
        let mut tracker = CostTracker::new();
        let usage = UsageSummary {
            input_tokens: 1000,
            output_tokens: 500,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        };

        tracker.start_turn();
        tracker.record_usage(&usage, &ModelId::sonnet());
        tracker.end_turn(1, &usage, &ModelId::sonnet());

        let summary = tracker.format_summary();
        assert!(summary.contains("Session cost: $"));
        assert!(summary.contains("1 turns"));
    }

    #[test]
    fn cost_tracker_turn_history() {
        let mut tracker = CostTracker::new();
        let usage = UsageSummary {
            input_tokens: 1000,
            output_tokens: 500,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        };

        for i in 1..=3 {
            tracker.start_turn();
            tracker.record_usage(&usage, &ModelId::sonnet());
            tracker.end_turn(i, &usage, &ModelId::sonnet());
        }

        let history = tracker.get_turn_history();
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].turn_number, 1);
        assert_eq!(history[2].turn_number, 3);
    }

    #[test]
    fn custom_pricing_overrides_default() {
        let mut tracker = CostTracker::new();
        tracker.register_pricing(
            "claude-sonnet-5-5".into(),
            ModelPricing {
                input_per_million: 1.0,
                output_per_million: 1.0,
                cache_read_per_million: 0.0,
                cache_creation_per_million: 0.0,
            },
        );

        let usage = UsageSummary {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        };

        tracker.start_turn();
        let cost = tracker.record_usage(&usage, &ModelId::sonnet());
        assert!((cost - 2.0).abs() < 1e-9); // 1.0 + 1.0 with custom pricing
    }

    #[test]
    fn load_pricing_from_valid_json() {
        let json = r#"{
            "custom-model": {
                "input_per_million": 1.0,
                "output_per_million": 2.0,
                "cache_read_per_million": 0.1,
                "cache_creation_per_million": 0.5
            }
        }"#;
        let tmpfile = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmpfile.path(), json).unwrap();
        let pricing = load_pricing_from_path(tmpfile.path());
        assert!(pricing.contains_key("custom-model"));
        let model = pricing.get("custom-model").unwrap();
        assert!((model.input_per_million - 1.0).abs() < 1e-9);
        assert!((model.output_per_million - 2.0).abs() < 1e-9);
    }

    #[test]
    fn load_pricing_missing_file_returns_empty() {
        let pricing = load_pricing_from_path(std::path::Path::new("/nonexistent/pricing.json"));
        assert!(pricing.is_empty());
    }

    #[test]
    fn load_pricing_invalid_json_returns_empty() {
        let tmpfile = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmpfile.path(), "not json").unwrap();
        let pricing = load_pricing_from_path(tmpfile.path());
        assert!(pricing.is_empty());
    }
}
