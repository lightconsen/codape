//! Watches test commands and reminds the model when an edit turns a test red.
//!
//! A code change is judged by the repository's tests, and a test that passed
//! before an edit and fails after it is the strongest evidence the approach is
//! wrong. Models do not always read it that way: on `fastlane__fastlane-19207`
//! the run changed `Actions.sh(zip_command)` to `Actions.sh(*zip_command)`, saw
//! six existing specs fail on the arity change, and then rewrote the spec
//! expectations to match the new call shape instead of reconsidering the
//! change. The repository's own contract was the answer, and the run discarded
//! it.
//!
//! The guard does not block anything. It records the failure count of every
//! recognized test command and, when a command that was green earlier in the
//! run comes back red — or when the first run of a test command after a source
//! edit comes back red — queues one system reminder for the next turn. The
//! model still decides what to do; it just cannot miss the signal.
//!
//! Parsing is deliberately narrow. A command is a test command only when its
//! output carries a runner's own summary line (gtest, cargo, rspec, pytest,
//! ctest, jest, mocha, go). An arbitrary command that prints the word "failed"
//! is not one, because a false reminder costs a turn and can derail a run.

use crate::engine::PendingToolCall;
use crate::types::ToolResult;
use std::collections::{HashMap, HashSet};

/// Tool names whose input names a shell command.
const SHELL_TOOLS: [&str; 2] = ["Bash", "PowerShell"];

/// Tool names that write a file.
const EDIT_TOOLS: [&str; 4] = ["Edit", "Write", "MultiEdit", "NotebookEdit"];

/// What the guard knows about the run so far.
#[derive(Debug, Default)]
pub struct TestGuard {
    /// Last failure count per test command.
    last_failures: HashMap<String, usize>,
    /// Commands already reminded about, so one red command is not repeated
    /// every turn.
    warned: HashSet<String>,
    /// Whether a non-test file has been edited in this run.
    source_edited: bool,
    /// Reminders waiting to be injected into the next request.
    pending: Vec<String>,
}

impl TestGuard {
    /// A guard that has seen nothing yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Observe one batch of tool calls and their results.
    ///
    /// `calls` and `results` are matched by tool-call id, not position: the
    /// scheduler returns results in call order today, but the engine should
    /// not depend on that staying true.
    pub fn observe(&mut self, calls: &[PendingToolCall], results: &[ToolResult]) {
        let by_id: HashMap<&str, &ToolResult> = results
            .iter()
            .map(|result| (result.tool_call_id.as_str(), result))
            .collect();
        for call in calls {
            let Some(result) = by_id.get(call.id.as_str()) else {
                continue;
            };
            if result.is_error {
                continue;
            }
            if EDIT_TOOLS.contains(&call.name.as_str()) {
                if let Some(path) = call.input.get("file_path").and_then(|v| v.as_str())
                    && !looks_like_test_path(path)
                {
                    self.source_edited = true;
                }
                continue;
            }
            if !SHELL_TOOLS.contains(&call.name.as_str()) {
                continue;
            }
            let Some(command) = call.input.get("command").and_then(|v| v.as_str()) else {
                continue;
            };
            let Some(failures) = failure_count(&result.output) else {
                continue;
            };
            self.record(command, failures);
        }
    }

    /// Take the reminders queued since the last turn, if any.
    pub fn take_pending(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pending)
    }

    fn record(&mut self, command: &str, failures: usize) {
        let key = normalize_command(command);
        let previous = self.last_failures.insert(key.clone(), failures);
        if failures == 0 {
            // A green run clears the one-shot: the next red run is new
            // information, not the same failure repeated.
            self.warned.remove(&key);
            return;
        }
        if previous == Some(0) {
            self.warned.insert(key);
            self.pending.push(format!(
                "<system-reminder>\nTest guard: this test command passed earlier \
                 in the run and now reports {failures} failure(s). A regression \
                 like this is evidence that the edit violates the existing \
                 contract. Revert the change or take a different approach; do \
                 not edit the test to match the code.\n</system-reminder>"
            ));
        } else if self.source_edited && self.warned.insert(key) {
            self.pending.push(format!(
                "<system-reminder>\nTest guard: this test command reports \
                 {failures} failure(s) after your edit. Existing tests encode \
                 the contract. If it passed before your edit, revert and find a \
                 change that keeps it green rather than changing the test; if \
                 it was already failing, ignore this note.\n</system-reminder>"
            ));
        }
    }
}

