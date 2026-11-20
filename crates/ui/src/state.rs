//! UI state — separate from business kernel for immediate-mode rendering.

use crate::history_search::HistorySearch;
use crate::input::InputBuffer;
use crate::message_list::MessageView;
use crate::notification_center::NotificationCenter;
use crate::overlay::OverlayStack;
use crate::permission_bar::PermissionBar;
use crate::search::SearchState;
use crate::selector::MessageSelector;
use crate::statusline::Statusline;
use crate::statusline_setup::StatuslineSetup;
use crate::themes::Theme;
use crate::voice::VoiceState;

/// The active view mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    /// Main REPL view.
    MainRepl,
    /// Permission modal view.
    PermissionModal,
    /// Message selector view.
    MessageSelector,
    /// Command palette view.
    CommandPalette,
    /// Help overlay view.
    HelpOverlay,
    /// Search mode.
    Search,
    /// Voice recording mode.
    VoiceRecording,
    /// Buddy companion panel.
    #[cfg(feature = "buddy")]
    BuddyPanel,
    /// Statusline setup wizard.
    StatuslineSetup,
}

/// Layout mode for the main content area.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LayoutMode {
    /// Single pane: full width for messages.
    #[default]
    SinglePane,
    /// Horizontal split: left pane shows file/content, right pane shows chat.
    HorizontalSplit {
        /// Width percentage for the left pane (10-90).
        left_percent: u16,
    },
}

/// A single visible plan step in the interactive TUI.
#[derive(Debug, Clone, Default)]
pub struct PlanStep {
    /// Short user-facing label.
    pub label: String,
    /// Whether this step has completed.
    pub complete: bool,
}

/// Lightweight plan state for the current interactive turn.
#[derive(Debug, Clone, Default)]
pub struct InteractivePlan {
    /// Visible steps tracked for the current turn.
    pub steps: Vec<PlanStep>,
    /// Zero-based current step index.
    pub current_step: usize,
    /// Label for the active step.
    pub current_label: Option<String>,
}

impl InteractivePlan {
    /// Clear the current plan.
    pub fn reset(&mut self) {
        self.steps.clear();
        self.current_step = 0;
        self.current_label = None;
    }

    /// Render the full plan for an overlay.
    ///
    /// This is what `/plan` prints. The statusline used to have a shorter
    /// form of it (`Step 2/4: ...`) as a fallback label; the semantic phase
    /// took that slot, so the long form is the only reader left.
    #[must_use]
    pub fn render_detail(&self) -> String {
        if self.steps.is_empty() {
            return "No active plan.\nStart a turn to populate the current plan.".into();
        }

        let mut lines = Vec::with_capacity(self.steps.len() + 2);
        lines.push(format!(
            "Current step: {}/{}",
            self.current_step.saturating_add(1).min(self.steps.len()),
            self.steps.len()
        ));
        lines.push(String::new());
        for (idx, step) in self.steps.iter().enumerate() {
            let marker = if step.complete {
                "[x]"
            } else if idx == self.current_step {
                "[>]"
            } else {
                "[ ]"
            };
            lines.push(format!("{marker} {}", step.label));
        }
        lines.join("\n")
    }
}

/// Status of one todo row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TodoStatus {
    /// Not yet started.
    #[default]
    Pending,
    /// Currently being worked on.
    InProgress,
    /// Done.
    Completed,
}

/// One row of the todo panel.
///
/// A display copy of the tool list: this crate depends only on `codape-core`,
/// so the session maps its own item type onto this one rather than sharing it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TodoEntry {
    /// Short subject shown on the row.
    pub subject: String,
    /// Longer description. Kept for a detail view; the panel shows the
    /// subject only so a row stays one line.
    pub description: String,
    /// Current status.
    pub status: TodoStatus,
}

/// The task list the model maintains through `TodoWrite`.
///
/// The tool owns the real list; this is the copy the renderer paints, so a row
/// appears as soon as the tool result reaches the loop.
#[derive(Debug, Clone, Default)]
pub struct TodoPanel {
    items: Vec<TodoEntry>,
}

/// Rows the panel will draw before it stops and counts the remainder.
const MAX_TODO_ROWS: usize = 8;

impl TodoPanel {
    /// Replace the visible list.
    pub fn set_items(&mut self, items: Vec<TodoEntry>) {
        self.items = items;
    }

    /// The current list.
    #[must_use]
    pub fn items(&self) -> &[TodoEntry] {
        &self.items
    }

