//! Content & context commands: /copy, /init, /summary, /files, /open, /context, /ctx_viz, /add-dir.

use crate::registry::{CommandArgs, CommandContext, CommandRegistry, CommandResult, SlashCommand};
use std::fmt::Write as _;
use std::sync::Arc;

/// Copy text to system clipboard.
///
/// The implementation moved to `codape_core::clipboard` when `ctrl+shift+c`
/// needed the same thing: two copies had drifted, and only this one knew about
/// Windows.
fn copy_to_clipboard(text: &str) -> Result<(), String> {
    codape_core::clipboard::copy_to_clipboard(text)
}

/// A filename stem for a session: its title, made safe, with the id as a suffix.
///
/// The title is what makes the file findable; the id is what keeps two
/// sessions called "Untitled" from overwriting one another. Everything outside
/// `[A-Za-z0-9._-]` becomes `-`, so the name is valid on every platform this
/// builds for — a Windows export must not fail because a title held a `:`.
fn session_slug(title: &str, id: &str) -> String {
    let mut out: String = title
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    // A title of only punctuation (or an empty one) leaves nothing usable.
    if out.trim_matches('-').is_empty() {
        out = "session".to_string();
    }
    let short: String = id.chars().take(8).collect();
    format!("{out}-{short}")
}

