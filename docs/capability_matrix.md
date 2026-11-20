# Codape Capability Matrix

## Legend

| Symbol | Meaning |
|--------|---------|
| ✅ | Implemented |
| 🔵 | Planned |

---

## Core Call Patterns

| Capability | Status | Notes |
|------------|--------|-------|
| Main agent streaming | ✅ | `QueryEngine::submit_streaming` |
| Side query (single-shot) | ✅ | `orchestrator::run_side_query` |
| Lightweight query | ✅ | `orchestrator::run_lightweight_query` |
| Forked agent | ✅ | `orchestrator::run_forked_agent` — used transitively by background features (`run_background_feature` → `run_forked_agent_with_profile` → `run_forked_agent`); Agent-tool delegation uses the manager path |
| Background feature | ✅ | `orchestrator::run_background_feature` |
| Writable flag for forked agents | ✅ | `BackgroundFeatureRequest::writable` |

## Prompt System

| Capability | Status | Notes |
|------------|--------|-------|
| Base prompt compiler | ✅ | `PromptCompiler` |
| Instruction files | ✅ | `INSTRUCTION_FILE_NAMES`: `AGENTS.md` first, `CLAUDE.md` second, from every directory up to the FS root plus the config home (both names, same precedence) — see `docs/ai.md` |
| Prompt profiles | ✅ | `PromptProfile` enum + registry |
| Model profiles | ✅ | `ModelProfile::Main/Lightweight/Background` |
| Mode prompt switching | ✅ | `/advisor`, `/bughunter`, `/reviewer` via commands |
| Feature-specific prompts | ✅ | `crates/core/src/prompts/features/*.rs` |
| Override system prompt | ✅ | `CommandResult::override_system_prompt` |
| Append system prompt | ✅ | `CommandResult::append_system_prompt` |

## AI Features

| Feature | Status | Call Pattern | Notes |
|---------|--------|--------------|-------|
| Session title | ✅ | Lightweight | `prompts::session_title` |
| Permission explanation | ✅ | Side query | `ExplainPermissionHook` |
| Compact summary | ✅ | Background | `/compact`, truncates via `engine.compact_session_messages(3)` |
| Session memory update | ✅ | Background | `/update-memory`, persists to `SessionMemoryStore` |
| Extract memories | ✅ | Background | `/extract-memories`, persists to `LongTermStore` |
| Prompt suggestion | ✅ | Background | Auto-triggered every 5 messages, rendered as ghost text in TUI input |
| Agent summary | ✅ | Background | `/agent-summary` |
| Away summary | ✅ | Background | `/away-summary` |
| Magic docs | ✅ | Background | `/magic-docs` |
| Codebase overview | ✅ | Background | `/codebase-overview` |
| README generation | ✅ | Background | `/generate-readme` |
| Agent generation | ✅ | Side query | `/agent-generation <description>` |

## Tools

| Capability | Status | Notes |
|------------|--------|-------|
| Local tool registry | ✅ | `ToolRegistry` + `RegistryToolDispatcher` |
| Per-tool feature gate | ✅ | `required_feature_flag`, `superseded_by_provider_web_tools` + `set_provider_web_tools` |
| Web search (local) | ✅ | `WebSearchTool` — configurable backend chain |
| Web fetch (local) | ✅ | `WebFetchTool`, gated on `web_browser_tool` |
| Provider-run web search/fetch | ✅ | `server_tools` declares `web_search_20260209` / `web_fetch_20260209`; Anthropic's own endpoint only |
| Provider-run code execution | ❌ | Deliberate. `code_execution_20260120` is a real standalone tool, but it runs in the provider's sandbox, which cannot see the repository being edited, and declaring it beside the `_20260209` web tools opens a second execution environment. Codape's work happens in this working directory, over `Bash` |
| Provider-run blocks round-trip | ✅ | `MessageContent::ServerBlock`, replayed verbatim in order — the check is a `*_tool_result` suffix match, so a code-execution result would round-trip unchanged if one were ever declared |
| `pause_turn` resume | ✅ | `StopDisposition::Resume`, bounded by `MAX_PROVIDER_RESUMES` |

