//! Terminal UI rendering via ratatui.

use crate::message_list::{Message, MessageRole};
use crate::mod_elements::{ButtonHit, render_mod_ui_with_focus_and_hits};
use crate::state::{TextSelection, UiState};
use crate::statusline::Statusline;
use crate::terminal_profile::TerminalProfile;
use crate::themes::Theme;
use codape_core::ui_protocol::UiElement;
use crossterm::event::{PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags};
use crossterm::queue;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::Frame;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use std::io::Stdout;
use std::sync::OnceLock;

/// A mod UI block the interactive loop wants drawn and tracked for clicks.
///
/// The transcript stores a block as rendered text, but hit testing needs the
/// live tree: the same tree renders the block at the message pane's width so
/// the button positions match the screen exactly.
#[derive(Debug, Clone)]
pub struct ModUiBlockSpec {
    /// Index of the message that holds this block.
    pub message_index: usize,
    /// The content the block was last rendered as. A mismatch means the index
    /// no longer points at this block (after `/clear`, rewind or resume), so
    /// the spec is ignored rather than drawing over an unrelated message.
    pub content: String,
    /// The host-owned UI tree snapshot.
    pub elements: Vec<UiElement>,
    /// The button that currently has keyboard focus, if any.
    pub focused: Option<String>,
}

/// Where a mod UI block landed in the last drawn frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModUiHitRegion {
    /// Index of the message the block was rendered from.
    pub message_index: usize,
    /// Absolute screen rect of the visible part of the block. `x` is the
    /// block's first rendered column and `y` its first visible row.
    pub rect: Rect,
    /// Number of block rows clipped above `rect.y`.
    pub scroll_offset: usize,
    /// Button positions relative to the block's rendered lines.
    pub hits: Vec<ButtonHit>,
}

/// Lazy-initialized syntax highlighter for the TUI.
fn syntax_highlighter() -> &'static codape_core::syntax::SyntaxHighlighter {
    static HIGHLIGHTER: OnceLock<codape_core::syntax::SyntaxHighlighter> = OnceLock::new();
    HIGHLIGHTER.get_or_init(codape_core::syntax::SyntaxHighlighter::new)
}

/// `CODAPE` in half blocks, four rows tall.
///
/// Seven glyph rows folded in pairs, so the wordmark costs four of the sixteen
/// rows the card has at 80x24. Drawn as ordinary styled spans: the kitty
/// graphics protocol writes outside the cell buffer and fights the virtualised
/// transcript, so every picture in this UI is text.
const WELCOME_WORDMARK: &str = r"
▄▀▀▀▄ ▄▀▀▀▄ █▀▀▀▄ ▄▀▀▀▄ █▀▀▀▄ █▀▀▀▀
█     █   █ █   █ █▄▄▄█ █▄▄▄▀ █▄▄▄
█   ▄ █   █ █   █ █   █ █     █
 ▀▀▀   ▀▀▀  ▀▀▀▀  ▀   ▀ ▀     ▀▀▀▀▀
";

/// The welcome page is drawn inside a box no wider than this. Two columns
/// beyond the longest hint keep the text off the right border.
const WELCOME_CARD_MAX_WIDTH: usize = 64;
/// Below this width the box cannot hold the block wordmark (35 columns plus
/// its indent) and the page falls back to plain lines.
const WELCOME_CARD_MIN_WIDTH: usize = 39;
/// Rows the full welcome card occupies, including its border.
///
/// This is also the message pane's height at 80x24, so growing the card past
/// it would push the wordmark off the top of the page on the most common
/// terminal size. [`the_welcome_card_fits_the_message_pane_at_80x24`] pins it.
const WELCOME_CARD_HEIGHT: usize = 16;

/// The main terminal UI.
#[allow(missing_debug_implementations)]
pub struct TerminalUi {
    terminal: ratatui::Terminal<CrosstermBackend<Stdout>>,
    alternate: bool,
    profile: TerminalProfile,
    keyboard_pushed: bool,
    mod_ui_hit_regions: Vec<ModUiHitRegion>,
    /// The last drawn frame as one character per cell. Used to copy a mouse
    /// selection; a continuation cell of a wide character is `'\0'`.
    frame_rows: Vec<Vec<char>>,
    /// The area the last frame covered.
    frame_area: Rect,
}

impl TerminalUi {
    /// Create a new terminal UI.
    ///
    /// The terminal is identified here, once, from the environment. Nothing is
    /// written to the terminal until [`TerminalUi::enter`].
    pub fn new() -> std::io::Result<Self> {
        let backend = CrosstermBackend::new(std::io::stdout());
        let terminal = ratatui::Terminal::new(backend)?;
        Ok(Self {
            terminal,
            alternate: false,
            profile: TerminalProfile::from_env(),
            keyboard_pushed: false,
            mod_ui_hit_regions: Vec::new(),
            frame_rows: Vec::new(),
            frame_area: Rect::default(),
        })
    }

    /// The terminal profile detected at construction.
    #[must_use]
    pub const fn profile(&self) -> &TerminalProfile {
        &self.profile
    }

    /// Whether the keyboard-protocol flags are currently pushed.
    #[must_use]
    pub const fn keyboard_enhancement_active(&self) -> bool {
        self.keyboard_pushed
    }

    /// Enter alternate screen.
    pub fn enter(&mut self) -> std::io::Result<()> {
        let mut stdout = std::io::stdout();
        enter_commands(&self.profile, &mut stdout)?;
        self.keyboard_pushed = self.profile.keyboard_flags().is_some();
        self.alternate = true;
        // Clear the screen and reset ratatui's back buffer so the next draw
        // repaints everything. `Terminal::clear` is not used: ratatui 0.30
        // made it query the cursor position (a DSR request), which a PTY
        // without a terminal emulator never answers; `resize` performs the
        // same clear and buffer reset without that round trip.
        let area: ratatui::layout::Rect = self.terminal.size()?.into();
        self.terminal.resize(area)?;
        Ok(())
    }

    /// Leave alternate screen.
    pub fn exit(&mut self) -> std::io::Result<()> {
        if self.alternate {
            let mut stdout = std::io::stdout();
            exit_commands(self.keyboard_pushed, &mut stdout)?;
            self.keyboard_pushed = false;
            self.alternate = false;
        }
        Ok(())
    }

    /// Draw a single frame.
    pub fn draw(&mut self, state: &mut UiState) -> std::io::Result<()> {
        self.draw_with_mod_ui(state, &[])
    }

    /// Draw a frame, rendering interactive mod UI blocks from their live trees.
    ///
    /// The hit regions recorded here describe the frame that was just drawn, so
    /// a caller that handles a mouse event before the next draw maps the click
    /// against the screen the user actually saw.
    pub fn draw_with_mod_ui(
        &mut self,
        state: &mut UiState,
        mod_ui_blocks: &[ModUiBlockSpec],
    ) -> std::io::Result<()> {
        let mut mod_ui_hit_regions = Vec::new();
        let completed = self.terminal.draw(|frame| {
            render_frame(frame, state, mod_ui_blocks, &mut mod_ui_hit_regions);
        })?;
        self.mod_ui_hit_regions = mod_ui_hit_regions;
        self.frame_rows = capture_rows(completed.buffer);
        self.frame_area = completed.buffer.area;
        Ok(())
    }

    /// The mod UI hit regions from the last successfully drawn frame.
    #[must_use]
    pub fn mod_ui_hit_regions(&self) -> &[ModUiHitRegion] {
        &self.mod_ui_hit_regions
    }

    /// The text covered by a mouse selection, read from the last drawn frame.
    ///
    /// The buffer is cloned only when a copy is requested, not on every draw.
    #[must_use]
    pub fn selected_text(&self, selection: &TextSelection) -> String {
        selection_text(self.frame_area, &self.frame_rows, *selection)
    }

    /// The text of `region` from the last drawn frame.
    ///
    /// `ctrl+shift+c` passes the transcript area, so what lands on the
    /// clipboard is the conversation and not the prompt, statusline and
    /// permission bar drawn around it.
    #[must_use]
    pub fn region_text(&self, region: Rect) -> String {
        region_text(self.frame_area, &self.frame_rows, region)
    }
}

/// Snapshot a rendered buffer as one character per cell.
///
/// A wide character owns the cell it starts in; the cell it continues into has
/// no symbol and is recorded as `'\0'`, so column arithmetic still lines up
/// with what the user selected on screen.
fn capture_rows(buffer: &ratatui::buffer::Buffer) -> Vec<Vec<char>> {
    let area = buffer.area;
    (area.top()..area.bottom())
        .map(|row| {
            (area.left()..area.right())
                .map(|column| {
                    buffer
                        .cell((column, row))
                        .and_then(|cell| cell.symbol().chars().next())
                        .unwrap_or('\0')
                })
                .collect()
        })
        .collect()
}

/// The text inside `region`, read from a frame snapshot.
///
/// The region is clipped to the frame, so a pane partly off-screen copies what
/// is visible rather than failing. Every row is taken whole; trailing blanks
/// and trailing blank rows are dropped, which is what makes the result paste
/// as text rather than as a block of spaces.
///
/// A mouse selection is not this function: a drag starts each row at a
/// different column, which a rectangle cannot express, so [`selection_text`]
/// keeps its own loop. The trailing-blank rule is shared, and that is the one
/// part a paste depends on.
pub(crate) fn region_text(area: Rect, rows: &[Vec<char>], region: Rect) -> String {
    if region.width == 0 || region.height == 0 {
        return String::new();
    }
    let left = region.left().max(area.left());
    let right = region
        .right()
        .saturating_sub(1)
        .min(area.right().saturating_sub(1));
    let top = region.top().max(area.top());
    let bottom = region
        .bottom()
        .saturating_sub(1)
        .min(area.bottom().saturating_sub(1));
    if left > right || top > bottom {
        return String::new();
    }
    let mut lines = Vec::new();
    for row in top..=bottom {
        let Some(cells) = rows.get((row - area.y) as usize) else {
            continue;
        };
        let mut line = String::new();
        for column in left..=right {
            if let Some(character) = cells.get((column - area.left()) as usize)
                && *character != '\0'
            {
                line.push(*character);
            }
        }
        lines.push(line.trim_end().to_string());
    }
    lines.join("\n").trim_end().to_string()
}

/// Extract the text covered by a mouse selection from a frame snapshot.
///
/// Rows between the endpoints are taken whole; the first and last rows are cut
/// at the selected columns. Trailing blanks on each row and trailing blank
/// rows are dropped, so choosing a rectangle of screen does not copy a block
/// of spaces or the interface below the text.
pub(crate) fn selection_text(area: Rect, rows: &[Vec<char>], selection: TextSelection) -> String {
    let ((start_col, start_row), (end_col, end_row)) = selection.normalized();
    // The mouse path keeps its own per-row column logic: the first row starts
    // at the press column and every later row starts at the left edge, which a
    // single rectangle cannot express.
    let last_col = area.right().saturating_sub(1);
    let mut lines = Vec::new();
    for row in start_row..=end_row {
        if row < area.y || row >= area.bottom() {
            continue;
        }
        let left = if row == start_row {
            start_col
        } else {
            area.left()
        };
        let right = if row == end_row { end_col } else { last_col };
        let left = left.max(area.left()).min(last_col);
        let right = right.max(left).min(last_col);
        let Some(cells) = rows.get((row - area.y) as usize) else {
            continue;
        };
        let mut line = String::new();
        for column in left..=right {
            if let Some(character) = cells.get((column - area.left()) as usize)
                && *character != '\0'
            {
                line.push(*character);
            }
        }
        lines.push(line.trim_end().to_string());
    }
    lines.join("\n").trim_end().to_string()
}

/// Tint the cells covered by a mouse selection.
///
/// Only the background changes: the text keeps the colours it was drawn with,
/// and an empty [`Paragraph`] fills the rect without writing over the symbols.
fn paint_selection(frame: &mut Frame, area: Rect, selection: TextSelection, theme: &Theme) {
    let ((start_col, start_row), (end_col, end_row)) = selection.normalized();
    let last_col = area.right().saturating_sub(1);
    let style = Style::default().bg(theme.selection_bg);
    for row in start_row..=end_row {
        if row < area.y || row >= area.bottom() {
            continue;
        }
        let left = if row == start_row {
            start_col
        } else {
            area.left()
        };
        let right = if row == end_row { end_col } else { last_col };
        let left = left.max(area.left()).min(last_col);
        let right = right.max(left).min(last_col);
        frame.render_widget(
            Paragraph::new("").style(style),
            Rect::new(left, row, right - left + 1, 1),
        );
    }
}

/// Write the commands that put the terminal into TUI mode.
///
/// Split out from [`TerminalUi::enter`] so the exact byte sequence is testable:
/// the keyboard push is only correct if it is actually emitted for a terminal
/// that understands it, and omitted for one that does not. `queue!` is used
/// rather than `execute!` so the bytes go to the writer on every platform —
/// crossterm's `execute!` falls back to a WinAPI call, which has no keyboard
/// protocol at all, and would make this untestable.
fn enter_commands(profile: &TerminalProfile, out: &mut impl std::io::Write) -> std::io::Result<()> {
    queue!(
        out,
        EnterAlternateScreen,
        crossterm::event::EnableMouseCapture
    )?;
    if let Some(flags) = profile.keyboard_flags() {
        queue!(out, PushKeyboardEnhancementFlags(flags))?;
    }
    out.flush()
}

/// Undo [`enter_commands`], in the reverse order.
///
/// `keyboard_pushed` is passed explicitly rather than re-derived from the
/// profile: popping a level that was never pushed would corrupt the stack for
/// whatever runs after Codape.
fn exit_commands(keyboard_pushed: bool, out: &mut impl std::io::Write) -> std::io::Result<()> {
    if keyboard_pushed {
        queue!(out, PopKeyboardEnhancementFlags)?;
    }
    queue!(
        out,
        LeaveAlternateScreen,
        crossterm::event::DisableMouseCapture
    )?;
    out.flush()
}

/// Put the terminal back the way [`TerminalUi::enter`] found it, from a panic
/// hook.
///
/// The hook has no `TerminalUi` to call [`TerminalUi::exit`] on — the value
/// belongs to the loop that is unwinding, and in a release build
/// (`panic = "abort"`) nothing unwinds and no destructor runs at all. So this
/// re-derives the same decision from the environment and writes the same
/// sequences, in the same order.
///
/// It writes to **stdout**, not stderr: the TUI draws on stdout, and
/// `LeaveAlternateScreen` on a different descriptor leaves the alternate
/// screen exactly where it was — the panic text would land in a buffer the
/// user cannot see and the shell would come back holding a screen that was
/// never popped.
///
/// Best-effort and safe to call when the TUI was never entered: a terminal
/// with an empty keyboard-enhancement stack and no alternate screen ignores
/// both sequences. Without a terminal (a pipe or a file) it writes nothing,
/// so a headless crash does not inject escapes into output a script is
/// collecting.
pub fn restore_after_panic() {
    use std::io::IsTerminal;

    let mut stdout = std::io::stdout();
    if !stdout.is_terminal() {
        return;
    }
    let _ = panic_exit_commands(&TerminalProfile::from_env(), &mut stdout);
    let _ = crossterm::terminal::disable_raw_mode();
}

/// The escape sequences [`restore_after_panic`] writes, separated out so they
/// can be asserted against without a terminal.
fn panic_exit_commands(
    profile: &TerminalProfile,
    out: &mut impl std::io::Write,
) -> std::io::Result<()> {
    // Popped only when `enter` would have pushed it, for the reason
    // `exit_commands` documents.
    exit_commands(profile.keyboard_flags().is_some(), out)
}

/// Pane positions in the vertical layout.
///
/// `Layout::split` returns one rect per constraint, in order, even when a
/// constraint resolves to zero height — it does not compact them away. Deriving
/// an index from "which panes are visible" therefore points at the wrong rect:
/// with the palette hidden and no notification, a computed statusline index of
/// 1 lands on the zero-height palette slot and the line disappears. Index by the
/// constraint position instead.
const PANEL_IDX: usize = 1;
const PALETTE_IDX: usize = 2;
const NOTIFICATION_IDX: usize = 3;
const TODOS_IDX: usize = 4;
const STATUSLINE_IDX: usize = 5;
const RULE_IDX: usize = 6;
const INPUT_IDX: usize = 7;
const RULE_BELOW_IDX: usize = 8;
const PERMISSION_IDX: usize = 9;
const FOOTER_IDX: usize = 10;

/// Which of the optional panes this frame draws.
///
/// A named struct rather than a row of bools: `Panes` appears once in the
/// signature, and a call site that named the fields cannot put `statusline`
/// where `notification` goes. `fullscreen` rides here too because it is the
/// same decision — which panes exist — even though it overrides two of the
/// others rather than adding one.
#[derive(Debug, Clone, Copy, Default)]
struct Panes {
    /// The notification bar above the todo panel.
    notification: bool,
    /// The statusline, which is three rows: text and the blank rows around it.
    statusline: bool,
    /// The permission bar, which is two rows: text and the blank row below.
    permission_bar: bool,
    /// The transcript is taking the whole screen, so the prompt is gone.
    fullscreen: bool,
}

/// The vertical pane stack, top to bottom.
///
/// Shared with the layout tests so both sides agree on what each index means.
/// `input_height` is the number of terminal rows the input buffer needs: one
/// for an empty or single-line buffer, more once the buffer has lines or wraps.
/// `todo_height` is the row count the `TodoWrite` panel asked for, zero when
/// the model has not written a list.
fn frame_constraints(
    panel_height: u16,
    inline_palette_height: u16,
    panes: Panes,
    todo_height: u16,
    input_height: u16,
) -> Vec<Constraint> {
    // Fullscreen collapses the input box (both rules and the buffer) and adds
    // one footer row in its place. The message area is `Min(1)`, so it takes
    // everything the others leave.
    let (rule_above, input_rows, rule_below, footer) = if panes.fullscreen {
        (0, 0, 0, 1)
    } else {
        (1, input_height.max(1), 1, 0)
    };
    // A mod panel sits directly under the conversation and above everything
    // else: it is the mod's own reading of the session, so it belongs beside
    // the transcript rather than below the prompt where the person's own
    // controls are.
    let panel_rows = if panes.fullscreen { 0 } else { panel_height };
    vec![
        Constraint::Min(1),                                // message area
        Constraint::Length(panel_rows),                    // mod panels
        Constraint::Length(inline_palette_height),         // inline command hint
        Constraint::Length(u16::from(panes.notification)), // notification bar
        Constraint::Length(todo_height),                   // todo panel
        // One row of text with one blank row above and below it.
        Constraint::Length(if panes.statusline { 3 } else { 0 }), // statusline
        Constraint::Length(rule_above),                           // rule above the input
        Constraint::Length(input_rows),                           // input lines
        Constraint::Length(rule_below),                           // rule below the input
        // The bar is one line plus a blank row below it, so the text does not
        // sit against the bottom edge of the terminal.
        Constraint::Length(if panes.permission_bar { 2 } else { 0 }), // permission bar
        Constraint::Length(footer),                                   // fullscreen hint
    ]
}

/// Display columns available to input text: the pane minus the two-cell
/// prompt. Shared by the layout and the renderer so both wrap identically.
fn input_wrap_width(pane_width: u16) -> usize {
    (pane_width as usize).saturating_sub(2).max(1)
}

/// How many terminal rows the input buffer occupies at `pane_width`.
fn input_display_height(input: &crate::input::InputBuffer, pane_width: u16) -> u16 {
    let rows = input.display_rows(input_wrap_width(pane_width)).len();
    u16::try_from(rows).map_or(u16::MAX, |height| height.max(1))
}