    /// Whether the panel has anything to draw.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Drop the list.
    pub fn clear(&mut self) {
        self.items.clear();
    }

    /// Terminal rows the panel needs: one per item, plus a summary row when
    /// the list is longer than what fits.
    #[must_use]
    pub fn display_height(&self) -> u16 {
        if self.items.is_empty() {
            return 0;
        }
        let rows = self.items.len().min(MAX_TODO_ROWS);
        let overflow = u16::from(self.items.len() > MAX_TODO_ROWS);
        u16::try_from(rows).map_or(u16::MAX, |rows| rows + overflow)
    }

    /// Render one row as `(marker, text)` pairs.
    ///
    /// The last pair is the "+N more" summary when the list overflows.
    #[must_use]
    pub fn render_rows(&self) -> Vec<(char, String)> {
        let mut rows: Vec<(char, String)> = self
            .items
            .iter()
            .take(MAX_TODO_ROWS)
            .map(|item| (Self::marker(item.status), item.subject.clone()))
            .collect();
        if self.items.len() > MAX_TODO_ROWS {
            let hidden = self.items.len() - MAX_TODO_ROWS;
            rows.push(('…', format!("{hidden} more")));
        }
        rows
    }

    /// Status glyph for one item.
    #[must_use]
    pub const fn marker(status: TodoStatus) -> char {
        match status {
            TodoStatus::Pending => '□',
            TodoStatus::InProgress => '■',
            TodoStatus::Completed => '✓',
        }
    }
}

/// Richer permission-request details for overlays and notifications.
#[derive(Debug, Clone, Default)]
pub struct PermissionDetail {
    /// Requested tool/action.
    pub tool: String,
    /// Raw tool arguments summary.
    pub args: String,
    /// Risk summary or explanation shown to the user.
    pub risk_summary: String,
}

/// A text selection over the rendered frame.
///
/// Coordinates are absolute terminal cells `(column, row)`. The anchor is where
/// the selection started; the cursor is its free end. Selection is linear: each
/// row between the two points is copied whole up to the first/last column.
///
/// Two things fill it in. A mouse drag starts it at the press and moves the
/// cursor until the button is released, which copies. The keyboard starts it on
/// the newest transcript line and grows it upward with `shift`+arrows, so the
/// transcript can be read and copied without a pointer.
///
/// It is a selection over *cells of the frame*, not over messages: scrolling
/// the transcript moves the content under the highlight, and what is copied is
/// what the highlight is on. That is what the mouse path has always done, and
/// the highlight is visible either way, so nothing is copied unseen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextSelection {
    /// Where the selection began.
    pub anchor: (u16, u16),
    /// The free end of the selection.
    pub cursor: (u16, u16),
}

impl TextSelection {
    /// Start a selection at a cell.
    #[must_use]
    pub const fn new(column: u16, row: u16) -> Self {
        Self {
            anchor: (column, row),
            cursor: (column, row),
        }
    }

    /// The newest line of `area`, which is where a keyboard selection starts.
    ///
    /// Whole lines, not cells: the transcript is prose, every row between the
    /// two ends is copied whole anyway, and the cell-precision case is what
    /// the mouse drag is for.
    ///
    /// The anchor is the bottom row's *right* edge and the free end its left.
    /// Not the other way round: [`Self::normalized`] orders by row, so once the
    /// free end has moved up the anchor is the selection's last row, and the
    /// copy stops that row at the anchor's column. A bottom-left anchor would
    /// leave the newest line selected up to column zero.
    #[must_use]
    pub const fn keyboard_origin(area: ratatui::layout::Rect) -> Self {
        let bottom = area.bottom().saturating_sub(1);
        Self {
            anchor: (area.right().saturating_sub(1), bottom),
            cursor: (area.left(), bottom),
        }
    }

    /// Move the free end of the selection.
    pub const fn drag_to(&mut self, column: u16, row: u16) {
        self.cursor = (column, row);
    }

    /// Take one more row in, upward.
    ///
    /// Saturates at the top of the area, so the whole visible transcript is a
    /// key away rather than one press per row.
    pub fn extend_up(&mut self, area: ratatui::layout::Rect) {
        self.cursor = (area.left(), self.cursor.1.saturating_sub(1).max(area.top()));
    }

