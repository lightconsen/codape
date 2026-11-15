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
    #[cfg(feature = "voice")]
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

    /// Render a short statusline label like "Step 2/4: Running tests".
    #[must_use]
    pub fn status_label(&self) -> Option<String> {
        let total = self.steps.len();
        let label = self.current_label.as_ref()?;
        if total == 0 {
            Some(label.clone())
        } else {
            Some(format!(
                "Step {}/{}: {}",
                self.current_step.saturating_add(1).min(total),
                total,
                label
            ))
        }
    }

    /// Render the full plan for an overlay.
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

/// A mouse text selection over the rendered frame.
///
/// Coordinates are absolute terminal cells `(column, row)`. The anchor is where
/// the drag started; the cursor follows the pointer. Selection is linear: each
/// row between the two points is copied whole up to the first/last column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextSelection {
    /// Where the drag began.
    pub anchor: (u16, u16),
    /// Where the pointer is now.
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

    /// Move the free end of the selection.
    pub const fn drag_to(&mut self, column: u16, row: u16) {
        self.cursor = (column, row);
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
        }
    }
}

impl Default for UiState {
    fn default() -> Self {
        Self::new()
    }
}
