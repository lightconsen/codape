//! Session management commands: /clear, /exit, /session.

use crate::registry::{CommandContext, CommandRegistry, CommandResult, SlashCommand};
use std::fmt::Write as _;
use std::sync::Arc;

/// Register session management commands.
pub fn register(registry: &mut CommandRegistry) {
    // /clear — clear the current session messages
    registry.register(
        SlashCommand::local(
            "clear",
            "Clear the current session",
            true,
            true,
            true,
            Arc::new(|ctx: &CommandContext, _args: &[String]| {
                if ctx.message_count == 0 {
                    CommandResult::success(format!(
                        "Session '{}' is already empty (0 messages).",
                        ctx.session_id
                    ))
                } else {
                    CommandResult::success(format!(
                        "Clear requested for session '{}' ({} messages).\nThis command is handled by the REPL for actual message clearing.",
                        ctx.session_id, ctx.message_count
                    ))
                    .with_state_change(true)
                }
            }),
        )
        .with_destructive(),
    );

    // /exit — graceful exit
    registry.register(SlashCommand::local(
        "exit",
        "Exit the session",
        true,
        true,
        true,
        Arc::new(|_ctx: &CommandContext, _args: &[String]| {
            CommandResult::success("Exiting...").with_suppressed()
        }),
    ));

    // /session — show current session info
    registry.register(SlashCommand::local(
        "session",
        "Show current session info",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            let output = format!(
                "Session: {} ({})\nModel: {}\nMessages: {}\nElapsed: {}s\nWorking Dir: {}\nBranch: {}",
                ctx.session_id,
                ctx.session_title,
                ctx.model.0,
                ctx.message_count,
                ctx.elapsed_secs,
                ctx.cwd,
                ctx.git_branch,
            );
            CommandResult::success(output)
        }),
    ));

    // /todos — the task list the model keeps through TodoWrite
    //
    // The panel under the transcript draws the same rows, but it stops at eight
    // and elides the rest; this is the full list, with the descriptions the
    // panel never shows. The list belongs to the conversation, so it is read
    // from a context snapshot rather than from a store this crate cannot
    // reach.
    registry.register(SlashCommand::local(
        "todos",
        "Show the model's todo list",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            if ctx.todos.is_empty() {
                return CommandResult::success(
                    "No todos. The model writes them with TodoWrite while it works; the panel \
                     under the prompt draws the same list.",
                );
            }
            let done = ctx
                .todos
                .iter()
                .filter(|todo| todo.status == crate::registry::TodoStatus::Completed)
                .count();
            let mut output = format!("Todos ({done}/{} done)", ctx.todos.len());
            for todo in &ctx.todos {
                let _ = write!(output, "\n{} {}", todo.status.marker(), todo.subject);
                if !todo.description.is_empty() {
                    let _ = write!(output, "\n    {}", todo.description);
                }
            }
            CommandResult::success(output)
        }),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{TodoRow, TodoStatus};
    use codape_core::types::ModelId;

    fn ctx() -> CommandContext {
        CommandContext {
            session_title: "Test".into(),
            message_count: 10,
            elapsed_secs: 120,
            git_branch: "main".into(),
            ..CommandContext::new("/src".into(), "s1".into(), "Test".into(), ModelId::sonnet())
        }
    }

    #[test]
    fn clear_command() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("clear", &ctx(), &[]);
        assert!(result.output.contains("10 messages") || result.output.contains("empty"));
    }

    #[test]
    fn exit_command() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("exit", &ctx(), &[]);
        assert!(result.output.contains("Exiting"));
        assert!(result.suppress_output);
    }

    #[test]
    fn session_command() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("session", &ctx(), &[]);
        assert!(result.output.contains("s1"));
        assert!(result.output.contains("Test"));
        assert!(result.output.contains("claude-sonnet-5-5"));
        assert!(result.output.contains("10"));
        assert!(result.output.contains("120s"));
        assert!(result.output.contains("main"));
    }

    /// With no list, `/todos` says where the list comes from instead of
    /// printing an empty header.
    #[test]
    fn todos_without_a_list_explains_where_it_comes_from() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("todos", &ctx(), &[]);
        assert!(result.output.contains("No todos"), "{}", result.output);
        assert!(result.output.contains("TodoWrite"), "{}", result.output);
    }

    /// The command prints the whole list, including what the panel drops:
    /// the descriptions and every row past the eighth.
    #[test]
    fn todos_prints_every_row_with_its_description() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let mut c = ctx();
        c.todos = vec![
            TodoRow {
                subject: "Read the registry".into(),
                description: "find where the counts are pinned".into(),
                status: TodoStatus::Completed,
            },
            TodoRow {
                subject: "Add the commands".into(),
                description: String::new(),
                status: TodoStatus::InProgress,
            },
            TodoRow {
                subject: "Write the tests".into(),
                description: String::new(),
                status: TodoStatus::Pending,
            },
        ];
        let result = reg.execute("todos", &c, &[]);
        assert!(
            result.output.starts_with("Todos (1/3 done)"),
            "{}",
            result.output
        );
        assert!(result.output.contains("✓ Read the registry"));
        assert!(result.output.contains("■ Add the commands"));
        assert!(result.output.contains("□ Write the tests"));
        // A description is indented under its own subject.
        assert!(
            result
                .output
                .contains("\n    find where the counts are pinned"),
            "{}",
            result.output
        );
    }
}