## Surfaces

Codape ships **one** `UiSurface` implementation — the terminal. The abstraction
(`codape-core::surface`) is host-facing, and these rows say what exists here as
opposed to what a host can add.

| Capability | Status | Notes |
|------------|--------|-------|
| Surface abstraction | ✅ | `SurfaceId`, `SurfaceCapabilities`, `SurfaceDescriptor`, `UiSurface`, `SurfaceRegistry` in `codape-core::surface` |
| Terminal surface | ✅ | `codape-ui::surface::TerminalSurface`; the only registered `UiSurface` |
| Picture capability | ✅ | `images` follows the live terminal's `ImageProtocol`, so the descriptor and the transcript renderer cannot disagree |
| Reserved surface ids | ✅ | `KNOWN_SURFACE_IDS`: `terminal`, `ide`, `desktop`, `web` — names a host may register; no implementation here |
| Desktop / mobile hosts | ❌ | No desktop app and no mobile host in this repository. `/desktop` reports "active" only for a registered `desktop` surface; `/mobile` names `codape bridge` as the remote path in |
| Editor surface | ❌ | An editor drives a session through `codape ide bridge` (text tool calls over MCP), not `UiSurface`. See `docs/surfaces.md` → *What is implemented* |
| Mod panel pane | ✅ | A mod's `$.ui.panel(slot, …)` draws into a named slot the host holds, so it stays on screen beside the conversation and updates in place instead of appending a transcript block. `codape_ui::panel::PanelStore` owns the slots; the terminal draws them in their own band between the transcript and the prompt. Four slots at a time; a fifth `Set` is dropped and reported. Not clickable and not focusable — a mod that wants buttons uses a `$.ui.render` block |

### Interactive surfaces in the terminal

Every row is reachable from a key as well as from the slash command named
beside it, and the key is rebindable in `keybindings.json`. The rows themselves
are built in `crates/cli/src/session_lists.rs` and drawn by one
`codape_ui::picker::Picker`.

