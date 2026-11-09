//! The per-request output ceiling a session asks the provider for.
//!
//! `QueryEngine::DEFAULT_MAX_TOKENS` is a starting point, not a policy: a
//! reasoning model on a hard step can spend the whole budget thinking and emit
//! nothing, and a headless run then closes with `error_max_output_tokens`
//! having done no work. Only an agent definition could raise it, which left a
//! benchmark adapter — or any script driving the binary — with no way to ask
//! for more room.
//!
//! This module gives that a name: `CODAPE_MAX_TOKENS` raises or lowers the
//! session ceiling, and a raised value is clamped to half the model's context
//! window.
//!
//! # Why the default stays where it is
//!
//! The compaction line is `window − output_reserve − trigger_buffer`, and the
//! provider rejects a request where `input + max_tokens > window`. The room
//! above that line is `output_reserve + trigger_buffer` — 33 000 tokens with
//! the default config — and `DEFAULT_MAX_TOKENS` is 32 768 precisely so a
//! request sent at the line still fits. It cannot simply be raised: it is a
//! plain number, checked against no model, and DeepSeek's whole window is
//! 64 000, so a flat 65 536 would make every DeepSeek request fail before it
//! was ever sent.
//!
//! A caller that wants more room can ask, because it knows its model and can
//! check the window. What that buys is a later request being rejected by the
//! provider and recovered by compaction, rather than a reasoning model running
//! out of room with nothing to compact — which is the failure two benchmark
//! runs actually hit.

use codape_core::types::ResolvedContextWindow;

/// The environment variable that sets the session's output ceiling.
///
/// Precedence: a named agent's `max_tokens` wins (an explicit configuration
/// beats an environment variable), then this, then
/// [`codape_core::engine::QueryEngine::DEFAULT_MAX_TOKENS`].
pub const MAX_TOKENS_ENV: &str = "CODAPE_MAX_TOKENS";

/// The smallest share of the window the clamp leaves for input.
///
/// A ceiling above half the window cannot be recovered from. The provider
/// rejects the request, and the only remedy is compaction, which has to bring
/// the context below `window − max_tokens` — below half the window — while the
/// compaction line itself never drops under half the window. Half is where
/// those two meet.
const MIN_INPUT_SHARE_DIVISOR: usize = 2;

/// The ceiling to request, given what is set in the environment.
///
/// An unset, empty or non-positive value leaves `default` alone. Anything that
/// parses wins outright — raising or lowering the ceiling is the caller's
/// call. The window is not consulted here; see [`resolve_ceiling`], which is
/// what a session should call.
#[must_use]
pub fn desired_ceiling_from_env(default: u32) -> u32 {
    env_ceiling().unwrap_or(default)
}

/// What the environment names, if it names a usable value.
///
/// `None` for unset, blank, zero and anything that does not parse, so callers
/// can tell "the operator asked for this" from "nobody said anything".
fn env_ceiling() -> Option<u32> {
    let value = std::env::var(MAX_TOKENS_ENV).ok()?;
    match value.trim().parse::<u32>() {
        Ok(tokens) if tokens > 0 => Some(tokens),
        _ => {
            if !value.trim().is_empty() {
                tracing::warn!(
                    value = %value,
                    "ignoring {MAX_TOKENS_ENV}: expected a positive integer"
                );
            }
            None
        }
    }
}

/// Clamp `ceiling` so it can never take more than half of `window`.
///
/// This is the boundary between "one rejected request, recoverable by
/// compacting" and "a request that no context size can make acceptable". The
/// provider rejects `input + max_tokens > window`; the remedy is compaction,
/// which has to bring the context below `window − ceiling`. Compaction's own
/// line never drops below half the window, so a ceiling above half the window
/// is a request compaction cannot make room for.
///
/// A guessed window is never used to clamp: it is a placeholder for a model
/// nobody knows, and shrinking a request to fit a guess would cut output for a
/// limit that may not exist. `window == 0` is treated the same way.
#[must_use]
pub fn clamp_to_window(ceiling: u32, window: ResolvedContextWindow) -> u32 {
    if window.tokens == 0 || window.source.is_guess() {
        return ceiling;
    }
    let room = window.tokens / MIN_INPUT_SHARE_DIVISOR;
    let room = u32::try_from(room).unwrap_or(u32::MAX);
    if ceiling > room {
        tracing::debug!(
            ceiling,
            room,
            window = window.tokens,
            "lowering {MAX_TOKENS_ENV} to leave half the window for input"
        );
        room.max(1)
    } else {
        ceiling
    }
}

