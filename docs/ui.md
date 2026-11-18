# ui Module Inspection

## Goal

Terminal user interface via ratatui. Renders messages, statusline, input line, command palette overlays, Vim mode, themes, and handles all keyboard input. Identifies the host terminal and asks it for an unambiguous key encoding when it can.

## Design

### Key Types

- **`TerminalUi`** (`terminal.rs:55`) — Main ratatui wrapper. Manages alternate screen, the keyboard-protocol push/pop, and the draw loop.
- **`TerminalProfile`** (`terminal_profile.rs`) — What the environment says about the host terminal: `kind`, raw `TERM_PROGRAM`, and the chosen `keyboard_protocol`.
- **`TerminalKind`** (`terminal_profile.rs`) — Iterm2 / VsCode / Cursor / WindowsTerminal / Kitty / WezTerm / Ghostty / AppleTerminal / Vte / Unknown.
- **`KeyboardProtocol`** (`terminal_profile.rs`) — `Disambiguate` (push `DISAMBIGUATE_ESCAPE_CODES`) or `Legacy`.
- **`UiState`** (`state.rs`) — Central UI state: messages, input, overlays, statusline.
- **`MessageView`** (`message_list.rs:35`) — Virtualized message list with scroll offset and visible count.
- **`Message`** (`message_list.rs:22`) — Rendered message with role, content, tool_name, streaming flag.
- **`MessageRole`** (`message_list.rs:7`) — User/Assistant/ToolResult/ToolCall/System/Thinking. `Thinking` is the model's reasoning, kept as its own role so it is not interleaved into the answer: it is folded by default with a first-line preview, toggled by `Ctrl+E`, and dropped from exports.
- **`diff`** (`diff.rs`) — `parse_unified` classifies `git diff` text into `DiffLineKind` (`FileHeader`, `HunkHeader`, `Context`, `Added`, `Removed`, `NoNewline`) and `stat` counts the `+N -M` summary. Classification reads only the first byte and tracks whether it is inside a hunk, because a hunk's content line may itself begin with `++` — the diff's own `+` prefix is what the parser sees. Text that is not a diff is kept as header lines rather than dropped, so `/diff` in a non-repository directory still shows its message.
- **`Statusline`** (`statusline.rs:8`) — Model, tokens, cost, git branch, session title, thinking spinner. The permission mode is not here: it lives in `PermissionBar` (`permission_bar.rs`), drawn on its own line. Neither is the phase, which is `thinking_label`: the session fills it from `InteractivePhase::status_label()` (`Planning...`, `Running tests · cargo test -p codape-cli`, `Waiting for permission: …`), and it is the only label the footer has — the plan panel's `Step N/M` used to compete for the slot and lost every frame, so that field is gone.
- **`InputBuffer`** (`input.rs`) — Cursor-positioned text input with history.
- **`OverlayStack`** (`overlay.rs`) — Modal dialog stack: PermissionDialog, Notification, CommandPalette, Detail, Confirm, ModelPicker, List. `Detail` is the one body that can be longer than the screen — the help page is — so its box grows to hold the body up to the terminal and `Up`/`Down`/`PageUp`/`PageDown` scroll it; the offset lives on the variant rather than in `UiState` because the next document to open starts at the top. `ModelPicker` holds both halves of the model decision — the model list and the effort level — because a model with no effort and an effort the model does not have are both mistakes; `Tab` moves focus between them and `←`/`→` clamp at the ends rather than wrapping. `List` is any "titled set of rows you narrow by typing and pick from": the agent roster, the rewind points, the theme list, the installed plugins and their marketplaces, and the earlier sessions of this workspace. It holds nothing but a [`Picker`], so the six surfaces share one overlay, one set of keys and one renderer; the session supplies the rows.
- **`Picker`** (`picker.rs`) — a filterable modal list: `PickerItem { key, label, detail, enabled }`, a query typed into it, and a selection that wraps at both ends. `key` is the string the session gets back on accept and is never displayed; the label and detail are. An item can be disabled (a row that exists to explain what to do next, such as "no plugins installed"), and `refusal()` says why accepting the current selection did nothing. Row semantics ride on the key strings (`follow:<id>`, `logs:<id>`, `plugin:<name>`, `marketplace:<name>`, `session:<id>`, a bare number for a rewind point, a bare name for a theme), so no parallel enum has to cross the crate boundary.
- **`help`** (`help.rs`) — the help page, generated from the keybinding table rather than written beside it, so a rebound key is never described by a stale paragraph. `SECTIONS` groups the actions by what a person is trying to do — writing a prompt, choosing and inspecting, reading the transcript, a list is open, a dialog is open — because someone looking for "how do I switch models" does not know that lives under `chat:modelPicker`. An action with no binding is dropped rather than shown as "(unbound)": the page answers "what can I press".
- **`KeyBindings`** (`keybindings.rs`) — the shortcut table: a fixed catalogue of `Action`s, the `KeyStroke`s bound to each, and `KeyBindings::load` which merges `keybindings.json` over the defaults. `resolve(context, event)` answers what a key event means; `table()` is what `/keybindings` prints. Parsing, aliases (`escape`/`esc`, `opt`/`alt`/`meta`), reserved-key refusal and the move-a-key rule all live here, so the REPL holds one lookup instead of a literal comparison per shortcut. The exceptions are deliberate: `ctrl+c` (interrupt) and `ctrl+d` (exit) stay literal and are refused a binding, because they are the shell's signals before the application sees them.
- **`VimState`** (`vim.rs`) — Vim mode state machine: Normal, Insert, Visual (`v`) and Visual-line (`V`). Selection anchors and the selected text live in `InputBuffer` (`input.rs`), which is the layer that knows the buffer contents.
- **`Theme`** (`themes.rs`) — Color scheme definitions.
- **`mod_elements`** (`mod_elements.rs`) — Pure renderer for the UI tree a mod contributes (`UiElement` from `codape_core::ui_protocol`). `render_mod_ui` returns styled `Line`s and `render_mod_ui_text` returns the plain block used by the transcript and headless output. Boxes nest with box-drawing borders and titles, `UiStyle` maps to the existing modifiers/colors, and buttons render as `[label]`. `render_mod_ui_with_focus` / `render_mod_ui_text_with_focus` mark one button as `> [label] <` with reversed emphasis. The renderer keeps no TUI state: focus is an argument, so a tree can be rendered after the mod runtime is gone. `render_mod_ui_with_hits` / `render_mod_ui_with_focus_and_hits` return the same lines plus `ButtonHit { id, row, col_start, col_end }` positions relative to the block; `button_at(rect, scroll_offset, column, row, hits)` is the pure click mapping (the focus marker is included in the rendered width, so it cannot shift a hit). Interactive key routing (Tab/Shift+Tab, Enter, Esc) and click routing live in `repl.rs`; headless output is not interactive.