    /// Give one row back, downward, undoing [`Self::extend_up`].
    ///
    /// The free end moves down; the anchor stays put, so coming back to its
    /// row restores the whole line the selection began on.
    pub fn extend_down(&mut self, area: ratatui::layout::Rect) {
        let bottom = area.bottom().saturating_sub(1);
        self.cursor = (area.left(), self.cursor.1.saturating_add(1).min(bottom));
    }

    /// Take the whole visible transcript.
    pub const fn extend_all(&mut self, area: ratatui::layout::Rect) {
        self.cursor = (area.left(), area.top());
    }

    /// Go back to the newest line alone.
    pub const fn extend_last(&mut self, area: ratatui::layout::Rect) {
        self.cursor = (area.left(), area.bottom().saturating_sub(1));
    }

    /// The selection with its start point before its end point.
    #[must_use]
    pub fn normalized(&self) -> ((u16, u16), (u16, u16)) {
        if (self.anchor.1, self.anchor.0) <= (self.cursor.1, self.cursor.0) {
            (self.anchor, self.cursor)
        } else {
            (self.cursor, self.anchor)
        }
    }

    /// Whether the pointer has not moved since the press.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.anchor == self.cursor
    }
}

/// One view the left pane remembers.
///
/// A whole pane's content rather than a handle to it: the pane draws strings
/// it already holds, so a tab is the four fields it takes to redraw one. The
/// scroll offset comes with it, which is the point — switching away and back
/// should return you to the line you were on, not to the top.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneTab {
    /// The border title, as the pane shows it.
    pub title: String,
    /// The body.
    pub content: String,
    /// Whether the body is a diff, so its lines are coloured by kind.
    pub is_diff: bool,
    /// Lines the tab is scrolled down.
    pub scroll: usize,
}

/// UI state, owned exclusively by the rendering thread.
#[derive(Debug)]
pub struct UiState {
    /// Currently active view.
    pub active_view: ViewMode,
    /// Input buffer for user typing.
    pub input: InputBuffer,
    /// In-progress mouse selection over the frame, if any.
    pub selection: Option<TextSelection>,
    /// Message list with virtualization.
    pub messages: MessageView,
    /// Overlay stack.
    pub overlays: OverlayStack,
    /// Search state.
    pub search: SearchState,
    /// An open Ctrl+R reverse history search.
    ///
    /// While this is `Some`, the input line shows the query and the matched
    /// entries instead of the buffer, and keys go to the search rather than
    /// to the buffer.
    pub history_search: Option<HistorySearch>,
    /// Message selector (for multi-select mode).
    pub selector: Option<MessageSelector>,
    /// Statusline data.
    pub statusline: Statusline,
    /// Permission bar (below input line).
    pub permission_bar: PermissionBar,
    /// Statusline setup wizard state.
    pub statusline_setup: StatuslineSetup,
    /// Notification center.
    pub notifications: NotificationCenter,
    /// Active semantic color theme.
    pub theme: Theme,
    /// Lightweight plan state for the current interactive turn.
    pub plan: InteractivePlan,
    /// Task list written by the `TodoWrite` tool.
    pub todos: TodoPanel,
    /// Recent concise system activity rows for overlays and notifications.
    pub activity_log: Vec<String>,
    /// Details for the currently pending permission request.
    pub permission_detail: Option<PermissionDetail>,
    /// Whether compact mode is forced even on large terminals.
    pub force_compact: bool,
    /// Layout mode (single pane or split view).
    pub layout_mode: LayoutMode,
    /// Content shown in the left pane of split view.
    pub left_pane_content: String,
    /// Title shown on the left pane border (e.g., filename).
    pub left_pane_title: String,
    /// The left pane is showing a diff, so its lines are coloured by kind.
    ///
    /// A flag rather than a second content field: the pane shows one thing at
    /// a time, and a file opened with `/open` after `/diff` has to replace it.
    pub left_pane_is_diff: bool,
    /// Lines the diff pane is scrolled down.
    ///
    /// Clamped against the pane's drawn height when the renderer reads it, so
    /// a frame that shrank since the key was pressed cannot scroll past the
    /// end.
    pub left_pane_scroll: usize,
    /// The pane's open views, oldest first, when more than one is open.
    ///
    /// A tab is a view the pane remembers: `/diff`, then `/open src/lib.rs`,
    /// then the next file, leaves three, and switching back to the diff is a
    /// key rather than a command that has to rebuild it. Empty means the pane
    /// has a single view and no tab bar — the common case, and the one where
    /// a bar of one tab is pure cost.
    pub pane_tabs: Vec<PaneTab>,
    /// Which of [`Self::pane_tabs`] is showing.
    pub pane_tab_index: usize,
    /// Whether the transcript is filling the screen.
    ///
    /// Not a [`ViewMode`]: the view is the same view, drawn into more rows. A
    /// second mode would need every key branch to ask which mode it is in, and
    /// the answer here is only ever "does the input box have room".
    pub transcript_fullscreen: bool,
    /// Whether the todo panel is drawn.
    ///
    /// The panel is the model's own outline and is on by default; this is the
    /// person's switch to get the rows back for the transcript. It does not
    /// throw the list away: [`Self::todos`] keeps its items, so switching it
    /// back on shows the same list.
    pub todos_visible: bool,
    /// What the full-screen footer says, when the session set one.
    ///
    /// Set by whoever toggles [`Self::transcript_fullscreen`], because the
    /// renderer has no keymap: it is the session that knows which key its table
    /// resolves to, and a footer naming the built-in default would tell a
    /// person who rebound it to press a key that does nothing.
    pub transcript_footer: String,
    /// How the transcript draws an attached picture.
    ///
    /// Defaults to [`crate::transcript_image::ImageProtocol::None`] so a state
    /// built in a test or a headless path keeps the `[image: …]` caption. The
    /// interactive terminal sets it from the detected terminal before the
    /// first draw.
    pub image_protocol: crate::transcript_image::ImageProtocol,
    /// The voice input state machine.
    ///
    /// Carried on the state even when no transcription command is configured:
    /// the machine is what says "idle", and a field that appeared and
    /// disappeared would make every reader ask whether voice was on.
    pub voice: VoiceState,
    /// The prompt that was set aside, waiting to be taken back.
    ///
    /// One slot, not a stack. Stashing is "I need to ask something else
    /// first", which happens once at a time; a stack would make the second
    /// stash hide the first with no key that reaches it. Empty is the ordinary
    /// case and means nothing is held.
    ///
    /// On the state rather than in the session so the footer can draw it: a
    /// stashed prompt that nothing shows is a prompt people forget they have.
    pub stashed_input: Option<String>,
    /// The mod panels the session is showing.
    ///
    /// On the state, not in the engine: the panels are the host's, and the
    /// engine's `EngineEvent::ModPanel` only says what changed. Living here is
    /// what lets the same slots survive a redraw, a resize and a `/clear` —
    /// they are not transcript content, so clearing the conversation must not
    /// clear them.
    pub panels: crate::panel::PanelStore,
}