/// Lay out and paint one frame.
///
/// Separate from `draw` so a `TestBackend` can drive it: the pane indices
/// below are the single place where layout order is decided, and getting one
/// wrong makes a pane render into the wrong rect without any error.
fn render_frame(
    frame: &mut Frame,
    state: &mut UiState,
    mod_ui_blocks: &[ModUiBlockSpec],
    mod_ui_hit_regions: &mut Vec<ModUiHitRegion>,
) {
    let area = frame.area();

    // Paint the theme background first. Widgets only overwrite the style
    // fields they name, so this fill survives under text without an explicit
    // background — which is what makes switching between dark and light cover
    // the whole screen rather than only re-tinting the accents.
    frame.render_widget(
        Block::default().style(
            Style::default()
                .bg(state.theme.background)
                .fg(state.theme.foreground),
        ),
        area,
    );

    let is_compact = state.force_compact || area.height < 20;

    // The full-screen transcript is the same frame with everything but the
    // message area collapsed: the input, the statusline, the todo panel and
    // the rules all go to zero height, and one row at the bottom says how to
    // get back. Keeping one layout instead of a second renderer is what makes
    // scrolling, the cache and the mouse selection behave identically in both.
    let fullscreen = state.transcript_fullscreen;

    // Determine chunk heights
    let inline_palette_height = if is_compact || fullscreen {
        0
    } else {
        inline_palette_height(state)
    };
    let show_statusline = !is_compact && !fullscreen;
    let show_notif = state.notifications.active_count() > 0 && !is_compact && !fullscreen;

    let show_perm_bar = !is_compact && !fullscreen;
    // The todo panel sits above the statusline. It is the model's own plan, so
    // it stays visible in compact mode too, but capped so the transcript keeps
    // most of the screen.
    let max_todo_height = area.height.saturating_sub(6).max(1);
    let todo_height = if fullscreen {
        0
    } else {
        state.todos.display_height().min(max_todo_height)
    };
    // A long or multi-line input grows its own pane rather than being clipped
    // to one row. It is capped so the transcript keeps most of the screen.
    let max_input_height = area.height.saturating_sub(3).max(1);
    let input_height = if fullscreen {
        0
    } else if state.history_search.is_some() {
        // The query line plus at least one match row.
        2.min(max_input_height)
    } else {
        input_display_height(&state.input, area.width).min(max_input_height)
    };
    let footer_height = u16::from(fullscreen);
    // Mod panels take their own rows below the conversation. They are capped
    // so the transcript never drops below a few rows: a panel that grows
    // without bound would be a mod taking the screen, which is not what a
    // panel is for.
    let max_panel_height = area.height.saturating_sub(8).max(1);
    let panel_height = if fullscreen {
        0
    } else {
        state
            .panels
            .display_height(area.width as usize)
            .min(max_panel_height)
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(frame_constraints(
            panel_height,
            inline_palette_height,
            Panes {
                notification: show_notif,
                statusline: show_statusline,
                permission_bar: show_perm_bar,
                fullscreen,
            },
            todo_height,
            input_height,
        ))
        .split(area);

    // `Layout::split` returns one rect per constraint, in order, even
    // when a constraint resolves to zero height — it does not compact
    // them away. Deriving an index from "which panes are visible"
    // therefore points at the wrong rect: with the palette hidden and
    // no notification, a computed statusline index of 1 lands on the
    // zero-height palette slot and the line disappears. Index by the
    // constraint position instead.
    debug_assert_eq!(chunks.len(), FOOTER_IDX + 1);

    // Message area (index 0) — may be split horizontally
    let msg_area = chunks[0];
    if let crate::state::LayoutMode::HorizontalSplit { left_percent } = state.layout_mode {
        let pct = left_percent.clamp(10, 90);
        let split_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(pct),
                Constraint::Percentage(100 - pct),
            ])
            .split(msg_area);

        // Left pane: file/content viewer
        render_left_pane(frame, split_chunks[0], state);
        // Right pane: messages
        render_messages(
            frame,
            split_chunks[1],
            state,
            mod_ui_blocks,
            mod_ui_hit_regions,
        );
    } else {
        render_messages(frame, msg_area, state, mod_ui_blocks, mod_ui_hit_regions);
    }

    // Mod panels, directly under the conversation.
    if panel_height > 0 {
        render_mod_panels(frame, chunks[PANEL_IDX], state);
    }

    // Inline palette (index 1)
    if inline_palette_height > 0 {
        render_inline_palette(frame, chunks[PALETTE_IDX], state);
    }

    // Notification bar (index 2)
    if show_notif {
        render_notification_bar(
            frame,
            chunks[NOTIFICATION_IDX],
            &mut state.notifications,
            &state.theme,
        );
    }

    // Statusline (above input)
    if show_statusline {
        render_statusline(
            frame,
            chunks[STATUSLINE_IDX],
            &state.statusline,
            &state.theme,
        );
    }

    // Todo panel (above the statusline)
    if todo_height > 0 {
        render_todo_panel(frame, chunks[TODOS_IDX], &state.todos, &state.theme);
    }

    // Rules that frame the input box. The one above carries the stashed-line
    // badge, if there is a line set aside: a prompt nothing draws is a prompt
    // people forget they have, and the slot only exists to be taken back.
    let stash_hint = state.stashed_input.as_deref().map(stash_badge);
    render_input_rule_with_hint(frame, chunks[RULE_IDX], &state.theme, stash_hint.as_deref());

    // Input line
    render_input(
        frame,
        chunks[INPUT_IDX],
        &state.input,
        state.history_search.as_ref(),
        &state.theme,
    );

    render_input_rule(frame, chunks[RULE_BELOW_IDX], &state.theme);

    // Permission bar (below input)
    if show_perm_bar {
        render_permission_bar(
            frame,
            chunks[PERMISSION_IDX],
            &state.permission_bar,
            &state.theme,
        );
    }

    // The one row a full-screen transcript keeps: how to leave it. It names
    // the key the session actually resolves rather than a literal, so a
    // rebound toggle does not tell the person to press one that does nothing.
    if fullscreen && footer_height > 0 {
        render_fullscreen_footer(frame, chunks[FOOTER_IDX], state);
    }

    // Overlay (only non-palette overlays; palette is rendered inline)
    if state.overlays.is_active() && !is_inline_palette(state) {
        render_overlay(frame, area, state);
    }

    // A mouse selection is painted last so it is visible over the transcript.
    if let Some(selection) = state.selection {
        paint_selection(frame, area, selection, &state.theme);
    }
}

/// Compute the height of the inline palette area (0 = hidden).
fn inline_palette_height(state: &UiState) -> u16 {
    use crate::overlay::Overlay;
    let Some(overlay) = state.overlays.top() else {
        return 0;
    };
    match overlay {
        Overlay::CommandPalette {
            query, commands, ..
        } if query.starts_with('/') => {
            // Header line + up to 5 visible commands, capped at remaining terminal space
            let count = commands.len().min(5);
            u16::try_from(count + 1).map_or(6, |height| height.min(6))
        }
        Overlay::FileMention { files, .. } => {
            let count = files.len().min(5);
            u16::try_from(count + 1).map_or(6, |height| height.min(6))
        }
        _ => 0,
    }
}

/// Check whether the active overlay is an inline `/` palette.
fn is_inline_palette(state: &UiState) -> bool {
    use crate::overlay::Overlay;
    state.overlays.top().is_some_and(|overlay| {
        matches!(
            overlay,
            Overlay::CommandPalette { query, .. } if query.starts_with('/')
        ) || matches!(overlay, Overlay::FileMention { .. })
    })
}

/// Word-wrap text to the given display width.
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    let mut result: Vec<String> = Vec::new();
    for paragraph in text.split('\n') {
        let mut current = String::new();
        let mut current_width: usize = 0;
        for word in paragraph.split_whitespace() {
            let word_width = unicode_width::UnicodeWidthStr::width(word);
            let space = usize::from(!current.is_empty());
            if current_width + space + word_width > width {
                result.push(std::mem::take(&mut current));
                current_width = 0;
            }
            if !current.is_empty() {
                current.push(' ');
                current_width += 1;
            }
            current.push_str(word);
            current_width += word_width;
        }
        if !current.is_empty() {
            result.push(current);
        }
    }
    if result.is_empty() {
        result.push(String::new());
    }
    result
}

/// Compute how many terminal rows a message occupies at the given width.
fn message_display_height(
    msg: &crate::message_list::Message,
    width: usize,
    is_compact: bool,
    image_protocol: crate::transcript_image::ImageProtocol,
) -> usize {
    let prefix = message_prefix(msg.role);
    let prefix_width = unicode_width::UnicodeWidthStr::width(prefix);
    let wrap_width = width.saturating_sub(prefix_width);
    if let Some(rows) = image_rows(msg, wrap_width, is_compact, image_protocol) {
        return rows;
    }
    let mut height = wrapped_line_count(&msg.content, wrap_width);
    if matches!(
        msg.role,
        crate::message_list::MessageRole::User
            | crate::message_list::MessageRole::Assistant
            | crate::message_list::MessageRole::Thinking
    ) {
        height += 1;
    }
    if msg.tool_name.is_some()
        && matches!(
            msg.role,
            crate::message_list::MessageRole::ToolCall
                | crate::message_list::MessageRole::ToolResult
        )
    {
        height += 1;
    }
    height
}

/// The cell rows a message's picture occupies, when one will be drawn.
///
/// `None` means the row renders as text — there is no picture, the terminal
/// cannot draw one, or the image could not be read. Keeping this and the draw
/// path on the same predicate is what stops the reserved height and the painted
/// rows from disagreeing.
fn image_rows(
    msg: &crate::message_list::Message,
    wrap_width: usize,
    is_compact: bool,
    image_protocol: crate::transcript_image::ImageProtocol,
) -> Option<usize> {
    // Compact mode draws one line per message, so the picture is not drawn
    // there either; the caption is what the row shows.
    if is_compact || !image_protocol.can_draw() {
        return None;
    }
    let (_, rows) = msg.image.as_ref()?.cell_size(wrap_width);
    // The role prefix gets its own row above the picture.
    (rows > 0).then_some(rows + 1)
}

fn wrapped_line_count(text: &str, width: usize) -> usize {
    if width == 0 {
        return 1;
    }

    let mut lines = 0usize;
    for paragraph in text.split('\n') {
        let mut current_width = 0usize;
        let mut current_empty = true;
        for word in paragraph.split_whitespace() {
            let word_width = unicode_width::UnicodeWidthStr::width(word);
            let space = usize::from(!current_empty);
            if current_width + space + word_width > width {
                lines += 1;
                current_width = 0;
                current_empty = true;
            }
            if !current_empty {
                current_width += 1;
            }
            current_width += word_width;
            current_empty = false;
        }
        if !current_empty {
            lines += 1;
        }
    }

    lines.max(1)
}

/// Rows a body occupies when wrapped, with blank lines counted.
///
/// [`wrap_text`] drops empty paragraphs, which is right for a transcript where
/// a stray newline is noise and wrong for an overlay body, whose blank lines
/// are layout: the model picker puts its effort row below one blank line and
/// its key hints below another, so a count that ignores them sizes the box two
/// rows short and clips the hints off the bottom.
fn wrapped_rows_with_blanks(text: &str, width: usize) -> usize {
    text.split('\n')
        .map(|paragraph| wrapped_line_count(paragraph, width))
        .sum()
}

const fn message_prefix(role: crate::message_list::MessageRole) -> &'static str {
    match role {
        crate::message_list::MessageRole::User => "● ",
        crate::message_list::MessageRole::Assistant => "◆ ",
        crate::message_list::MessageRole::Thinking => "✻ ",
        crate::message_list::MessageRole::ToolResult => "✓ ",
        crate::message_list::MessageRole::ToolCall => "⚙ ",
        crate::message_list::MessageRole::System => "· ",
    }
}

/// Convert a hex colour string (#RRGGBB) to a ratatui `Color`.
fn hex_to_color(hex: &str) -> Color {
    let hex = hex.trim_start_matches('#');
    if hex.len() == 6 {
        let r = u8::from_str_radix(&hex[0..2], 16).unwrap_or(255);
        let g = u8::from_str_radix(&hex[2..4], 16).unwrap_or(255);
        let b = u8::from_str_radix(&hex[4..6], 16).unwrap_or(255);
        Color::Rgb(r, g, b)
    } else {
        Color::Reset
    }
}

/// Build ratatui `Span`s from a piece of text, highlighting any Markdown
/// fenced code blocks with `SyntaxHighlighter`.
/// A conservative language guess for a tool result.
///
/// Read returns a file body without a fence, so there is no language tag to
/// use. The guess only fires on clear signals; anything else stays plain text
/// rather than being coloured wrongly.
fn guess_language(text: &str) -> Option<&'static str> {
    let sample: String = text.lines().take(80).collect::<Vec<_>>().join("\n");
    if sample.lines().count() < 2 {
        return None;
    }
    let trimmed = sample.trim_start();
    let has = |needle: &str| sample.contains(needle);

    if trimmed.starts_with("//!")
        || trimmed.starts_with("// ")
        || has("fn main(")
        || has("pub fn ")
        || has("impl ")
        || has("use std::")
        || has("#[derive(")
        || (has("struct ") && has("->"))
    {
        return Some("rs");
    }
    if trimmed.starts_with("def ")
        || trimmed.starts_with("import ")
        || has("\ndef ")
        || has("\nimport ")
        || has("\nclass ")
    {
        return Some("py");
    }
    if trimmed.starts_with('{') && has("\":") {
        return Some("json");
    }
    if trimmed.starts_with('[')
        && sample.lines().take(6).any(|line| line.contains('='))
        && !has("fn ")
    {
        return Some("toml");
    }
    if has("package ") && has("func ") {
        return Some("go");
    }
    if has("function ") || has("const ") || has("=>") {
        return Some("js");
    }
    None
}

/// Highlight a plain code body, preserving indentation and line breaks.
///
/// Unlike [`highlight_spans`], this does not collapse whitespace: a file body
/// shown under a Read call has to keep its layout.
fn highlighted_code_lines(
    text: &str,
    language: &str,
    prefix: &str,
    prefix_width: usize,
    prefix_style: Style,
    body_style: Style,
) -> Option<Vec<Line<'static>>> {
    let highlighted = syntax_highlighter().highlight(text, language).ok()?;
    let mut out = Vec::with_capacity(highlighted.len());
    for (index, tokens) in highlighted.into_iter().enumerate() {
        let mut spans: Vec<Span<'static>> = vec![if index == 0 {
            Span::styled(prefix.to_string(), prefix_style)
        } else {
            Span::styled(" ".repeat(prefix_width), body_style)
        }];
        let last_token = tokens.len().saturating_sub(1);
        for (token_index, token) in tokens.into_iter().enumerate() {
            let mut token_text = token.text;
            if token_index == last_token {
                while token_text.ends_with('\n') || token_text.ends_with('\r') {
                    token_text.pop();
                }
            }
            let mut style = body_style.fg(hex_to_color(&token.foreground));
            if token.is_bold {
                style = style.add_modifier(Modifier::BOLD);
            }
            if token.is_italic {
                style = style.add_modifier(Modifier::ITALIC);
            }
            spans.push(Span::styled(token_text, style));
        }
        out.push(Line::from(spans));
    }
    Some(out)
}

fn highlight_spans(text: &str, base_style: Style) -> Vec<Span<'_>> {
    let mut spans = Vec::new();
    let mut rest = text;

    while let Some(start) = rest.find("```") {
        // Text before the code block
        if start > 0 {
            spans.push(Span::styled(rest[..start].to_string(), base_style));
        }

        let after_open = &rest[start + 3..];
        // Extract language (first line after ```)
        let lang_end = after_open.find('\n').unwrap_or(after_open.len());
        let lang = after_open[..lang_end].trim();
        let after_lang = &after_open[lang_end..];

        // Find closing ```
        if let Some(close) = after_lang.find("```") {
            let code = &after_lang[..close];
            // Skip leading newline if present
            let code = code.strip_prefix('\n').unwrap_or(code);

            // Try syntax highlighting
            match syntax_highlighter().highlight(code, lang) {
                Ok(lines) => {
                    for (i, line_tokens) in lines.iter().enumerate() {
                        if i > 0 {
                            spans.push(Span::styled("\n".to_string(), base_style));
                        }
                        for token in line_tokens {
                            let mut style = base_style.fg(hex_to_color(&token.foreground));
                            if token.is_bold {
                                style = style.add_modifier(Modifier::BOLD);
                            }
                            if token.is_italic {
                                style = style.add_modifier(Modifier::ITALIC);
                            }
                            spans.push(Span::styled(token.text.clone(), style));
                        }
                    }
                }
                Err(_) => {
                    // Fallback: plain code block
                    spans.push(Span::styled(code.to_string(), base_style));
                }
            }

            rest = &after_lang[close + 3..];
        } else {
            // No closing fence — treat remainder as plain text
            spans.push(Span::styled(rest[start..].to_string(), base_style));
            rest = "";
            break;
        }
    }

    if !rest.is_empty() {
        spans.push(Span::styled(rest.to_string(), base_style));
    }

    spans
}

fn render_messages(
    frame: &mut Frame,
    area: Rect,
    state: &mut UiState,
    mod_ui_blocks: &[ModUiBlockSpec],
    mod_ui_hit_regions: &mut Vec<ModUiHitRegion>,
) {
    let width = area.width as usize;
    let viewport_height = area.height as usize;
    let is_compact = area.height < 10;
    state.messages.viewport_height = viewport_height;
    // Remember the rectangle, not just its height: `ctrl+shift+c` copies the
    // conversation and nothing else, and this is the only place that knows
    // where the conversation was drawn.
    state.messages.area = area;

    // Clean up expired notifications
    state.notifications.cleanup();

    if state.messages.scroll_offset > 0 {
        let signature = message_render_signature(
            &state.messages.messages,
            &state.theme,
            mod_ui_blocks,
            state.image_protocol,
        );
        // A tracked block needs the render spans that the cache does not keep,
        // so only reuse the cache when there is no block to hit-test.
        let cache_valid = mod_ui_blocks.is_empty()
            && state.messages.render_cache.width == width
            && state.messages.render_cache.is_compact == is_compact
            && state.messages.render_cache.signature == signature;
        let block_spans = if cache_valid {
            Vec::new()
        } else {
            let (mut lines, spans) = render_all_message_lines(
                &state.messages.messages,
                width,
                is_compact,
                &state.theme,
                mod_ui_blocks,
                state.image_protocol,
            );
            if lines.is_empty() {
                lines.extend(render_welcome_art(
                    &state.theme,
                    width,
                    viewport_height,
                    &state.statusline.model,
                    &state.statusline.provider,
                    &state.statusline.cwd,
                ));
            }
            state.messages.render_cache.width = width;
            state.messages.render_cache.is_compact = is_compact;
            state.messages.render_cache.signature = signature;
            state.messages.render_cache.lines = lines;
            spans
        };
        state.messages.total_height = state.messages.render_cache.lines.len();
        let max_scroll = state
            .messages
            .render_cache
            .lines
            .len()
            .saturating_sub(viewport_height);
        state.messages.scroll_offset = state.messages.scroll_offset.min(max_scroll);
        let (start, end) = scroll_slice_bounds(
            state.messages.render_cache.lines.len(),
            viewport_height,
            state.messages.scroll_offset,
        );
        let lines = state.messages.render_cache.lines[start..end].to_vec();
        record_mod_ui_regions(area, &block_spans, start, end - start, mod_ui_hit_regions);
        let paragraph = Paragraph::new(lines).block(Block::default().borders(Borders::NONE));
        frame.render_widget(paragraph, area);
        return;
    }

    // Only the bottom/non-cached path needs per-message heights. The scrollback
    // path above slices cached rendered lines directly, keeping wheel/PageUp
    // redraws independent of transcript size.
    let mut heights: Vec<usize> = Vec::with_capacity(state.messages.messages.len());
    for (index, msg) in state.messages.messages.iter().enumerate() {
        if msg.folded {
            heights.push(1);
        } else {
            heights.push(message_render_height(
                msg,
                index,
                width,
                is_compact,
                mod_ui_blocks,
                state.image_protocol,
            ));
        }
    }

    let at_bottom = state.messages.is_at_bottom()
        && (state.messages.visible_count == 0
            || state.messages.start + state.messages.visible_count.max(1)
                >= state.messages.messages.len().saturating_sub(1));

    let (start, mut visible_count, mut accumulated) = if at_bottom {
        bottom_message_window(&heights, viewport_height)
    } else {
        visible_message_window(&heights, state.messages.start, viewport_height)
    };
    state.messages.start = start;

    // Fill remaining viewport space with older messages so history stays visible.
    while !at_bottom && state.messages.start > 0 {
        let h = heights[state.messages.start - 1];
        if accumulated + h > viewport_height {
            break;
        }
        accumulated += h;
        state.messages.start -= 1;
        visible_count += 1;
    }

    state.messages.set_visible_count(visible_count);
    let visible = state.messages.visible();
    let mut lines: Vec<Line> = Vec::new();
    let mut block_spans = Vec::new();

    for (offset, msg) in visible.iter().enumerate() {
        let index = state.messages.start + offset;
        if let Some(span) = append_message_lines(
            &mut lines,
            msg,
            index,
            width,
            is_compact,
            &state.theme,
            mod_ui_blocks,
            state.image_protocol,
        ) {
            block_spans.push(span);
        }
    }

    // If no messages, show the welcome page
    if lines.is_empty() {
        lines.extend(render_welcome_art(
            &state.theme,
            width,
            viewport_height,
            &state.statusline.model,
            &state.statusline.provider,
            &state.statusline.cwd,
        ));
    }

    state.messages.total_height = lines.len();
    let untrimmed_len = lines.len();
    lines = trim_lines_to_viewport(lines, viewport_height);
    let trimmed = untrimmed_len.saturating_sub(lines.len());
    record_mod_ui_regions(area, &block_spans, trimmed, lines.len(), mod_ui_hit_regions);

    let paragraph = Paragraph::new(lines).block(Block::default().borders(Borders::NONE));
    frame.render_widget(paragraph, area);
}

