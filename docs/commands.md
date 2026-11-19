# commands Module

## Goal

Implements the `/` slash command system that users type in the REPL. Provides a registry with type-safe dispatch across 80 commands covering sessions, configuration, diagnostics, Git integration, context management, macros, history, and advanced features.

## Design

### Key Types

- **`CommandRegistry`** (`registry.rs`) — Vec-based registry mapping command names to handler closures. Supports `find`, `list`, `execute`, and availability filtering. Validates argument count before dispatch via `CommandArgs`.
- **`CommandContext`** (`registry.rs`) — Per-invocation context: cwd, model, session state, token usage, cost, feature flags, git branch, theme, vim mode, env vars, and `pipe_input: Option<String>` for command chaining.
- **`CommandHandler`** (`registry.rs`) — `Arc<dyn Fn(&CommandContext, &[String]) -> CommandResult + Send + Sync>`.
- **`SlashCommand`** (`registry.rs`) — Metadata: name, description, kind (`Local`/`LocalUi`/`Prompt`/`Standalone`), `supports_non_interactive`, `remote_safe`, `bridge_safe`, `uses_agent_backend`, `destructive: bool`, `args: CommandArgs`, handler.
- **`CommandResult`** (`registry.rs`) — Output, `state_changed`, `suppress_output`, `extra_context`, `submit_prompt`, `append_system_prompt`, `override_system_prompt`, `prompt_profile`, `allowed_tools_override`, `scrollable: bool`, `deferred: Option<CommandDeferred>`.
- **`CommandDeferred`** (`registry.rs`) — Work a synchronous handler hands to the session because the crate cannot do it itself: `PluginInstall { source }` is carried out by the REPL (headless: appended to the command's output; TUI: a `tokio::spawn` reporting through the shell-result channel). Same hand-off shape as `pending_setup`.
- **`CommandArgs`** (`registry.rs`) — Argument validation enum: `None` (no validation), `AtLeast(n)`, `AtMost(n)`, `Exactly(n)`. Validated pre-dispatch in `execute()`.
- **`CommandHistory`** (`history.rs`) — Ring buffer (max 1000) with JSONL file persistence (`~/.codape/command_history.jsonl`). Tracks `{cmd, args, timestamp}` per entry. Supports `append`, `list(n)`, `search(query)`.
- **`MacroRegistry`** (`macro_commands.rs`) — User-defined command sequences as `HashMap<String, Vec<String>>` with JSON persistence (`~/.codape/macros.json`). Supports `define`, `delete`, `get`, `list`, `load`.

### Command Inventory (81 commands across 13 registered modules)

| Module | Commands | Purpose |
|--------|----------|---------|
| `session_commands` | `/clear`, `/exit`, `/session` | Session lifecycle |
| `config_commands` | `/config`, `/model`, `/permissions`, `/settings` | Core configuration |
| `stats_commands` | `/stats`, `/cost`, `/usage` | Usage statistics |
| `content_commands` | `/compact`, `/help`, `/version` | Session content & help |
| `session_extension_commands` | `/resume`, `/rename`, `/rewind` | Session persistence & manipulation |
| `config_extension_commands` | `/theme`, `/hooks`, `/mcp`, `/plugin`, `/memory`, `/skills`, `/tasks`, `/plan` | Extended configuration |
| `diagnostic_commands` | `/status`, `/doctor` | System health checks |
| `context_commands` | `/copy`, `/init`, `/summary`, `/files`, `/context`, `/add-dir` | Context management. `/copy [<n>…]` puts the session's text on the clipboard: no argument copies all of it, numbers copy those messages using `/rewind`'s numbering. Reasoning rows are left out |
| `integration_commands` | `/chrome`, `/desktop`, `/mobile`, `/voice`, `/vim`, `/remote-env`, `/remote-setup`, `/statusline-setup` | External integrations |
| `git_commands` | `/commit`, `/commit-push-pr`, `/review`, `/security-review`, `/autofix-pr`, `/branch` | Git & code review |
| `debug_commands` | `/debug-tool-call`, `/heapdump`, `/mock-limits`, `/perf-issue`, `/ant-trace`, `/break-cache`, `/reload-plugins` | Debugging tools |
| `advanced_commands` | `/bridge`, `/bridge-kick`, `/teleport`, `/effort`, `/agents`, `/keybindings`, `/ide`, `/ultraplan`, `/insights`, `/thinkback`, `/thinkback-play`, `/statusline` | Advanced/feature-gated; `/effort`, `/agents`, `/keybindings` and `/ide` are REPL built-ins the registry also lists so `/help` shows them |
| `misc_commands` | `/advisor`, `/btw`, `/good-claude`, `/stickers`, `/bughunter`, `/issue`, `/env`, `/diff`, `/oauth-refresh`, `/backfill-sessions`, `/history` | Miscellaneous. `/history [n]` and `/history search <q>` read the session's recorded command lines. |
| `macro_commands` | `/macro` (REPL built-in) | `MacroRegistry` loads, persists and expands macros. `/macro define <name> <command[; command…]>`, `list`, `run <name>`, `delete <name>` are handled by the REPL; a defined macro also runs by typing `/<name>`. |

### Architecture Patterns

- **Registry pattern** — Commands register at startup via `register_all_commands()` called from `registry.rs`.
- **Closure-based handlers** — Each command is an `Arc<dyn Fn>` with shared `CommandContext`.
- **Availability matrix** — Commands filtered by `supports_non_interactive`, `remote_safe`, `bridge_safe` for different execution contexts.
- **Agent backend flag** — Commands can opt into sub-agent execution via `with_agent_backend()`.
- **Argument validation** — `CommandArgs` enum validated pre-dispatch; `CommandArgs::None` accepts any count, while `AtLeast(n)`, `AtMost(n)`, `Exactly(n)` enforce constraints. Production declarers: `/btw` and `/add-dir` require at least one argument, `/rename` at least one word (titles may contain spaces), and `/model`, `/theme` and `/resume` accept at most one. `/history` keeps form-specific checks because `search` takes a multiword query.
- **Destructive confirmation** — Commands marked `.with_destructive()` trigger a `Confirm` overlay in TUI mode (y/n), skipped in headless mode.
- **Command piping** — Input split on `|` operator; segments executed sequentially with output of each passed as `pipe_input` to the next via `CommandContext`.
- **Macro expansion** — Command names resolved against `MacroRegistry` before registry dispatch; macro sequences expanded and executed in order.
- **Scrollable output** — `CommandResult.scrollable` flag causes TUI to render output as an `Assistant` message (scrollable in message list) rather than a single-line `System` message.
- **Command history** — Every command execution appended to `CommandHistory` ring buffer with file persistence; `/history` command queries and searches.
- **Modular organization** — Commands grouped by domain across 13 registered modules, each with a `register(&mut CommandRegistry)` function.

## Current Status

### What Works

- Full command registry with name-to-handler mapping, argument validation, and availability filtering.
- `/clear`, `/exit`, `/help`, `/version`, `/compact` fully functional. `/clear` requires confirmation in TUI mode.
- `/model` switches active model and persists the choice. It reaches the engine the turn is built from, not just the REPL's own record — it used to print the new name while every later request still carried the model the process started with. Every path (`/model`, `-m`, a settings `model`, an MCP server's model hint) resolves aliases through one table, so the interactive and headless paths cannot disagree; see `docs/core.md` → *Model ids*. `/permissions` manages permission mode.
- `/cost`, `/stats`, `/usage` report token usage and cost breakdown.
- Git commands (`/commit`, `/commit-push-pr`, `/review`, `/branch`) work via git CLI and prompt profiles.
- `/theme` lists and changes themes with available theme detection. With no argument in the TUI it opens the theme list rather than printing it; `/theme <name>` still applies one directly.
- `/rewind [<message-number>]` truncates the engine session to the first `n` messages and restores the files those messages changed. `n` is how many to keep, so the newest message is a no-op rather than an error — the registry path and the REPL path agree on that, and the list's first row is exactly that number. With no argument in the TUI it opens the rewritable points as a list, numbered from the engine's message list — the same numbering `perform_rewind` takes — so a person picks a row instead of reading a count off a usage line and retyping it.
- `/status` and `/doctor` show comprehensive system health (API key, MCP, git, model, feature flags).
- `/memory` reads and searches long-term memory entries.
- `/hooks`, `/mcp`, `/plugin`, `/skills`, `/tasks` list and manage subsystem state. `/hooks` names each installed hook with its event, kind, tier and target — taken from the registry the session built, not from a directory — and `/hooks test <event>` fires that installed executor and reports what happened, which is the one question about hooks a running session can answer honestly: rewriting the file would not reach the executor it already holds, so `/hooks` does not offer to. `/mcp` reports the servers `.mcp.json` configures even when the session is not connected, and `/mcp add <name> (command | --url <url>)`, `/mcp remove <name>` and `/mcp test <name>` write that file, drop a server from it, or connect to one and list its tools — the same operations as `codape mcp add|remove|test`, reachable without leaving the session. A server added this way joins the session's tools on the next start, because the tool registry was built at startup. `/plugin` reads both the project's `.codape/plugins` and the config home; with no argument in the TUI it opens the installed plugins and configured marketplaces as a list, with each row naming the plugin's version, state and path or the marketplace's URL. `/plugin install <source>` installs from a directory, git URL, `github:owner/repo` shorthand or archive. `/plugin components` lists the themes, output styles, workflows, monitors, LSP servers, channels, binaries, types and experimental data contributed by active plugins.
- `/settings [<key> [<value>]]` is the command half of the settings menu. Bare `/settings` in the TUI opens the same menu `alt+c` does; in a headless run it lists the six keys the menu knows with their current values, `/settings <key>` reports one, and a third argument is refused because the registry holds no validator for it — the menu is where a value is written, and it checks the value against the row's own choices. The same read-modify-write `settings.json` `/config set` uses.
- `/keybindings` prints the live keymap: every action, its keystrokes and its context, the files it was read from, and every problem found in them. It re-reads the files before printing, so a person who just edited `keybindings.json` sees the edit without restarting. Bindings live in `keybindings.json` under the config home and `.codape/`; see [ui.md](ui.md) → *Architecture Patterns*.
- `/agents [list|logs <id> [count]|follow <id|all>]` is a REPL built-in: `list` names the agent definitions the session can spawn and the agents it has spawned, `logs` replays one agent's log, and `follow` subscribes to its events. A registry entry mirrors it so `/help` lists it.
- `/ide` prints the same thing `codape ide` prints: the MCP snippet an editor needs to launch `codape ide bridge`, and the state of the daemon that bridge would reach. It is the in-session way to discover the editor integration without leaving the REPL.
- `/style [name]` lists or applies an output style as additional system instructions. `default`, `explanatory` and `learning` ship with the binary; a plugin style of the same name wins.
- `/resume [id|title fragment]` accepts an exact session id; anything else is resolved through the session index, and an ambiguous fragment lists the candidates instead of guessing. With no argument it opens the same picker `alt+s` does (`app:sessionPicker`) — the sessions listed newest first, narrowed by typing, resumed on Enter.
- Feature-gated commands (`/ultraplan`, `/thinkback`, `/thinkback-play`, `/voice`, `/ctx_viz`, `/heapdump`, `/mock-limits`, `/stickers`) check `CommandContext.feature_flags`, which the REPL now fills from the resolved `FeatureFlags` (env + `--betas`).
- `/bridge` and `/bridge-kick` check `CODAPE_BRIDGE_URL` env var.
- `/insights` aggregates project file counts, git info, and session token usage.
- `/ultraplan` activates a session-scoped planning mode: it adds multi-phase planning
  instructions to the prompt and writes nothing to `settings.json` (`ultraplan` is not a
  settings key, so persisting it would make the next load reject the whole file).