impl UiState {
    /// Create a new UiState with defaults.
    #[must_use]
    pub fn new() -> Self {
        Self {
            active_view: ViewMode::MainRepl,
            input: InputBuffer::new(),
            selection: None,
            messages: MessageView::new(1000),
            overlays: OverlayStack::new(),
            search: SearchState::new(),
            history_search: None,
            selector: None,
            statusline: Statusline::new(),
            permission_bar: PermissionBar::new(),
            statusline_setup: StatuslineSetup::new(),
            notifications: NotificationCenter::new(),
            theme: Theme::dark(),
            plan: InteractivePlan::default(),
            todos: TodoPanel::default(),
            activity_log: Vec::new(),
            permission_detail: None,
            force_compact: false,
            layout_mode: LayoutMode::SinglePane,
            left_pane_content: String::new(),
            left_pane_title: String::new(),
            left_pane_is_diff: false,
            left_pane_scroll: 0,
            pane_tabs: Vec::new(),
            pane_tab_index: 0,
            voice: VoiceState::new(),
            transcript_fullscreen: false,
            todos_visible: true,
            transcript_footer: String::new(),
            image_protocol: crate::transcript_image::ImageProtocol::None,
            stashed_input: None,
            panels: crate::panel::PanelStore::new(),
        }
    }

