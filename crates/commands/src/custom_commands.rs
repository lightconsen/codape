//! Custom slash commands defined in Markdown.
//!
//! A project or a user can add a command by dropping a Markdown file in
//! `.codape/commands/` (or the config home's `commands/` directory):
//!
//! ```markdown
//! ---
//! description: Review the staged diff as a security reviewer
//! argument-hint: <path>
//! ---
//! Review the staged diff. Pay particular attention to $ARGUMENTS.
//! ```
//!
//! The file name is the command name (`review.md` is `/review`); the
//! frontmatter's `description` is what `/help` prints, and the body is the
//! prompt the model receives. `$ARGUMENTS` is replaced with everything the
//! user typed after the command, and `$1`…`$9` with the individual words.
//!
//! Why this lives in `codape-commands` rather than the CLI: the loader has to
//! produce a [`SlashCommand`] whose handler runs like any built-in, and the
//! CLI executes a registry — it does not assemble one. A command discovered at
//! startup is therefore the same kind of object as one compiled in, which is
//! what makes `/help`, the palette, and the macro expander treat it identically
//! without special cases.
//!
//! Loading is deliberately strict about nothing and forgiving about everything
//! else: a file that is unreadable or has unusable frontmatter is skipped with
//! a warning naming it, because a typo in one command must not take the session
//! down at startup.

use std::path::{Path, PathBuf};

use compact_str::CompactString;

use crate::registry::{CommandHandler, CommandRegistry, CommandResult, SlashCommand};

/// A command parsed from a Markdown file, before it becomes a [`SlashCommand`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownCommand {
    /// The command name, taken from the file stem.
    pub name: String,
    /// One-line description shown by `/help` and the palette.
    pub description: String,
    /// The prompt body with `$ARGUMENTS`/`$1`…`$9` left unexpanded.
    pub body: String,
    /// The file it came from, for diagnostics.
    pub path: PathBuf,
    /// Hint shown for the expected arguments, if the file declares one.
    pub argument_hint: Option<String>,
}

/// Parse a Markdown command file.
///
/// Returns `None` only when the file has no usable name — a file stem that is
/// empty or not UTF-8. Frontmatter is best-effort: an unreadable `description`
/// line is ignored rather than failing the file, and a missing description gets
/// a generated one. An empty body is *not* a reason to reject either: a command
/// whose entire effect is its description is odd but harmless, and rejecting it
/// would make the file silently do nothing.
#[must_use]
pub fn parse_markdown_command(path: &Path, content: &str) -> Option<MarkdownCommand> {
    let name = path.file_stem()?.to_str()?.trim().to_string();
    if name.is_empty() {
        return None;
    }

    let (frontmatter, body) = split_frontmatter(content);
    let mut description = String::new();
    let mut argument_hint = None;

    if let Some(frontmatter) = frontmatter {
        for line in frontmatter.lines() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let value = value
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .to_string();
            match key.trim() {
                "description" => description = value,
                "argument-hint" | "argument_hint" => argument_hint = Some(value),
                _ => {}
            }
        }
    }

    // A command with no description still shows up; the file name is what the
    // palette would show anyway. Say what it is rather than leaving the column
    // blank, so `/help` never prints an empty line.
    if description.is_empty() {
        description = format!("Custom command from {}", path.display());
    }

    Some(MarkdownCommand {
        name,
        description,
        body: body.trim().to_string(),
        path: path.to_path_buf(),
        argument_hint,
    })
}

/// Split a Markdown file into its YAML frontmatter and its body.
///
/// Returns `(None, content)` when the file does not start with a `---` block
/// that a later `---` closes — such a file is a plain prompt with no metadata,
/// not an error.
fn split_frontmatter(content: &str) -> (Option<&str>, &str) {
    let Some(rest) = content.strip_prefix("---") else {
        return (None, content);
    };
    // The opening fence must end its line, or `---foo` would be read as one.
    let Some(rest) = rest
        .strip_prefix("\r\n")
        .or_else(|| rest.strip_prefix('\n'))
    else {
        return (None, content);
    };
    // Find a closing fence: a line that is exactly `---`.
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            let frontmatter = &rest[..offset];
            let body = &rest[offset + line.len()..];
            return (Some(frontmatter), body);
        }
        offset += line.len();
    }
    (None, content)
}