| Surface | Key | Also | Notes |
|---------|-----|------|-------|
| Model + effort picker | `ctrl+p` (`chat:modelPicker`) | `/model` | Both halves in one dialog: `Tab` moves between the model list and the effort level, `←`/`→` clamp at the ends |
| Effort slider | `alt+e` (`chat:effortSlider`) | `/effort` | The effort level alone, with a six-stop track: `←`/`→` move it, `Enter` applies, `Esc` leaves the session as it was |
| Voice input | `alt+m` (`chat:voiceInput`) | `/voice` | With `voice_mode` on, runs `voice.command` from `settings.json` and puts the last non-empty line of its stdout on the prompt at the cursor. The microphone belongs to that command: no audio stack or STT client is built in. No command configured is a notice naming the setting, not a key that does nothing |
| Agent roster | `ctrl+g` | `/agents` | Lists spawned and defined agents; each has a row to follow it, one for its log, one to pin it to the top of the roster for the session (`/agents pin|unpin <id>` is the idempotent command form), and one each to rename it and put it in a group — agents sharing a group sort together (ungrouped first, then groups by name; pins still outrank groups), and the group is shown beside the name |
| Rewind selector | `alt+r` | `/rewind` | Rows are numbered from the engine session, the same numbering `perform_rewind` takes |
| Session picker | `alt+s` (`app:sessionPicker`) | `/resume` (no argument) | Every session this workspace has a transcript for, newest first, with its title, age and id; typing filters, Enter resumes. It reads the same merged index-plus-transcripts view `/resume <fragment>` searches, and a row takes the same path `/resume <id>` does, including the "a turn is running" guard |
| Theme picker | `ctrl+y` | `/theme` | Installed themes with the current one marked; picking applies it |
| Plugin browser | `alt+p` | `/plugin` | An install row first (it opens a field for a directory, git remote, archive URL or marketplace name and queues the same `PluginInstall` work `/plugin install <source>` does), then three rows per installed plugin — open, switch on/off, pin — then configured marketplaces with their URL. A switched-off plugin keeps its rows |
| Attachment browser | `alt+v` | — | The images waiting on the next turn: a row shows one in the transcript, another takes it off the turn |
| Help page | `alt+h` | `/keybindings` (the diagnostic) | Generated from the binding table, grouped by intent, and it names where the bindings came from |
| Full-screen transcript | `ctrl+t` | — | Layout flag: the message area takes every row and one footer row says how to leave |
| Diff file jump | `alt+up` / `alt+down` | — | Lands on the next/previous `diff --git` header; does not wrap. A patch of several files also lists them above the patch, marked with the one the pane is showing and each file's own `+N -M` |
| Pane tabs | `alt+left` / `alt+right` (`app:panePreviousTab` / `app:paneNextTab`) | — | Every `/diff` and `/open` is a view the pane remembers, with its own scroll. The bar appears only from the second view on, numbers each one (`1:diff 2:src/lib.rs`), cuts the overflow with an ellipsis, and wraps at both ends. Re-running the same command refreshes its tab rather than adding one |
| Transcript selection | mouse drag, `ctrl+shift+c` (`app:copyTranscript`) | `/copy [<n>…]` | Press on the transcript and drag to select; releasing copies the selected text to the clipboard and says how many characters went. A click inside a mod UI button is that button rather than the start of a selection. The selection is drawn by the TUI, so a repaint cannot take it away the way the terminal's own selection can. Without a pointer the same selection is made with the keyboard: `shift+up`/`shift+down` walk a line at a time up and back on the newest transcript row, `shift+home` takes everything on screen and `shift+end` returns to that last line, all as `selection:extendUp`/`extendDown`/`extendAll`/`last`. The highlight is the same one the drag draws, so what a copy takes is always on screen. `ctrl+shift+c` copies the selection when there is one and otherwise just takes the transcript as it is drawn — the message area only, never the prompt, statusline or permission bar. `/copy` is the same text again from the other end: no argument copies the whole session rather than the visible part, numbers copy those messages, and the numbering is `/rewind`'s — message 1 is the first one, so `/copy 3` and `/rewind 3` name the same turn |
| Settings menu | `alt+c` (`app:settings`) | `/settings` | One row per setting this menu knows how to change — model, permission mode, vim mode, theme, prompt caching, telemetry — with the current value beside it. `Enter` steps the row to its next value and leaves the menu up, because adjusting several settings is the point; a row that only takes effect on the next start says `· next session`. Rows write `settings.json` and the live ones apply to the session immediately, so the file and the running session do not disagree |

Settings are still files — `settings.json`, `keybindings.json`, `.codape/` — and
the slash commands remain the full way to write them. The menu is the subset
worth stepping through without remembering a value, not a second place settings
live.

### Writing a prompt

| Surface | Key | Notes |
|---------|-----|-------|
| External editor | `ctrl+x` (`chat:externalEditor`) | Writes the line to a temporary `.md` file, hands the terminal to `visual`/`editor` from `settings.json` or else `$VISUAL`/`$EDITOR`, and reads the file back when the editor exits successfully. The prompt file is one trailing newline shorter than what the editor saved, and a non-zero exit leaves the line exactly as it was. Deliberately **not** run through a shell: `$EDITOR` names a program and its flags, and a shell would also run `&&` chains and globs the person did not write |
| Set a line aside | `ctrl+s` (`chat:stash`) | One slot, two directions: with nothing held the line goes into the slot and the prompt empties; with something held it comes back and whatever was on the line takes its place. The rule above the input says `set aside: …` while something is held, because a prompt nothing draws is a prompt people forget they have |
| Replace the running turn | `ctrl+n` (`chat:sendNow`) | Enter during a turn queues the line behind it; this stops the turn and sends the line instead. The abandoned answer keeps its text with a `(superseded)` marker, so nothing that streamed is thrown away. Does nothing when no turn is running or the line is empty — an idle Enter is the same request, and stopping nothing would only append a marker to an answer nobody was writing |

## Command System