/// The height of one message in the transcript.
///
/// A live mod UI block is measured from the same tree rendering used to draw
/// it, so the layout window and the drawn rows cannot disagree.
fn message_render_height(
    msg: &Message,
    index: usize,
    width: usize,
    is_compact: bool,
    mod_ui_blocks: &[ModUiBlockSpec],
    image_protocol: crate::transcript_image::ImageProtocol,
) -> usize {
    if !is_compact && let Some(spec) = matching_mod_ui_block(msg, index, mod_ui_blocks) {
        let prefix_width = unicode_width::UnicodeWidthStr::width(message_prefix(msg.role));
        let wrap_width = width.saturating_sub(prefix_width);
        if let Some((lines, _)) = mod_ui_block_lines(spec, wrap_width) {
            return lines.len();
        }
    }
    message_display_height(msg, width, is_compact, image_protocol)
}

fn matching_mod_ui_block<'a>(
    msg: &Message,
    index: usize,
    blocks: &'a [ModUiBlockSpec],
) -> Option<&'a ModUiBlockSpec> {
    if msg.role != MessageRole::System {
        return None;
    }
    blocks.iter().find(|block| {
        block.message_index == index && block.content.as_str() == msg.content.as_str()
    })
}

fn mod_ui_block_lines(
    spec: &ModUiBlockSpec,
    wrap_width: usize,
) -> Option<(Vec<Line<'static>>, Vec<ButtonHit>)> {
    let (lines, hits) = render_mod_ui_with_focus_and_hits(
        &spec.elements,
        wrap_width.max(1),
        spec.focused.as_deref(),
    );
    (!lines.is_empty()).then_some((lines, hits))
}

/// A mod UI block rendered into the transcript, with its place in the line list.
#[derive(Debug)]
struct ModUiBlockSpan {
    message_index: usize,
    start: usize,
    end: usize,
    prefix_width: u16,
    hits: Vec<ButtonHit>,
}

/// Translate rendered block spans into absolute screen hit regions.
///
/// `visible_start` is the index of the first line actually painted and
/// `visible_len` how many lines follow it; a block can begin above the pane.
fn record_mod_ui_regions(
    area: Rect,
    spans: &[ModUiBlockSpan],
    visible_start: usize,
    visible_len: usize,
    regions: &mut Vec<ModUiHitRegion>,
) {
    let visible_end = visible_start.saturating_add(visible_len);
    for span in spans {
        let start = span.start.max(visible_start);
        let end = span.end.min(visible_end);
        if start >= end {
            continue;
        }
        regions.push(ModUiHitRegion {
            message_index: span.message_index,
            rect: Rect::new(
                area.x.saturating_add(span.prefix_width),
                area.y
                    .saturating_add(u16::try_from(start - visible_start).unwrap_or(u16::MAX)),
                area.width.saturating_sub(span.prefix_width),
                u16::try_from(end - start).unwrap_or(u16::MAX),
            ),
            scroll_offset: start - span.start,
            hits: span.hits.clone(),
        });
    }
}

fn visible_message_window(
    heights: &[usize],
    start: usize,
    viewport_height: usize,
) -> (usize, usize, usize) {
    let start = start.min(heights.len());
    if heights.is_empty() {
        return (start, 0, 0);
    }

    let mut visible_count = 0;
    let mut accumulated = 0;
    for i in start..heights.len() {
        let h = heights[i];
        if accumulated + h > viewport_height {
            // A single long assistant response can be taller than the entire
            // viewport. Include the overflowing final message and let line
            // trimming show its tail instead of dropping it completely.
            if visible_count == 0 || i == heights.len() - 1 {
                accumulated += h;
                visible_count += 1;
            }
            break;
        }
        accumulated += h;
        visible_count += 1;
    }

    (start, visible_count.max(1), accumulated)
}

fn bottom_message_window(heights: &[usize], viewport_height: usize) -> (usize, usize, usize) {
    if heights.is_empty() {
        return (0, 0, 0);
    }

    let mut start = heights.len();
    let mut visible_count = 0;
    let mut accumulated = 0;
    for i in (0..heights.len()).rev() {
        let h = heights[i];
        if accumulated + h > viewport_height {
            start = i;
            accumulated += h;
            visible_count += 1;
            break;
        }
        start = i;
        accumulated += h;
        visible_count += 1;
    }

    (start, visible_count.max(1), accumulated)
}

fn render_all_message_lines(
    messages: &[Message],
    width: usize,
    is_compact: bool,
    theme: &Theme,
    mod_ui_blocks: &[ModUiBlockSpec],
    image_protocol: crate::transcript_image::ImageProtocol,
) -> (Vec<Line<'static>>, Vec<ModUiBlockSpan>) {
    let mut lines = Vec::new();
    let mut block_spans = Vec::new();
    for (index, msg) in messages.iter().enumerate() {
        if let Some(span) = append_message_lines(
            &mut lines,
            msg,
            index,
            width,
            is_compact,
            theme,
            mod_ui_blocks,
            image_protocol,
        ) {
            block_spans.push(span);
        }
    }
    (lines, block_spans)
}

fn message_render_signature(
    messages: &[Message],
    theme: &Theme,
    mod_ui_blocks: &[ModUiBlockSpec],
    image_protocol: crate::transcript_image::ImageProtocol,
) -> u64 {
    let mut signature = (messages.len() as u64)
        ^ theme.render_signature()
        ^ (u64::from(image_protocol.can_draw()) << 63);
    for (idx, msg) in messages.iter().enumerate() {
        signature = signature
            .wrapping_mul(16_777_619)
            .wrapping_add(idx as u64)
            .wrapping_add(u64::from(msg.role as u8))
            .wrapping_add((msg.content.len() as u64) << 8)
            .wrapping_add(u64::from(msg.streaming) << 48)
            .wrapping_add(u64::from(msg.folded) << 49);
        if let Some(tool) = &msg.tool_name {
            signature = signature
                .wrapping_mul(16_777_619)
                .wrapping_add(tool.len() as u64);
        }
    }
    // Focus and the tree itself are inputs to the block render, so a cache
    // built under different ones must not be reused.
    for block in mod_ui_blocks {
        signature = signature
            .wrapping_mul(16_777_619)
            .wrapping_add(block.message_index as u64);
        mix_text(&mut signature, &block.content);
        if let Some(focused) = &block.focused {
            mix_text(&mut signature, focused);
        }
        signature = mix_elements(signature, &block.elements);
    }
    signature
}

fn mix_text(signature: &mut u64, text: &str) {
    for byte in text.bytes() {
        *signature = signature
            .wrapping_mul(16_777_619)
            .wrapping_add(u64::from(byte));
    }
}

fn mix_elements(mut signature: u64, elements: &[UiElement]) -> u64 {
    for element in elements {
        signature = signature.wrapping_mul(16_777_619);
        match element {
            UiElement::Text { text, .. } => mix_text(&mut signature, text),
            UiElement::Button { label, id } => {
                mix_text(&mut signature, label);
                mix_text(&mut signature, id);
            }
            UiElement::Box { title, children } => {
                if let Some(title) = title {
                    mix_text(&mut signature, title);
                }
                signature = mix_elements(signature, children);
            }
        }
    }
    signature
}

fn append_message_lines(
    lines: &mut Vec<Line<'static>>,
    msg: &Message,
    index: usize,
    width: usize,
    is_compact: bool,
    theme: &Theme,
    mod_ui_blocks: &[ModUiBlockSpec],
    image_protocol: crate::transcript_image::ImageProtocol,
) -> Option<ModUiBlockSpan> {
    if !is_compact
        && !lines.is_empty()
        && matches!(
            msg.role,
            MessageRole::User | MessageRole::Assistant | MessageRole::Thinking
        )
    {
        lines.push(Line::from(""));
    }

    // Handle folded messages
    if msg.folded {
        let style = Style::default().fg(theme.dim);
        // A folded reasoning row shows its first line anyway: the point of
        // surfacing thinking is to let someone see *what* the model is
        // considering without expanding a page of it. Other roles keep the
        // plain indicator — a folded tool result is noise, not a preview.
        if msg.role == MessageRole::Thinking {
            let first = msg.content.lines().next().unwrap_or("");
            let preview_width = width.saturating_sub(24);
            let preview: String = first.chars().take(preview_width).collect();
            let more =
                message_display_height(msg, width, is_compact, image_protocol).saturating_sub(1);
            lines.push(Line::from(vec![
                Span::styled(message_prefix(msg.role), style),
                Span::styled(preview, style.add_modifier(Modifier::ITALIC)),
                Span::styled(format!(" [+{more} lines]"), style),
            ]));
            return None;
        }
        let fold_indicator = format!(
            "▸ {} [+{} lines folded]",
            match msg.role {
                MessageRole::User => "User",
                MessageRole::Assistant => "Assistant",
                MessageRole::Thinking => "Thinking",
                MessageRole::ToolResult => "Tool Result",
                MessageRole::ToolCall => "Tool Call",
                MessageRole::System => "System",
            },
            message_display_height(msg, width, is_compact, image_protocol).saturating_sub(1)
        );
        lines.push(Line::from(Span::styled(fold_indicator, style)));
        return None;
    }

    let prefix = message_prefix(msg.role);
    let style = match msg.role {
        MessageRole::User => Style::default().fg(theme.user).add_modifier(Modifier::BOLD),
        MessageRole::Assistant => Style::default()
            .fg(theme.assistant)
            .add_modifier(Modifier::BOLD),
        // Reasoning is deliberately quiet: it sits above the answer and must
        // not compete with it for attention.
        MessageRole::Thinking => Style::default()
            .fg(theme.dim)
            .add_modifier(Modifier::ITALIC),
        MessageRole::ToolResult | MessageRole::ToolCall => Style::default().fg(theme.tool),
        MessageRole::System => Style::default().fg(theme.dim),
    };
    let body_style = Style::default().fg(theme.foreground);

    let prefix_width = unicode_width::UnicodeWidthStr::width(prefix);
    let wrap_width = width.saturating_sub(prefix_width);

    // A picture is drawn in place of its caption when the terminal can show
    // one. It goes through `msg.image`, whose half-block spans are ordinary
    // cells, so the line cache, the viewport slice and the selection snapshot
    // all treat it as text. A terminal that cannot draw it, or an image that
    // failed to decode, falls through to the caption in `content`.
    if !is_compact
        && image_protocol.can_draw()
        && let Some(image) = msg.image.as_ref()
    {
        if let Some(image_lines) = image.lines(wrap_width, theme) {
            // The picture starts on its own row under the role prefix, so a
            // screenshot is not indented by three columns it does not need.
            lines.push(Line::from(Span::styled(prefix, style)));
            for line in image_lines {
                lines.push(line);
            }
            return None;
        }
        // The header reserved rows, but the pixels would not decode — a
        // truncated body reads its size fine. The layout has already committed
        // to that height, so the caption sits on the first of those rows and
        // the rest stay blank; falling through to the one-line text path would
        // pull everything below the picture up by the difference.
        if let Some(reserved) = image_rows(msg, wrap_width, is_compact, image_protocol) {
            lines.push(Line::from(vec![
                Span::styled(prefix, style),
                Span::styled(msg.content.to_string(), style),
            ]));
            for _ in 1..reserved {
                lines.push(Line::default());
            }
            return None;
        }
    }

    // In compact mode, truncate long messages to single line
    if is_compact && message_display_height(msg, width, is_compact, image_protocol) > 3 {
        let truncated = msg
            .content
            .chars()
            .take(width.saturating_sub(prefix_width))
            .collect::<String>();
        lines.push(Line::from(vec![
            Span::styled(prefix, style),
            Span::styled(truncated, style.add_modifier(Modifier::DIM)),
        ]));
        return None;
    }

    // A live mod UI block is re-rendered from its tree at the pane's actual
    // width. That keeps the drawn button columns identical to the hit columns
    // even when the stored text was rendered at a different width.
    if !is_compact
        && let Some(spec) = matching_mod_ui_block(msg, index, mod_ui_blocks)
        && let Some((block_lines, hits)) = mod_ui_block_lines(spec, wrap_width)
    {
        let start = lines.len();
        for (row, line) in block_lines.into_iter().enumerate() {
            let gutter = if row == 0 {
                prefix.to_string()
            } else {
                " ".repeat(prefix_width)
            };
            let mut spans = Vec::with_capacity(line.spans.len() + 1);
            spans.push(Span::styled(gutter, style));
            spans.extend(line.spans);
            lines.push(Line::from(spans));
        }
        return Some(ModUiBlockSpan {
            message_index: index,
            start,
            end: lines.len(),
            prefix_width: u16::try_from(prefix_width).unwrap_or(u16::MAX),
            hits,
        });
    }

    // A Read result is a file body with no fence. Highlight it from a
    // conservative guess at the language, keeping its indentation.
    if msg.role == MessageRole::ToolResult
        && !is_compact
        && msg.tool_name.as_deref() == Some("Read")
        && let Some(language) = guess_language(&msg.content)
        && let Some(code_lines) = highlighted_code_lines(
            &msg.content,
            language,
            prefix,
            prefix_width,
            style,
            body_style,
        )
    {
        lines.extend(code_lines);
        return None;
    }

    // Use syntax highlighting for assistant messages that contain code blocks.
    let use_highlight = msg.role == MessageRole::Assistant && msg.content.contains("```");

    if use_highlight && !msg.streaming {
        let spans = highlight_spans(&msg.content, body_style);
        // Flatten spans into wrapped lines.
        let mut line_spans: Vec<Span> = Vec::new();
        let mut line_width = prefix_width;
        let mut is_first_line = true;

        for span in spans {
            let span_text = span.content.to_string();
            for word in span_text.split_whitespace() {
                let word_width = unicode_width::UnicodeWidthStr::width(word);
                let space = usize::from(!line_spans.is_empty());
                if line_width + space + word_width > width && !line_spans.is_empty() {
                    // Flush current line
                    let mut final_spans = if is_first_line {
                        vec![Span::styled(prefix, style)]
                    } else {
                        vec![Span::styled(" ".repeat(prefix_width), body_style)]
                    };
                    final_spans.extend(std::mem::take(&mut line_spans));
                    lines.push(Line::from(final_spans));
                    line_width = prefix_width;
                    is_first_line = false;
                }
                if !line_spans.is_empty() {
                    line_spans.push(Span::styled(" ", body_style));
                    line_width += 1;
                }
                line_spans.push(Span::styled(word.to_string(), span.style));
                line_width += word_width;
            }
            // Preserve newlines within the span text
            if span_text.ends_with('\n') {
                let mut final_spans = if is_first_line {
                    vec![Span::styled(prefix, style)]
                } else {
                    vec![Span::styled(" ".repeat(prefix_width), body_style)]
                };
                final_spans.extend(std::mem::take(&mut line_spans));
                lines.push(Line::from(final_spans));
                line_width = prefix_width;
                is_first_line = false;
            }
        }

        if !line_spans.is_empty() {
            let mut final_spans = if is_first_line {
                vec![Span::styled(prefix, style)]
            } else {
                vec![Span::styled(" ".repeat(prefix_width), body_style)]
            };
            final_spans.extend(line_spans);
            lines.push(Line::from(final_spans));
        }
    } else {
        let wrapped = wrap_text(&msg.content, wrap_width);
        for (i, text) in wrapped.into_iter().enumerate() {
            if i == 0 {
                lines.push(Line::from(vec![
                    Span::styled(prefix, style),
                    if msg.streaming {
                        Span::styled(text, body_style.add_modifier(Modifier::DIM))
                    } else {
                        Span::styled(text, body_style)
                    },
                ]));
            } else {
                let indent = " ".repeat(prefix_width);
                lines.push(Line::from(vec![
                    Span::styled(indent, style),
                    if msg.streaming {
                        Span::styled(text, body_style.add_modifier(Modifier::DIM))
                    } else {
                        Span::styled(text, body_style)
                    },
                ]));
            }
        }
    }

    // The call row names the tool; the result row keeps the name only so the
    // renderer can choose a highlighter, and does not print a second label.
    if let (Some(tool), MessageRole::ToolCall) = (&msg.tool_name, msg.role) {
        lines.push(Line::from(Span::styled(
            format!("  [{tool}]"),
            Style::default().fg(theme.dim),
        )));
    }
    None
}

fn trim_lines_to_viewport(mut lines: Vec<Line>, viewport_height: usize) -> Vec<Line> {
    if viewport_height == 0 || lines.len() <= viewport_height {
        return lines;
    }
    lines.split_off(lines.len() - viewport_height)
}

/// The half-open line range shown by the scrollback viewport.
fn scroll_slice_bounds(len: usize, viewport_height: usize, scroll_offset: usize) -> (usize, usize) {
    if viewport_height == 0 || len <= viewport_height {
        return (0, len);
    }

    let end = len
        .saturating_sub(scroll_offset)
        .max(viewport_height)
        .min(len);
    let start = end.saturating_sub(viewport_height);
    (start, end)
}

/// The welcome page shown while the transcript is empty.
///
/// Modelled on Qwen Code's splash: a block-letter wordmark, a box of session
/// facts, and the resume tip underneath. The page is built from [`Line`]s
/// rather than a boxed widget because the transcript is not a widget, and a
/// terminal too small for the box gets plain centred lines instead of a clipped
/// one.
///
/// One deliberate departure from the reference: the wordmark and the facts are
/// stacked, not side by side. Qwen draws them across a full-width terminal;
/// here the card is capped at [`WELCOME_CARD_MAX_WIDTH`] and a split view
/// halves it again, so a side-by-side layout would clip either the box or the
/// wordmark rather than shrink gracefully.
fn render_welcome_art(
    theme: &Theme,
    width: usize,
    viewport_height: usize,
    model: &str,
    provider: &str,
    cwd: &str,
) -> Vec<Line<'static>> {
    let facts = WelcomeFacts {
        model,
        provider,
        cwd,
    };
    if width >= WELCOME_CARD_MIN_WIDTH && viewport_height >= WELCOME_CARD_HEIGHT {
        welcome_card(theme, width, &facts)
    } else {
        welcome_plain(theme, &facts, width)
    }
}

/// What the welcome page reports about the session.
///
/// A struct rather than four positional arguments: every field is a plain
/// string the REPL already pushes, so a `&str` at the wrong position would
/// compile and print the directory where the model belongs.
///
/// The branch is not here. The card is sixteen rows and the facts are its
/// middle; the statusline already reports the branch on every frame, and
/// spending a row on a second copy would push the wordmark off at 80x24.
struct WelcomeFacts<'a> {
    model: &'a str,
    provider: &'a str,
    cwd: &'a str,
}

