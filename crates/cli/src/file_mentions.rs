//! `@path` file mentions and the completion list behind them.
//!
//! Typing `@src/lib.rs` in a prompt is a request to attach that file, the way
//! `--file src/lib.rs` does for a headless run. The token is expanded into a
//! `--- path ---` block before the turn is submitted, so the model reads the
//! file instead of spending a tool call finding it.
//!
//! The same token drives completion: [`mention_at`] reports the partial path
//! under the cursor, and [`complete`] narrows the project's file list for the
//! palette.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Largest file that will be attached. A binary or a giant log would eat the
/// whole context; the model can still read it with the Read tool.
pub const MAX_FILE_BYTES: usize = 512 * 1024;

/// How many completion candidates the palette may hold.
pub const MAX_COMPLETIONS: usize = 200;

/// The `@` token the cursor is inside, if any.
///
/// Returns the byte offset of the `@` and the text typed after it. A token
/// starts at whitespace or the beginning of the buffer, so `user@host` — an
/// email address — is not a mention.
#[must_use]
pub fn mention_at(text: &str, cursor: usize) -> Option<(usize, &str)> {
    let before = &text[..cursor.min(text.len())];
    // Walk back to the whitespace that opens the token.
    let start = before.rfind(char::is_whitespace).map_or(0, |index| {
        index + before[index..].chars().next().map_or(1, char::len_utf8)
    });
    let token = &before[start..];
    let rest = token.strip_prefix('@')?;
    // An already-closed mention — `@src/lib.rs ` — has a space after it, which
    // the walk-back already crossed; a newline or a second `@` means the user
    // moved on to something else.
    Some((start, rest))
}

/// Every `@path` in `text`, in order, with the byte range it occupies.
#[must_use]
pub fn mentions(text: &str) -> Vec<(String, std::ops::Range<usize>)> {
    let mut found = Vec::new();
    let bytes = text.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        let at_token_start = index == 0 || bytes[index - 1].is_ascii_whitespace();
        if bytes[index] == b'@' && at_token_start {
            let start = index;
            let mut end = index + 1;
            while end < bytes.len() && !bytes[end].is_ascii_whitespace() {
                end += 1;
            }
            let path = &text[index + 1..end];
            if !path.is_empty() {
                found.push((path.to_string(), start..end));
            }
            index = end;
        } else {
            index += 1;
        }
    }
    found
}

/// What expanding a prompt's `@path` mentions produced.
#[derive(Debug, Default)]
pub struct Expansion {
    /// The prompt with text files inlined. An image mention becomes a
    /// `[image: path]` marker instead, since its bytes travel separately.
    pub text: String,
    /// Text files that were attached.
    pub attached: Vec<PathBuf>,
    /// Images that were attached, ready for the turn.
    pub images: Vec<codape_core::types::ImageBlock>,
    /// Mentions that could not be attached, each with the reason.
    pub skipped: Vec<String>,
}

impl Expansion {
    /// Whether anything was attached or refused.
    #[must_use]
    pub const fn changed_anything(&self) -> bool {
        !self.attached.is_empty() || !self.images.is_empty() || !self.skipped.is_empty()
    }
}

/// Expand every `@path` in `text` into a labelled file block, or into an image
/// when the path names one.
///
/// A path that does not exist, is not a regular file, is too large, or names an
/// image format the model cannot take is left in the text untouched and
/// reported in `skipped`, so the model still sees what the person meant and the
/// person is told why it was not attached.
#[must_use]
pub fn expand(text: &str, cwd: &Path) -> Expansion {
    let entries = mentions(text);
    if entries.is_empty() {
        return Expansion {
            text: text.to_string(),
            ..Expansion::default()
        };
    }

    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    let mut attached = Vec::new();
    let mut images = Vec::new();
    let mut skipped = Vec::new();
    for (token, range) in entries {
        out.push_str(&text[cursor..range.start]);
        let path = resolve(&token, cwd);
        // An image is read as bytes, never as text: `read_to_string` would
        // fail on it, and the model needs the picture, not a filename.
        if crate::image_input::is_image_path(&path) {
            match crate::image_input::load_image(&path) {
                Ok(image) => {
                    let _ = write!(out, "[image: {}]", path.display());
                    images.push(image);
                }
                Err(error) => {
                    skipped.push(error);
                    out.push_str(&text[range.clone()]);
                }
            }
            cursor = range.end;
            continue;
        }
        match std::fs::read_to_string(&path) {
            Ok(content) if content.len() <= MAX_FILE_BYTES => {
                let _ = write!(out, "--- {} ---\n{content}", path.display());
                attached.push(path);
                // Keep the line from running into whatever follows.
                out.push('\n');
            }
            Ok(_) => {
                skipped.push(format!(
                    "{} (larger than {MAX_FILE_BYTES} bytes)",
                    path.display()
                ));
                out.push_str(&text[range.clone()]);
            }
            Err(error) => {
                skipped.push(format!("{} ({error})", path.display()));
                out.push_str(&text[range.clone()]);
            }
        }
        cursor = range.end;
    }
    out.push_str(&text[cursor..]);
    Expansion {
        text: out,
        attached,
        images,
        skipped,
    }
}

