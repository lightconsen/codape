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

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(text.as_bytes())
            .map_err(|error| format!("could not write to `{program}`: {error}"))?;
    }
    // Dropping the handle above closes the pipe, which is what tells `pbcopy`
    // the text is complete; without the wait it would still be reading.

    let status = child
        .wait()
        .map_err(|error| format!("`{program}` did not finish: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("`{program}` exited with {status}"))
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
    #[cfg(unix)]
    #[test]
    fn a_helper_that_fails_is_reported() {
        let error = copy_through("sh", &["-c", "exit 3"], "text")
            .expect_err("a failing helper must not succeed");
        assert!(error.contains("exited with"), "{error}");
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