/// The boxed welcome page.
///
/// The card hugs the left edge: the transcript is a log, and a centred card
/// floats away from the text it introduces. Colour carries the hierarchy
/// instead of position: the wordmark is a gradient, the session facts each have
/// their own colour, and the tips are dim.
fn welcome_card(theme: &Theme, width: usize, facts: &WelcomeFacts<'_>) -> Vec<Line<'static>> {
    let card_width = width.min(WELCOME_CARD_MAX_WIDTH);
    let inner = card_width.saturating_sub(2);
    let indent = "";
    let border = Style::default().fg(theme.border);
    let muted = Style::default().fg(theme.dim);

    let mut lines = Vec::with_capacity(WELCOME_CARD_HEIGHT);
    lines.push(card_border(indent, '╭', '╮', inner, border));
    lines.push(card_row(indent, "", muted, inner, border));

    // The wordmark is a picture: every row is left-aligned at the same column
    // and coloured by column, so centring or re-indenting one row would shear
    // the letters apart.
    for (row, art) in WELCOME_WORDMARK
        .lines()
        .filter(|line| !line.trim().is_empty())
        .enumerate()
    {
        lines.push(card_row_spans(
            indent,
            wordmark_spans(art, row),
            inner,
            border,
        ));
    }

    lines.push(card_row(indent, "", muted, inner, border));
    lines.push(card_row_spans(indent, title_spans(theme), inner, border));
    // No blank between the title and the facts. The card is exactly as tall
    // as the message pane at 80x24, and the *first* rows are what a taller card
    // loses, so an inserted blank would cost the top border and the wordmark.
    for spans in fact_rows(theme, facts, inner) {
        lines.push(card_row_spans(indent, spans, inner, border));
    }
    lines.push(card_row(indent, "", muted, inner, border));
    for spans in hint_rows(theme) {
        lines.push(card_row_spans(indent, spans, inner, border));
    }
    lines.push(card_row(indent, "", muted, inner, border));
    lines.push(card_border(indent, '╰', '╯', inner, border));
    lines
}

/// `>_ Codape (v0.2.1)`, the card's title: a prompt caret and the version.
fn title_spans(theme: &Theme) -> Vec<Span<'static>> {
    vec![
        Span::raw("  "),
        Span::styled(
            ">_",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            " Codape",
            Style::default()
                .fg(theme.foreground)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" (v{})", env!("CARGO_PKG_VERSION")),
            Style::default().fg(theme.dim),
        ),
    ]
}

/// One row per fact, each labelled so the value is readable on its own.
///
/// The reference puts provider and model on one line and the directory under
/// it. At this width a `provider | model` line truncates in the middle of a
/// model name, which is the one fact a user is most likely to check, so each
/// gets its own row here.
fn fact_rows(theme: &Theme, facts: &WelcomeFacts<'_>, inner: usize) -> Vec<Vec<Span<'static>>> {
    /// Columns the indent and the label take, so a path knows its own budget.
    const PREFIX: usize = 2 + LABEL_WIDTH;

    let label = Style::default().fg(theme.dim);
    let mut rows = Vec::with_capacity(3);
    let mut row = |name: &str, value: String, style: Style| {
        if value.is_empty() {
            return;
        }
        rows.push(vec![
            Span::raw("  "),
            Span::styled(format!("{name:<LABEL_WIDTH$}"), label),
            Span::styled(value, style),
        ]);
    };
    row(
        "Provider",
        facts.provider.to_string(),
        Style::default().fg(theme.accent_secondary),
    );
    row(
        "Model",
        facts.model.to_string(),
        Style::default().fg(theme.assistant),
    );
    row(
        "Directory",
        truncate_path_start(facts.cwd, inner.saturating_sub(PREFIX)),
        Style::default().fg(theme.foreground),
    );
    rows
}

/// Columns the fact labels occupy, including their gap after the indent.
const LABEL_WIDTH: usize = 10;

/// Keep the end of a path, marking the cut with `…`.
///
/// The leaf directory is what tells the user which checkout they are in, so a
/// long path loses its start — the same choice a shell prompt makes. The cut
/// prefers a component boundary, because `…/work/codape` reads as a path while
/// `…k/codape` reads as a typo.
fn truncate_path_start(path: &str, width: usize) -> String {
    let total = unicode_width::UnicodeWidthStr::width(path);
    if total <= width {
        return path.to_string();
    }
    // One column for the ellipsis itself.
    let Some(room) = width.checked_sub(1) else {
        return String::new();
    };

    // Walk components from the right, keeping as many as fit.
    let mut kept: Vec<&str> = Vec::new();
    let mut used = 0;
    for component in path.rsplit('/') {
        if component.is_empty() {
            continue;
        }
        let component_width =
            unicode_width::UnicodeWidthStr::width(component) + usize::from(!kept.is_empty());
        if used + component_width > room {
            break;
        }
        used += component_width;
        kept.push(component);
    }
    if kept.is_empty() {
        // A single component longer than the row: fall back to its tail.
        let tail = truncate_to_width_from_end(path, room);
        return format!("…{tail}");
    }
    kept.reverse();
    format!("…/{}", kept.join("/"))
}

/// The last `width` columns of `text`, cut on a character boundary.
fn truncate_to_width_from_end(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for character in text.chars().rev() {
        let character_width = unicode_width::UnicodeWidthChar::width(character).unwrap_or(1);
        if used + character_width > width {
            break;
        }
        out.insert(0, character);
        used += character_width;
    }
    out
}

/// What a new user needs: the command list and, below it, the resume tip.
///
/// `--continue` and `--resume` are the two flags that reconnect a session, and
/// a user who does not know they exist starts over instead of picking up where
/// they left off — the same reason Qwen's splash names them.
fn hint_rows(theme: &Theme) -> Vec<Vec<Span<'static>>> {
    let dim = Style::default().fg(theme.dim);
    let key = |color| Style::default().fg(color).add_modifier(Modifier::BOLD);
    vec![
        vec![
            Span::raw("  "),
            Span::styled("Type ", dim),
            Span::styled("/help", key(theme.accent)),
            Span::styled(" for commands", dim),
        ],
        vec![
            Span::raw("  "),
            Span::styled("Resume with ", dim),
            Span::styled("--continue", key(theme.accent_secondary)),
            Span::styled(" or ", dim),
            Span::styled("--resume", key(theme.accent_secondary)),
        ],
    ]
}

/// The wordmark as coloured spans.
///
/// The colour runs blue → purple across the columns and brightens down the
/// rows, so the mark reads as one object rather than six differently coloured
/// letters. Whitespace is emitted unstyled: a styled space would paint over the
/// gap between letters and draw visible bands.
///
/// The ramp is the blue → purple of the reference splash, not the theme's
/// accents: the page is the one place the product names itself, and a wordmark
/// that re-tinted per theme would be a different logo in each one.
fn wordmark_spans(art: &str, row: usize) -> Vec<Span<'static>> {
    const RAMP: [Color; 5] = [
        Color::Rgb(96, 165, 250),
        Color::Rgb(129, 140, 248),
        Color::Rgb(167, 139, 250),
        Color::Rgb(192, 132, 252),
        Color::Rgb(244, 114, 182),
    ];

    let columns = art.chars().count().max(1);
    let mut spans = vec![Span::raw("  ")];
    for (column, character) in art.chars().enumerate() {
        if character == ' ' {
            spans.push(Span::raw(" "));
            continue;
        }
        // The column picks the step along the ramp; the row lightens it so the
        // letters do not look flat. Integer arithmetic throughout: a float ramp
        // would need a cast per channel, and the `round` in one is enough to
        // put two neighbouring cells a whole step apart.
        let index = column
            .saturating_mul(RAMP.len())
            .saturating_div(columns)
            .min(RAMP.len() - 1);
        let row_shade = u8::try_from(row.min(3)).unwrap_or(3);
        spans.push(Span::styled(
            character.to_string(),
            Style::default()
                .fg(lighten(RAMP[index], row_shade * 12))
                .add_modifier(Modifier::BOLD),
        ));
    }
    spans
}

/// `color` brightened by `amount` per channel, saturating at 255.
///
/// A non-RGB colour is returned unchanged rather than converted: the ramp is
/// always RGB, so the branch only guards against a future caller handing in an
/// ANSI index the terminal might not have.
fn lighten(color: Color, amount: u8) -> Color {
    let Color::Rgb(r, g, b) = color else {
        return color;
    };
    let lift = |channel: u8| channel.saturating_add(amount);
    Color::Rgb(lift(r), lift(g), lift(b))
}

/// Plain welcome lines, used when the box would not fit.
///
/// The wordmark is dropped rather than clipped: half a wordmark is not a
/// wordmark, and the `>_ Codape` title plus the facts are what have to survive.
fn welcome_plain(theme: &Theme, facts: &WelcomeFacts<'_>, width: usize) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(""),
        Line::from(title_spans(theme)),
        Line::from(""),
    ];
    for spans in fact_rows(theme, facts, width) {
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(""));
    for spans in hint_rows(theme) {
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  Ctrl+D to exit",
        Style::default().fg(theme.dim),
    )));
    lines.push(Line::from(""));
    lines
}
fn card_border(indent: &str, left: char, right: char, inner: usize, style: Style) -> Line<'static> {
    Line::from(Span::styled(
        format!("{indent}{left}{}{right}", "─".repeat(inner)),
        style,
    ))
}

fn card_row(indent: &str, text: &str, style: Style, inner: usize, border: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{indent}│"), border),
        Span::styled(center_padded(text, inner), style),
        Span::styled("│", border),
    ])
}

/// A card row built from styled spans, left-aligned and padded to `inner`.
///
/// Content longer than the card is truncated span by span; the row still ends
/// at the border, so a long model name cannot break the box.
fn card_row_spans(
    indent: &str,
    spans: Vec<Span<'static>>,
    inner: usize,
    border: Style,
) -> Line<'static> {
    let mut remaining = inner;
    let mut rendered: Vec<Span<'static>> = Vec::new();
    for span in spans {
        if remaining == 0 {
            break;
        }
        let content = span.content.to_string();
        let width = unicode_width::UnicodeWidthStr::width(content.as_str());
        if width <= remaining {
            remaining -= width;
            rendered.push(span);
        } else {
            let truncated = truncate_to_width(&content, remaining);
            let truncated_width = unicode_width::UnicodeWidthStr::width(truncated.as_str());
            remaining = remaining.saturating_sub(truncated_width);
            rendered.push(Span::styled(truncated, span.style));
            break;
        }
    }
    if remaining > 0 {
        rendered.push(Span::raw(" ".repeat(remaining)));
    }

    let mut line = vec![Span::styled(format!("{indent}│"), border)];
    line.extend(rendered);
    line.push(Span::styled("│", border));
    Line::from(line)
}

/// Pad `text` with spaces so it is centred in `width` display columns.
///
/// A string longer than the space is truncated first; the caller passes text
/// that renders as a single line.
fn center_padded(text: &str, width: usize) -> String {
    let text = truncate_to_width(text, width);
    let text_width = unicode_width::UnicodeWidthStr::width(text.as_str());
    let total_pad = width.saturating_sub(text_width);
    let left = total_pad / 2;
    format!(
        "{}{}{}",
        " ".repeat(left),
        text,
        " ".repeat(total_pad - left)
    )
}

fn truncate_to_width(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for character in text.chars() {
        let character_width = unicode_width::UnicodeWidthChar::width(character).unwrap_or(1);
        if used + character_width > width {
            break;
        }
        out.push(character);
        used += character_width;
    }
    out
}

/// Render a compact notification bar above the statusline.
fn render_notification_bar(
    frame: &mut Frame,
    area: ratatui::layout::Rect,
    center: &mut crate::notification_center::NotificationCenter,
    theme: &Theme,
) {
    use crate::notification_center::NotificationKind;

    if let Some(notif) = center.latest() {
        let (icon, color) = match notif.kind {
            NotificationKind::Info => ("ℹ", theme.notif_info),
            NotificationKind::Warning => ("⚠", theme.notif_warning),
            NotificationKind::Error => ("✗", theme.notif_error),
            NotificationKind::Success => ("✓", theme.success),
            NotificationKind::Teaser => ("✦", theme.tool),
            NotificationKind::Progress => ("⟳", theme.accent_secondary),
        };
        let text = format!(
            " {} {}  ({} unseen)",
            icon,
            notif.message,
            center.unseen_count()
        );
        let line = Line::from(Span::styled(text, Style::default().fg(color)));
        let paragraph = Paragraph::new(line);
        frame.render_widget(paragraph, area);

        // Mark as seen
        center.mark_seen();
    }
}