### Layout (bottom-up)

```
[Message area]     — Scrollable, word-wrapped messages
[Inline palette]   — /command hints (0-6 lines, dynamic height)
[Notification bar] — 0-1 line, latest unseen notification
[Todo panel]       — 0-9 lines: one row per TodoWrite item, plus "+N more"
[Statusline]       — 3 rows: text with a blank row above and below
[Rule]             — 1 row above the input
[Input lines]      — 1+ rows: "❯ " + user input, growing with the buffer
[Rule]             — 1 row below the input
[Permission bar]   — 0-2 rows: mode line and the blank row under it
[Fullscreen hint]  — 0-1 row: how to leave the full-screen transcript
```

Every pane owns its own `Constraint`, including the ones that resolve to zero height. `Layout::split` does not compact zero-height rects away, so the pane indices (`PALETTE_IDX`…`FOOTER_IDX` in `terminal.rs`) index the constraint order, never "which panes happen to be visible".

**Full-screen transcript** is a layout flag (`UiState::transcript_fullscreen`), not a `ViewMode`. The view is the same view drawn into more rows: every pane except the message area collapses to zero height and a single footer row replaces the input box. That keeps scrolling, the render cache and mouse selection exactly as they are — a second mode would need every key branch to ask which mode it is in, and the only question here is whether the input box has room. The footer text (`UiState::transcript_footer`) is written by the session, which is the side that knows what its keymap resolved the toggle to; a footer naming the built-in default would tell someone who rebound it to press a key that does nothing. It is clipped, not wrapped, because a wrapped footer would take a second row out of the transcript.

## Terminal adaptation

### What is handled

