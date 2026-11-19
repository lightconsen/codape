//! Writing the prompt in `$EDITOR`: the editor owns the keyboard, this owns
//! the text.
//!
//! The input line is one row. That is the right shape for a sentence and the
//! wrong one for a prompt with a code block, a stack trace, a list of files.
//! Every editor-driven agent answers this the same way — hand the buffer to
//! whatever the person already writes in, and take it back — and the contract
//! is exactly `$EDITOR`'s: a file path as the last argument, the exit status
//! saying whether to keep the file, and the file itself as the answer.
//!
//! Which program: `visual` then `editor` in `settings.json`, then `$VISUAL`,
//! then `$EDITOR`. The setting first because a person who configured one here
//! meant it here; `$VISUAL` before `$EDITOR` because that is the split the
//! shell made — `$VISUAL` is the full-screen editor, `$EDITOR` the line one —
//! and this wants the full-screen one.
//!
//! Unlike the voice command, this one is deliberately **not** run through a
//! shell. `$EDITOR` is conventionally a program plus arguments (`code -w`,
//! `vim -u NONE`), and splitting on spaces is what every program that reads it
//! does; a shell would additionally interpret the person's quoting, `&&`
//! chains and globs, which is not what an editor variable means.

use std::path::Path;

/// Where an explicit editor is configured.
///
/// `editor` is the name the standard editor variables use; `visual` sits
/// beside it so the full-screen preference can be stated without exporting
/// anything.
pub const EDITOR_KEY: &str = "editor";
/// The full-screen variant of [`EDITOR_KEY`].
pub const VISUAL_KEY: &str = "visual";

/// The editor to run, from settings, if settings name one.
#[must_use]
pub fn command_from_settings(settings: &serde_json::Value) -> Option<String> {
    for key in [VISUAL_KEY, EDITOR_KEY] {
        let text = settings
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        if !text.is_empty() {
            return Some(text.to_string());
        }
    }
    None
}