    /// Give the pane a new view, remembering the one it was showing.
    ///
    /// The live fields stay the source of truth for what is drawn and which
    /// tab is current; `pane_tabs` is the shelf behind them. Storing the
    /// outgoing view before replacing it is what makes "open something else"
    /// non-destructive, and a tab bar of one is never drawn, so a pane with a
    /// single view looks exactly as it did before tabs existed.
    pub fn pane_show(&mut self, title: String, content: String, is_diff: bool) {
        self.remember_pane_view();
        // Opening what is already open is not a second view: `/diff` after
        // `/diff` is a refresh of the same patch, and two tabs with the same
        // title would differ only by when they were taken. The existing tab
        // is reused, in place, so the bar does not grow on a repeated command.
        if let Some(index) = self
            .pane_tabs
            .iter()
            .position(|tab| tab.is_diff == is_diff && tab.title == title)
        {
            self.pane_tabs[index] = PaneTab {
                title,
                content,
                is_diff,
                scroll: 0,
            };
            self.pane_show_tab(index);
            return;
        }
        self.left_pane_title = title;
        self.left_pane_content = content;
        self.left_pane_is_diff = is_diff;
        self.left_pane_scroll = 0;
        self.pane_tab_index = self.pane_tabs.len();
        self.pane_tabs.push(PaneTab {
            title: self.left_pane_title.clone(),
            content: self.left_pane_content.clone(),
            is_diff,
            scroll: 0,
        });
    }

    /// Put the view on screen back on the shelf.
    ///
    /// Called before a new view replaces it and before switching away. A pane
    /// with nothing in it and a pane that is scrolled to the top are both
    /// "nothing worth remembering": pushing them would fill the shelf with
    /// empty tabs and make the bar appear for no reason.
    pub fn remember_pane_view(&mut self) {
        if self.left_pane_content.is_empty() {
            return;
        }
        let snapshot = PaneTab {
            title: self.left_pane_title.clone(),
            content: self.left_pane_content.clone(),
            is_diff: self.left_pane_is_diff,
            scroll: self.left_pane_scroll,
        };
        if let Some(existing) = self.pane_tabs.get_mut(self.pane_tab_index) {
            *existing = snapshot;
        } else {
            self.pane_tabs.push(snapshot);
            self.pane_tab_index = self.pane_tab_count().saturating_sub(1);
        }
    }

    /// How many views the pane remembers, the one on screen included.
    #[must_use]
    pub fn pane_tab_count(&self) -> usize {
        self.pane_tabs.len().max(1)
    }

    /// Switch to the next tab, or the previous one.
    ///
    /// Wraps: with two or three tabs there is no meaningful "end" to stop at,
    /// and a key that does nothing at the edge reads as broken. Does nothing
    /// at all with one view, because there is nothing to switch to.
    pub fn pane_cycle(&mut self, forward: bool) {
        let count = self.pane_tabs.len();
        if count < 2 {
            return;
        }
        self.remember_pane_view();
        let current = self.pane_tab_index.min(count - 1);
        let next = if forward {
            (current + 1) % count
        } else {
            (current + count - 1) % count
        };
        self.pane_show_tab(next);
    }

    /// Draw the tab at `index`, leaving the live fields pointed at it.
    fn pane_show_tab(&mut self, index: usize) {
        let Some(tab) = self.pane_tabs.get(index).cloned() else {
            return;
        };
        self.left_pane_title = tab.title;
        self.left_pane_content = tab.content;
        self.left_pane_is_diff = tab.is_diff;
        self.left_pane_scroll = tab.scroll;
        self.pane_tab_index = index;
    }
}