| Terminal | Identified | Key encoding |
|---|---|---|
| kitty (`KITTY_WINDOW_ID`) | yes | kitty keyboard protocol (disambiguate) |
| WezTerm (`WEZTERM_PANE`) | yes | kitty keyboard protocol (disambiguate) |
| Ghostty (`TERM_PROGRAM=ghostty`) | yes | kitty keyboard protocol (disambiguate) |
| iTerm2 (`TERM_PROGRAM=iTerm.app`) | yes | legacy |
| VS Code (`TERM_PROGRAM=vscode`) | yes | legacy |
| Cursor (`CURSOR_TRACE_ID`) | yes | legacy |
| Windows Terminal (`WT_SESSION`) | yes | legacy |
| Terminal.app (`TERM_PROGRAM=Apple_Terminal`) | yes | legacy |
| VTE family (`VTE_VERSION`) | yes | legacy |
| anything else | `Unknown` | legacy |

Identification reads the environment through a caller-supplied lookup (`TerminalProfile::detect`), so every rule above is covered by tests that describe a terminal without touching the process environment. Detection order matters and is pinned by tests: Cursor reports `TERM_PROGRAM=vscode`, and VS Code's integrated terminal on Windows can also carry `WT_SESSION`.

### Keyboard protocol — why this is a correctness item, not cosmetics

Under the legacy encoding `Ctrl+I` and `Tab`, `Ctrl+M` and `Enter`, and `Ctrl+[` and `Esc` are the same bytes on the wire; no decoder can separate them. The kitty keyboard protocol's `DISAMBIGUATE_ESCAPE_CODES` (`CSI > 1 u`) makes them distinct events, and crossterm decodes the resulting CSI-u sequences.

`TerminalUi::enter()` pushes the flag and `exit()` pops it (`terminal.rs`, `enter_commands` / `exit_commands`). The pop is conditional on the push having happened — popping a level that was never pushed would corrupt the stack for whatever runs after Codape.

The push is emitted **only** for kitty, WezTerm and Ghostty. VS Code, Cursor, iTerm2 and Windows Terminal are identified but deliberately left on the legacy encoding: their support for the protocol could not be verified from here, and guessing is worse than the status quo. Users on those terminals can opt in:

```
CODAPE_KEYBOARD_PROTOCOL=disambiguate   # force the kitty keyboard protocol
CODAPE_KEYBOARD_PROTOCOL=legacy         # force the terminal default
```

The picture renderer has its own switch, `CODAPE_IMAGE_PROTOCOL=blocks|off`; see
*Pictures in the transcript* below.

### Decoding is robust either way

`input::key_event_to_action` does not assume which encoding is in play:

- **Key releases are not keypresses.** Windows always reports releases, and the protocol reports them under `REPORT_EVENT_TYPES`; without the filter in `input::key_press`, every character is typed twice. Autorepeat is kept — it is a real keypress.
- **Control chords are case-insensitive.** Depending on the active enhancement flags, `Ctrl+Shift+C` is reported as `'c'` or `'C'` with CONTROL; both mean the same key.
- **Control characters are never inserted.** CSI-u can deliver `Char('\n')` where the legacy encoding delivered a named key. A single-line buffer must never take one.

### Mouse input

`TerminalUi::enter` enables mouse capture (`EnableMouseCapture`) and `exit`
disables it, so crossterm delivers wheel and button events. Wheel events scroll
the transcript in `repl.rs`.

Mod UI buttons are clickable. Before each draw the REPL passes a
`ModUiBlockSpec` for the newest block (message index, live `UiElement` tree and
focused id) to `TerminalUi::draw_with_mod_ui`. The transcript renderer
(`terminal.rs`) re-renders that block with
`render_mod_ui_with_focus_and_hits` at the message pane's actual content width,
draws those lines with the system-message prefix, and records a
`ModUiHitRegion` — the block's absolute `rect`, the number of block rows
clipped above it (`scroll_offset`), and its `ButtonHit`s — for the frame that
was just drawn. On `MouseEventKind::Down(MouseButton::Left)` the REPL asks
`mod_elements::button_at` to map the screen coordinates; a hit runs the same
activation path as `Enter`. Clicks are dropped while an overlay, a permission
confirmation or an MCP form owns the screen, and a stale spec is ignored when
its message content no longer matches (after `/clear`, rewind or resume). The
compact transcript (message pane under ten rows) truncates blocks to one line
and records no hit regions, so clicks are not routed there.