/// The editor to run, from settings or from the environment.
///
/// `env` is a lookup rather than `std::env::var` so a test can state the
/// environment it means instead of mutating the process's.
#[must_use]
pub fn resolve(
    settings: &serde_json::Value,
    env: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    if let Some(command) = command_from_settings(settings) {
        return Some(command);
    }
    for name in ["VISUAL", "EDITOR"] {
        let text = env(name).unwrap_or_default();
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    None
}

/// Split an editor command into program and arguments.
///
/// Whitespace separates, except inside single or double quotes, which are
/// removed. Not a shell parser: no escapes, no expansions, no operators. That
/// is the whole point — `$EDITOR` names a program and its flags, and anything
/// more would run a script the person did not write.
#[must_use]
pub fn split_command(command: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    for character in command.chars() {
        match quote {
            Some(open) if character == open => {
                quote = None;
                started = true;
            }
            Some(_) => current.push(character),
            None if character == '"' || character == '\'' => {
                quote = Some(character);
                started = true;
            }
            None if character.is_whitespace() => {
                if started || !current.is_empty() {
                    parts.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            None => {
                current.push(character);
                started = true;
            }
        }
    }
    if started || !current.is_empty() {
        parts.push(current);
    }
    parts
}

/// The prompt file's name, for the buffer's temporary directory.
///
/// A `.md` suffix rather than `.txt`: editors that pick a syntax from the
/// extension should treat a prompt with code fences as Markdown, and the ones
/// that do not ignore it.
pub const FILE_NAME: &str = "codape-prompt.md";

/// Take the prompt back out of the file the editor saved.
///
/// One trailing newline goes: every editor ends a text file with one, and a
/// prompt that carried it would look like it had a stray blank line. More than
/// one is kept, because a person who left two blank lines at the end meant to.
///
/// Text that is only whitespace is `None` — an editor quit after emptying the
/// file means "nothing here", not "a prompt of spaces".
#[must_use]
pub fn prompt_from_file(text: &str) -> Option<String> {
    let trimmed = text.strip_suffix('\n').unwrap_or(text);
    let trimmed = trimmed.strip_suffix('\r').unwrap_or(trimmed);
    if trimmed.trim().is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// The path the editor is pointed at, inside the given directory.
#[must_use]
pub fn prompt_path(dir: &Path) -> std::path::PathBuf {
    dir.join(FILE_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_string())
        }
    }

    #[test]
    fn the_setting_wins_over_the_environment() {
        let settings = serde_json::json!({"editor": "nvim"});
        assert_eq!(
            resolve(&settings, env(&[("EDITOR", "nano")])).as_deref(),
            Some("nvim")
        );
    }

    #[test]
    fn visual_is_preferred_over_editor() {
        assert_eq!(
            resolve(
                &serde_json::json!({}),
                env(&[("VISUAL", "code"), ("EDITOR", "vi")])
            )
            .as_deref(),
            Some("code")
        );
        assert_eq!(
            resolve(&serde_json::json!({}), env(&[("EDITOR", "vi")])).as_deref(),
            Some("vi")
        );
    }

    #[test]
    fn nothing_configured_is_nothing_to_run() {
        assert!(resolve(&serde_json::json!({}), env(&[])).is_none());
        // An empty variable is not a program name.
        assert!(resolve(&serde_json::json!({}), env(&[("EDITOR", "   ")])).is_none());
        // A non-string setting is skipped, and the environment is still read.
        let settings = serde_json::json!({"editor": 7});
        assert_eq!(
            resolve(&settings, env(&[("EDITOR", "vi")])).as_deref(),
            Some("vi")
        );
    }

    #[test]
    fn the_visual_setting_beats_the_editor_setting() {
        let settings = serde_json::json!({"visual": "code -w", "editor": "vi"});
        assert_eq!(command_from_settings(&settings).as_deref(), Some("code -w"));
    }

    #[test]
    fn a_command_splits_on_whitespace_and_honours_quotes() {
        assert_eq!(split_command("vim"), vec!["vim"]);
        assert_eq!(split_command("code -w"), vec!["code", "-w"]);
        assert_eq!(
            split_command("  vim   -u   NONE  "),
            vec!["vim", "-u", "NONE"]
        );
        // A quoted argument with a space in it is one argument...
        assert_eq!(
            split_command("sh -c 'nvim --cmd set nu'"),
            vec!["sh", "-c", "nvim --cmd set nu"]
        );
        // ...and a quote of the other kind inside it is a literal character,
        // not the end of the argument. Nesting is not a thing a shell does for
        // a single-quoted string either.
        assert_eq!(
            split_command(r#"sh -c 'nvim --cmd "set nu"'"#),
            vec!["sh", "-c", r#"nvim --cmd "set nu""#]
        );
        assert_eq!(
            split_command("\"/Applications/My Editor/bin/ed\""),
            vec!["/Applications/My Editor/bin/ed"]
        );
    }

    #[test]
    fn a_shell_operator_is_just_an_argument() {
        // Not interpreted: `$EDITOR` names a program, and running the rest
        // would execute something the person did not ask for.
        assert_eq!(
            split_command("vim && rm -rf /"),
            vec!["vim", "&&", "rm", "-rf", "/"]
        );
    }

    #[test]
    fn one_trailing_newline_comes_off_and_the_rest_stays() {
        assert_eq!(prompt_from_file("fix it\n").as_deref(), Some("fix it"));
        assert_eq!(prompt_from_file("fix it").as_deref(), Some("fix it"));
        assert_eq!(
            prompt_from_file("line one\nline two\n").as_deref(),
            Some("line one\nline two")
        );
        // A person who left a deliberate blank line keeps it.
        assert_eq!(prompt_from_file("fix it\n\n").as_deref(), Some("fix it\n"));
        // CRLF from an editor on Windows.
        assert_eq!(prompt_from_file("fix it\r\n").as_deref(), Some("fix it"));
    }

    #[test]
    fn an_emptied_file_is_no_prompt() {
        assert!(prompt_from_file("").is_none());
        assert!(prompt_from_file("\n").is_none());
        assert!(prompt_from_file("   \n\t\n").is_none());
    }

    #[test]
    fn the_file_sits_in_the_directory_it_is_given() {
        let dir = Path::new("/tmp/example");
        assert_eq!(prompt_path(dir), dir.join(FILE_NAME));
        assert_eq!(
            std::path::Path::new(FILE_NAME).extension(),
            Some(std::ffi::OsStr::new("md")),
            "{FILE_NAME}"
        );
    }
}