impl Default for UiState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shown(state: &UiState) -> (String, &str) {
        (
            state.left_pane_title.clone(),
            state.left_pane_content.as_str(),
        )
    }

    /// The area these tests select in: columns 2..=11, rows 5..=9.
    const AREA: ratatui::layout::Rect = ratatui::layout::Rect {
        x: 2,
        y: 5,
        width: 10,
        height: 5,
    };

    #[test]
    fn a_keyboard_selection_starts_on_the_newest_line() {
        let selection = TextSelection::keyboard_origin(AREA);
        // The last row, whole. The anchor is the row's end because
        // `normalized` sorts by row and the copy cuts the last row there.
        assert_eq!(selection.normalized(), ((2, 9), (11, 9)));
        assert!(!selection.is_empty());
    }

    #[test]
    fn shift_up_takes_one_more_row_and_saturates() {
        let mut selection = TextSelection::keyboard_origin(AREA);
        selection.extend_up(AREA);
        assert_eq!(selection.normalized(), ((2, 8), (11, 9)));
        selection.extend_all(AREA);
        assert_eq!(selection.normalized(), ((2, 5), (11, 9)));
        // Another press at the top is not a way to leave the area.
        selection.extend_up(AREA);
        assert_eq!(selection.normalized(), ((2, 5), (11, 9)));
    }

    #[test]
    fn shift_down_gives_the_rows_back_and_stops_at_the_last_line() {
        let mut selection = TextSelection::keyboard_origin(AREA);
        selection.extend_up(AREA);
        selection.extend_up(AREA);
        assert_eq!(selection.normalized(), ((2, 7), (11, 9)));
        selection.extend_down(AREA);
        assert_eq!(selection.normalized(), ((2, 8), (11, 9)));
        selection.extend_down(AREA);
        assert_eq!(
            selection.normalized(),
            ((2, 9), (11, 9)),
            "back on the origin's row is the whole line, not a cell"
        );
        // It never grows downward past the last line it began on.
        selection.extend_down(AREA);
        assert_eq!(selection.normalized(), ((2, 9), (11, 9)));
        selection.extend_last(AREA);
        assert_eq!(selection.normalized(), ((2, 9), (11, 9)));
    }

    #[test]
    fn one_view_draws_no_tab_bar() {
        let mut state = UiState::new();
        assert_eq!(state.pane_tab_count(), 1);
        state.pane_show("a.rs".into(), "one".into(), false);
        // One view is one tab, and a bar of one costs a line to say nothing.
        assert_eq!(state.pane_tabs.len(), 1);
        assert_eq!(state.pane_tab_index, 0);
    }

    #[test]
    fn a_second_view_keeps_the_first_on_the_shelf() {
        let mut state = UiState::new();
        state.pane_show("a.rs".into(), "one".into(), false);
        state.pane_show("b.rs".into(), "two".into(), false);
        assert_eq!(state.pane_tabs.len(), 2);
        assert_eq!(state.pane_tab_index, 1);
        assert_eq!(shown(&state), ("b.rs".to_string(), "two"));
        state.pane_cycle(false);
        assert_eq!(shown(&state), ("a.rs".to_string(), "one"));
    }

    #[test]
    fn cycling_wraps_in_both_directions() {
        let mut state = UiState::new();
        state.pane_show("a.rs".into(), "one".into(), false);
        state.pane_show("b.rs".into(), "two".into(), false);
        state.pane_cycle(true);
        assert_eq!(
            state.left_pane_title, "a.rs",
            "the last tab wraps to the first"
        );
        state.pane_cycle(false);
        assert_eq!(
            state.left_pane_title, "b.rs",
            "and the first back to the last"
        );
    }

    #[test]
    fn switching_away_and_back_returns_to_the_same_line() {
        let mut state = UiState::new();
        state.pane_show("diff".into(), "one\ntwo\nthree".into(), true);
        state.left_pane_scroll = 2;
        state.pane_show("b.rs".into(), "two".into(), false);
        state.pane_cycle(false);
        // The point of a tab is not just the content but where you were in
        // it: coming back to the top would lose the place you were reading.
        assert_eq!(state.left_pane_scroll, 2);
    }

    #[test]
    fn switching_saves_the_scroll_of_the_view_being_left() {
        let mut state = UiState::new();
        state.pane_show("a.rs".into(), "one\ntwo\nthree".into(), false);
        state.pane_show("b.rs".into(), "two\ntwo\nthree".into(), false);
        state.pane_cycle(false);
        state.left_pane_scroll = 2;
        state.pane_cycle(true);
        state.pane_cycle(false);
        assert_eq!(state.left_pane_scroll, 2);
    }

    #[test]
    fn reopening_the_same_title_refreshes_it_in_place() {
        let mut state = UiState::new();
        state.pane_show("diff  1 file(s)".into(), "old".into(), true);
        state.pane_show("b.rs".into(), "two".into(), false);
        state.pane_show("diff  1 file(s)".into(), "new".into(), true);
        // `/diff` twice is a refresh, not a third view: two tabs with one
        // title would differ only by when they were taken.
        assert_eq!(state.pane_tabs.len(), 2);
        assert_eq!(state.pane_tab_index, 0);
        assert_eq!(state.left_pane_content, "new");
    }

    #[test]
    fn cycling_with_one_view_does_nothing() {
        let mut state = UiState::new();
        state.pane_show("a.rs".into(), "one".into(), false);
        state.pane_cycle(true);
        state.pane_cycle(false);
        assert_eq!(shown(&state), ("a.rs".to_string(), "one"));
    }

    #[test]
    fn a_pane_with_nothing_in_it_is_not_a_tab() {
        let mut state = UiState::new();
        // The session shows the pane before anything is opened in it, so an
        // empty view must not take a slot and make the bar appear.
        state.remember_pane_view();
        assert!(state.pane_tabs.is_empty());
    }
}
