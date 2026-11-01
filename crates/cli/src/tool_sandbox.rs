//! Whether tool execution gets the OS sandbox.
//!
//! The shell-tool sandbox is on by default. `CODEREX_SANDBOX=off` turns it off
//! for the whole process. That escape hatch is for environments that are
//! already an isolation boundary — a benchmark or CI container, a throwaway VM
//! — where the extra filter is redundant and blocks legitimate work such as
//! `apt-get install`, `git clone` or `curl`.
//!
//! It deliberately affects only the tool-execution config. Hook, MCP and WASM
//! sandboxes are separate and keep their own policy.

use coderex_tools::ExecutionConfig;

/// Environment variable that overrides the shell-tool sandbox.
pub const SANDBOX_ENV: &str = "CODEREX_SANDBOX";

/// Whether `CODEREX_SANDBOX` names an off value.
///
/// Anything unset or unrecognised leaves the sandbox on; only explicit
/// negatives disable it.
#[must_use]
pub fn enabled_from(value: Option<&str>) -> bool {
    !value.is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "off" | "0" | "false" | "no" | "disabled"
        )
    })
}

/// Whether the shell-tool sandbox is enabled for this process.
#[must_use]
pub fn enabled() -> bool {
    enabled_from(std::env::var(SANDBOX_ENV).ok().as_deref())
}

/// The tool-execution config with the sandbox switch applied.
#[must_use]
pub fn execution_config() -> ExecutionConfig {
    ExecutionConfig {
        sandbox_enabled: enabled(),
        ..ExecutionConfig::default()
    }
}

#[cfg(test)]
mod tests {
    use super::enabled_from;

    #[test]
    fn the_sandbox_is_on_unless_explicitly_off() {
        assert!(enabled_from(None));
        for value in ["off", "0", "false", "no", "disabled", " OFF ", "False"] {
            assert!(!enabled_from(Some(value)), "{value:?} should disable it");
        }
        for value in ["on", "1", "true", "yes", ""] {
            assert!(enabled_from(Some(value)), "{value:?} should leave it on");
        }
    }
}