/// Whether a path names a test file rather than the code under test.
fn looks_like_test_path(path: &str) -> bool {
    let path = path.replace('\\', "/").to_lowercase();
    path.contains("/test/")
        || path.contains("/tests/")
        || path.contains("/spec/")
        || path.contains("/__tests__/")
        || path.ends_with("_test.rs")
        || path.ends_with("_test.py")
        || path.ends_with("_test.go")
        || path.contains("_test.")
        || path.contains("_spec.")
        || path.contains(".test.")
        || path.contains(".spec.")
}

/// Normalize whitespace so a re-run of the same command matches its baseline.
fn normalize_command(command: &str) -> String {
    command.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The number of failing tests in a runner's summary, if this is a test run.
///
/// Returns `None` when the output carries no recognized summary; a build
/// failure or an arbitrary command is not a red test.
#[must_use]
pub fn failure_count(output: &str) -> Option<usize> {
    let mut best: Option<usize> = None;
    let mut saw_green = false;
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // GoogleTest: `[  FAILED  ] 6 tests, listed below:` and
        // `[  PASSED  ] 119 tests.`
        if let Some(rest) = line.strip_prefix("[  FAILED  ]")
            && let Some(count) = leading_number(rest)
            && rest.to_lowercase().contains("test")
        {
            best = Some(best.unwrap_or(0).max(count));
            continue;
        }
        if line.starts_with("[  PASSED  ]") {
            saw_green = true;
            continue;
        }
        // cargo test: `test result: FAILED. 3 passed; 2 failed; ...` or
        // `test result: ok. 42 passed; 0 failed; ...`
        if line.starts_with("test result:") {
            if line.contains("FAILED") {
                if let Some(count) = number_before(line, "failed") {
                    best = Some(best.unwrap_or(0).max(count));
                }
            } else if line.contains("ok.") {
                saw_green = true;
            }
            continue;
        }
        // RSpec: `9 examples, 6 failures` / `9 examples, 0 failures`
        if line.contains("example") && line.contains("failure") {
            if let Some(count) = number_before(line, "failure") {
                best = Some(best.unwrap_or(0).max(count));
            }
            continue;
        }
        // CTest: `100% tests passed, 0 tests failed out of 12` or
        // `The following tests FAILED:`. Checked before the pytest shape,
        // which it also matches (`failed` and `passed` on one line).
        if line.contains("tests failed out of") {
            if let Some(count) = number_before(line, "tests failed") {
                best = Some(best.unwrap_or(0).max(count));
            }
            continue;
        }
        // pytest: `= 2 failed, 10 passed in 0.31s =`
        if line.contains("failed") && (line.contains("passed") || line.contains("error")) {
            if let Some(count) = number_before(line, "failed") {
                best = Some(best.unwrap_or(0).max(count));
            }
            continue;
        }
        if line.starts_with("The following tests FAILED:") {
            best = Some(best.unwrap_or(1).max(1));
            continue;
        }
        // Jest: `Tests: 1 failed, 10 passed, 11 total`
        if let Some(rest) = line.strip_prefix("Tests:")
            && let Some(count) = number_before(rest, "failed")
        {
            best = Some(best.unwrap_or(0).max(count));
            continue;
        }
        if line.starts_with("Tests:") && line.contains("passed") {
            saw_green = true;
            continue;
        }
        // Mocha: `3 passing` / `1 failing`
        if let Some(count) = leading_number_after(line, "failing") {
            best = Some(best.unwrap_or(0).max(count));
            continue;
        }
        if line.contains("passing") && !line.contains("failing") {
            saw_green = true;
            continue;
        }
        // Go: `--- FAIL: TestX` / `--- PASS: TestX`
        if line.starts_with("--- FAIL:") {
            best = Some(best.unwrap_or(0) + 1);
            continue;
        }
        if line.starts_with("--- PASS:") {
            saw_green = true;
        }
    }
    match best {
        Some(count) => Some(count),
        None if saw_green => Some(0),
        None => None,
    }
}