| Capability | Status | Notes |
|------------|--------|-------|
| Slash commands | ✅ | `CommandRegistry` |
| Command effects (profile, tools, prompt) | ✅ | `CommandResult` |
| Non-interactive support | ✅ | `supports_non_interactive` flag |
| Remote-safe filtering | ✅ | `remote_safe` flag |
| Bridge-safe filtering | ✅ | `bridge_safe` flag |
| Agent-backend commands | ✅ | `uses_agent_backend` flag |

## Background Agent Infrastructure

| Capability | Status | Notes |
|------------|--------|-------|
| Observable tasks | ✅ | `BackgroundTask` with ID |
| Cancelable tasks | ✅ | `CancellationToken` |
| Result delivery | ✅ | `mpsc` channel to UI |
| Read-only enforcement | ✅ | `writable: false` filters tools |
| Task polling in TUI loop | ✅ | `poll_background_results()` |

## Memory System

| Capability | Status | Notes |
|------------|--------|-------|
| Session facts storage | ✅ | `SessionMemoryStore` |
| Long-term memory storage | ✅ | `LongTermStore` |
| AI-powered memory update | ✅ | `/update-memory`, writes facts + persists |
| AI-powered memory extraction | ✅ | `/extract-memories`, writes topics to store |
| Team memory sync | ✅ | `TeamMemorySyncService` with SHA-256 checksum delta sync, org scoping, tombstone deletes. Wired as `/team-memory <upsert|read|list|delete|sync>` |
| Auto-dream | ✅ | Threshold-based session consolidation with LLM integration. Wired as `/auto-dream <enable|disable|status>` |

## Agent Management

| Capability | Status | Notes |
|------------|--------|-------|
| Agent spawning | ✅ | `AgentManager::spawn_agent` |
| Context fork | ✅ | `ForkedContext` |
| Tool whitelist | ✅ | `SpawnOptions::allowed_tools` |
| Message channel | ✅ | `mpsc::UnboundedSender` |
| Lifecycle tracking | ✅ | `TaskManager` integration |
| Teammate events | ✅ | `TeammateEvent` callback |
| Resume support | ✅ | `AgentManager::send_message` continues a finished agent: its task is reopened under the same id and the next turn is seeded from `AgentInstance::transcript`, the child's conversation captured after every attempt. In-process only — a new session has no transcript to continue — and `register_agent`/`resume_agent` do not exist. See `docs/agents.md` → *Resuming an agent*. |
| Forked agent execution | ✅ | `orchestrator::run_forked_agent()` is reached through background features; the `AgentManager` wrapper was removed. |

## Interoperability

| Capability | Status | Notes |
|------------|--------|-------|
| MCP client (other servers → Codape) | ✅ | `mcp_servers` / `.mcp.json` / plugin-contributed servers; `codape mcp add|remove|test` |
| MCP server (Codape → another agent) | ✅ | `codape mcp serve` over stdio — `initialize`/`ping`/`tools/list`/`tools/call`; publishes the file, search and shell tools, minus anything the permission rules would stop to ask about. See `docs/mcp.md` |
| Marketplace registration (CLI) | ✅ | `codape plugin marketplace list|add <name> <url>|remove <name>`, sharing `MarketplaceSetting::validated` with the in-session command |
| Marketplace registration (in session) | ✅ | `/plugin marketplace [list|add <name> <https-url>|remove <name>]`; writes `settings.json` under `marketplaces` |
| Instruction files from other tools | ✅ | `AGENTS.md` read beside `CLAUDE.md` |

## REPL Commands

| Command | Description |
|---------|-------------|
| `/compact` | Compact session context, truncate conversation messages |
| `/agent-summary` | Summarize recent agent activity |
| `/away-summary` | Summarize what happened while user was away |
| `/magic-docs` | Generate documentation for current codebase |
| `/codebase-overview` | Generate high-level codebase overview |
| `/generate-readme` | Generate project README |
| `/agent-generation <desc>` | Create a custom agent with description |
| `/update-memory` | Update session memory from recent conversation |
| `/extract-memories` | Extract long-term memories from conversation |
| `/auto-dream <enable|disable|status>` | Toggle background session consolidation |
| `/team-memory <sub>` | Upsert, read, list, delete, or sync team memories |
| `/advisor`, `/bughunter`, `/reviewer` | Switch prompt profiles |