/// Completion candidates for a partial mention.
///
/// `files` is the project's file list as relative paths. Matching is a
/// case-insensitive substring on the relative path, sorted so a prefix match
/// on the file name comes first: typing `@main` should offer `src/main.rs`
/// before `src/domain/maintenance.rs`.
#[must_use]
pub fn complete(prefix: &str, files: &[String]) -> Vec<String> {
    let needle = prefix.to_lowercase();
    let mut matches: Vec<(u8, usize, &String)> = files
        .iter()
        .filter_map(|file| {
            let lower = file.to_lowercase();
            if !lower.contains(&needle) {
                return None;
            }
            let name = lower.rsplit('/').next().unwrap_or(&lower);
            let rank = if name.starts_with(&needle) {
                0
            } else if lower.starts_with(&needle) {
                1
            } else {
                2
            };
            Some((rank, file.len(), file))
        })
        .collect();
    matches.sort();
    matches
        .into_iter()
        .take(MAX_COMPLETIONS)
        .map(|(_, _, file)| file.clone())
        .collect()
}

/// Every file under `cwd`, relative to it, for mention completion.
///
/// `.gitignore` is honoured, so a mention list does not offer build output or
/// a vendored tree. The list is bounded so a mono-repo does not spend the
/// whole turn's memory on paths nobody will type.
///
/// Separators are always `/`. A mention is typed once and has to mean the same
/// thing on both platforms, so Windows' `src\lib.rs` is offered as
/// `src/lib.rs` — the form the query is written in, and the form `resolve`
/// accepts on either.
#[must_use]
pub fn project_files(cwd: &Path) -> Vec<String> {
    let Ok(files) = codape_core::collect_files(cwd) else {
        return Vec::new();
    };
    files
        .into_iter()
        .filter_map(|path| {
            path.strip_prefix(cwd)
                .ok()
                .map(|relative| relative.to_string_lossy().replace('\\', "/"))
        })
        .take(MAX_PROJECT_FILES)
        .collect()
}

/// How many files the completion list is built from.
pub const MAX_PROJECT_FILES: usize = 20_000;

/// Resolve a mention against the session's working directory.
///
/// `~` and `~/…` are expanded, because a path outside the project is the one
/// case where someone types the home shorthand.
fn resolve(token: &str, cwd: &Path) -> PathBuf {
    if token == "~" {
        if let Some(home) = home::home_dir() {
            return home;
        }
    } else if let Some(rest) = token.strip_prefix("~/")
        && let Some(home) = home::home_dir()
    {
        return home.join(rest);
    }
    let path = Path::new(token);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_at_the_cursor_is_found() {
        let text = "look at @src/li";
        assert_eq!(mention_at(text, text.len()), Some((8, "src/li")));
    }

    #[test]
    fn an_email_is_not_a_mention() {
        let text = "mail me at john@example.com";
        assert_eq!(mention_at(text, text.len()), None);
    }

    #[test]
    fn a_closed_mention_is_not_under_the_cursor() {
        let text = "@src/lib.rs now fix it";
        assert_eq!(mention_at(text, text.len()), None);
        // Putting the cursor back inside it finds it again.
        assert_eq!(mention_at(text, 5), Some((0, "src/")));
    }

    #[test]
    fn all_mentions_are_listed_with_ranges() {
        let text = "@a.txt and @b/c.txt";
        let found = mentions(text);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].0, "a.txt");
        assert_eq!(&text[found[0].1.clone()], "@a.txt");
        assert_eq!(found[1].0, "b/c.txt");
    }

    #[test]
    fn expansion_inlines_the_file_and_keeps_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("note.txt"), "hello").unwrap();

        let expansion = expand("read @note.txt please", tmp.path());
        let expanded = &expansion.text;
        assert!(expanded.starts_with("read --- "), "{expanded}");
        assert!(expanded.contains("hello"), "{expanded}");
        assert!(expanded.ends_with(" please"), "{expanded}");
        assert_eq!(expansion.attached.len(), 1);
        assert!(expansion.skipped.is_empty());
    }

    #[test]
    fn a_missing_file_is_reported_and_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let expansion = expand("read @nope.txt", tmp.path());
        assert_eq!(expansion.text, "read @nope.txt");
        assert!(expansion.attached.is_empty());
        assert_eq!(expansion.skipped.len(), 1);
        assert!(
            expansion.skipped[0].contains("nope.txt"),
            "{:?}",
            expansion.skipped
        );
    }

    /// A picture is attached as an image block, not inlined as text — and the
    /// line it came from is not silently rewritten into something the model
    /// would read as prose.
    #[test]
    fn an_image_mention_becomes_an_image_block() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("shot.png"),
            [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a],
        )
        .unwrap();

        let expansion = expand("what is wrong with @shot.png ?", tmp.path());
        assert_eq!(expansion.images.len(), 1);
        assert_eq!(expansion.images[0].media_type, "image/png");
        assert!(expansion.attached.is_empty());
        assert!(expansion.skipped.is_empty());
        assert!(expansion.text.contains("[image:"), "{}", expansion.text);
        assert!(!expansion.text.contains("PNG"), "base64 leaked into text");
    }

    #[test]
    fn completion_ranks_a_file_name_prefix_first() {
        let files = vec![
            "src/domain/maintenance.rs".to_string(),
            "src/main.rs".to_string(),
            "docs/maintainers.md".to_string(),
            "docs/readme.md".to_string(),
        ];
        let matches = complete("main", &files);
        // `main.rs`'s own name starts with the prefix, so it comes first;
        // "maintainers" matches only mid-name.
        assert_eq!(matches[0], "src/main.rs");
        assert!(!matches.contains(&"docs/readme.md".to_string()));
        assert!(matches.contains(&"docs/maintainers.md".to_string()));
        assert!(matches.contains(&"src/domain/maintenance.rs".to_string()));
    }

    #[test]
    fn completion_is_bounded() {
        let files: Vec<String> = (0..(MAX_COMPLETIONS * 2))
            .map(|index| format!("file{index}.rs"))
            .collect();
        assert_eq!(complete("file", &files).len(), MAX_COMPLETIONS);
    }
}