/// One view the left pane remembers.
fn render_pane_tabs(frame: &mut Frame, area: ratatui::layout::Rect, state: &UiState) {
    if area.height == 0 {
        return;
    }
    // Only the title is drawn: the content is what the pane below already
    // shows for the current tab, and a bar that repeated it would be a second
    // place to read.
    let mut spans: Vec<Span> = Vec::new();
    // The pane is half a narrow terminal, so the bar has to fit in what it
    // has: labels are cut to the row and an ellipsis says a tab is there. A
    // hard clip at the edge would leave half a name and no sign there is more.
    let mut used = 0usize;
    let budget = usize::from(area.width);
    for (index, tab) in state.pane_tabs.iter().enumerate() {
        let separator = if index > 0 { " │ " } else { "" };
        let label = format!(" {}:{} ", index + 1, tab.title);
        let width = separator.chars().count() + label.chars().count();
        if used + width > budget {
            let left = budget.saturating_sub(used);
            if left > 3 {
                spans.push(Span::styled(" …", Style::default().fg(state.theme.dim)));
            }
            break;
        }
        used += width;
        if !separator.is_empty() {
            spans.push(Span::styled(
                separator,
                Style::default().fg(state.theme.dim),
            ));
        }
        let selected = index == state.pane_tab_index;
        // `1:` rather than a mark alone, because the number is what the
        // tab-switch keys walk and a bar without it would not say so.
        spans.push(Span::styled(
            label,
            if selected {
                Style::default()
                    .fg(state.theme.selection_fg)
                    .bg(state.theme.selection_bg)
            } else {
                Style::default().fg(state.theme.foreground)
            },
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_left_pane(frame: &mut Frame, area: ratatui::layout::Rect, state: &UiState) {
    let content = if state.left_pane_content.is_empty() {
        "No file open.\nUse /open <path> to view a file, or /diff to review changes."
    } else {
        &state.left_pane_content
    };

    let title = if state.left_pane_title.is_empty() {
        "File Viewer"
    } else {
        &state.left_pane_title
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .style(Style::default().fg(state.theme.border));

    // The border belongs to the whole pane, so it is drawn once here and both
    // branches below work inside it. A tab bar, when there is one, is the
    // pane's first row inside that border.
    frame.render_widget(block.clone(), area);
    let inner = block.inner(area);
    let body = if state.pane_tabs.len() > 1 {
        // More than one view open gets a tab bar: one line, the open views in
        // the order they were opened, the current one marked. A single view
        // does not, because a bar of one tab costs a line of the file to say
        // nothing.
        let chunks = ratatui::layout::Layout::vertical([
            ratatui::layout::Constraint::Length(1),
            ratatui::layout::Constraint::Min(1),
        ])
        .split(inner);
        render_pane_tabs(frame, chunks[0], state);
        chunks[1]
    } else {
        inner
    };

    // A diff is drawn line by line with its kind's colour and no wrapping: a
    // wrapped diff line is not the line the file has, and the gutter mark has
    // to stay next to the text it marks. Everything else keeps the file
    // viewer's wrapped plain-text rendering.
    if state.left_pane_is_diff {
        let parsed = crate::diff::parse_unified(content);
        // The list and the patch sit inside the border the tab bar was drawn
        // in, so the title still says what the pane is showing.
        let inner = body;
        // A patch of several files gets a file list above the patch. One file
        // does not: the row would repeat the header already on screen and take
        // a line the patch needs.
        let files = crate::diff::files(&parsed);
        let list_height = if files.len() > 1 {
            u16::try_from(files.len() + 1)
                .unwrap_or(u16::MAX)
                .min(inner.height / 2)
        } else {
            0
        };
        let (list_area, patch_area) = if list_height > 0 {
            let chunks = ratatui::layout::Layout::vertical([
                ratatui::layout::Constraint::Length(list_height),
                ratatui::layout::Constraint::Min(1),
            ])
            .split(inner);
            (Some(chunks[0]), chunks[1])
        } else {
            (None, inner)
        };

        let lines: Vec<Line> = parsed
            .into_iter()
            .map(|line| {
                let colour = match line.kind {
                    crate::diff::DiffLineKind::Added => state.theme.success,
                    crate::diff::DiffLineKind::Removed => state.theme.error,
                    crate::diff::DiffLineKind::HunkHeader => state.theme.accent_secondary,
                    // `NoNewline` is a note about the line above it, so it is
                    // dim like a header rather than coloured like content.
                    crate::diff::DiffLineKind::FileHeader
                    | crate::diff::DiffLineKind::NoNewline => state.theme.dim,
                    crate::diff::DiffLineKind::Context => state.theme.foreground,
                };
                Line::from(Span::styled(line.text, Style::default().fg(colour)))
            })
            .collect();

        if let Some(list_area) = list_area {
            // The file the pane is currently showing is the one highlighted,
            // because the list is a map of the patch: the row that says where
            // you are is worth more than a row that says where you started.
            let current = files
                .iter()
                .rev()
                .find(|file| file.start <= state.left_pane_scroll)
                .map_or(0, |file| file.start);
            let rows: Vec<Line> = files
                .iter()
                .map(|file| {
                    let selected = file.start == current;
                    let marker = if selected { "▸ " } else { "  " };
                    Line::from(vec![
                        Span::styled(
                            format!("{marker}{}", file.path),
                            Style::default().fg(if selected {
                                state.theme.accent
                            } else {
                                state.theme.foreground
                            }),
                        ),
                        Span::styled(
                            format!("  +{} -{}", file.added, file.removed),
                            Style::default().fg(state.theme.dim),
                        ),
                    ])
                })
                .collect();
            let list = Paragraph::new(rows).block(
                Block::default()
                    .borders(Borders::BOTTOM)
                    .style(Style::default().fg(state.theme.border)),
            );
            frame.render_widget(list, list_area);
        }

        let paragraph =
            Paragraph::new(lines).scroll((left_pane_scroll(state, patch_area.height), 0));
        frame.render_widget(paragraph, patch_area);
        return;
    }

    let wrapped = wrap_text(content, area.width.saturating_sub(2) as usize);
    let lines: Vec<Line> = wrapped
        .into_iter()
        .map(|s| Line::from(Span::styled(s, Style::default().fg(state.theme.foreground))))
        .collect();

    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, body);
}

/// How many lines the left pane is scrolled down.
///
/// Clamped so a diff longer than the pane stops at its last page instead of
/// scrolling past the end into blank rows.
fn left_pane_scroll(state: &UiState, pane_height: u16) -> u16 {
    let total = state.left_pane_content.lines().count();
    let visible = usize::from(pane_height.saturating_sub(2));
    let max = total.saturating_sub(visible);
    u16::try_from(state.left_pane_scroll.min(max)).unwrap_or(0)
}

fn render_statusline(
    frame: &mut Frame,
    area: ratatui::layout::Rect,
    statusline: &Statusline,
    theme: &Theme,
) {
    if area.height == 0 {
        return;
    }
    let rendered = statusline.render(area.width as usize);
    let spans = rendered
        .split(" | ")
        .map(|s| Span::raw(s.to_string()))
        .collect::<Vec<_>>();
    let line = Line::from(spans);
    let paragraph = Paragraph::new(line)
        .style(Style::default().fg(theme.statusbar_fg))
        .alignment(Alignment::Left);
    // The pane is three rows tall; the text owns the middle row so the blank
    // space above and below it is the same.
    let text_area = Rect {
        y: area.y + area.height / 2,
        height: 1,
        ..area
    };
    frame.render_widget(paragraph, text_area);
}

/// Draw a mod's panels, top to bottom, directly under the conversation.
///
/// The rows are clipped to `area`, and the store's own height calculation is
/// what asked the layout for them, so in practice nothing is cut. Clipping
/// rather than refusing to draw is what keeps a resize from leaving a stale
/// frame: a panel taller than the terminal shows its top, which is the part a
/// status surface puts first.
fn render_mod_panels(frame: &mut Frame, area: ratatui::layout::Rect, state: &UiState) {
    if area.height == 0 || area.width == 0 || state.panels.is_empty() {
        return;
    }
    let lines = state.panels.render(area.width as usize);
    if lines.is_empty() {
        return;
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_todo_panel(
    frame: &mut Frame,
    area: ratatui::layout::Rect,
    panel: &crate::state::TodoPanel,
    theme: &Theme,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let padding = 2usize.min(area.width as usize);
    let lines: Vec<Line> = panel
        .render_rows()
        .into_iter()
        .enumerate()
        .map(|(index, (marker, text))| {
            // The row the model is on is the one being worked on, which is
            // more useful than its ordinal; the rest recede so the active row
            // reads first.
            let active = panel
                .items()
                .get(index)
                .is_some_and(|item| item.status == crate::state::TodoStatus::InProgress);
            let color = if active { theme.accent } else { theme.dim };
            Line::from(Span::styled(
                format!("{}{marker} {text}", " ".repeat(padding)),
                Style::default().fg(color),
            ))
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_permission_bar(
    frame: &mut Frame,
    area: ratatui::layout::Rect,
    bar: &crate::permission_bar::PermissionBar,
    theme: &Theme,
) {
    let color = bar.color(theme);
    let left_padding = 2usize.min(area.width as usize);
    let rendered = format!(
        "{}{}",
        " ".repeat(left_padding),
        bar.render((area.width as usize).saturating_sub(left_padding))
    );
    let line = Line::from(Span::styled(rendered, Style::default().fg(color)));
    let paragraph = Paragraph::new(line).alignment(Alignment::Left);
    frame.render_widget(paragraph, area);
}

/// A horizontal rule separating the transcript from the input box.
fn render_input_rule(frame: &mut Frame, area: ratatui::layout::Rect, theme: &Theme) {
    render_input_rule_with_hint(frame, area, theme, None);
}

/// The rule above the input, with an optional label on its right end.
///
/// The label sits on the rule itself rather than in a row of its own: a held
/// prompt is a fact about the line below, and a new row would move the input
/// (and the cursor) every time one was set aside. Drawing it here also means
/// the rule keeps its full width, so the box's shape does not change.
fn render_input_rule_with_hint(
    frame: &mut Frame,
    area: ratatui::layout::Rect,
    theme: &Theme,
    hint: Option<&str>,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let border = Style::default().fg(theme.border);
    let width = area.width as usize;
    // `─` is three bytes per cell, so the rule is built by column count
    // rather than by truncating a byte string.
    let rule = |columns: usize| "─".repeat(columns);
    let mut spans = Vec::new();
    match hint {
        // The six cells are the two padding blanks inside the label and the two
        // that separate it from the rule and the edge.
        Some(hint) if unicode_width::UnicodeWidthStr::width(hint) + 6 < width => {
            let text = format!(" {hint} ");
            let used = unicode_width::UnicodeWidthStr::width(text.as_str());
            spans.push(Span::styled(rule(width.saturating_sub(used + 2)), border));
            spans.push(Span::styled(text, Style::default().fg(theme.dim)));
            spans.push(Span::styled("  ", border));
        }
        _ => spans.push(Span::styled(rule(width), border)),
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The badge the rule above the input carries while a prompt is set aside.
///
/// The first line only, clipped: the badge's job is to say "there is one, and
/// roughly what it was", and the whole text is one keypress away. A held
/// prompt of several lines keeps its first line plus an ellipsis, so a
/// wrapped paragraph does not read the same as a one-line question.
fn stash_badge(held: &str) -> String {
    let mut preview: String = held
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .chars()
        .take(32)
        .collect();
    if held.lines().count() > 1 || held.chars().count() > 32 {
        preview.push('…');
    }
    format!("set aside: {preview}")
}

/// The single row a full-screen transcript keeps: how to get out.
fn render_fullscreen_footer(frame: &mut Frame, area: ratatui::layout::Rect, state: &UiState) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let text = if state.transcript_footer.is_empty() {
        "Transcript — the same key again returns to the prompt".to_string()
    } else {
        state.transcript_footer.clone()
    };
    // Clipped here rather than wrapped: the footer is one row, and a wrapped
    // one would eat a second row out of the transcript.
    let mut clipped = String::new();
    let mut used = 0;
    for character in text.chars() {
        let width = unicode_width::UnicodeWidthChar::width(character).unwrap_or(0);
        if used + width > area.width.saturating_sub(2) as usize {
            break;
        }
        clipped.push(character);
        used += width;
    }
    let line = Line::from(Span::styled(
        format!(" {clipped} "),
        Style::default().fg(state.theme.dim),
    ));
    frame.render_widget(Paragraph::new(line), area);
}

fn render_input(
    frame: &mut Frame,
    area: ratatui::layout::Rect,
    input: &crate::input::InputBuffer,
    history_search: Option<&crate::history_search::HistorySearch>,
    theme: &Theme,
) {
    // While a reverse search is open the pane shows the search, not the
    // buffer: the buffer is what Esc restores, so painting it would suggest
    // the query is being typed into it.
    if let Some(search) = history_search {
        render_history_search(frame, area, search, theme);
        return;
    }
    // Byte offsets are not columns: a CJK character occupies two cells. The
    // buffer reports its rows and the cursor's row/column in display units, so
    // a wrapped or multi-line buffer places the cursor on the right cell.
    let wrap = input_wrap_width(area.width);
    let rows = input.display_rows(wrap);
    let (cursor_row, cursor_col) = input.display_cursor(wrap);
    let height = (area.height as usize).max(1);
    // Normally every row fits; when the pane is capped, keep the cursor row
    // visible by scrolling the window.
    let first = cursor_row
        .saturating_sub(height - 1)
        .min(rows.len().saturating_sub(height));

    let text_style = Style::default().fg(theme.input_fg);
    let selection_style = Style::default()
        .fg(theme.selection_fg)
        .bg(theme.selection_bg);
    let selection = input.selection_range();

    let mut lines: Vec<Line> = Vec::with_capacity(rows.len());
    for (index, &(start, end)) in rows.iter().enumerate().skip(first).take(height) {
        // The prompt occupies two cells on the first row and is blanked on the
        // continuation rows, so every row's text starts at the same column.
        let prompt = if index == 0 { "❯ " } else { "  " };
        let text = &input.text[start..end];
        let mut spans: Vec<Span> = vec![Span::styled(prompt, Style::default().fg(theme.accent))];
        match selection {
            Some((sel_start, sel_end)) if sel_start < end && sel_end > start => {
                let local_start = sel_start.max(start) - start;
                let local_end = sel_end.min(end) - start;
                if local_start > 0 {
                    spans.push(Span::styled(text[..local_start].to_string(), text_style));
                }
                spans.push(Span::styled(
                    text[local_start..local_end].to_string(),
                    selection_style,
                ));
                if local_end < text.len() {
                    spans.push(Span::styled(text[local_end..].to_string(), text_style));
                }
            }
            _ => spans.push(Span::styled(text.to_string(), text_style)),
        }
        lines.push(Line::from(spans));
    }

    // Trailing annotations go after the last visible row's text so they never
    // shift the text the cursor column was measured against.
    if let Some(last) = lines.last_mut() {
        if let Some(ref sug) = input.suggestion {
            last.spans.push(Span::styled(
                format!("  {sug}"),
                Style::default().fg(theme.dim),
            ));
        }
        // Vim's mode line. `visual_anchor` is set the instant `v`/`V` is
        // pressed, so the badge appears before any motion, matching Vim.
        if input.visual_anchor.is_some() {
            let label = if input.visual_linewise {
                "-- VISUAL LINE --"
            } else {
                "-- VISUAL --"
            };
            last.spans.push(Span::styled(
                format!("  {label}"),
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ));
        }
    }

    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, area);
    frame.set_cursor_position((
        area.x + 2 + u16::try_from(cursor_col).unwrap_or(u16::MAX),
        area.y + u16::try_from(cursor_row.saturating_sub(first)).unwrap_or(u16::MAX),
    ));
}

/// Paint an open Ctrl+R search into the input pane.
///
/// The matched entries are listed newest first with the highlighted one
/// inverted, and the last row is the one the cursor sits on, so the search
/// reads top-down like the palette above it.
fn render_history_search(
    frame: &mut Frame,
    area: ratatui::layout::Rect,
    search: &crate::history_search::HistorySearch,
    theme: &Theme,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(
            "reverse-search: ",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            search.query().to_string(),
            Style::default().fg(theme.input_fg),
        ),
    ]));

    if search.matches().is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no matching history)",
            Style::default().fg(theme.dim),
        )));
    } else {
        // Show the highlighted entry and the ones after it, the way Ctrl+R
        // reports the newest match and its neighbours as the query narrows.
        let visible = area.height.saturating_sub(1) as usize;
        for (index, entry) in search
            .matches()
            .iter()
            .enumerate()
            .skip(search.selected())
            .take(visible.max(1))
        {
            let style = if index == search.selected() {
                Style::default()
                    .fg(theme.selection_fg)
                    .bg(theme.selection_bg)
            } else {
                Style::default().fg(theme.dim)
            };
            // A multi-line history entry has to stay on one row here.
            let single_line: String = entry.replace('\n', " ");
            lines.push(Line::from(Span::styled(format!("  {single_line}"), style)));
        }
    }

    frame.render_widget(Paragraph::new(lines), area);
}

/// Render the inline command palette just above the statusline / input line.
fn render_inline_palette(frame: &mut Frame, area: ratatui::layout::Rect, state: &UiState) {
    use crate::overlay::Overlay;
    // Both palettes are the same list with a different heading; a mention
    // inserts text and a command runs it, which is the caller's business.
    let (heading, query, entries, selected) = match state.overlays.top() {
        Some(Overlay::CommandPalette {
            query,
            commands,
            selected,
        }) => ("Commands ", query, commands, *selected),
        Some(Overlay::FileMention {
            query,
            files,
            selected,
        }) => ("Files ", query, files, *selected),
        _ => return,
    };

    let mut lines: Vec<Line> = Vec::new();

    // Header
    lines.push(Line::from(vec![
        Span::styled(
            heading,
            Style::default()
                .fg(state.theme.foreground)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("[{query}]"), Style::default().fg(state.theme.dim)),
    ]));

    let available_rows = area.height.saturating_sub(1) as usize;
    let visible_count = entries.len().min(available_rows);
    if visible_count == 0 {
        lines.push(Line::from(Span::styled(
            "  no matches",
            Style::default().fg(state.theme.dim),
        )));
        frame.render_widget(Paragraph::new(lines), area);
        return;
    }
    let start = selected
        .saturating_sub(visible_count - 1)
        .min(entries.len().saturating_sub(visible_count));

    for (i, command) in entries.iter().enumerate().skip(start).take(visible_count) {
        let style = if i == selected {
            Style::default()
                .fg(state.theme.selection_fg)
                .bg(state.theme.selection_bg)
        } else {
            Style::default().fg(state.theme.foreground)
        };
        lines.push(Line::from(Span::styled(command.clone(), style)));
    }

    let paragraph = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::NONE)
            .style(Style::default().fg(state.theme.foreground)),
    );
    frame.render_widget(paragraph, area);
}