Keyboard focus is unchanged: `Tab`/`Shift+Tab`/`Enter`/`Esc` work in every
terminal, and they are the fallback where the terminal does not report mouse
events.

### Not handled

- **VS Code GPU acceleration.** Claude Code disables VS Code's GPU acceleration; Codape does not. There is no way to do it from inside the process — `terminal.integrated.gpuAcceleration` is a VS Code setting that requires editing `settings.json` and reloading the window. Shipping a flag that only pretends to change it would be worse than not shipping one, so VS Code is detected and nothing further is claimed.
- **`modifyOtherKeys`.** crossterm 0.28 does not decode xterm's `CSI 27 ; modifier ; code ~` form. We avoid the situation rather than mis-decode it: nothing here asks a terminal to enable modifyOtherKeys.
- **Terminals not in the table above** are `Unknown` and get the legacy encoding.

### Pictures in the transcript

An attached picture — `--image shot.png`, `@shot.png`, a Ctrl+V paste — is drawn
in the transcript, not just sent. `crates/ui/src/transcript_image.rs` decodes it
and turns it into half-block cells: `▀` with the top pixel as the foreground and
the bottom one as the background, so a cell carries two rows of pixels.

**Why not the kitty graphics protocol.** kitty, WezTerm, Ghostty and iTerm2 can
place real pixels, and that is what `ratatui-image` drives. But those protocols
write *outside* ratatui's cell buffer: the terminal anchors the image to a screen
position and keeps it there while the text moves. Codape's transcript
virtualises, scrolls by rendered line, caches line spans, and hit-tests mod UI
blocks by row, so an image living outside the buffer would drift, duplicate, and
survive redraws that moved its text. Half blocks make the picture ordinary
`Span`s, which means scrolling, the line cache, the virtualised viewport and the
selection snapshot all work on it with no special cases. The cost is resolution,
not correctness.

Detection is `ImageProtocol::detect` in the same module, from `COLORTERM` and
`TerminalProfile::kind`. Terminal.app is deliberately excluded: it never gained
24-bit SGR, so a half block there lands in the default colour and reads as a
broken picture. `CODAPE_IMAGE_PROTOCOL=blocks|off` overrides the choice. Where
nothing can be drawn — no truecolor, an unreadable header, more than 40 million
decoded pixels — the row keeps its `[image: image/png]` caption, which is also
what a copy of the transcript produces.

The picture is capped at 20 rows and scaled to the pane width, never cropped.
The pixel size comes from the header, so the layout reserves the right number of
rows before anything is decoded; the decode happens on the first draw and is
cached per width and theme background. A payload whose header reads but whose
pixels do not — a truncated file — keeps the caption on the rows already
reserved for it, so the rest of the transcript does not jump up when a picture
turns out to be unreadable.

## Architecture Patterns

- **ratatui widget composition** — Paragraph, Block, Layout with Constraint-based sizing.
- **Vim modal editing** — Normal, Insert, Visual and Visual-line modes. `VimState::handle_key` classifies normal/visual keys into `VimAction`s; `input::key_event_to_action` remains the classifier for non-Vim (insert) input.
  - **Motions and operators carry a count.** A digit prefix (`3w`, `5x`, `2>>`) is accumulated by `VimState` and carried inside the `VimAction` variant (`WordForward(3)`, `DeleteChar(5)`, `Indent(2)`), so the REPL applies it exactly as many times as asked. A leading `0` with no count is the `0` motion (jump to start); `0` after a digit is part of the number.
  - **Normal-mode operators** include `x` (delete char), `dd`/`D`, `dw`/`de`, `cc`/`cw`/`C` (change), `s`/`S` (substitute), `o`/`O` (open line below/above), `p`/`P` (paste), and `>`/`<` (indent/outdent the line).
  - **Visual mode** supports `h/j/k/l`, `w/b/e`, `0/$`, `gg/G` motions (with counts), `v`/`V` to switch selection kind, `y` to yank, `d`/`x` to delete, `c`/`s` to change the selection, `p` to paste over it, `>`/`<` to indent and leave, and `Esc` to leave. The statusline shows a `-- VISUAL --` / `-- VISUAL LINE --` indicator while a selection is anchored.
  - Selections operate on byte ranges while cursor movement and word motions step whole UTF-8 characters, so multi-byte input is never split. The selected range is highlighted with the theme's `selection_fg`/`selection_bg`, and the terminal cursor is positioned by display width (`unicode-width`) rather than byte offset.
  - **Single-line limitation.** The input buffer is one line, so the line-oriented Vim commands are applied to that single line: `j`/`k`, `gg`/`G`, and `o`/`O` cannot move between lines or open a new one below/above a real newline. `o`/`O` place the cursor at the end/start of the one line and enter Insert; `>`/`<` indent or outdent that line. This is the honest ceiling of a single-line editor and is noted rather than faked.
