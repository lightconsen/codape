//! Voice input: Codape owns the buffer, the host owns the microphone.
//!
//! There is no audio stack in this binary and adding one would be a second
//! product — device enumeration, permissions, a capture callback, an STT
//! client. What the terminal *can* do is run a command the person configured
//! and take its stdout as the transcript. That is the same contract `$EDITOR`
//! has: the shell has the microphone (sox, ffmpeg, a vendor CLI, a script that
//! calls an API) and this program has the text field.
//!
//! The setting is `voice.command` in the user's `settings.json`:
//!
//! ```json
//! { "voice": { "command": "record-and-transcribe.sh" } }
//! ```
//!
//! Deliberately a command and not a fixed program name: which recorder and
//! which recogniser a person has is their machine's business, and a hardcoded
//! default would be a guess that fails on every machine that differs.
//!
//! The command runs with the session's working directory and its environment,
//! because the key it needs is already there — the same reason the API key is
//! read from the environment rather than passed on a command line, where `ps`
//! would see it.

use std::path::Path;

/// Where the transcription command is configured.
///
/// Nested under `voice` rather than a flat `voiceCommand` key so the block can
/// grow (a language, a timeout, an append mode) without a second top-level
/// name for each.
pub const COMMAND_KEY: &str = "voice.command";

/// Read the configured transcription command, if there is one.
///
/// Returns `None` for a missing file, a missing key, an empty string, or a
/// document that is not JSON. A hand-edited settings file must not stop a
/// session from starting, and "no command" already has a defined meaning —
/// voice input is off — so every unreadable case collapses to it.
#[must_use]
pub fn command_from_settings(settings: &serde_json::Value) -> Option<String> {
    let text = settings.get("voice")?.get("command")?.as_str()?.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

/// Read the configured command out of a settings file on disk.
///
/// The path is a parameter rather than resolved here so a test can point at a
/// fixture; the session passes its own `settings.json`.
#[must_use]
pub fn command_from_file(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let settings = serde_json::from_str::<serde_json::Value>(&text).ok()?;
    command_from_settings(&settings)
}

/// Take the transcript out of a command's stdout.
///
/// A recogniser prints one line; a shell script may print a banner first. The
/// rule is "the last non-empty line", which is what a script that logs and then
/// answers produces, and is stable for the one-line case too. Surrounding
/// whitespace goes with it — a trailing newline pasted into the prompt would
/// look like a stray blank line.
#[must_use]
pub fn transcript_from_stdout(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

/// The shell that runs the command on this platform.
#[must_use]
pub const fn shell_program() -> &'static str {
    if cfg!(windows) { "cmd" } else { "sh" }
}

/// The argument that makes `shell_program` take a command string.
#[must_use]
pub const fn shell_args(command: &str) -> [&str; 2] {
    if cfg!(windows) {
        ["/C", command]
    } else {
        ["-c", command]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_is_read_from_its_own_block() {
        let settings = serde_json::json!({
            "voice": { "command": "scripts/stt.sh --lang en" }
        });
        assert_eq!(
            command_from_settings(&settings).as_deref(),
            Some("scripts/stt.sh --lang en")
        );
    }

    #[test]
    fn a_missing_empty_or_odd_command_is_no_command() {
        // Every one of these means "voice input is off", which is a state the
        // session already knows how to be in.
        for settings in [
            serde_json::json!({}),
            serde_json::json!({ "voice": {} }),
            serde_json::json!({ "voice": { "command": "" } }),
            serde_json::json!({ "voice": { "command": "   " } }),
            serde_json::json!({ "voice": { "command": 7 } }),
            serde_json::json!({ "voice": "not an object" }),
            serde_json::json!([1, 2, 3]),
        ] {
            assert!(
                command_from_settings(&settings).is_none(),
                "{settings} produced a command"
            );
        }
    }

    #[test]
    fn an_unreadable_file_is_no_command() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(command_from_file(&dir.path().join("absent.json")).is_none());
        let broken = dir.path().join("broken.json");
        std::fs::write(&broken, "{ not json").expect("write");
        assert!(command_from_file(&broken).is_none());
    }

    #[test]
    fn the_last_non_empty_line_is_the_transcript() {
        // A script that says what it is doing and then answers.
        let stdout = "listening...\nrecording\nfix the failing test\n\n";
        assert_eq!(
            transcript_from_stdout(stdout).as_deref(),
            Some("fix the failing test")
        );
        assert_eq!(
            transcript_from_stdout("  just this  \n").as_deref(),
            Some("just this")
        );
    }

    #[test]
    fn silence_is_not_a_transcript() {
        // The microphone may have heard nothing, and an empty prompt is not
        // something to insert: the state machine treats it as "keep listening".
        assert!(transcript_from_stdout("").is_none());
        assert!(transcript_from_stdout("\n\n   \n").is_none());
    }

    #[test]
    fn the_shell_arguments_wrap_the_whole_command() {
        // Passed as one argument so the shell parses it: quoting rules belong
        // to the shell the person configured, not to this program.
        let command = "stt --lang en && say done";
        let args = shell_args(command);
        assert_eq!(args[1], command);
        assert!(["sh", "cmd"].contains(&shell_program()));
    }
}