/// The ceiling for a session, from the environment and the model's window.
///
/// The clamp applies only to a value the environment supplied. The engine's own
/// default is left exactly as it is when nothing is set, so a session that does
/// not opt in behaves the same as it did before this module existed.
#[must_use]
pub fn resolve_ceiling(default: u32, window: ResolvedContextWindow) -> u32 {
    env_ceiling().map_or(default, |ceiling| clamp_to_window(ceiling, window))
}

/// The window to clamp against, for a caller that has no engine yet.
///
/// Resolves through the same chain the engine uses, so the clamp sees an
/// environment or settings override rather than only the built-in table.
#[must_use]
pub fn window_for(model: &codape_core::types::ModelId) -> ResolvedContextWindow {
    codape_core::types::ContextWindowResolver::from_env_and_client_data().resolve_detailed(&model.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use codape_core::types::ContextWindowSource;

    fn with_env<T>(name: &str, value: Option<&str>, run: impl FnOnce() -> T) -> T {
        let _guard = crate::ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = std::env::var_os(name);
        // SAFETY: ENV_LOCK is held and no other test mutates this variable.
        #[allow(unsafe_code)]
        unsafe {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
        let result = run();
        // SAFETY: as above; ENV_LOCK is still held.
        #[allow(unsafe_code)]
        unsafe {
            match previous {
                Some(previous) => std::env::set_var(name, previous),
                None => std::env::remove_var(name),
            }
        }
        result
    }

    fn known(tokens: usize) -> ResolvedContextWindow {
        ResolvedContextWindow::new(tokens, ContextWindowSource::Table)
    }

    #[test]
    fn the_environment_raises_and_lowers_the_ceiling() {
        with_env(MAX_TOKENS_ENV, None, || {
            assert_eq!(desired_ceiling_from_env(32_768), 32_768);
        });
        with_env(MAX_TOKENS_ENV, Some("65536"), || {
            assert_eq!(desired_ceiling_from_env(32_768), 65_536);
        });
        with_env(MAX_TOKENS_ENV, Some("8192"), || {
            assert_eq!(desired_ceiling_from_env(32_768), 8_192);
        });
    }

    #[test]
    fn a_bad_environment_value_leaves_the_default_alone() {
        for value in ["0", "-1", "lots", "", "  "] {
            with_env(MAX_TOKENS_ENV, Some(value), || {
                assert_eq!(
                    desired_ceiling_from_env(32_768),
                    32_768,
                    "{value:?} was accepted"
                );
            });
        }
    }

    /// DeepSeek's window is 64 000, so a 65 536 ceiling is more than the whole
    /// window and no context size could make the request acceptable.
    #[test]
    fn a_ceiling_never_takes_the_whole_window() {
        let clamped = clamp_to_window(65_536, known(64_000));
        assert_eq!(clamped, 32_000, "half of 64 000");
        assert!(u64::from(clamped) < 64_000, "no room was left for input");
    }

    #[test]
    fn a_ceiling_that_fits_is_left_alone() {
        assert_eq!(clamp_to_window(32_768, known(200_000)), 32_768);
        assert_eq!(clamp_to_window(65_536, known(200_000)), 65_536);
    }

    /// A guessed window must not shrink a request: it is a placeholder for a
    /// model nobody knows, and the guess may not describe a real limit.
    #[test]
    fn a_guessed_window_does_not_clamp() {
        let guessed = ResolvedContextWindow::new(32_000, ContextWindowSource::Unknown);
        assert_eq!(clamp_to_window(65_536, guessed), 65_536);
        assert_eq!(clamp_to_window(65_536, known(0)), 65_536);
    }

    #[test]
    fn the_resolved_ceiling_combines_both_rules() {
        with_env(MAX_TOKENS_ENV, Some("65536"), || {
            assert_eq!(resolve_ceiling(32_768, known(200_000)), 65_536);
            assert_eq!(resolve_ceiling(32_768, known(64_000)), 32_000);
            assert_eq!(
                resolve_ceiling(
                    32_768,
                    ResolvedContextWindow::new(32_000, ContextWindowSource::Unknown)
                ),
                65_536
            );
        });
    }

    /// Unset means the engine default, untouched by any window.
    #[test]
    fn the_unset_ceiling_does_not_depend_on_the_window() {
        with_env(MAX_TOKENS_ENV, None, || {
            assert_eq!(resolve_ceiling(32_768, known(64_000)), 32_768);
            assert_eq!(resolve_ceiling(32_768, known(200_000)), 32_768);
        });
    }
}