- `/thinkback-play` reviews the saved transcripts and hands the session a prompt for the
  next turn — the same `extra_context` + `append_system_prompt` mechanism `/advisor` uses.
  With no transcripts it says so instead of promising a turn.
- `/backfill-sessions` writes the session index for transcripts that predate it. The
  registry reports the request and the REPL performs the write (it owns the workspace
  index), so the output says how many rows were written, not just how many files exist.
- `/voice` needs `voice_mode` and reports whether a transcription command is configured, naming it, because
  that command is what owns the microphone. `alt+m` runs it and puts its last output
  line on the prompt at the cursor; the built-in state machine only tracks
  idle → transcribing → done. It used to answer "Toggled" while nothing listened.
- `/desktop` and `/mobile` report the truth about surfaces and remote clients instead of
  reading `CODAPE_DESKTOP`/`CODAPE_MOBILE` as if they were connections: both name the
  surface actually presenting the session and what it can do, `/desktop` says "active"
  only for a registered `desktop` surface, and `/mobile` names `codape bridge` as the
  path in because there is no mobile host here.
- `/stickers` says it is not implemented — no Codape surface renders image messages.
- `/heapdump` writes a process snapshot (version, OS, pid, RSS/VSZ/threads, open file
  descriptors) to `/tmp/codape-heap.prof` and reports the byte count. It is a process
  snapshot, not a heap profile, and the command says so in its output.