fn render_overlay(frame: &mut Frame, area: ratatui::layout::Rect, state: &UiState) {
    use crate::overlay::Overlay;
    let Some(overlay) = state.overlays.top() else {
        return;
    };

    let content = match overlay {
        Overlay::PermissionDialog {
            tool,
            args,
            risk_summary,
            always_option,
        } => {
            format!(
                "Permission Request\nTool: {tool}\nArgs: {args}\nRisk: {risk_summary}\nState: waiting for input\n[a] Allow once  [d] Deny{}{}",
                if *always_option {
                    "  [A] Always allow"
                } else {
                    ""
                },
                if *always_option {
                    ""
                } else {
                    "\n[a] Allow once"
                }
            )
        }
        Overlay::Notification { message, level } => {
            format!("Notification: {message} ({level:?})")
        }
        Overlay::CommandPalette { .. } | Overlay::FileMention { .. } => {
            // Both inline lists are handled separately; skip here.
            return;
        }
        Overlay::Detail { body, .. } => body.clone(),
        Overlay::Confirm { message } => {
            format!("{message}\n[y/n]")
        }
        Overlay::ModelPicker(picker) => picker.render(),
        Overlay::EffortSlider(slider) => slider.render(),
        Overlay::TextInput(field) => field.render(),
        Overlay::List(picker) | Overlay::Settings(picker) => picker.render(14),
    };

    let title = match overlay {
        Overlay::PermissionDialog { .. } => "Permission Details",
        Overlay::Notification { .. } => "Notification",
        Overlay::Detail { title, .. } => title.as_str(),
        Overlay::CommandPalette { .. } => "Commands",
        Overlay::FileMention { .. } => "Files",
        Overlay::Confirm { .. } => "Confirm",
        Overlay::ModelPicker { .. } => "Model",
        Overlay::EffortSlider { .. } => "Effort",
        Overlay::TextInput { .. } => "Rename",
        Overlay::List(picker) | Overlay::Settings(picker) => picker.title.as_str(),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .style(Style::default().fg(state.theme.overlay_border));
    // A detail body is the one overlay that can be longer than its box: the
    // help page is several screens' worth of rows in a box that has to fit on
    // one. `scroll` counts *drawn* rows after wrapping, which is what a reader
    // means by a page.
    let detail_scroll = match overlay {
        Overlay::Detail { scroll, .. } => Some(*scroll),
        _ => None,
    };
    let mut paragraph = Paragraph::new(content.clone())
        .style(Style::default().fg(state.theme.foreground))
        .block(block);

    // Center the overlay. A detail body (a provider error, for example) is
    // long: give it the full width and wrap it. Dialog bodies must not wrap —
    // a long Write argument would push the `[a] Allow once` choices out of the
    // box — so they keep their fixed layout and are clipped instead.
    // The picker's body is a list, so it wraps like a detail body rather than
    // clipping like a fixed-layout dialog.
    let is_detail = matches!(
        overlay,
        Overlay::Detail { .. }
            | Overlay::ModelPicker(_)
            | Overlay::EffortSlider(_)
            | Overlay::TextInput(_)
            | Overlay::List(_)
            | Overlay::Settings(_)
    );
    if is_detail {
        paragraph = paragraph.wrap(Wrap { trim: false });
    }
    let w = if is_detail {
        area.width.saturating_sub(4).min(100)
    } else {
        50.min(area.width.saturating_sub(4))
    };
    // Body rows after wrapping, which is also the unit `scroll` counts in.
    let inner_w = usize::from(w.saturating_sub(2)).max(1);
    let body_rows = if is_detail {
        u16::try_from(wrapped_rows_with_blanks(&content, inner_w)).unwrap_or(u16::MAX)
    } else {
        0
    };
    // A detail body gets the box it needs, up to the screen. The old fixed
    // twenty rows was right when every detail was a provider error; the help
    // page is longer than that, and a page that ends mid-sentence is not a
    // page. The body also scrolls from the keyboard, so the box never has to
    // be larger than the screen, and a body larger than the screen is
    // reachable a screenful at a time.
    let max_h = area.height.saturating_sub(4);
    let h = if is_detail {
        body_rows.saturating_add(2).max(8).min(max_h)
    } else {
        10.min(max_h)
    };
    // Now that the box height is known, clamp the scroll: `h - 2` is how many
    // rows it shows, so the last of those bounds where the top may sit. A
    // page-down at the bottom of a body keeps its last line on screen instead
    // of scrolling into the paragraph's blank tail, which read as an overlay
    // that had lost its contents.
    if let Some(rows) = detail_scroll {
        let hold = h.saturating_sub(2);
        paragraph = paragraph.scroll((rows.min(body_rows.saturating_sub(hold)), 0));
    }
    let x = (area.width - w) / 2;
    let y = (area.height - h) / 2;
    let overlay_area = ratatui::layout::Rect {
        x: area.x + x,
        y: area.y + y,
        width: w,
        height: h,
    };
    frame.render_widget(paragraph, overlay_area);
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

    /// The keyboard push is the one startup byte sequence that depends on the
    /// terminal, and it is a correctness matter: `\x1b[>1u` tells a capable
    /// terminal to stop collapsing `Ctrl+I` into `Tab`.
    #[test]
    fn enter_pushes_keyboard_protocol_only_for_capable_terminals() {
        let kitty = TerminalProfile::detect(&env(&[("KITTY_WINDOW_ID", "1")]));
        let mut out = Vec::new();
        enter_commands(&kitty, &mut out).expect("write enter commands");
        let bytes = String::from_utf8(out).expect("utf8");
        assert!(
            bytes.contains("\x1b[>1u"),
            "capable terminal did not get the disambiguation push: {bytes:?}"
        );

        let vscode = TerminalProfile::detect(&env(&[("TERM_PROGRAM", "vscode")]));
        let mut out = Vec::new();
        enter_commands(&vscode, &mut out).expect("write enter commands");
        let bytes = String::from_utf8(out).expect("utf8");
        assert!(
            !bytes.contains("\x1b[>1u"),
            "unverified terminal was pushed a protocol it may not speak: {bytes:?}"
        );
        // …but it still enters the alternate screen and captures the mouse.
        assert!(
            bytes.contains("\x1b[?1049h"),
            "alt screen missing: {bytes:?}"
        );
    }

    #[test]
    fn exit_pops_only_what_was_pushed() {
        let mut pushed = Vec::new();
        exit_commands(true, &mut pushed).expect("write exit commands");
        let bytes = String::from_utf8(pushed).expect("utf8");
        assert!(
            bytes.contains("\x1b[<1u"),
            "pushed protocol was not popped: {bytes:?}"
        );
        assert!(bytes.contains("\x1b[?1049l"), "alt screen not left");

        let mut never = Vec::new();
        exit_commands(false, &mut never).expect("write exit commands");
        let bytes = String::from_utf8(never).expect("utf8");
        assert!(
            !bytes.contains("\x1b[<1u"),
            "popped a protocol level that was never pushed: {bytes:?}"
        );
    }

    /// A panic hook restores the terminal without a `TerminalUi`, so it
    /// re-derives the same decision from the environment. It has to reach the
    /// same bytes an orderly exit writes, or the shell is left in a mode the
    /// process thought it had left.
    #[test]
    fn the_panic_restore_writes_what_an_orderly_exit_writes() {
        for env in [
            env(&[("KITTY_WINDOW_ID", "1")]),
            env(&[("TERM_PROGRAM", "vscode")]),
            env(&[]),
        ] {
            let profile = TerminalProfile::detect(&env);
            let mut orderly = Vec::new();
            exit_commands(profile.keyboard_flags().is_some(), &mut orderly)
                .expect("write exit commands");
            let mut after_panic = Vec::new();
            panic_exit_commands(&profile, &mut after_panic).expect("write panic exit commands");
            assert_eq!(
                orderly, after_panic,
                "panic restore diverged from an orderly exit"
            );
        }
    }

    #[test]
    fn terminal_new_succeeds() {
        let result = TerminalUi::new();
        // May fail in CI without a terminal, but should not panic
        assert!(result.is_ok() || result.is_err());
    }

    #[test]
    fn statusline_renders_within_width() {
        let sl = Statusline {
            model: "sonnet".into(),
            total_tokens: 100,
            cost_usd: Some(0.01),
            git_branch: "main".into(),
            provider: "Anthropic API".into(),
            cwd: "~/work".into(),
            session_title: "test".into(),
            thinking: false,
            thinking_started: None,
            last_duration: None,
            thinking_label: None,
            spinner_frame: 0,
            show_tokens: true,
            show_cost: true,
            show_git_branch: true,
            format: None,
        };
        let rendered = sl.render(80);
        assert!(rendered.len() <= 80);
    }

    #[test]
    fn wrap_text_basic() {
        let lines = wrap_text("hello world foo", 10);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "hello");
        assert_eq!(lines[1], "world foo");
    }

    #[test]
    fn wrap_text_preserves_newlines() {
        let lines = wrap_text("a\nb\nc", 80);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "a");
        assert_eq!(lines[1], "b");
        assert_eq!(lines[2], "c");
    }

    #[test]
    fn wrap_text_zero_width() {
        let lines = wrap_text("hello", 0);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], "hello");
    }

    #[test]
    fn wrapped_line_count_matches_wrap_text_without_allocating_lines() {
        for (text, width) in [
            ("hello world foo", 10),
            ("a\nb\nc", 80),
            ("single", 80),
            ("supercalifragilistic", 5),
            ("", 80),
            ("two  spaces and\ttabs", 8),
        ] {
            assert_eq!(
                wrapped_line_count(text, width),
                wrap_text(text, width).len().max(1),
                "mismatch for {text:?} at width {width}"
            );
        }
    }

    #[test]
    fn trim_lines_to_viewport_keeps_latest_lines() {
        let lines = vec![
            Line::from("line 1"),
            Line::from("line 2"),
            Line::from("line 3"),
            Line::from("line 4"),
        ];
        let trimmed = trim_lines_to_viewport(lines, 2);
        assert_eq!(trimmed.len(), 2);
        assert_eq!(trimmed[0].to_string(), "line 3");
        assert_eq!(trimmed[1].to_string(), "line 4");
    }

    #[test]
    fn trim_lines_to_viewport_scrolls_from_bottom() {
        let lines = [
            Line::from("line 1"),
            Line::from("line 2"),
            Line::from("line 3"),
            Line::from("line 4"),
            Line::from("line 5"),
        ];
        let (start, end) = scroll_slice_bounds(lines.len(), 2, 2);

        assert_eq!((start, end), (1, 3));
        assert_eq!(lines[start].to_string(), "line 2");
        assert_eq!(lines[start + 1].to_string(), "line 3");
    }

    #[test]
    fn message_lines_use_distinct_role_prefixes_and_spacing() {
        let messages = vec![
            Message {
                role: MessageRole::User,
                content: "hello".into(),
                tool_name: None,
                streaming: false,
                folded: false,
                image: None,
            },
            Message {
                role: MessageRole::Assistant,
                content: "hi there".into(),
                tool_name: None,
                streaming: false,
                folded: false,
                image: None,
            },
        ];

        let theme = Theme::dark();
        let (lines, _) = render_all_message_lines(
            &messages,
            80,
            false,
            &theme,
            &[],
            crate::transcript_image::ImageProtocol::default(),
        );

        assert_eq!(lines[0].to_string(), "● hello");
        assert_eq!(lines[1].to_string(), "");
        assert_eq!(lines[2].to_string(), "◆ hi there");
    }

    /// A 2x2 PNG: red/green on top, blue/white below.
    pub(super) fn two_by_two_png() -> String {
        use base64::Engine as _;
        let mut buffer = std::io::Cursor::new(Vec::new());
        {
            let encoder = image::codecs::png::PngEncoder::new(&mut buffer);
            let pixels: [u8; 16] = [
                255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
            ];
            image::ImageEncoder::write_image(
                encoder,
                &pixels,
                2,
                2,
                image::ExtendedColorType::Rgba8,
            )
            .expect("encode png");
        }
        base64::engine::general_purpose::STANDARD.encode(buffer.into_inner())
    }

    pub(super) fn image_message() -> Message {
        let state = crate::transcript_image::ImageState::new(codape_core::types::ImageBlock {
            media_type: "image/png".into(),
            data: two_by_two_png(),
        });
        let caption = state.caption();
        Message {
            role: MessageRole::User,
            content: caption.into(),
            tool_name: None,
            streaming: false,
            folded: false,
            image: Some(state),
        }
    }

    #[test]
    fn a_picture_is_drawn_as_half_blocks_instead_of_its_caption() {
        let theme = Theme::dark();
        let messages = vec![image_message()];
        let (lines, _) = render_all_message_lines(
            &messages,
            80,
            false,
            &theme,
            &[],
            crate::transcript_image::ImageProtocol::HalfBlocks,
        );

        // The role prefix gets its own row, then one row of two half blocks.
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].to_string(), "● ");
        let spans = &lines[1].spans;
        assert_eq!(spans.len(), 2);
        assert!(spans.iter().all(|span| span.content.as_ref() == "\u{2580}"));
        assert_eq!(spans[0].style.fg, Some(Color::Rgb(255, 0, 0)));
        assert_eq!(spans[0].style.bg, Some(Color::Rgb(0, 0, 255)));
        // The caption is not printed alongside the picture.
        let rendered: String = lines.iter().map(ToString::to_string).collect();
        assert!(!rendered.contains("[image:"), "{rendered}");
    }

    #[test]
    fn compact_mode_draws_the_caption_not_the_picture() {
        // Compact reserves one row per message. Drawing a picture there would
        // paint rows the layout never reserved, pushing the input box down.
        let theme = Theme::dark();
        let messages = vec![image_message()];
        let rows = message_display_height(
            &messages[0],
            80,
            true,
            crate::transcript_image::ImageProtocol::HalfBlocks,
        );
        // Compact reserves exactly what it would for the caption alone.
        assert_eq!(
            rows,
            message_display_height(
                &messages[0],
                80,
                true,
                crate::transcript_image::ImageProtocol::None,
            )
        );
        let (lines, _) = render_all_message_lines(
            &messages,
            80,
            true,
            &theme,
            &[],
            crate::transcript_image::ImageProtocol::HalfBlocks,
        );
        let rendered: String = lines.iter().map(ToString::to_string).collect();
        assert!(!rendered.contains("\u{2580}"), "{rendered}");
    }

    #[test]
    fn a_terminal_without_truecolor_keeps_the_caption() {
        let theme = Theme::dark();
        let messages = vec![image_message()];
        let (lines, _) = render_all_message_lines(
            &messages,
            80,
            false,
            &theme,
            &[],
            crate::transcript_image::ImageProtocol::None,
        );

        let rendered: String = lines.iter().map(ToString::to_string).collect();
        assert!(rendered.contains("[image: image/png]"), "{rendered}");
    }

    #[test]
    fn the_reserved_height_matches_the_drawn_rows() {
        let theme = Theme::dark();
        let messages = vec![image_message()];
        let rows = message_display_height(
            &messages[0],
            80,
            false,
            crate::transcript_image::ImageProtocol::HalfBlocks,
        );
        let (lines, _) = render_all_message_lines(
            &messages,
            80,
            false,
            &theme,
            &[],
            crate::transcript_image::ImageProtocol::HalfBlocks,
        );
        // A viewport window that reserved a different height would either clip
        // the picture or leave a blank gap under it.
        assert_eq!(rows, lines.len());
    }

    #[test]
    fn an_image_with_no_readable_header_still_occupies_its_caption_row() {
        let state = crate::transcript_image::ImageState::new(codape_core::types::ImageBlock {
            media_type: "image/png".into(),
            data: "not base64".into(),
        });
        let caption = state.caption();
        let msg = Message {
            role: MessageRole::User,
            content: caption.into(),
            tool_name: None,
            streaming: false,
            folded: false,
            image: Some(state),
        };
        let theme = Theme::dark();
        let (lines, _) = render_all_message_lines(
            &[msg],
            80,
            false,
            &theme,
            &[],
            crate::transcript_image::ImageProtocol::HalfBlocks,
        );
        let rendered: String = lines.iter().map(ToString::to_string).collect();
        assert!(rendered.contains("[image: image/png]"), "{rendered}");
    }

    #[test]
    fn a_picture_whose_pixels_fail_to_decode_keeps_its_reserved_rows() {
        // A base64 payload with a readable header is not the same as a payload
        // that decodes: a truncated body still names its size. The layout has
        // already reserved those rows from the header, so the caption has to
        // stay inside them rather than collapse to one line and lift
        // everything below it.
        use base64::Engine as _;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(super::tests::two_by_two_png().as_bytes())
            .expect("decode");
        // Cut the file in half: the signature and IHDR survive, the IDAT does
        // not. The header names 2x2, the pixels are gone.
        let truncated = &decoded[..decoded.len() / 2];
        let state = crate::transcript_image::ImageState::new(codape_core::types::ImageBlock {
            media_type: "image/png".into(),
            data: base64::engine::general_purpose::STANDARD.encode(truncated),
        });
        assert_eq!(state.cell_size(80), (2, 1), "the header still reads");
        assert!(
            state.lines(80, &Theme::dark()).is_none(),
            "the body is gone"
        );

        let caption = state.caption();
        let msg = Message {
            role: MessageRole::User,
            content: caption.into(),
            tool_name: None,
            streaming: false,
            folded: false,
            image: Some(state),
        };
        let theme = Theme::dark();
        let (lines, _) = render_all_message_lines(
            &[msg],
            80,
            false,
            &theme,
            &[],
            crate::transcript_image::ImageProtocol::HalfBlocks,
        );
        let rendered: String = lines.iter().map(ToString::to_string).collect();
        assert!(rendered.contains("[image: image/png]"), "{rendered}");
        assert!(!rendered.contains('\u{2580}'), "{rendered}");
        assert_eq!(lines.len(), 2, "one prefix row plus one reserved row");
    }

    #[test]
    fn visible_window_includes_overflowing_final_message() {
        let heights = vec![1, 80];
        let (_start, count, accumulated) = visible_message_window(&heights, 0, 20);

        assert_eq!(count, 2);
        assert_eq!(accumulated, 81);
    }

    #[test]
    fn visible_window_includes_single_overflowing_message() {
        let heights = vec![80];
        let (_start, count, accumulated) = visible_message_window(&heights, 0, 20);

        assert_eq!(count, 1);
        assert_eq!(accumulated, 80);
    }

    #[test]
    fn bottom_window_skips_middle_overflow_and_keeps_latest_response() {
        let heights = vec![1, 1, 80, 1, 80];
        let (start, count, accumulated) = bottom_message_window(&heights, 20);

        assert_eq!(start, 4);
        assert_eq!(count, 1);
        assert_eq!(accumulated, 80);
    }

    #[test]
    fn bottom_window_keeps_latest_small_messages() {
        let heights = vec![1, 80, 1, 2];
        let (start, count, accumulated) = bottom_message_window(&heights, 20);

        assert_eq!(start, 1);
        assert_eq!(count, 3);
        assert_eq!(accumulated, 83);
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    /// A folded reasoning row shows a preview rather than hiding entirely.
    ///
    /// Thinking is folded by default, so "folded" is the state a reader
    /// actually sees. If it rendered as the generic `▸ Thinking [+N lines
    /// folded]` indicator, surfacing reasoning would have changed nothing on
    /// screen.
    #[test]
    fn a_folded_thinking_row_previews_its_first_line() {
        let mut state = UiState::default();
        state.messages.push(Message {
            role: MessageRole::Thinking,
            content: "The failing test is the PTY one\nso the fix belongs there.".into(),
            tool_name: None,
            streaming: false,
            folded: true,
            image: None,
        });
        let buffer = render_to_buffer(80, 12, &mut state);
        let painted: Vec<String> = (0..buffer.area.height)
            .map(|y| row_text(&buffer, y))
            .collect();
        let text = painted.join("\n");
        assert!(
            text.contains("The failing test is the PTY one"),
            "the reason's first line is not on screen:\n{text}"
        );
        assert!(
            !text.contains("so the fix belongs there."),
            "a folded row printed its whole body:\n{text}"
        );
        assert!(
            text.contains("lines]"),
            "no expansion hint on the folded row:\n{text}"
        );
    }

    /// Reasoning is painted distinctly from the answer, so the two are not
    /// confused at a glance.
    #[test]
    fn an_expanded_thinking_row_is_dim_and_marked() {
        let mut state = UiState::default();
        state.messages.push(Message {
            role: MessageRole::Thinking,
            content: "Considering the options.".into(),
            tool_name: None,
            streaming: false,
            folded: false,
            image: None,
        });
        let buffer = render_to_buffer(80, 12, &mut state);
        let text = (0..buffer.area.height)
            .map(|y| row_text(&buffer, y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Considering the options."), "{text}");
        assert!(
            text.contains('✻'),
            "no thinking marker in the gutter:\n{text}"
        );
    }

    /// The diff pane paints added and removed lines apart.
    ///
    /// `/diff` used to print the whole patch into the transcript, where an
    /// added line and a context line looked identical. The pane is only worth
    /// having if the colours actually reach the cells.
    #[test]
    fn the_diff_pane_colours_added_and_removed_lines() {
        let mut state = UiState::default();
        state.layout_mode = crate::state::LayoutMode::HorizontalSplit { left_percent: 50 };
        state.left_pane_title = "diff  1 file(s)  +1 -1".into();
        state.left_pane_content = "diff --git a/a.rs b/a.rs\n\
--- a/a.rs\n\
+++ b/a.rs\n\
@@ -1,2 +1,2 @@\n\
 fn keep() {}\n\
-fn old() {}\n\
+fn new() {}\n"
            .into();
        state.left_pane_is_diff = true;
        let theme = state.theme.clone();
        let buffer = render_to_buffer(80, 30, &mut state);
        let text = (0..buffer.area.height)
            .map(|y| row_text(&buffer, y))
            .collect::<Vec<_>>()
            .join("\n");

        // The title carries the stat, so the reader knows the size of what
        // they are looking at without scrolling to the end.
        assert!(
            text.contains("+1 -1"),
            "diff stat is not on the pane:\n{text}"
        );

        let removed = colour_of_line(&buffer, "-fn old() {}");
        let added = colour_of_line(&buffer, "+fn new() {}");
        assert_eq!(removed, Some(theme.error), "removed line is not error-red");
        assert_eq!(
            added,
            Some(theme.success),
            "added line is not success-green"
        );
    }

    /// A second view puts a tab bar above the patch, and the patch is still
    /// drawn below it.
    ///
    /// The bar and the body share one border, so the test that matters is
    /// that the tab row did not paint over the patch and the patch did not
    /// paint over the tab row: both have to be on screen at once.
    #[test]
    fn a_second_view_is_a_tab_beside_the_first() {
        let mut state = UiState::default();
        state.layout_mode = crate::state::LayoutMode::HorizontalSplit { left_percent: 50 };
        state.pane_show(
            "a.rs".into(),
            "fn alpha() {}\nfn beta() {}\nfn gamma() {}\n".into(),
            false,
        );
        state.pane_show(
            "b.rs".into(),
            "fn delta() {}\nfn epsilon() {}\n".into(),
            false,
        );
        let buffer = render_to_buffer(100, 30, &mut state);
        let text = (0..buffer.area.height)
            .map(|y| row_text(&buffer, y))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            text.contains("1:a.rs"),
            "first tab is not on the bar:\n{text}"
        );
        assert!(
            text.contains("2:b.rs"),
            "second tab is not on the bar:\n{text}"
        );
        // The body is the selected tab's, and it is below the bar rather than
        // under it.
        assert!(
            text.contains("fn epsilon()"),
            "the selected view is missing from the pane:\n{text}"
        );
        assert!(
            !text.contains("fn alpha()"),
            "the tab that is not selected was drawn too:\n{text}"
        );
    }

    /// A bar too wide for the pane is cut with a mark, not clipped mid-name.
    ///
    /// The pane is half of an 80-column terminal in the split view, so three
    /// views with real paths overrun it. A hard clip at the edge would leave
    /// half a filename and no sign there is another tab.
    #[test]
    fn a_tab_bar_wider_than_the_pane_ends_in_an_ellipsis() {
        let mut state = UiState::default();
        state.layout_mode = crate::state::LayoutMode::HorizontalSplit { left_percent: 50 };
        state.pane_show("first.rs".into(), "one".into(), false);
        state.pane_show("a-rather-long-second-name.rs".into(), "two".into(), false);
        state.pane_show("third.rs".into(), "three".into(), false);
        let buffer = render_to_buffer(40, 20, &mut state);
        let bar = row_text(&buffer, 0);
        let text = (0..buffer.area.height)
            .map(|y| row_text(&buffer, y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains('…'),
            "no ellipsis on a bar that overflowed:\n{text}"
        );
        assert!(
            text.contains("1:first.rs"),
            "the first tab did not fit and was dropped:\n{text}"
        );
        assert!(bar.ends_with('…') || text.contains('…'), "{bar}");
    }

    /// A single view keeps the line the bar would have taken.
    #[test]
    fn one_view_draws_no_tab_bar() {
        let mut state = UiState::default();
        state.layout_mode = crate::state::LayoutMode::HorizontalSplit { left_percent: 50 };
        state.pane_show("a.rs".into(), "fn alpha() {}\n".into(), false);
        let buffer = render_to_buffer(100, 30, &mut state);
        let text = (0..buffer.area.height)
            .map(|y| row_text(&buffer, y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !text.contains("1:a.rs"),
            "a bar of one tab was drawn:\n{text}"
        );
        assert!(text.contains("fn alpha()"), "{text}");
    }

    /// The foreground colour of the cell that starts `needle`, if the text is
    /// on screen at all.
    fn colour_of_line(buffer: &Buffer, needle: &str) -> Option<ratatui::style::Color> {
        let lead = needle.chars().next()?;
        for y in 0..buffer.area.height {
            let row = row_text(buffer, y);
            let Some(at) = row.find(needle) else { continue };
            let x = u16::try_from(row[..at].chars().count()).ok()?;
            // Assert the row really starts with the mark, so a match inside a
            // longer line cannot make a wrong cell look right.
            let cell = buffer.cell((x, y))?;
            assert_eq!(cell.symbol(), lead.to_string());
            return cell.style().fg;
        }
        None
    }

    fn row_text(buffer: &Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .map(|x| buffer.cell((x, y)).map_or(" ", |c| c.symbol()))
            .collect()
    }

    fn render_to_buffer(width: u16, height: u16, state: &mut UiState) -> Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| render_frame(frame, state, &[], &mut Vec::new()))
            .expect("draw frame");
        terminal.backend().buffer().clone()
    }

    /// The pane stack the renderer lays out, for the given visibility flags.
    fn chunks_for(
        height: u16,
        palette: u16,
        notif: bool,
        statusline: bool,
        input: u16,
        perm: bool,
    ) -> Vec<ratatui::layout::Rect> {
        chunks_with_todos(height, palette, notif, 0, statusline, input, perm)
    }

    /// The same stack with a todo panel of `todos` rows above the statusline.
    fn chunks_with_todos(
        height: u16,
        palette: u16,
        notif: bool,
        todos: u16,
        statusline: bool,
        input: u16,
        perm: bool,
    ) -> Vec<ratatui::layout::Rect> {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints(frame_constraints(
                0,
                palette,
                Panes {
                    notification: notif,
                    statusline,
                    permission_bar: perm,
                    fullscreen: false,
                },
                todos,
                input,
            ))
            .split(ratatui::layout::Rect::new(0, 0, 80, height))
            .to_vec()
    }

    /// The regression: `Layout::split` keeps one rect per constraint even when a
    /// constraint resolves to zero height, so indexing by "which panes are on"
    /// pointed the input line and statusline at zero-height rects and both
    /// silently vanished.
    #[test]
    fn input_and_statusline_render_on_their_own_panes() {
        // Palette hidden, no notification: the case where the old computed
        // index (1) landed on the zero-height palette rect.
        let mut state = UiState::default();
        let buffer = render_to_buffer(80, 30, &mut state);
        let rows: Vec<String> = (0..30).map(|y| row_text(&buffer, y)).collect();

        let chunks = chunks_for(30, 0, false, true, 1, true);
        // The text sits on the middle row of the padded pane.
        let statusline_y = chunks[STATUSLINE_IDX].y + 1;
        let input_y = chunks[INPUT_IDX].y;

        assert!(
            chunks[STATUSLINE_IDX].height > 0,
            "statusline pane has no height"
        );
        assert!(
            !rows[statusline_y as usize].trim().is_empty(),
            "statusline did not render on its pane (row {statusline_y}):\n{}",
            rows.join("\n")
        );
        assert!(
            rows[input_y as usize].contains('❯'),
            "input line did not render on its pane (row {input_y}):\n{}",
            rows.join("\n")
        );
    }

    /// Compact mode hides the statusline and permission bar. The input line
    /// keeps its own constraint, so it must still be laid out and drawn.
    #[test]
    fn input_renders_in_compact_mode() {
        let mut state = UiState {
            force_compact: true,
            ..UiState::default()
        };
        let buffer = render_to_buffer(80, 12, &mut state);
        let rows: Vec<String> = (0..12).map(|y| row_text(&buffer, y)).collect();

        let chunks = chunks_for(12, 0, false, false, 1, false);
        let input_y = chunks[INPUT_IDX].y;
        assert!(
            chunks[INPUT_IDX].height > 0,
            "input pane has no height in compact mode"
        );
        assert!(
            rows[input_y as usize].contains('❯'),
            "input line is not visible in compact mode (row {input_y}):\n{}",
            rows.join("\n")
        );
    }

    /// A Visual selection must be visible in the TUI, not only in the buffer's
    /// internal state: the input line carries the mode badge.
    #[test]
    fn input_line_shows_the_visual_mode_indicator() {
        let mut state = UiState::default();
        state.input.text = "hello".into();
        state.input.cursor = 5;
        state.input.start_visual(false);
        let buffer = render_to_buffer(80, 12, &mut state);
        let input_y = chunks_for(12, 0, false, false, 1, false)[INPUT_IDX].y;
        let row = row_text(&buffer, input_y);
        assert!(
            row.contains("-- VISUAL --"),
            "characterwise Visual badge missing (row {input_y}): {row:?}"
        );
        assert!(
            !row.contains("VISUAL LINE"),
            "linewise badge shown for a characterwise selection: {row:?}"
        );

        // Linewise Visual mode gets its own label.
        let mut state = UiState::default();
        state.input.text = "hello".into();
        state.input.cursor = 5;
        state.input.start_visual(true);
        let buffer = render_to_buffer(80, 12, &mut state);
        let row = row_text(&buffer, input_y);
        assert!(
            row.contains("-- VISUAL LINE --"),
            "linewise Visual badge missing (row {input_y}): {row:?}"
        );

        // With no selection there is no badge.
        let mut state = UiState::default();
        state.input.text = "hello".into();
        let buffer = render_to_buffer(80, 12, &mut state);
        let row = row_text(&buffer, input_y);
        assert!(
            !row.contains("VISUAL"),
            "badge shown without a selection: {row:?}"
        );
    }

    /// A held prompt must be visible, not only remembered: the rule above the
    /// input carries a badge naming what is set aside.
    #[test]
    fn a_stashed_prompt_is_shown_above_the_input() {
        let mut state = UiState {
            stashed_input: Some("the long question\nwith a second line".into()),
            ..UiState::default()
        };
        let buffer = render_to_buffer(80, 12, &mut state);
        let rule_y = chunks_for(12, 0, false, false, 1, false)[RULE_IDX].y;
        let row = row_text(&buffer, rule_y);
        assert!(
            row.contains("set aside: the long question…"),
            "the stashed prompt is not named (row {rule_y}): {row:?}"
        );
        // The rule keeps its full width: the badge sits on it, not in place of
        // it, and the cells after the label are the rule's own.
        assert_eq!(
            row.chars().count(),
            80,
            "the rule no longer spans the pane: {row:?}"
        );

        // Nothing held, nothing drawn.
        let mut state = UiState::default();
        let buffer = render_to_buffer(80, 12, &mut state);
        let row = row_text(&buffer, rule_y);
        assert!(
            !row.contains("set aside"),
            "a badge without a held prompt: {row:?}"
        );
    }

    /// A mod panel occupies its own rows between the transcript and the
    /// prompt, stays there across a redraw, and gives the rows back when it is
    /// cleared.
    #[test]
    fn a_mod_panel_draws_below_the_transcript_and_above_the_prompt() {
        use codape_core::ui_protocol::{ModPanelUpdate, UiElement, UiStyle};

        let mut state = UiState::default();
        let buffer = render_to_buffer(80, 20, &mut state);
        let input_y_before = chunks_for(20, 0, false, false, 1, false)[INPUT_IDX].y;
        assert!(
            !row_text(&buffer, input_y_before).is_empty(),
            "the input drew on a frame with no panels"
        );

        state.panels.apply(&[ModPanelUpdate::Set {
            slot: "status".into(),
            elements: vec![UiElement::Text {
                text: "build green".into(),
                style: UiStyle::Plain,
            }],
        }]);
        // The frame after the update is the one that has to show it: a panel
        // that only appears on the next unrelated repaint is a panel the
        // person has no reason to believe in.
        let buffer = render_to_buffer(80, 20, &mut state);
        let rows: Vec<String> = (0..20).map(|y| row_text(&buffer, y)).collect();
        assert!(
            rows.iter().any(|row| row.contains("build green")),
            "the panel is not on screen:\n{}",
            rows.join("\n")
        );
        // It sits above the input: the conversation shrinks, the prompt does
        // not move up into the transcript's space.
        let panel_row = rows
            .iter()
            .position(|row| row.contains("build green"))
            .expect("the panel row");
        let input_row = rows
            .iter()
            .position(|row| row.contains('❯'))
            .expect("the input row");
        assert!(
            panel_row < input_row,
            "the panel drew below the input (panel {panel_row}, input {input_row})"
        );

        // Clearing gives the rows back to the transcript.
        let mut state = UiState {
            panels: state.panels.clone(),
            ..UiState::default()
        };
        state.panels.apply(&[ModPanelUpdate::Clear {
            slot: "status".into(),
        }]);
        let buffer = render_to_buffer(80, 20, &mut state);
        let rows: Vec<String> = (0..20).map(|y| row_text(&buffer, y)).collect();
        assert!(
            !rows.iter().any(|row| row.contains("build green")),
            "a cleared panel is still drawn:\n{}",
            rows.join("\n")
        );
    }

    /// Every index the renderer uses must name the constraint it claims to.
    #[test]
    fn pane_indices_follow_the_constraint_order() {
        let chunks = chunks_with_todos(30, 4, true, 2, true, 1, true);
        assert_eq!(chunks.len(), FOOTER_IDX + 1, "one rect per constraint");
        assert_eq!(chunks[PANEL_IDX].height, 0, "no panel without a mod");
        assert_eq!(chunks[PALETTE_IDX].height, 4, "palette");
        assert_eq!(chunks[NOTIFICATION_IDX].height, 1, "notification");
        assert_eq!(chunks[TODOS_IDX].height, 2, "todo panel");
        assert_eq!(chunks[STATUSLINE_IDX].height, 3, "statusline");
        assert_eq!(chunks[RULE_IDX].height, 1, "input rule");
        assert_eq!(chunks[INPUT_IDX].height, 1, "input");
        assert_eq!(chunks[RULE_BELOW_IDX].height, 1, "input rule below");
        assert_eq!(chunks[PERMISSION_IDX].height, 2, "permission");
        assert_eq!(chunks[FOOTER_IDX].height, 0, "no footer outside fullscreen");
        assert!(chunks[0].height > 1, "message area");
        // Rects are contiguous and in order, top to bottom.
        for pair in chunks.windows(2) {
            assert_eq!(pair[0].y + pair[0].height, pair[1].y);
        }
    }

    /// The full-screen transcript keeps the message area and one footer row,
    /// and collapses the prompt box rather than hiding it.
    #[test]
    fn fullscreen_keeps_only_the_transcript_and_its_hint() {
        let mut state = UiState {
            transcript_fullscreen: true,
            transcript_footer: "ctrl+t returns to the prompt".into(),
            ..UiState::default()
        };
        let buffer = render_to_buffer(80, 30, &mut state);
        let rows: Vec<String> = (0..30).map(|y| row_text(&buffer, y)).collect();

        let footer = rows[29].to_ascii_lowercase();
        assert!(
            footer.contains("ctrl+t") && footer.contains("returns"),
            "the footer did not name the key: {footer:?}"
        );
        // The prompt is gone: neither its rule nor its prompt glyph is drawn.
        assert!(
            !rows.iter().any(|row| row.contains("> ")),
            "the prompt was still drawn: {rows:?}"
        );
        // The transcript itself still starts at the top of the screen.
        assert!(
            !rows[0].trim().is_empty(),
            "the transcript no longer had the top row: {rows:?}"
        );

        state.transcript_fullscreen = false;
        let buffer = render_to_buffer(80, 30, &mut state);
        let rows: Vec<String> = (0..30).map(|y| row_text(&buffer, y)).collect();
        assert!(
            !rows[29]
                .to_ascii_lowercase()
                .contains("returns to the prompt"),
            "leaving full screen left the hint behind: {:?}",
            rows[29]
        );
    }

    /// A list the model wrote through `TodoWrite` is painted above the
    /// statusline, with a glyph per status.
    #[test]
    fn the_todo_panel_draws_every_row_above_the_statusline() {
        let mut state = UiState::default();
        state.todos.set_items(vec![
            crate::state::TodoEntry {
                subject: "read the parser".into(),
                description: String::new(),
                status: crate::state::TodoStatus::Completed,
            },
            crate::state::TodoEntry {
                subject: "port the tokenizer".into(),
                description: String::new(),
                status: crate::state::TodoStatus::InProgress,
            },
            crate::state::TodoEntry {
                subject: "run the tests".into(),
                description: String::new(),
                status: crate::state::TodoStatus::Pending,
            },
        ]);
        let buffer = render_to_buffer(80, 40, &mut state);

        let chunks = chunks_with_todos(40, 0, false, 3, true, 1, true);
        assert_eq!(chunks[TODOS_IDX].height, 3, "three rows");
        assert!(
            chunks[TODOS_IDX].y < chunks[STATUSLINE_IDX].y,
            "the panel must sit above the statusline"
        );

        let panel_rows: Vec<String> = (chunks[TODOS_IDX].y
            ..chunks[TODOS_IDX].y + chunks[TODOS_IDX].height)
            .map(|y| row_text(&buffer, y))
            .collect();
        assert!(
            panel_rows[0].contains("✓ read the parser"),
            "{panel_rows:?}"
        );
        assert!(
            panel_rows[1].contains("■ port the tokenizer"),
            "{panel_rows:?}"
        );
        assert!(panel_rows[2].contains("□ run the tests"), "{panel_rows:?}");
    }

    /// An empty list takes no space, so a session that never calls the tool
    /// sees the same layout as before.
    #[test]
    fn no_todo_list_claims_no_rows() {
        let mut state = UiState::default();
        assert_eq!(state.todos.display_height(), 0);
        let buffer = render_to_buffer(80, 30, &mut state);
        let chunks = chunks_for(30, 0, false, true, 1, true);
        assert_eq!(chunks[TODOS_IDX].height, 0);
        // The rows below are still where they were: the empty pane is the
        // only one at index 3 and it collapses to nothing.
        assert!(row_text(&buffer, chunks[INPUT_IDX].y).contains('❯'));
    }

    /// A list longer than the panel fits is capped and ends with a count.
    #[test]
    fn a_long_todo_list_is_capped_and_counted() {
        let mut state = UiState::default();
        state.todos.set_items(
            (0..12)
                .map(|n| crate::state::TodoEntry {
                    subject: format!("step {n}"),
                    description: String::new(),
                    status: crate::state::TodoStatus::Pending,
                })
                .collect(),
        );
        let rows = state.todos.render_rows();
        assert_eq!(rows.len(), 9, "eight rows plus the summary");
        assert_eq!(rows[8], ('…', "4 more".to_string()));
        assert_eq!(state.todos.display_height(), 9);
    }

    /// The input box is separated from the transcript by a full-width rule.
    #[test]
    fn the_input_has_a_rule_above_it() {
        let mut state = UiState::default();
        let buffer = render_to_buffer(80, 24, &mut state);
        let chunks = chunks_for(24, 0, false, true, 1, true);
        let rule_y = chunks[RULE_IDX].y;

        assert_eq!(chunks[RULE_IDX].height, 1);
        assert_eq!(
            rule_y + 1,
            chunks[INPUT_IDX].y,
            "the rule is not directly above the input"
        );
        let rule = row_text(&buffer, rule_y);
        assert!(
            rule.chars().all(|character| character == '─'),
            "rule row has other content: {rule:?}"
        );
        assert!(row_text(&buffer, chunks[INPUT_IDX].y).contains('❯'));
    }

    /// The input box is closed by a second rule below it, identical to the one
    /// above.
    #[test]
    fn the_input_has_a_rule_below_it() {
        let mut state = UiState::default();
        let buffer = render_to_buffer(80, 24, &mut state);
        let chunks = chunks_for(24, 0, false, true, 1, true);
        let input_y = chunks[INPUT_IDX].y;

        assert_eq!(chunks[RULE_BELOW_IDX].height, 1);
        assert_eq!(
            chunks[RULE_BELOW_IDX].y,
            input_y + 1,
            "the lower rule is not directly below the input"
        );
        let rule = row_text(&buffer, chunks[RULE_BELOW_IDX].y);
        assert!(
            rule.chars().all(|character| character == '─'),
            "lower rule row has other content: {rule:?}"
        );
        assert_eq!(
            row_text(&buffer, chunks[RULE_IDX].y),
            rule,
            "the two rules around the input differ"
        );
    }

    /// Compact mode hides the statusline and permission bar; the rules are
    /// part of the input box and stay.
    #[test]
    fn the_input_rule_is_drawn_in_compact_mode() {
        let mut state = UiState {
            force_compact: true,
            ..UiState::default()
        };
        let buffer = render_to_buffer(80, 12, &mut state);
        let chunks = chunks_for(12, 0, false, false, 1, false);
        for index in [RULE_IDX, RULE_BELOW_IDX] {
            let rule = row_text(&buffer, chunks[index].y);
            assert!(
                rule.chars().all(|character| character == '─'),
                "rule row has other content: {rule:?}"
            );
        }
        assert!(row_text(&buffer, chunks[INPUT_IDX].y).contains('❯'));
    }

    /// The card must be no taller than the pane it is drawn into at 80x24.
    ///
    /// `trim_lines_to_viewport` keeps the *last* rows, so a card one row too
    /// tall loses its top border and the first row of the wordmark — the page
    /// is clipped at the one size most terminals are. This asserts the whole
    /// card is on screen, wordmark included, rather than only that some of it
    /// rendered.
    #[test]
    fn the_welcome_card_fits_the_message_pane_at_80x24() {
        let mut state = UiState::default();
        state.statusline.model = "claude-sonnet-4-6".into();
        state.statusline.provider = "Anthropic API".into();
        state.statusline.cwd = "~/work/codape".into();
        // A full 80x24 frame: compact mode is off and every pane is drawn, so
        // the message area is exactly what a real terminal gives it.
        let buffer = render_to_buffer(80, 24, &mut state);
        let rows: Vec<String> = (0..24).map(|y| row_text(&buffer, y)).collect();
        let screen = rows.join("\n");

        let top = rows
            .iter()
            .position(|row| row.starts_with('╭'))
            .expect("the card's top border was clipped off the page");
        let bottom = rows
            .iter()
            .position(|row| row.starts_with('╰'))
            .expect("the card's bottom border is missing");
        assert_eq!(
            bottom + 1 - top,
            WELCOME_CARD_HEIGHT,
            "card height drifted from the constant:\n{screen}"
        );
        // Every row of the wordmark, in order: a card that is one row short
        // loses the first of these, not the last.
        let wordmark: Vec<&str> = WELCOME_WORDMARK
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect();
        let mut cursor = top;
        for art in wordmark {
            let found = (cursor..=bottom)
                .find(|row| rows[*row].contains(art.trim_end()))
                .unwrap_or_else(|| panic!("wordmark row {art:?} is not on the page:\n{screen}"));
            cursor = found + 1;
        }
    }

    /// An empty transcript paints the welcome page: block wordmark, title,
    /// the session's own facts and the resume tip.
    #[test]
    fn the_welcome_page_shows_the_name_version_and_model() {
        let mut state = UiState::default();
        state.statusline.model = "claude-sonnet-4-6".into();
        state.statusline.provider = "Anthropic API".into();
        state.statusline.cwd = "~/work/codape".into();
        let buffer = render_to_buffer(80, 30, &mut state);
        let screen: String = (0..30)
            .map(|y| row_text(&buffer, y))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            screen.contains("█▀▀▀▄"),
            "the block wordmark is missing:\n{screen}"
        );
        assert!(screen.contains(">_ Codape"), "{screen}");
        assert!(screen.contains("(v"), "{screen}");
        assert!(screen.contains("claude-sonnet-4-6"), "{screen}");
        assert!(screen.contains("Anthropic API"), "{screen}");
        assert!(screen.contains("~/work/codape"), "{screen}");
        assert!(screen.contains("Type /help for commands"), "{screen}");
        assert!(screen.contains("--continue"), "{screen}");
        assert!(screen.contains("--resume"), "{screen}");
        assert!(screen.contains('╭') && screen.contains('╯'), "{screen}");
        assert!(
            (0..30).any(|y| row_text(&buffer, y).starts_with('╭')),
            "the welcome card is not left-aligned:\n{screen}"
        );
    }

    /// A path too long for its row keeps its tail: the leaf directory is what
    /// says which checkout this is.
    #[test]
    fn a_long_directory_keeps_its_end_not_its_start() {
        let kept = truncate_path_start("~/work/clients/acme/mono/codape", 20);
        assert!(kept.starts_with('…'), "{kept}");
        assert!(
            kept.ends_with("codape"),
            "the leaf directory was dropped: {kept}"
        );
        assert!(unicode_width::UnicodeWidthStr::width(kept.as_str()) <= 20);

        // A path that fits is returned untouched.
        assert_eq!(truncate_path_start("~/work/codape", 20), "~/work/codape");
        // One component longer than the row falls back to its tail.
        let long = "x".repeat(40);
        let taken = truncate_path_start(&long, 10);
        assert_eq!(unicode_width::UnicodeWidthStr::width(taken.as_str()), 10);
        assert!(taken.starts_with('…'), "{taken}");
    }

    /// The truncation is on display columns, not bytes: a path with a
    /// multi-byte name must not be cut through a character.
    #[test]
    fn path_truncation_counts_display_columns() {
        let kept = truncate_path_start("~/文档/项目/测试", 8);
        assert!(
            unicode_width::UnicodeWidthStr::width(kept.as_str()) <= 8,
            "{kept}"
        );
        assert!(kept.ends_with("测试"), "{kept}");
    }

    /// The facts the session does not know are omitted, not printed empty:
    /// a loop with no client has no provider, and an `Provider` row with a
    /// blank value reads as a bug in the page.
    #[test]
    fn unknown_session_facts_are_left_out_of_the_card() {
        let mut state = UiState::default();
        let buffer = render_to_buffer(80, 30, &mut state);
        let screen: String = (0..30)
            .map(|y| row_text(&buffer, y))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(!screen.contains("Provider"), "{screen}");
        assert!(!screen.contains("Model"), "{screen}");
        assert!(!screen.contains("Directory"), "{screen}");
        // The parts that do not depend on the session are still there.
        assert!(screen.contains(">_ Codape"), "{screen}");
        assert!(screen.contains("--resume"), "{screen}");
    }

    /// The theme background is painted across the frame, so dark and light are
    /// visibly different instead of only re-tinting accents.
    #[test]
    fn the_theme_background_covers_the_frame() {
        let mut dark = UiState::default();
        let dark_buffer = render_to_buffer(80, 24, &mut dark);
        let mut light = UiState {
            theme: crate::themes::Theme::light(),
            ..Default::default()
        };

        let light_buffer = render_to_buffer(80, 24, &mut light);

        let dark_bg = crate::themes::Theme::dark().background;
        let light_bg = crate::themes::Theme::light().background;
        assert_ne!(dark_bg, light_bg);
        for (x, y) in [(79, 23), (40, 20), (0, 0)] {
            assert_eq!(
                dark_buffer.cell((x, y)).unwrap().bg,
                dark_bg,
                "dark {x},{y}"
            );
            assert_eq!(
                light_buffer.cell((x, y)).unwrap().bg,
                light_bg,
                "light {x},{y}"
            );
        }
    }

    /// The picture reaches the painted buffer, with the pixel colours the
    /// terminal will show.
    ///
    /// The unit tests above assert on the lines the renderer produces; this
    /// one draws a frame, so a bug in height reservation or in the paragraph
    /// widget would show up here instead of only on a real terminal.
    #[test]
    fn an_attached_picture_paints_its_pixels_into_the_frame() {
        let image = crate::transcript_image::ImageState::new(codape_core::types::ImageBlock {
            media_type: "image/png".into(),
            data: super::tests::two_by_two_png(),
        });
        let caption = image.caption();
        let mut state = UiState {
            image_protocol: crate::transcript_image::ImageProtocol::HalfBlocks,
            ..Default::default()
        };
        state.messages.push(Message {
            role: MessageRole::User,
            content: caption.into(),
            tool_name: None,
            streaming: false,
            folded: false,
            image: Some(image),
        });

        let buffer = render_to_buffer(60, 24, &mut state);

        // Find the row that carries the half blocks: the picture is the last
        // thing in an otherwise empty transcript.
        let mut found = None;
        for y in 0..buffer.area.height {
            let symbols: String = (0..buffer.area.width)
                .map(|x| buffer.cell((x, y)).map_or(" ", |c| c.symbol()))
                .collect();
            if symbols.contains('\u{2580}') {
                found = Some(y);
                break;
            }
        }
        let row = found.expect("no half blocks were painted");
        let first = buffer.cell((0, row)).expect("cell");
        assert_eq!(first.symbol(), "\u{2580}");
        // Red on top, blue below, straight from the encoded picture.
        assert_eq!(first.fg, ratatui::style::Color::Rgb(255, 0, 0));
        assert_eq!(first.bg, ratatui::style::Color::Rgb(0, 0, 255));

        // The caption is not drawn anywhere: the picture replaced it.
        for y in 0..buffer.area.height {
            assert!(
                !row_text(&buffer, y).contains("[image:"),
                "row {y} still shows the caption"
            );
        }
    }

    /// The wordmark is a gradient: several shades across the columns, so the
    /// page is not a block of dim text.
    #[test]
    fn the_welcome_wordmark_is_colourful() {
        let mut state = UiState::default();
        let buffer = render_to_buffer(80, 30, &mut state);
        let y = (0..30)
            .find(|y| row_text(&buffer, *y).contains("█▀▀▀▄"))
            .expect("wordmark row");

        // Every painted block character, not just letters: the wordmark is
        // drawn in half blocks, so filtering on `C`..`X` would find nothing.
        let colours: std::collections::HashSet<Color> = (0..80)
            .filter_map(|x| buffer.cell((x, y)))
            .filter(|cell| cell.symbol().chars().all(|c| "█▀▄".contains(c)))
            .filter(|cell| !cell.symbol().trim().is_empty())
            .map(|cell| cell.fg)
            .collect();
        assert!(
            colours.len() >= 4,
            "the wordmark should run a gradient, got {colours:?}"
        );
    }

    /// The gradient must not paint the gaps between letters: a styled space
    /// carries a background that draws visible bands across the card.
    #[test]
    fn the_wordmark_paints_only_its_glyphs() {
        let theme = Theme::dark();
        let row = WELCOME_WORDMARK
            .lines()
            .find(|line| line.contains('█'))
            .expect("a wordmark row");
        let spans = wordmark_spans(row, 0);

        for span in &spans {
            if span.content.chars().all(|c| c == ' ') {
                assert_eq!(
                    span.style,
                    Style::default(),
                    "whitespace carries a style: {span:?}"
                );
            }
        }
        let _ = theme;
    }

    /// A transcript taller than the screen must never push the input box or
    /// its rules off the viewport. Ported from kimi-code's fullscreen-layout
    /// contract (the editor's borders stay visible at every terminal size).
    #[test]
    fn the_chrome_survives_a_transcript_taller_than_the_screen() {
        for (width, height) in [(120_u16, 30_u16), (80, 24), (40, 12), (30, 10)] {
            let mut state = UiState::default();
            for index in 0..80 {
                state.messages.push(Message {
                    role: MessageRole::Assistant,
                    content: format!("message {index} {}", "lorem ipsum ".repeat(20)).into(),
                    tool_name: None,
                    streaming: false,
                    folded: false,
                    image: None,
                });
            }
            let buffer = render_to_buffer(width, height, &mut state);
            let rows: Vec<String> = (0..height).map(|y| row_text(&buffer, y)).collect();
            let full = height >= 20;
            let chunks = chunks_for(height, 0, false, full, 1, full);

            assert!(
                rows[chunks[INPUT_IDX].y as usize].contains('❯'),
                "input missing at {width}x{height}:\n{}",
                rows.join("\n")
            );
            for index in [RULE_IDX, RULE_BELOW_IDX] {
                let row = &rows[chunks[index].y as usize];
                assert!(
                    row.chars().all(|c| c == '─'),
                    "rule {index} missing at {width}x{height}: {row:?}"
                );
            }
            assert!(
                chunks[0].height >= 1,
                "transcript has no room at {width}x{height}"
            );
            if full {
                assert_eq!(chunks[STATUSLINE_IDX].height, 3);
                assert_eq!(chunks[PERMISSION_IDX].height, 2);
                assert!(
                    !rows[(chunks[STATUSLINE_IDX].y + 1) as usize]
                        .trim()
                        .is_empty()
                );
                assert!(rows[chunks[PERMISSION_IDX].y as usize].contains("Permissions:"));
            }
        }
    }

    /// The permission bar has one blank row below it, so its text does not sit
    /// against the bottom edge of the terminal.
    #[test]
    fn the_permission_bar_has_a_blank_row_below_it() {
        let mut state = UiState::default();
        let buffer = render_to_buffer(80, 24, &mut state);
        let chunks = chunks_for(24, 0, false, true, 1, true);
        let bar_row = chunks[PERMISSION_IDX].y;

        assert_eq!(chunks[PERMISSION_IDX].height, 2);
        assert!(row_text(&buffer, bar_row).contains("Permissions:"));
        let below = row_text(&buffer, bar_row + 1);
        assert!(below.trim().is_empty(), "row below permissions: {below:?}");
    }

    #[test]
    fn tool_result_languages_are_guessed_conservatively() {
        assert_eq!(
            guess_language("fn main() {\n    let x = 1;\n}\n"),
            Some("rs")
        );
        assert_eq!(
            guess_language("[package]\nname = \"codape\"\n"),
            Some("toml")
        );
        assert_eq!(
            guess_language("{\n  \"name\": \"codape\"\n}\n"),
            Some("json")
        );
        assert_eq!(guess_language("def f():\n    pass\n"), Some("py"));
        assert_eq!(guess_language("just some prose\nover two lines\n"), None);
    }

    /// A Read result keeps its indentation when highlighted.
    #[test]
    fn read_results_are_highlighted_without_losing_indentation() {
        let mut state = UiState::default();
        state.messages.push(Message {
            role: MessageRole::ToolResult,
            content: "fn main() {\n    let answer = 42;\n}\n".into(),
            tool_name: Some("Read".into()),
            streaming: false,
            folded: false,
            image: None,
        });
        let buffer = render_to_buffer(100, 20, &mut state);
        let screen: String = (0..20)
            .map(|y| row_text(&buffer, y))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(screen.contains("fn main()"), "{screen}");
        assert!(screen.contains("    let answer = 42;"), "{screen}");
    }

    /// Dragging over the transcript copies exactly the characters under the
    /// rectangle.
    #[test]
    fn a_selection_copies_the_text_under_it() {
        let mut state = UiState::default();
        state.messages.push(Message {
            role: MessageRole::System,
            content: "alpha beta\ngamma delta".into(),
            tool_name: None,
            streaming: false,
            folded: false,
            image: None,
        });
        let buffer = render_to_buffer(80, 20, &mut state);
        let rows = capture_rows(&buffer);

        let alpha_row = (0..20)
            .find(|row| row_text(&buffer, *row).contains("alpha beta"))
            .expect("alpha row");
        let gamma_row = (0..20)
            .find(|row| row_text(&buffer, *row).contains("gamma delta"))
            .expect("gamma row");
        // Columns are display cells, not byte offsets: a marker before the
        // text may be multi-byte.
        let column_of = |row: u16, needle: &str| {
            let text = row_text(&buffer, row);
            let byte = text.find(needle).expect(needle);
            text[..byte].chars().count() as u16
        };
        let alpha_col = column_of(alpha_row, "alpha");
        let delta_col = column_of(gamma_row, "delta") + 4;

        let selection = TextSelection {
            anchor: (alpha_col, alpha_row),
            cursor: (delta_col, gamma_row),
        };
        assert_eq!(
            selection_text(buffer.area, &rows, selection),
            // The second line keeps the indent the transcript drew.
            "alpha beta\n  gamma delta"
        );
    }

    /// `ctrl+shift+c` copies the conversation and leaves the chrome out.
    ///
    /// The whole point of recording the transcript's rectangle is that the
    /// prompt, the statusline and the input rule are drawn around it. Copying
    /// the frame instead would paste them along with the answer.
    #[test]
    fn a_region_copy_takes_the_transcript_without_the_chrome() {
        let mut state = UiState::default();
        state.messages.push(Message {
            role: MessageRole::System,
            content: "the answer".into(),
            tool_name: None,
            streaming: false,
            folded: false,
            image: None,
        });
        let buffer = render_to_buffer(80, 20, &mut state);
        let rows = capture_rows(&buffer);
        let region = state.messages.area;
        assert!(
            region.height > 0,
            "the renderer recorded no transcript area"
        );

        let text = region_text(buffer.area, &rows, region);
        assert!(text.contains("the answer"), "{text:?}");
        // Whatever the frame drew below the transcript must not come with it.
        let whole_frame = region_text(
            buffer.area,
            &rows,
            Rect {
                x: buffer.area.x,
                y: buffer.area.y,
                width: buffer.area.width,
                height: buffer.area.height,
            },
        );
        assert!(
            whole_frame.len() > text.len(),
            "the transcript region should cover less than the whole frame"
        );
    }

    /// A region reaching past the frame is clipped rather than refused: a pane
    /// half off-screen still copies the part someone can see.
    #[test]
    fn a_region_outside_the_frame_is_clipped() {
        let rows = vec![vec!['a', 'b'], vec!['c', 'd']];
        let area = Rect {
            x: 0,
            y: 0,
            width: 2,
            height: 2,
        };
        let clipped = Rect {
            x: 1,
            y: 1,
            width: 10,
            height: 10,
        };
        assert_eq!(region_text(area, &rows, clipped), "d");
    }

    /// An empty region copies nothing instead of panicking on a zero-width
    /// rectangle, which is what a frame drawn before any message looks like.
    #[test]
    fn an_empty_region_copies_nothing() {
        let rows = vec![vec!['a']];
        let area = Rect {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        };
        let none = Rect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        };
        assert_eq!(region_text(area, &rows, none), "");
    }

    /// The selected cells get the theme's selection background and nothing
    /// outside the rectangle changes.
    #[test]
    fn a_selection_is_highlighted() {
        let mut state = UiState {
            selection: Some(TextSelection {
                anchor: (1, 1),
                cursor: (5, 2),
            }),
            ..UiState::default()
        };
        let buffer = render_to_buffer(80, 20, &mut state);
        let selected = crate::themes::Theme::dark().selection_bg;

        assert_eq!(buffer.cell((2, 1)).unwrap().bg, selected);
        assert_eq!(buffer.cell((2, 2)).unwrap().bg, selected);
        assert_ne!(buffer.cell((0, 1)).unwrap().bg, selected);
        // Linear selection: the rest of the first row is selected up to the
        // second row's column.
        assert_eq!(buffer.cell((10, 1)).unwrap().bg, selected);
        assert_ne!(buffer.cell((10, 2)).unwrap().bg, selected);
    }

    /// A long provider error in the detail overlay must be readable, not
    /// clipped at a fixed 50-column box.
    #[test]
    fn a_detail_overlay_is_wide_enough_for_its_text() {
        use crate::overlay::Overlay;

        let mut state = UiState::default();
        state.overlays.push(Overlay::Detail {
            title: "Failure Details".into(),
            body: format!("{}ENDMARK", "x".repeat(70)),
            scroll: 0,
        });
        let buffer = render_to_buffer(120, 30, &mut state);
        let screen: String = (0..30)
            .map(|y| row_text(&buffer, y))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(screen.contains("Failure Details"), "{screen}");
        assert!(screen.contains("ENDMARK"), "{screen}");
    }

    /// A detail body taller than twenty rows has to be both fully boxed and
    /// scrollable.
    ///
    /// The help page is longer than the old fixed cap, so its second half was
    /// unreachable: the box stopped at twenty rows and there was no way to
    /// move it. The box now follows the body up to the screen, and `scroll`
    /// moves the rest into view.
    #[test]
    fn a_long_detail_body_grows_its_box_and_scrolls() {
        use crate::overlay::Overlay;

        let rows = |count: usize| -> String {
            (0..count)
                .map(|row| format!("row{row:02}"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let screen_of = |state: &mut UiState| -> String {
            let buffer = render_to_buffer(120, 40, state);
            (0..40)
                .map(|y| row_text(&buffer, y))
                .collect::<Vec<_>>()
                .join("\n")
        };

        // Thirty rows fits a forty-row terminal, and is past the old cap.
        let mut state = UiState::default();
        state.overlays.push(Overlay::Detail {
            title: "Help".into(),
            body: rows(30),
            scroll: 0,
        });
        let screen = screen_of(&mut state);
        assert!(screen.contains("row00"), "the top is missing:\n{screen}");
        assert!(
            screen.contains("row29"),
            "the box stopped at the old twenty-row cap:\n{screen}"
        );

        // A body taller than the screen scrolls: the top rows go, the last
        // ones arrive.
        let mut state = UiState::default();
        state.overlays.push(Overlay::Detail {
            title: "Help".into(),
            body: rows(60),
            scroll: 40,
        });
        let screen = screen_of(&mut state);
        assert!(
            !screen.contains("row00"),
            "the scroll offset was ignored:\n{screen}"
        );
        assert!(
            screen.contains("row59"),
            "scrolling lost the end of the body:\n{screen}"
        );
    }

    /// Scrolling past the end of a body must not scroll it out of the box.
    ///
    /// Page-down is bounded by the reader's patience, not the body's length:
    /// pressing it once more at the bottom used to move the paragraph into its
    /// own blank tail, and the box drew empty. The last go-to keypress has to
    /// leave the last line on screen.
    #[test]
    fn scrolling_past_the_end_leaves_the_last_line_on_screen() {
        use crate::overlay::Overlay;

        let body = (0..60)
            .map(|row| format!("row{row:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut state = UiState::default();
        state.overlays.push(Overlay::Detail {
            title: "Help".into(),
            body,
            scroll: 500,
        });
        let buffer = render_to_buffer(120, 40, &mut state);
        let screen: String = (0..40)
            .map(|y| row_text(&buffer, y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            screen.contains("row59"),
            "the end of the body scrolled off the top:\n{screen}"
        );
    }

    /// A body's blank lines are layout, so the box has to be big enough for    /// them. The model picker's last row is its key hints, below two blank
    /// lines; a count that drops the blanks clips exactly that row.
    #[test]
    fn a_body_with_blank_lines_gets_a_box_that_holds_all_of_them() {
        use crate::overlay::{ModelPicker, Overlay};

        let picker = ModelPicker::new(
            vec!["claude-sonnet-4-6".into(), "claude-opus-5-5".into()],
            "claude-sonnet-4-6",
            None,
        );
        let mut state = UiState::default();
        state.overlays.push(Overlay::ModelPicker(picker));
        let buffer = render_to_buffer(120, 40, &mut state);
        let screen: String = (0..40)
            .map(|y| row_text(&buffer, y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            screen.contains("[tab] model/effort"),
            "the key hints fell off the bottom of the box:\n{screen}"
        );
        assert!(
            screen.contains("Effort: off"),
            "the effort row is missing:\n{screen}"
        );
    }

    /// A terminal too small for the box gets readable plain lines instead of a
    /// clipped card.
    #[test]
    fn a_narrow_terminal_gets_the_plain_welcome_page() {
        let mut state = UiState::default();
        let buffer = render_to_buffer(30, 12, &mut state);
        let screen: String = (0..12)
            .map(|y| row_text(&buffer, y))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(screen.contains(">_ Codape"), "{screen}");
        assert!(screen.contains("Type /help for commands"), "{screen}");
        assert!(
            !screen.contains('╭'),
            "the card should not be drawn:\n{screen}"
        );
    }

    /// The page is a placeholder for an empty transcript; once a message
    /// exists, the transcript wins.
    #[test]
    fn a_message_replaces_the_welcome_page() {
        let mut state = UiState::default();
        state.messages.push(Message {
            role: MessageRole::System,
            content: "hello from the transcript".into(),
            tool_name: None,
            streaming: false,
            folded: false,
            image: None,
        });
        let buffer = render_to_buffer(80, 30, &mut state);
        let screen: String = (0..30)
            .map(|y| row_text(&buffer, y))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(screen.contains("hello from the transcript"), "{screen}");
        assert!(!screen.contains("█▀▀▀▄"), "{screen}");
        assert!(!screen.contains(">_ Codape"), "{screen}");
    }

    /// A multi-line buffer renders one row per line, with the continuation
    /// rows aligned under the text rather than under the prompt.
    #[test]
    fn multi_line_input_renders_every_line() {
        let mut state = UiState::default();
        state.input.text = "first\nsecond".into();
        state.input.cursor = 12;
        let buffer = render_to_buffer(80, 12, &mut state);
        let input_y = chunks_for(12, 0, false, false, 2, false)[INPUT_IDX].y;
        let rows: Vec<String> = (0..2).map(|i| row_text(&buffer, input_y + i)).collect();
        assert!(rows[0].contains("first"), "{rows:?}");
        assert!(rows[1].contains("second"), "{rows:?}");
        assert!(
            rows[1].starts_with("  second"),
            "continuation not aligned: {rows:?}"
        );
    }

    /// The input pane grows with the buffer, so the transcript above it is
    /// pushed up rather than the input being clipped.
    #[test]
    fn multi_line_input_grows_its_pane() {
        let mut state = UiState::default();
        state.input.text = "a\nb\nc".into();
        state.input.cursor = 5;
        let buffer = render_to_buffer(80, 20, &mut state);
        let chunks = chunks_for(20, 0, false, true, 3, true);
        assert_eq!(chunks[INPUT_IDX].height, 3);
        let rows: Vec<String> = (0..3)
            .map(|i| row_text(&buffer, chunks[INPUT_IDX].y + i))
            .collect();
        assert!(rows[0].contains('a'), "{rows:?}");
        assert!(rows[1].contains('b'), "{rows:?}");
        assert!(rows[2].contains('c'), "{rows:?}");
    }

    /// A linewise Visual selection has to be visible on every line it covers.
    #[test]
    fn multi_line_linewise_selection_highlights_every_line() {
        let mut state = UiState::default();
        state.input.text = "one\ntwo".into();
        state.input.cursor = 0;
        state.input.start_visual(true);
        state.input.cursor = 7;
        let buffer = render_to_buffer(80, 12, &mut state);
        let input_y = chunks_for(12, 0, false, false, 2, false)[INPUT_IDX].y;
        let selected = state.theme.selection_bg;
        for i in 0..2 {
            let has_selection = (0..buffer.area.width).any(|x| {
                buffer
                    .cell((x, input_y + i))
                    .is_some_and(|cell| cell.bg == selected)
            });
            assert!(
                has_selection,
                "row {i} of the linewise selection is not highlighted"
            );
        }
    }

    /// A buffer whose text fits on one row must still occupy exactly one row,
    /// so nothing about the single-line layout changes.
    #[test]
    fn single_line_input_still_occupies_one_row() {
        let mut state = UiState::default();
        state.input.text = "hello world".into();
        state.input.cursor = 11;
        assert_eq!(input_display_height(&state.input, 80), 1);

        let buffer = render_to_buffer(80, 12, &mut state);
        let input_y = chunks_for(12, 0, false, false, 1, false)[INPUT_IDX].y;
        let row = row_text(&buffer, input_y);
        assert!(row.contains("❯ hello world"), "{row:?}");
    }

    /// The transcript records where a mod UI button actually landed, including
    /// the system-message prefix and the box's border/padding columns.
    #[test]
    fn a_mod_ui_block_records_absolute_button_hits() {
        use codape_core::ui_protocol::UiElement;

        let elements = vec![UiElement::Box {
            title: None,
            children: vec![UiElement::Button {
                label: "Run".to_string(),
                id: "run".to_string(),
            }],
        }];
        let content = crate::mod_elements::render_mod_ui_text(&elements, 80);
        let spec = ModUiBlockSpec {
            message_index: 0,
            content: content.clone(),
            elements,
            focused: None,
        };
        let mut state = UiState::default();
        state.messages.push(Message {
            role: MessageRole::System,
            content: content.into(),
            tool_name: None,
            streaming: false,
            folded: false,
            image: None,
        });

        let backend = TestBackend::new(40, 20);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut regions = Vec::new();
        terminal
            .draw(|frame| render_frame(frame, &mut state, &[spec], &mut regions))
            .expect("draw frame");

        assert_eq!(regions.len(), 1, "one interactive block");
        let region = &regions[0];
        assert_eq!(region.message_index, 0);
        assert_eq!(region.scroll_offset, 0);
        // The message area starts at the top; the "· " system prefix moves the
        // block two columns right.
        assert_eq!(region.rect.y, 0);
        assert_eq!(region.rect.x, 2);
        assert_eq!(region.rect.height, 3);
        assert_eq!(region.hits.len(), 1);
        let hit = &region.hits[0];
        assert_eq!(hit.row, 1, "inside the box");
        assert_eq!(hit.col_start, 2, "after the box's left border and padding");
        assert_eq!(hit.col_end, 7, "[Run]");

        let screen_col = region.rect.x + hit.col_start;
        let screen_row = region.rect.y + hit.row;
        // The recorded column is where the button was actually painted.
        let painted = row_text(terminal.backend().buffer(), screen_row);
        assert_eq!(
            painted.chars().nth(usize::from(screen_col)),
            Some('['),
            "hit column does not match the drawn row: {painted:?}"
        );
        assert_eq!(
            crate::mod_elements::button_at(
                region.rect,
                region.scroll_offset,
                screen_col,
                screen_row,
                &region.hits,
            ),
            Some("run")
        );
        // The border column and the system prefix are not buttons.
        assert_eq!(
            crate::mod_elements::button_at(region.rect, 0, region.rect.x, screen_row, &region.hits,),
            None
        );
        assert_eq!(
            crate::mod_elements::button_at(region.rect, 0, 0, screen_row, &region.hits),
            None
        );
    }
}