/// Expand `$ARGUMENTS` and `$1`…`$9` in a command body.
///
/// `$ARGUMENTS` is the whole argument string as typed, so a command that wants
/// the raw tail gets it with its spacing intact. The positional forms come from
/// splitting on whitespace. An argument that was not supplied expands to
/// nothing, and a `$` that names neither is left alone — a body talking about
/// shell variables should not be rewritten.
#[must_use]
pub fn expand_arguments(body: &str, arguments: &str) -> String {
    if !body.contains('$') {
        return body.to_string();
    }
    let words: Vec<&str> = arguments.split_whitespace().collect();

    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(index) = rest.find('$') {
        out.push_str(&rest[..index]);
        let after = &rest[index + 1..];
        if let Some(tail) = after.strip_prefix("ARGUMENTS") {
            out.push_str(arguments);
            rest = tail;
            continue;
        }
        if let Some(digit @ '1'..='9') = after.chars().next() {
            let position = digit as usize - '0' as usize;
            if let Some(word) = words.get(position - 1) {
                out.push_str(word);
            }
            rest = &after[digit.len_utf8()..];
        } else {
            // Not a placeholder we know: keep the `$` and move past it so the
            // same character cannot be reconsidered forever.
            out.push('$');
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// Discover Markdown commands under `dir` and register them.
///
/// Returns the names registered. A file that cannot be read or parsed is
/// skipped with a warning; the rest still load.
pub fn register_markdown_commands(dir: &Path, registry: &mut CommandRegistry) -> Vec<String> {
    let mut registered = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return registered;
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
        })
        .collect();
    // Directory order is whatever the filesystem returns; sort so `/help` lists
    // them the same way on every machine.
    paths.sort();

    for path in paths {
        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "skipping unreadable custom command");
                continue;
            }
        };
        let Some(command) = parse_markdown_command(&path, &content) else {
            tracing::warn!(path = %path.display(), "skipping custom command with no usable name or frontmatter");
            continue;
        };
        if registry.find(&command.name).is_some() {
            // A file must not shadow a built-in: `/compact` that did something
            // else would be a trap, and the person would have no way to tell
            // which one ran.
            tracing::warn!(
                name = %command.name,
                path = %command.path.display(),
                "custom command shadowed by a built-in of the same name; keeping the built-in"
            );
            continue;
        }
        registered.push(command.name.clone());
        registry.register(markdown_command_to_slash(command));
    }
    registered
}