- `/diff [--cached]` shows the working diff. In the TUI it fills the left pane of a split view — coloured per line, titled with the `+N -M` stat — instead of dumping the patch into the transcript where an added line and a context line looked the same; `PageUp`/`PageDown` scroll it. Headless and chained use (`/diff | /commit`) still print the text unchanged. A failure (not a repository, git missing) goes to the transcript rather than an empty pane.
- `/effort [off|low|medium|high|xhigh|max]` reports or sets the session's reasoning effort — the same call `Ctrl+P` makes, so the two paths cannot disagree. The engine holds the value and rebuilds its request context each turn, so it reaches the next request without rebuilding the client; see [cli.md](cli.md) → *Reasoning effort*.
- Bare `/model` opens the model picker (`Ctrl+P`): a model list plus an effort level, `Tab` to switch focus, `←`/`→` to change, `Enter` to apply. Values clamp at the ends rather than wrapping.
- Command chaining via `|` operator (e.g., `/diff | /commit`) passes output between commands.
- Custom Markdown commands (`custom_commands.rs`): a `.md` file under `<cwd>/.codape/commands/` or `<config home>/commands/` defines a command. The file stem is the name, optional frontmatter carries `description` (shown by `/help` and the palette) and `argument-hint`, and the body is the prompt. `$ARGUMENTS` expands to everything after the command, `$1`…`$9` to individual words. They are loaded at startup as ordinary `SlashCommand`s, so `/help`, the palette, chaining and macros treat them like built-ins; an unreadable or malformed file is skipped with a warning naming it rather than failing startup.
- Command history is recorded for built-ins and registry commands alike (both REPL paths record before dispatch) and persisted; `/history` reads it, with `[n]` and `search <query>`.
- `/macro define|list|run|delete` is a REPL built-in backed by `~/.codape/macros.json`; macro names also expand directly.
- `/trust` is a REPL built-in that reports or changes this workspace's trust decision — the one that lets a repository's `.codape/settings.json` bring `hooks`, `permissions.allow` and `mcp_servers`. `/trust` alone reports the standing decision and what the workspace asks for; `/trust yes` and `/trust no` record a new one. Neither reloads the session, because the hooks were installed at startup, and the output says a restart is needed rather than claiming the change took effect. See [config.md](config.md) → *Workspace Trust*.
- All commands have basic error handling with usage messages.

### Feature-Gated Commands

Several commands require feature flags to be enabled in `CommandContext`:
- `/ultraplan` — needs `ultraplan`
- `/thinkback` / `/thinkback-play` — needs `thinkback`
- `/voice` — needs `voice_mode`
- `/ctx_viz` — needs `ctx_viz`
- `/heapdump` — needs `heapdump`
- `/mock-limits` — needs `mock_limits`
- `/stickers` — needs `stickers`

These return an error message when the feature is not enabled. The flag names are accepted by `CODAPE_FEATURE_FLAGS` and `--betas`.

## Remaining Gaps

- **`/macro` is a REPL built-in, not a registry command.** It mutates the session's
  `MacroRegistry` and can therefore run in the TUI and headless paths, but remote/bridge
  sessions (which dispatch through the registry) do not see it.
- **Remote/bridge availability filtering is only used by the server.** The local TUI does not consult the availability matrix.