- **Virtualization** — `MessageView` computes visible messages based on per-message height (not just message count). `MessageView::insert` exists for a row that belongs above one already in the list: reasoning arrives after the turn's empty answer row was pushed and has to sit above it.
- **Split pane** — `/open <path>` and `/diff` put content beside the transcript: `LayoutMode::HorizontalSplit { left_percent }` splits the message area into a left pane and the conversation. The pane shows one thing at a time, so a file opened over a diff replaces it.

  | Command | Left pane | Title |
  |---|---|---|
  | `/open src/lib.rs` | the file, word-wrapped | the path as typed |
  | `/diff [--cached]` | the patch, coloured per line, unwrapped | `diff  1 file(s)  +2 -1` |

  A diff uses `success` for added lines, `error` for removed ones, `accent_secondary` for hunk headers and `dim` for file headers, and is never wrapped: a wrapped diff line is not the line the file has, and the gutter has to stay beside its text. `PageUp`/`PageDown` scroll the pane while a diff is showing — that is the thing being read — and the transcript otherwise. The scroll is clamped against the pane's drawn height at render time, so a resize cannot scroll past the end. A failed `/diff` (not a repository, git missing) lands in the transcript rather than leaving an empty coloured pane.
- **Word wrapping** — Custom `wrap_text()` with `unicode-width` for CJK support.
- **Overlay stack** — Modal dialogs can nest; inline palette is special-cased above statusline.
- **Shortcuts are data.** The REPL asks `KeyBindings::resolve` rather than comparing `KeyCode::Char(..)` itself. A binding names a context — `Global`, `Chat`, `Confirmation`, `PermissionDialog`, `Autocomplete` (the `/` list), `FileMention` (the `@` list), `HistorySearch` or `List` (a modal picker) — and the catalogue behind them is eight contexts over thirty-nine actions. `Global` is checked first, so a global key cannot be shadowed; `FileMention` inherits the palette's keys and adds `enter` (which the palette deliberately lacks, so Enter submits the line rather than completing a suggestion). A list is modal: while one is on top of the overlay stack the REPL routes the key through `Context::List` before anything else, so `enter` chooses a row and ordinary characters filter rather than reach the input line. `esc` closes it because a list is modal and `Cancel` lives in `Chat`, so both contexts are consulted for that one key. `keybindings.json` in the config home, then in `.codape/`, moves a binding: assigning a key takes it from whatever held it (so one key never means two things) and `null` removes it. An entry that names an unknown action or context, a typo'd modifier, or a reserved key (`ctrl+c`, `ctrl+d`, `ctrl+m`/`ctrl+[`/`ctrl+i`/`ctrl+h`, which terminals cannot distinguish from enter/esc/tab/backspace) is reported in `/keybindings` and skipped; it never fails startup. Text editing keys are deliberately not rebindable — they are the terminal's contract, not a preference.
- **Testable terminal I/O** — The startup byte sequence is written by a plain function over `impl Write` (`enter_commands`), so the keyboard push can be asserted on rather than assumed.

## Current Status

The `codape-ui` crate has no stubs. The integration gap below is real, and the Vim/Kittty boundaries in the checklist apply.

Known integration gap (owned by `crates/cli/src/repl.rs`, not this crate): the REPL matches key events inline rather than routing every key through `input::key_event_to_action`. The two safety properties that matter are nevertheless applied at the boundary: `repl.rs` calls `input::key_press` on every incoming event (release events are dropped), and `InputBuffer::apply(InsertChar)` rejects control characters, as does the REPL's Vim-insert path. `TerminalUi::profile()` is still not surfaced in the UI.

Tests: `cargo test -p codape-ui`.