/// Turn a parsed Markdown command into a registry entry.
///
/// The handler returns the expanded body as `extra_context` rather than
/// printing it. That is what makes it a prompt command: the REPL folds extra
/// context into the next request as a user message, so `/review` reaches the
/// model with the arguments already substituted and no pseudo-instruction text
/// shown to the person.
fn markdown_command_to_slash(command: MarkdownCommand) -> SlashCommand {
    let name = CompactString::from(command.name);
    let description = CompactString::from(command.description);
    let body = command.body;
    let argument_hint = command.argument_hint;
    let label = name.clone();
    let handler: CommandHandler = std::sync::Arc::new(move |_ctx, args| {
        let arguments = args.join(" ");
        if args.is_empty()
            && let Some(hint) = &argument_hint
        {
            return CommandResult::error(format!("This command expects arguments: {hint}"));
        }
        let expanded = expand_arguments(&body, &arguments);
        // One short line naming what ran, not the prompt itself: the body can
        // be pages long, and echoing it would push the answer off the screen
        // before the model had even been asked.
        CommandResult::success(format!("→ /{label}")).with_submit_prompt(expanded)
    });
    SlashCommand::prompt(name, description, true, handler)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(name: &str, content: &str) -> Option<MarkdownCommand> {
        parse_markdown_command(Path::new(&format!("/tmp/{name}.md")), content)
    }

    #[test]
    fn a_frontmatter_block_supplies_the_description_and_the_body_is_the_prompt() {
        let command = parse(
            "review",
            "---\ndescription: Review the staged diff\n---\nReview the staged diff.\n",
        )
        .expect("a well-formed command parses");
        assert_eq!(command.name, "review");
        assert_eq!(command.description, "Review the staged diff");
        assert_eq!(command.body, "Review the staged diff.");
    }

    #[test]
    fn a_file_without_frontmatter_is_still_a_command() {
        let command = parse("hello", "Say hello to the user.\n").expect("parses");
        assert_eq!(command.body, "Say hello to the user.");
        assert!(
            command.description.contains("hello.md"),
            "no description should still name the file: {}",
            command.description
        );
    }

    #[test]
    fn the_body_keeps_a_leading_horizontal_rule_and_text_after_it() {
        // The body's own `---` must not be read as a second frontmatter fence.
        let command = parse(
            "report",
            "---\ndescription: Report\n---\nIntro\n---\nTail\n",
        )
        .expect("parses");
        assert!(command.body.contains("Tail"), "{}", command.body);
        assert_eq!(command.description, "Report");
    }

    #[test]
    fn arguments_expand_whole_and_positionally() {
        assert_eq!(
            expand_arguments("Review $ARGUMENTS for $1 and $2", "src/lib.rs tests"),
            "Review src/lib.rs tests for src/lib.rs and tests"
        );
    }

    #[test]
    fn a_missing_positional_expands_to_nothing() {
        assert_eq!(expand_arguments("[$2]", "only"), "[]");
    }

    #[test]
    fn a_dollar_that_names_nothing_is_left_alone() {
        // A body explaining a shell variable should not be rewritten.
        assert_eq!(expand_arguments("run $HOME/bin", "/tmp"), "run $HOME/bin");
    }

    #[test]
    fn a_body_without_arguments_is_returned_unchanged() {
        assert_eq!(
            expand_arguments("no placeholders", "ignored"),
            "no placeholders"
        );
    }

    #[test]
    fn registering_a_directory_adds_one_entry_per_markdown_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            dir.path().join("review.md"),
            "---\ndescription: Review\n---\nReview this.\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("notes.txt"), "not a command").unwrap();
        let mut registry = CommandRegistry::new();
        let names = register_markdown_commands(dir.path(), &mut registry);
        assert_eq!(names, vec!["review".to_string()]);
        let command = registry.find("review").expect("registered");
        assert_eq!(command.description, "Review");
    }

    #[test]
    fn a_custom_command_does_not_shadow_a_built_in() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            dir.path().join("compact.md"),
            "---\ndescription: Not the real one\n---\nDo something else.\n",
        )
        .unwrap();
        let mut registry = CommandRegistry::new();
        crate::registry::register_all_commands(&mut registry);
        let before = registry
            .find("compact")
            .expect("built-in")
            .description
            .clone();
        let names = register_markdown_commands(dir.path(), &mut registry);
        assert!(names.is_empty(), "the built-in must win: {names:?}");
        assert_eq!(
            registry.find("compact").expect("still present").description,
            before
        );
    }

    #[test]
    fn a_registered_command_expands_its_arguments_into_a_submitted_prompt() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            dir.path().join("say.md"),
            "---\ndescription: Say it\n---\nSay $ARGUMENTS loudly.\n",
        )
        .unwrap();
        let mut registry = CommandRegistry::new();
        register_markdown_commands(dir.path(), &mut registry);

        let ctx = crate::registry::CommandContext::new(
            "/tmp".into(),
            "s1".into(),
            "test".into(),
            codape_core::types::ModelId::sonnet(),
        );
        let result = registry.execute("say", &ctx, &["hello".to_string()]);
        assert_eq!(
            result.submit_prompt.as_deref(),
            Some("Say hello loudly."),
            "the body with arguments substituted is the turn"
        );
        assert!(result.extra_context.is_none(), "it is a turn, not context");
        assert!(
            result.output.contains("/say"),
            "the banner names the command: {}",
            result.output
        );
    }

    #[test]
    fn a_command_with_a_missing_required_argument_says_how_to_call_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            dir.path().join("pick.md"),
            "---\ndescription: Pick one\nargument-hint: <name>\n---\nPick $1.\n",
        )
        .unwrap();
        let mut registry = CommandRegistry::new();
        register_markdown_commands(dir.path(), &mut registry);
        let ctx = crate::registry::CommandContext::new(
            "/tmp".into(),
            "s1".into(),
            "test".into(),
            codape_core::types::ModelId::sonnet(),
        );
        let result = registry.execute("pick", &ctx, &[]);
        assert!(
            result.submit_prompt.is_none(),
            "nothing should be submitted"
        );
        assert!(
            result.output.contains("<name>"),
            "the hint is the error: {}",
            result.output
        );
    }
}