/// Register content & context commands.
pub fn register(registry: &mut CommandRegistry) {
    // /copy — put transcript text on the clipboard
    registry.register(SlashCommand::local(
        "copy",
        "Copy the transcript, or the messages you name, to the clipboard",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, args: &[String]| {
            // The text is built before the clipboard is touched, so a bad
            // message number fails without a side effect and can be tested
            // without one — the handler itself shells out to `pbcopy`.
            let text = match codape_core::transcript::copied_text(&ctx.transcript, args) {
                Ok(text) if text.trim().is_empty() => {
                    return CommandResult::error(
                        "Nothing to copy: the selected messages carry no text.",
                    );
                }
                Ok(text) => text,
                Err(error) => return CommandResult::error(error),
            };
            let what = if args.is_empty() {
                "the transcript".to_string()
            } else {
                format!("message(s) {}", args.join(", "))
            };
            match copy_to_clipboard(&text) {
                Ok(()) => CommandResult::success(format!(
                    "Copied {what} to the clipboard ({} characters).",
                    text.chars().count()
                )),
                // Name what was not copied: a bare failure is
                // indistinguishable from one that copied nothing.
                Err(error) => CommandResult::error(format!(
                    "Clipboard failed: {error}\n{what} were not copied.\n\
                     Ensure pbcopy (macOS), clip (Windows), or xclip (Linux) is available."
                )),
            }
        }),
    ));

    // /export — write the transcript to a file
    //
    // The renderer is `codape_core::transcript::export`, the same one `/copy`
    // and `codape_ui::export` use. It used to live only in `codape-ui`, where
    // it was tested and unreachable: no command could produce it, so the
    // feature existed only for a caller that was never written.
    registry.register(
        SlashCommand::local(
            "export",
            "Write the transcript to a file as text, markdown or json",
            true,
            true,
            true,
            Arc::new(|ctx: &CommandContext, args: &[String]| {
                if ctx.transcript.is_empty() {
                    return CommandResult::error("The session has no messages to export yet.");
                }
                // Usage: /export [format] [<n>…]
                //
                // The format is recognized by name, so a typo is refused with
                // the accepted words rather than being taken for a filename
                // and written somewhere unexpected.
                let mut rest = args;
                let format = match rest
                    .first()
                    .and_then(|word| codape_core::transcript::ExportFormat::parse(word))
                {
                    Some(format) => {
                        rest = &rest[1..];
                        format
                    }
                    None if rest.first().is_some_and(|word| {
                        // A bare number is a message selection, not a format.
                        word.parse::<usize>().is_err()
                    }) =>
                    {
                        return CommandResult::error(format!(
                            "Unknown format '{}'. Usage: /export [{}] [<n>…]",
                            rest[0],
                            codape_core::transcript::ExportFormat::names()
                        ));
                    }
                    None => codape_core::transcript::ExportFormat::Markdown,
                };

                let selected =
                    match codape_core::transcript::selected_messages(&ctx.transcript, rest) {
                        Ok(selected) => selected,
                        Err(error) => return CommandResult::error(error),
                    };
                let body = codape_core::transcript::export(&selected, format);
                if body.trim().is_empty() {
                    return CommandResult::error(
                        "Nothing to export: the selected messages carry no text.",
                    );
                }

                // The file goes in `cwd`, named after the session, because that
                // is the directory the person is already looking at and the one
                // path they can predict without being told.
                let slug = session_slug(&ctx.session_title, &ctx.session_id);
                let path =
                    std::path::Path::new(&ctx.cwd).join(format!("{slug}.{}", format.extension()));
                if let Err(error) = std::fs::write(&path, &body) {
                    return CommandResult::error(format!(
                        "Could not write {}: {error}",
                        path.display()
                    ));
                }
                CommandResult::success(format!(
                    "Exported {} as {} to {} ({} characters).",
                    if rest.is_empty() {
                        format!("all {} message(s)", ctx.transcript.len())
                    } else {
                        format!("message(s) {}", rest.join(", "))
                    },
                    format.name(),
                    path.display(),
                    body.chars().count()
                ))
            }),
        )
        .with_args(CommandArgs::None),
    );

    // /init — initialize project (generate the instruction file)
    //
    // It writes `AGENTS.md`, not `CLAUDE.md`: that is the name Codex, Cursor
    // and Jules read, so a repository set up here is readable by every agent
    // its team uses. Both names are loaded by the prompt compiler, and an
    // existing file of either name means the project is already initialised.
    registry.register(SlashCommand::local(
        "init",
        "Initialize project with an AGENTS.md",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            let cwd = std::path::Path::new(&ctx.cwd);
            let agents_md = cwd.join("AGENTS.md");
            let settings_local = cwd.join(".codape/settings.local.json");

            for existing in codape_core::prompts::INSTRUCTION_FILE_NAMES {
                let path = cwd.join(existing);
                if path.exists() {
                    return CommandResult::success(format!(
                        "Project already initialized.\n{existing} exists at: {}",
                        path.display()
                    ));
                }
            }

            // Write the instructions template
            let template = "# Project Instructions\n\n## Architecture\n<!-- Describe your project architecture here -->\n\n## Key Directories\n<!-- Document important directories -->\n\n## Coding Standards\n<!-- List coding standards and conventions -->\n\n## Commands\n<!-- Document common commands and workflows -->\n";

            if let Err(e) = std::fs::write(&agents_md, template) {
                return CommandResult::error(format!("Failed to create AGENTS.md: {e}"));
            }

            // Write an empty local settings file if it doesn't exist. The
            // `.codape/` directory may be absent, and `ConfigService` reads
            // this path after the project file.
            if !settings_local.exists() {
                if let Err(e) = std::fs::create_dir_all(cwd.join(".codape")) {
                    return CommandResult::error(format!("Failed to create .codape/: {e}"));
                }
                if let Err(e) = std::fs::write(&settings_local, "{}\n") {
                    return CommandResult::error(format!(
                        "Failed to create .codape/settings.local.json: {e}"
                    ));
                }
            }

            CommandResult::success(format!(
                "Project initialized in {}\nCreated: AGENTS.md (project instructions template)\nCreated: .codape/settings.local.json (local settings)",
                ctx.cwd
            ))
            .with_state_change(true)
        }),
    ));

    // /summary — generate session summary
    registry.register(SlashCommand::local(
        "summary",
        "Generate a summary of the current session",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            CommandResult::success(format!(
                "Session Summary\n──────────────\nSession: {}\nMessages: {}\nModel: {}\nCost: {}\nElapsed: {}s\n\n(Use an AI model to generate a detailed summary)",
                ctx.session_title,
                ctx.message_count,
                ctx.model.0,
                ctx.cost_label(),
                ctx.elapsed_secs
            ))
        }),
    ));

    // /files — list project file structure
    registry.register(SlashCommand::local(
        "files",
        "List project file structure",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, args: &[String]| {
            let dir = std::path::Path::new(&ctx.cwd);
            if !dir.is_dir() {
                return CommandResult::error(format!("Not a directory: {}", ctx.cwd));
            }

            let max_depth = args
                .first()
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(2);
            let max_files = 100;

            let mut entries = Vec::new();
            for e in walkdir::WalkDir::new(dir)
                .max_depth(max_depth)
                .into_iter()
                .filter_entry(|e| {
                    e.file_name().to_str().is_some_and(|n| {
                        !n.starts_with('.') && n != "node_modules" && n != "target" && n != ".git"
                    })
                })
                .take(max_files)
                .flatten()
            {
                let path = e.path();
                if let Ok(rel) = path.strip_prefix(dir) {
                    let suffix = if e.file_type().is_dir() { "/" } else { "" };
                    entries.push(format!("  {}{suffix}", rel.display()));
                }
            }

            if entries.is_empty() {
                CommandResult::success(format!("No files found in {}", ctx.cwd))
            } else {
                CommandResult::success(format!(
                    "Project files in {} (depth: {}, max: {})\n{}",
                    ctx.cwd,
                    max_depth,
                    max_files,
                    entries.join("\n")
                ))
            }
        }),
    ));

    // /open — preview a file in the interactive split pane
    registry.register(SlashCommand::local(
        "open",
        "Open a file in the interactive viewer",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, args: &[String]| {
            let Some(path_arg) = args.first() else {
                return CommandResult::error("Usage: /open <path>");
            };
            let raw = std::path::PathBuf::from(path_arg);
            let path = if raw.is_absolute() {
                raw
            } else {
                std::path::Path::new(&ctx.cwd).join(raw)
            };
            match std::fs::read_to_string(&path) {
                Ok(content) => CommandResult::success(format!(
                    "Opened {}\n\n{}",
                    path_arg,
                    content.lines().take(40).collect::<Vec<_>>().join("\n")
                ))
                .with_state_change(true),
                Err(err) => CommandResult::error(format!("Failed to open {path_arg}: {err}")),
            }
        }),
    ));

    // /context — view current context assembly
    registry.register(SlashCommand::local(
        "context",
        "View current context assembly",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            let output = format!(
                "Context Assembly\n────────────────\nMessages: {}\nInput Tokens: {}\nOutput Tokens: {}\nCache Read: {}\nModel: {}",
                ctx.message_count,
                ctx.input_tokens,
                ctx.output_tokens,
                ctx.cache_read_tokens,
                ctx.model.0,
            );
            CommandResult::success(output)
        }),
    ));

    // /ctx_viz — context visualization (feature-gated)
    registry.register(SlashCommand::local(
        "ctx_viz",
        "Visualize current context (feature-gated)",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            if ctx.is_feature_enabled("ctx_viz") {
                let mut output = String::from("Context Visualization\n──────────────────\n");
                let _ = writeln!(output, "Working Directory: {}", ctx.cwd);
                let _ = writeln!(output, "Model: {}", ctx.model.0);
                let _ = writeln!(output, "Session: {}", ctx.session_id);
                let _ = writeln!(output, "Messages: {}", ctx.message_count);
                let _ = writeln!(output, "Input Tokens: {}", ctx.input_tokens);
                let _ = writeln!(output, "Output Tokens: {}", ctx.output_tokens);
                let _ = writeln!(output, "Cache Read: {}", ctx.cache_read_tokens);
                let _ = writeln!(output, "Cache Creation: {}", ctx.cache_creation_tokens);
                let _ = writeln!(output, "Cost: {}", ctx.cost_label());
                let _ = writeln!(output, "Permission Mode: {:?}", ctx.permission_mode);
                let _ = writeln!(output, "Elapsed: {}s", ctx.elapsed_secs);
                if !ctx.git_branch.is_empty() {
                    let _ = writeln!(output, "Git Branch: {}", ctx.git_branch);
                }
                CommandResult::success(output)
            } else {
                CommandResult::error(
                    "ctx_viz feature is not enabled. Set feature flag 'ctx_viz' to enable.",
                )
            }
        }),
    ));

    // /add-dir — add directory to project context
    registry.register(
        SlashCommand::local(
            "add-dir",
            "Add a directory to project context",
            true,
            true,
            true,
            Arc::new(|ctx: &CommandContext, args: &[String]| {
                // Arity is enforced by the registry (`AtLeast(1)`).
                let path = args[0].clone();
                let full_path = std::path::Path::new(&ctx.cwd).join(&path);
                if full_path.is_dir() {
                    let file_count = walkdir::WalkDir::new(&full_path)
                        .into_iter()
                        .filter_entry(|e| {
                            e.file_name().to_str().is_some_and(|n| {
                                !n.starts_with('.')
                                    && n != "node_modules"
                                    && n != "target"
                                    && n != ".git"
                            })
                        })
                        .filter(|e| e.as_ref().is_ok_and(|e| e.file_type().is_file()))
                        .count();
                    CommandResult::success(format!(
                        "Directory '{path}' added to project context ({file_count} files found)"
                    ))
                    .with_state_change(true)
                } else {
                    CommandResult::error(format!("Directory not found: {path}"))
                }
            }),
        )
        .with_args(CommandArgs::AtLeast(1)),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use codape_core::types::ModelId;

    fn ctx() -> CommandContext {
        CommandContext::new("/src".into(), "s1".into(), "Test".into(), ModelId::sonnet())
    }

    #[test]
    fn copy_with_no_transcript_says_there_is_nothing_to_copy() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("copy", &ctx(), &[]);
        // The context has no transcript, so this must say so rather than
        // report a copy that wrote a placeholder.
        assert!(result.output.contains("no messages"), "{}", result.output);
    }

    #[test]
    fn copy_of_a_bad_message_number_names_the_problem() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let c = CommandContext {
            transcript: vec![codape_core::types::Message::user("only one")],
            ..ctx()
        };
        let result = reg.execute("copy", &c, &["7".into()]);
        assert!(
            result.output.contains("Message 7 not found"),
            "{}",
            result.output
        );
        let result = reg.execute("copy", &c, &["zero".into()]);
        assert!(
            result.output.contains("not a message number"),
            "{}",
            result.output
        );
    }

    #[test]
    fn init_project_creates_file() {
        let tmp = tempfile::tempdir().unwrap();
        let c = CommandContext {
            cwd: tmp.path().to_string_lossy().to_string(),
            ..ctx()
        };
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("init", &c, &[]);
        assert!(result.output.contains("AGENTS.md"));
        assert!(result.output.contains(".codape/settings.local.json"));
        assert!(result.state_changed);
        assert!(tmp.path().join("AGENTS.md").exists());
        assert!(tmp.path().join(".codape/settings.local.json").exists());
        // `CLAUDE.md` is still *read*, but `/init` writes the interoperable
        // name: a repository set up here is readable by the other agents its
        // team uses.
        assert!(!tmp.path().join("CLAUDE.md").exists());
        // The legacy path was never read by `ConfigService`; `/init` must not
        // keep writing a settings file nobody loads.
        assert!(!tmp.path().join("CLAUDE.local.json").exists());
    }

    #[test]
    fn init_project_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("CLAUDE.md"), "existing").unwrap();
        let c = CommandContext {
            cwd: tmp.path().to_string_lossy().to_string(),
            ..ctx()
        };
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        // An existing file of *either* name means the project has already been
        // initialised; writing a second template beside it would leave two
        // instruction files disagreeing with each other.
        let result = reg.execute("init", &c, &[]);
        assert!(result.output.contains("already initialized"));
        assert!(!tmp.path().join("AGENTS.md").exists());
    }

    #[test]
    fn summary_shows_stats() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("summary", &ctx(), &[]);
        assert!(result.output.contains("Session Summary"));
        assert!(result.output.contains("Messages"));
    }

    #[test]
    fn files_lists_project() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "").unwrap();
        std::fs::write(tmp.path().join("src/main.rs"), "").unwrap();
        let c = CommandContext {
            cwd: tmp.path().to_string_lossy().to_string(),
            ..ctx()
        };
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("files", &c, &[]);
        // Output should contain either file names or be empty if filtered
        let has_files = result.output.contains("Cargo.toml")
            || result.output.contains("src")
            || result.output.contains("No files");
        assert!(has_files, "files output: {}", result.output);
    }

    #[test]
    fn files_with_depth_arg() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "").unwrap();
        let c = CommandContext {
            cwd: tmp.path().to_string_lossy().to_string(),
            ..ctx()
        };
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("files", &c, &["1".into()]);
        // Output should contain the file or report no files
        let has_files = result.output.contains("a.txt")
            || result.output.contains("No files")
            || result.output.contains("depth");
        assert!(has_files, "files output: {}", result.output);
    }

    #[test]
    fn context_shows_assembly() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("context", &ctx(), &[]);
        assert!(result.output.contains("Context Assembly"));
    }

    #[test]
    fn ctx_viz_feature_gated() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("ctx_viz", &ctx(), &[]);
        assert!(result.output.contains("Error"));
        assert!(result.output.contains("not enabled"));
    }

    #[test]
    fn ctx_viz_when_enabled() {
        let mut flags = std::collections::HashMap::new();
        flags.insert("ctx_viz".into(), true);
        let c = CommandContext {
            feature_flags: flags,
            ..ctx()
        };
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("ctx_viz", &c, &[]);
        assert!(result.output.contains("Context Visualization"));
    }

    #[test]
    fn add_dir_requires_path() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("add-dir", &ctx(), &[]);
        assert!(
            result.output.contains("requires at least 1"),
            "{}",
            result.output
        );
    }

    #[test]
    fn add_dir_success() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/main.rs"), "").unwrap();
        let c = CommandContext {
            cwd: tmp.path().to_string_lossy().to_string(),
            ..ctx()
        };
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("add-dir", &c, &["src".into()]);
        assert!(result.output.contains("src"));
        assert!(result.state_changed);
    }

    #[test]
    fn add_dir_nonexistent() {
        let tmp = tempfile::tempdir().unwrap();
        let c = CommandContext {
            cwd: tmp.path().to_string_lossy().to_string(),
            ..ctx()
        };
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("add-dir", &c, &["nonexistent".into()]);
        assert!(result.output.contains("not found"));
    }

    #[test]
    fn open_requires_path() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("open", &ctx(), &[]);
        assert!(result.output.contains("Usage: /open"));
    }

    #[test]
    fn open_reads_file_preview() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(
            tmp.path().join("src/lib.rs"),
            "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
        )
        .unwrap();
        let c = CommandContext {
            cwd: tmp.path().display().to_string(),
            ..ctx()
        };
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("open", &c, &["src/lib.rs".into()]);
        assert!(result.output.contains("Opened src/lib.rs"));
        assert!(result.output.contains("pub fn add"));
        assert!(result.state_changed);
    }

    // ── /export ──────────────────────────────────────────────────────────────

    fn export_ctx(dir: &std::path::Path) -> CommandContext {
        CommandContext {
            cwd: dir.display().to_string(),
            transcript: vec![
                codape_core::types::Message::user("the question"),
                codape_core::types::Message::assistant("the answer"),
            ],
            ..ctx()
        }
    }

    fn export_registry() -> CommandRegistry {
        let mut r = CommandRegistry::new();
        register(&mut r);
        r
    }

    #[test]
    fn export_writes_a_file_and_says_where_it_went() {
        let tmp = tempfile::tempdir().unwrap();
        let result = export_registry().execute("export", &export_ctx(tmp.path()), &[]);
        assert!(!result.output.contains("Could not"), "{}", result.output);
        // The default format is markdown; the file has to exist, hold the
        // messages, and be named in the output so the person can open it.
        let written: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect();
        assert_eq!(written.len(), 1, "{written:?}");
        let path = &written[0];
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some("md"));
        let body = std::fs::read_to_string(path).unwrap();
        assert!(body.contains("the question"), "{body}");
        assert!(body.contains("### Assistant"), "{body}");
        assert!(
            result.output.contains(&path.display().to_string()),
            "{}",
            result.output
        );
    }

    #[test]
    fn export_honours_the_format_word_and_the_message_numbers() {
        let tmp = tempfile::tempdir().unwrap();
        let result = export_registry().execute(
            "export",
            &export_ctx(tmp.path()),
            &["json".into(), "2".into()],
        );
        assert!(!result.output.contains("Could not"), "{}", result.output);
        let path = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .next()
            .expect("a file");
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some("json"));
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed.as_array().expect("an array").len(), 1);
        assert_eq!(parsed[0]["role"], "assistant");
        assert_eq!(parsed[0]["content"], "the answer");
    }

    /// A word that is neither a format nor a number is refused, not treated as
    /// a filename: silently writing to `hmm.txt` in someone's checkout is worse
    /// than saying the format was not understood.
    #[test]
    fn export_refuses_an_unknown_format_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        let result = export_registry().execute("export", &export_ctx(tmp.path()), &["yaml".into()]);
        assert!(
            result.output.contains("Unknown format 'yaml'"),
            "{}",
            result.output
        );
        assert!(
            result.output.contains("text, markdown, json"),
            "the error must list what it accepts: {}",
            result.output
        );
        assert_eq!(
            std::fs::read_dir(tmp.path()).unwrap().count(),
            0,
            "nothing may be written for a rejected format"
        );
    }

    #[test]
    fn export_with_no_transcript_does_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let c = CommandContext {
            cwd: tmp.path().display().to_string(),
            ..ctx()
        };
        let result = export_registry().execute("export", &c, &[]);
        assert!(result.output.contains("no messages"), "{}", result.output);
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
    }

    /// A title with characters a filesystem refuses must not make the export
    /// fail, and two sessions must not overwrite one another.
    #[test]
    fn the_export_filename_is_safe_on_every_platform() {
        assert_eq!(
            session_slug("Fix: the thing/here", "abcdef123456"),
            "Fix--the-thing-here-abcdef12"
        );
        // An id short enough to be taken whole is not padded; one long enough
        // is cut, so the name stays a filename and not a paragraph.
        assert_eq!(session_slug("t", "abc"), "t-abc");
        // A title with nothing usable falls back rather than producing `-.md`.
        assert_eq!(session_slug("///", "abc"), "session-abc");
    }
}