/// The integer at the start of `text`, if any.
fn leading_number(text: &str) -> Option<usize> {
    let digits: String = text
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// The integer immediately before `word` in `text`.
fn number_before(text: &str, word: &str) -> Option<usize> {
    let index = text.find(word)?;
    let before = text[..index].trim_end();
    let digits: String = before
        .chars()
        .rev()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    digits.parse().ok()
}

/// The integer at the start of a line that also contains `word`.
fn leading_number_after(line: &str, word: &str) -> Option<usize> {
    if !line.contains(word) {
        return None;
    }
    leading_number(line)
}

#[cfg(test)]
mod tests {
    use super::{TestGuard, failure_count};
    use crate::engine::PendingToolCall;
    use crate::types::ToolResult;

    #[test]
    fn reads_gtest_failures() {
        let output = "[  FAILED  ] 6 tests, listed below:\n[  FAILED  ] a\n[  FAILED  ] b\n\n\
                      6 FAILED TESTS\n\n[  PASSED  ] 119 tests.";
        assert_eq!(failure_count(output), Some(6));
    }

    #[test]
    fn reads_gtest_success() {
        assert_eq!(
            failure_count("[==========] 119 tests ran.\n[  PASSED  ] 119 tests."),
            Some(0)
        );
    }

    #[test]
    fn reads_rspec_failures() {
        assert_eq!(failure_count("9 examples, 6 failures"), Some(6));
    }

    #[test]
    fn reads_rspec_success() {
        assert_eq!(failure_count("9 examples, 0 failures"), Some(0));
    }

    #[test]
    fn reads_cargo_failures() {
        assert_eq!(
            failure_count("test result: FAILED. 3 passed; 2 failed; 0 ignored"),
            Some(2)
        );
    }

    #[test]
    fn reads_pytest_failures() {
        assert_eq!(
            failure_count("=== 2 failed, 10 passed in 0.31s ==="),
            Some(2)
        );
    }

    #[test]
    fn reads_ctest_success() {
        assert_eq!(
            failure_count("100% tests passed, 0 tests failed out of 12"),
            Some(0)
        );
    }

    #[test]
    fn ignores_output_without_a_runner_summary() {
        assert_eq!(failure_count("grep: 0 failed matches"), None);
        assert_eq!(failure_count("error: could not compile `fmt`"), None);
    }

    #[test]
    fn a_green_run_then_a_red_one_is_a_regression() {
        let mut guard = TestGuard::new();
        guard.observe(
            &[bash("c1", "cargo test")],
            &[result("c1", "test result: ok. 5 passed; 0 failed")],
        );
        guard.observe(
            &[bash("c2", "cargo test")],
            &[result("c2", "test result: FAILED. 3 passed; 2 failed")],
        );
        let reminders = guard.take_pending();
        assert_eq!(reminders.len(), 1);
        assert!(reminders[0].contains("passed earlier"));
        assert!(reminders[0].contains("2 failure"));
    }

    #[test]
    fn a_first_red_run_after_a_source_edit_is_flagged_once() {
        let mut guard = TestGuard::new();
        guard.observe(
            &[edit("e1", "/testbed/src/lib.rs")],
            &[result("e1", "updated")],
        );
        guard.observe(
            &[bash("c1", "bundle exec rspec spec/zip_spec.rb")],
            &[result("c1", "9 examples, 6 failures")],
        );
        assert_eq!(guard.take_pending().len(), 1);
        // The same command failing again is not a new signal.
        guard.observe(
            &[bash("c2", "bundle exec rspec spec/zip_spec.rb")],
            &[result("c2", "9 examples, 6 failures")],
        );
        assert!(guard.take_pending().is_empty());
    }

    #[test]
    fn a_red_run_before_any_source_edit_is_not_flagged() {
        let mut guard = TestGuard::new();
        guard.observe(
            &[bash("c1", "pytest -q")],
            &[result("c1", "1 failed, 2 passed in 0.1s")],
        );
        assert!(guard.take_pending().is_empty());
    }

    fn bash(id: &str, command: &str) -> PendingToolCall {
        PendingToolCall {
            id: id.into(),
            name: "Bash".into(),
            input: serde_json::json!({ "command": command }),
        }
    }

    fn edit(id: &str, path: &str) -> PendingToolCall {
        PendingToolCall {
            id: id.into(),
            name: "Edit".into(),
            input: serde_json::json!({ "file_path": path }),
        }
    }

    fn result(id: &str, output: &str) -> ToolResult {
        ToolResult {
            tool_call_id: id.into(),
            output: output.into(),
            is_error: false,
            images: Vec::new(),
        }
    }
}
