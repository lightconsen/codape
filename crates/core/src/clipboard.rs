//! Putting text on the system clipboard.
//!
//! Two features need this and they live in different crates: `/copy` in
//! `codape-commands` and `ctrl+shift+c` in `codape-ui`. It was written twice,
//! and the copies had drifted — only one knew about Windows, only one looked at
//! the helper's exit status. A person pressing the key and a person typing the
//! command are asking for the same thing, so they get the same implementation
//! here.
//!
//! No clipboard library: each platform's own helper is already installed
//! wherever a terminal is, and shelling out to it costs nothing at startup,
//! which a linked library would.

use std::io::Write;
use std::process::{Command, Stdio};

/// The clipboard helper this platform uses, and what to pass it.
///
/// `None` on a platform with no helper we know: the caller reports that rather
/// than running a command that does not exist.
#[must_use]
pub const fn helper() -> Option<(&'static str, &'static [&'static str])> {
    if cfg!(target_os = "macos") {
        Some(("pbcopy", &[]))
    } else if cfg!(target_os = "windows") {
        Some(("clip", &[]))
    } else if cfg!(target_os = "linux") {
        Some(("xclip", &["-selection", "clipboard"]))
    } else {
        None
    }
}

/// Put `text` on the system clipboard.
///
/// # Errors
///
/// Names the helper and what went wrong — it is not installed, it could not be
/// written to, or it exited non-zero. The failure a person actually hits is the
/// missing helper on a machine without one, and they can only fix that if they
/// are told which program was looked for.
pub fn copy_to_clipboard(text: &str) -> Result<(), String> {
    let Some((program, args)) = helper() else {
        return Err(format!(
            "no clipboard helper is known for this platform ({})",
            std::env::consts::OS
        ));
    };
    copy_through(program, args, text)
}

/// Run one clipboard program over `text`.
///
/// Split out from [`copy_to_clipboard`] so the failure paths can be tested
/// against a program that really exists (`cat`) instead of a real clipboard:
/// a test must never write to the machine's clipboard, and a test that only
/// ever exercises the success path would not have caught the missing Windows
/// branch that made this module necessary.
fn copy_through(program: &str, args: &[&str], text: &str) -> Result<(), String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("`{program}` could not be started: {error}"))?;

    // The `stdin` handle is consumed here, so it is dropped as soon as the
    // write finishes — closing the pipe, which is what tells `pbcopy` the text
    // is complete. Without that it would still be reading.
    let write_error = child
        .stdin
        .take()
        .and_then(|mut stdin| stdin.write_all(text.as_bytes()).err());

    let status = child
        .wait()
        .map_err(|error| format!("`{program}` did not finish: {error}"))?;
    let failure = (!status.success()).then(|| format!("`{program}` exited with {status}"));
    verdict(program, failure, write_error)
}

/// Decide whether a finished helper copied the text.
///
/// Split out so every combination can be tested without arranging a real
/// clipboard race. A helper is free to stop reading before the last byte
/// arrives, and then the write fails with `EPIPE` whether or not anything is
/// wrong: `xclip` returns to its shell once it holds the selection, and a
/// helper that rejects its input exits without draining it. Failing on the
/// write alone therefore reports a write error for a run that worked — and
/// hides the exit status that says why a run that did not work failed.
///
/// The exit status is the authority. A helper that failed is reported as
/// failed whatever the write did; only a helper that succeeded leaves the write
/// error meaningful, and only because a broken pipe at that point is the helper
/// having decided it had enough.
fn verdict(
    program: &str,
    failure: Option<String>,
    write_error: Option<std::io::Error>,
) -> Result<(), String> {
    if let Some(failure) = failure {
        return Err(failure);
    }
    match write_error {
        Some(error) if error.kind() != std::io::ErrorKind::BrokenPipe => {
            Err(format!("could not write to `{program}`: {error}"))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every platform this project builds for has a helper. `clip` on Windows
    /// is the branch one copy of this code was missing.
    #[test]
    fn the_supported_platforms_name_a_helper() {
        let (program, _) = helper().expect("no clipboard helper for this platform");
        assert!(!program.is_empty());
    }

    /// The error names the program, because "copy failed" leaves the person
    /// with nothing to install.
    #[test]
    fn a_missing_helper_is_named_in_the_error() {
        let error = copy_through("codape-no-such-clipboard-helper", &[], "text")
            .expect_err("a nonexistent program must not succeed");
        assert!(error.contains("codape-no-such-clipboard-helper"), "{error}");
        assert!(error.contains("could not be started"), "{error}");
    }

    /// A helper that exits non-zero is a failure, not a silent success. This is
    /// the branch that only reads the exit status.
    ///
    /// The helper here exits *without reading its standard input*, so the write
    /// races into `EPIPE`. That is not incidental to the test — it is the
    /// reason the test exists. `sh -c "exit 3"` on a loaded CI runner usually
    /// exits before the write lands, and the module used to fail on the write
    /// then, reporting ``could not write to `sh`: Broken pipe`` instead of the
    /// exit status. Whether the helper read or did not read decides which error
    /// surfaced, so this test passed or failed depending on machine load:
    /// observed failing in the Coverage job while passing in Test on the same
    /// commit.
    ///
    /// The payload is oversized so the pipe buffer cannot absorb it and the
    /// broken pipe is certain rather than likely.
    #[cfg(unix)]
    #[test]
    fn a_helper_that_fails_is_reported() {
        let text = "x".repeat(1 << 20);
        let error = copy_through("sh", &["-c", "exit 3"], &text)
            .expect_err("a failing helper must not succeed");
        assert!(
            error.contains("exited with"),
            "the exit status is what went wrong, so that is what to report: {error}"
        );
    }

    /// A helper that exits successfully after closing the pipe did its job.
    ///
    /// `xclip` takes the selection and returns; a writer still pushing bytes
    /// then sees `EPIPE` for a copy that worked. Reporting that as a failure
    /// would tell a Linux user their copy failed every time.
    ///
    /// The helper here exits *without reading anything*, and the payload is
    /// larger than the pipe buffer, so `write_all` cannot complete before the
    /// read end closes and the broken pipe is certain. A `cat`-based helper
    /// would drain the input and never fail, which is how an earlier version of
    /// this test passed while exercising nothing.
    #[cfg(unix)]
    #[test]
    fn a_helper_that_closed_the_pipe_after_succeeding_is_not_a_failure() {
        let text = "x".repeat(1 << 20);
        copy_through("sh", &["-c", "exit 0"], &text)
            .expect("a helper that took the text and succeeded is a successful copy");
    }

    /// A write error that is not a broken pipe is still a real error, even when
    /// the helper exits zero. Widening the ignored case to every write error
    /// would silently accept a lost copy.
    ///
    /// Tested on the decision rather than through a live pipeline: a pipe's
    /// write error cannot be made to be anything but `EPIPE` from the writing
    /// side. `/dev/full` fails inside the helper, not at our `write_all`, so a
    /// pipeline version of this test passed without exercising the branch.
    #[test]
    fn a_write_error_that_is_not_a_broken_pipe_is_still_reported() {
        let error = verdict(
            "sh",
            None,
            Some(std::io::Error::from_raw_os_error(28)), // ENOSPC
        )
        .expect_err("a write that failed for space is a lost copy");
        assert!(error.contains("could not write to `sh`"), "{error}");
    }

    /// A helper that failed is reported as failed even when the write also
    /// failed. The exit status is what says *why*, so it is the message that
    /// has to survive.
    #[test]
    fn a_failing_helper_is_reported_even_when_the_write_broke() {
        let error = verdict(
            "sh",
            Some("`sh` exited with exit status: 3".to_string()),
            Some(std::io::Error::from(std::io::ErrorKind::BrokenPipe)),
        )
        .expect_err("a failing helper must not succeed");
        assert_eq!(error, "`sh` exited with exit status: 3", "{error}");
    }

    /// A broken pipe from a helper that succeeded is not a failure: it stopped
    /// reading because it already had the text.
    #[test]
    fn a_broken_pipe_from_a_successful_helper_is_a_success() {
        verdict(
            "xclip",
            None,
            Some(std::io::Error::from(std::io::ErrorKind::BrokenPipe)),
        )
        .expect("the helper took the selection and returned");
    }

    /// Nothing went wrong at all.
    #[test]
    fn a_clean_run_is_a_success() {
        verdict("pbcopy", None, None).expect("a clean copy succeeds");
    }

    /// The text is written to the helper's stdin, which is verified without a
    /// clipboard by giving it `cat` and reading back what it wrote into a file.
    /// `stdout` is nulled inside `copy_through`, so the file is the sink.
    #[cfg(unix)]
    #[test]
    fn the_text_reaches_the_helper_standard_input() {
        let dir = std::env::temp_dir().join(format!("codape-clipboard-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        let sink = dir.join("out");
        let script = format!("cat > '{}'", sink.display());

        copy_through("sh", &["-c", &script], "the text that must arrive")
            .expect("cat as a clipboard must succeed");

        let written = std::fs::read_to_string(&sink).expect("the helper wrote nothing");
        assert_eq!(written, "the text that must arrive");
        std::fs::remove_dir_all(&dir).ok();
    }
}
