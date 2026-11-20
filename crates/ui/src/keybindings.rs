//! User-configurable keybindings.
//!
//! Every shortcut the REPL answers to used to be a literal `KeyCode::Char(..)`
//! comparison inside `handle_key_event`, so changing one meant editing Rust.
//! This module owns the table instead: a fixed catalogue of actions, the
//! keystrokes bound to each, and a `keybindings.json` that moves them.
//!
//! The file is keyed by *keystroke*, the way a person thinks about it:
//!
//! ```json
//! {
//!   "bindings": [
//!     { "context": "Global", "bindings": { "ctrl+o": "app:toggleToolOutput" } },
//!     { "context": "Confirmation", "bindings": { "ctrl+y": "confirm:yes" } }
//!   ]
//! }
//! ```
//!
//! Assigning a keystroke to an action takes it away from whatever held it, so
//! one key never means two things. `null` unbinds a keystroke entirely.
//!
//! Text editing keys (typing, backspace, cursor movement) are not here: they
//! are the terminal's contract, not a preference, and rebinding them would
//! make the input line unusable rather than configurable.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// The UI state a binding applies in.
///
/// A key means different things depending on what is on screen — `esc` closes
/// an autocomplete list, cancels a run, and denies a permission prompt — so a
/// binding names the context it is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Context {
    /// Active in every context; checked before the others.
    Global,
    /// The chat input line.
    Chat,
    /// A yes/no confirmation for a destructive command.
    Confirmation,
    /// The permission dialog.
    PermissionDialog,
    /// The `/` command palette.
    Autocomplete,
    /// The `@` file list.
    ///
    /// Its own context because accepting an entry there inserts a path, while
    /// accepting one in the `/` palette replaces the line with a command — and
    /// Enter means "take this entry" here but submits the prompt everywhere
    /// else. The palette's keys are inherited, so Tab completes either.
    FileMention,
    /// Reverse history search (opened by `history:search`).
    HistorySearch,
    /// A modal list — the agent roster, rewind points, themes, plugins.
    ///
    /// Its own context because these keys mean something only while such a
    /// list is open: `enter` chooses a row rather than submitting the prompt,
    /// and ordinary characters filter rather than type.
    List,
}

impl Context {
    /// The name this context has in `keybindings.json`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Global => "Global",
            Self::Chat => "Chat",
            Self::Confirmation => "Confirmation",
            Self::PermissionDialog => "PermissionDialog",
            Self::Autocomplete => "Autocomplete",
            Self::FileMention => "FileMention",
            Self::HistorySearch => "HistorySearch",
            Self::List => "List",
        }
    }

    /// Parse a context name from a file.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        // Case-insensitive so `chat` and `Chat` both work; the documented
        // spelling is capitalised, matching the reference.
        let lowered = name.to_ascii_lowercase();
        [
            Self::Global,
            Self::Chat,
            Self::Confirmation,
            Self::PermissionDialog,
            Self::Autocomplete,
            Self::FileMention,
            Self::HistorySearch,
            Self::List,
        ]
        .into_iter()
        .find(|context| context.as_str().to_ascii_lowercase() == lowered)
    }

    /// Every context, for help text and validation messages.
    #[must_use]
    pub const fn all() -> [Self; 8] {
        [
            Self::Global,
            Self::Chat,
            Self::Confirmation,
            Self::PermissionDialog,
            Self::Autocomplete,
            Self::FileMention,
            Self::HistorySearch,
            Self::List,
        ]
    }
}

/// A shortcut the REPL performs.
///
/// The list is deliberately short enough to read: each variant is a branch of
/// `handle_key_event` that used to compare a literal key. Adding one means
/// adding that branch too, so the enum is not a wish list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Action {
    /// Fold or unfold every tool call and result row.
    ToggleToolOutput,
    /// Fold or unfold the model's reasoning rows.
    ToggleThinking,
    /// Open the model and effort chooser.
    ModelPicker,
    /// Open the reasoning-effort slider alone.
    EffortSlider,
    /// Switch between one pane and a split view.
    ToggleSplitPane,
    /// Advance the permission mode (default → accept edits → plan → …).
    CyclePermissionMode,
    /// Attach the image on the clipboard to the next prompt.
    ImagePaste,
    /// Run the configured transcription command and put its text on the line.
    VoiceInput,
    /// Browse the images attached to the next prompt.
    AttachmentBrowser,
    /// Stop the running turn, clear the input line, or close the overlay.
    Cancel,
    /// Open reverse history search.
    HistorySearch,
    /// Answer yes to a destructive-command confirmation.
    ConfirmYes,
    /// Answer no to a destructive-command confirmation.
    ConfirmNo,
    /// Allow this one tool call.
    PermissionAllow,
    /// Allow this tool for the rest of the session.
    PermissionAllowSession,
    /// Deny the tool call.
    PermissionDeny,
    /// Move the highlight up in the palette.
    AutocompletePrevious,
    /// Move the highlight down in the palette.
    AutocompleteNext,
    /// Insert the highlighted entry.
    AutocompleteAccept,
    /// Close the palette and keep the text as typed.
    AutocompleteDismiss,
    /// Insert the highlighted path into the input line.
    FileMentionAccept,
    /// Move to the previous history-search match.
    HistorySearchPrevious,
    /// Move to the next history-search match.
    HistorySearchNext,
    /// Put the selected history entry on the input line.
    HistorySearchAccept,
    /// Close history search without changing the line.
    HistorySearchCancel,
    /// Write the prompt in `$EDITOR` and take back what comes out.
    ExternalEditor,
    /// Put the line aside, or take back the one that was put aside.
    StashInput,
    /// Stop the running turn and send the line now.
    ///
    /// Enter during a turn queues the line behind it. This is the other
    /// answer to the same situation: the person changed their mind and the
    /// half-finished answer is no longer wanted.
    SendNow,
    /// Open the settings menu.
    SettingsMenu,
    /// Open the agent roster.
    AgentsView,
    /// Open the help page.
    Help,
    /// Rewind the session to an earlier point, chosen from a list.
    Rewind,
    /// Choose the colour theme from a list.
    ThemePicker,
    /// Browse installed plugins and configured marketplaces.
    PluginBrowser,
    /// Pick a previous session of this workspace to resume.
    SessionPicker,
    /// Jump to the previous file in the diff pane.
    DiffPreviousFile,
    /// Jump to the next file in the diff pane.
    DiffNextFile,
    /// Switch the left pane to the view on its left.
    PanePreviousTab,
    /// Switch the left pane to the view on its right.
    PaneNextTab,
    /// Give the transcript the whole screen, or give the prompt its room back.
    TranscriptFullscreen,
    /// Open the background-job panel.
    ///
    /// The same jobs `JobList`/`JobOutput`/`JobKill` reach by name, as a list
    /// that can be read without remembering an id: choose a row to see its
    /// output, stop it, or forget it.
    JobsPanel,
    /// Show or hide the todo panel.
    ///
    /// The panel is the model's own list of the steps it means to take, and it
    /// is on by default. This is the person's switch: the rows go back to the
    /// transcript while it is off, and the list is still there when it is on
    /// again.
    ToggleTodos,
    /// Move the highlight up in a modal list.
    ListPrevious,
    /// Move the highlight down in a modal list.
    ListNext,
    /// Accept the highlighted row of a modal list.
    ListAccept,
    /// Copy the transcript as it is on screen to the clipboard.
    ///
    /// The keyboard half of copying. A mouse drag copies whatever rectangle
    /// was selected, which needs a pointer; `/copy <n>` copies named messages,
    /// which needs the message's number. This is the third case, and the one
    /// with no argument: the answer being read right now.
    ///
    /// With a selection made by the `SelectionExtend*` actions this copies
    /// that instead, so the keyboard can take part of the transcript as well
    /// as all of it.
    CopyTranscript,
    /// Take one more transcript row into the selection, upward.
    SelectionExtendUp,
    /// Give one transcript row back, downward.
    SelectionExtendDown,
    /// Select the whole visible transcript.
    SelectionExtendAll,
    /// Go back to the newest line alone.
    SelectionLast,
}

impl Action {
    /// The name this action has in `keybindings.json`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ToggleToolOutput => "app:toggleToolOutput",
            Self::ToggleThinking => "app:toggleThinking",
            Self::ModelPicker => "chat:modelPicker",
            Self::EffortSlider => "chat:effortSlider",
            Self::ToggleSplitPane => "app:toggleSplitPane",
            Self::CyclePermissionMode => "app:cyclePermissionMode",
            Self::ImagePaste => "chat:imagePaste",
            Self::VoiceInput => "chat:voiceInput",
            Self::AttachmentBrowser => "app:attachmentBrowser",
            Self::Cancel => "chat:cancel",
            Self::HistorySearch => "history:search",
            Self::ConfirmYes => "confirm:yes",
            Self::ConfirmNo => "confirm:no",
            Self::PermissionAllow => "permission:allow",
            Self::PermissionAllowSession => "permission:allowSession",
            Self::PermissionDeny => "permission:deny",
            Self::AutocompletePrevious => "autocomplete:previous",
            Self::AutocompleteNext => "autocomplete:next",
            Self::AutocompleteAccept => "autocomplete:accept",
            Self::AutocompleteDismiss => "autocomplete:dismiss",
            Self::FileMentionAccept => "fileMention:accept",
            Self::HistorySearchPrevious => "historySearch:previous",
            Self::HistorySearchNext => "historySearch:next",
            Self::HistorySearchAccept => "historySearch:accept",
            Self::HistorySearchCancel => "historySearch:cancel",
            Self::ExternalEditor => "chat:externalEditor",
            Self::StashInput => "chat:stash",
            Self::SendNow => "chat:sendNow",
            Self::SettingsMenu => "app:settings",
            Self::AgentsView => "app:agentsView",
            Self::Help => "app:help",
            Self::Rewind => "app:rewind",
            Self::ThemePicker => "app:themePicker",
            Self::PluginBrowser => "app:pluginBrowser",
            Self::SessionPicker => "app:sessionPicker",
            Self::DiffPreviousFile => "app:diffPreviousFile",
            Self::DiffNextFile => "app:diffNextFile",
            Self::PanePreviousTab => "app:panePreviousTab",
            Self::PaneNextTab => "app:paneNextTab",
            Self::TranscriptFullscreen => "app:transcriptFullscreen",
            Self::ToggleTodos => "app:toggleTodos",
            Self::JobsPanel => "app:jobsPanel",
            Self::ListPrevious => "list:previous",
            Self::ListNext => "list:next",
            Self::ListAccept => "list:accept",
            Self::CopyTranscript => "app:copyTranscript",
            Self::SelectionExtendUp => "selection:extendUp",
            Self::SelectionExtendDown => "selection:extendDown",
            Self::SelectionExtendAll => "selection:extendAll",
            Self::SelectionLast => "selection:last",
        }
    }

    /// The context this action is resolved in.
    #[must_use]
    pub const fn context(self) -> Context {
        match self {
            Self::ToggleToolOutput
            | Self::ToggleThinking
            | Self::ToggleSplitPane
            | Self::DiffPreviousFile
            | Self::DiffNextFile
            | Self::PanePreviousTab
            | Self::PaneNextTab
            | Self::TranscriptFullscreen
            | Self::ToggleTodos
            | Self::JobsPanel
            | Self::CopyTranscript
            | Self::SelectionExtendUp
            | Self::SelectionExtendDown
            | Self::SelectionExtendAll
            | Self::SelectionLast
            | Self::CyclePermissionMode => Context::Global,
            Self::ImagePaste
            | Self::VoiceInput
            | Self::Cancel
            | Self::HistorySearch
            | Self::ExternalEditor
            | Self::StashInput
            | Self::SendNow
            | Self::SettingsMenu
            | Self::ModelPicker
            | Self::EffortSlider
            | Self::AgentsView
            | Self::Help
            | Self::Rewind
            | Self::ThemePicker
            | Self::PluginBrowser
            | Self::SessionPicker
            | Self::AttachmentBrowser => Context::Chat,
            Self::ConfirmYes | Self::ConfirmNo => Context::Confirmation,
            Self::PermissionAllow | Self::PermissionAllowSession | Self::PermissionDeny => {
                Context::PermissionDialog
            }
            Self::AutocompletePrevious
            | Self::AutocompleteNext
            | Self::AutocompleteAccept
            | Self::AutocompleteDismiss => Context::Autocomplete,
            Self::FileMentionAccept => Context::FileMention,
            Self::HistorySearchPrevious
            | Self::HistorySearchNext
            | Self::HistorySearchAccept
            | Self::HistorySearchCancel => Context::HistorySearch,
            Self::ListPrevious | Self::ListNext | Self::ListAccept => Context::List,
        }
    }

    /// Parse an action name from a file.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        ALL_ACTIONS
            .into_iter()
            .find(|action| action.as_str() == name)
    }

    /// Every action, in a stable order.
    #[must_use]
    pub const fn all() -> [Self; 50] {
        ALL_ACTIONS
    }

    /// The keystrokes this action has unless `keybindings.json` moves them.
    #[must_use]
    pub const fn defaults(self) -> &'static [&'static str] {
        match self {
            Self::ToggleToolOutput => &["ctrl+o"],
            Self::ToggleThinking => &["ctrl+e"],
            Self::ModelPicker => &["ctrl+p"],
            // The effort half of the picker, on its own key: `alt+e` is
            // "effort" and its neighbour `alt+v` is the attachment browser,
            // which is the other thing this row of alt-keys opens.
            Self::EffortSlider => &["alt+e"],
            Self::ToggleSplitPane => &["ctrl+shift+s"],
            Self::CyclePermissionMode => &["shift+tab"],
            Self::ImagePaste => &["ctrl+v"],
            // The microphone key. `alt+m` sits beside `alt+v`'s attachment
            // browser and `alt+e`'s effort slider: the same row of single
            // keystrokes, each for one thing a person reaches for.
            Self::VoiceInput => &["alt+m"],
            // Next to the key that attaches one: `ctrl+v` adds a picture,
            // `alt+v` asks which ones are attached.
            Self::AttachmentBrowser => &["alt+v"],
            Self::Cancel | Self::AutocompleteDismiss | Self::HistorySearchCancel => &["esc"],
            Self::HistorySearch => &["ctrl+r"],
            Self::ConfirmYes => &["y", "Y"],
            Self::ConfirmNo => &["n", "N", "esc"],
            Self::PermissionAllow => &["a"],
            Self::PermissionAllowSession => &["A"],
            Self::PermissionDeny => &["d", "D"],
            // The palette and the history search navigate with the same keys;
            // they are separate actions because a binding may move one
            // without moving the other. Esc closes either, and also cancels a
            // run, which is why the three share a line. A modal list navigates
            // the same way, in its own context.
            Self::AutocompletePrevious | Self::HistorySearchPrevious | Self::ListPrevious => {
                &["up"]
            }
            Self::AutocompleteNext | Self::HistorySearchNext | Self::ListNext => &["down"],
            // The `/` palette completes on Tab. Enter is deliberately not in
            // its table, so it falls through and submits the line — a list
            // that swallowed Enter made the prompt untypeable while a
            // suggestion was showing. The `@` list inserts on Tab too, through
            // the palette bindings it inherits, and on Enter. A modal list
            // accepts on Enter, which is why it is in that arm.
            Self::AutocompleteAccept => &["tab"],
            // Enter is how a shell's reverse search hands back the command,
            // and it does not submit anything here either.
            Self::HistorySearchAccept => &["tab", "enter"],
            Self::FileMentionAccept | Self::ListAccept => &["enter"],
            // The list openers share a key row: they are peers in the sense
            // that a person reaches for whichever one they mean. Defaults are
            // single keystrokes; a chord is available to whoever writes one in
            // `keybindings.json`, but a default one would delay the key by the
            // chord timeout for every person who never types the second half.
            Self::AgentsView => &["ctrl+g"],
            Self::Help => &["alt+h"],
            Self::Rewind => &["alt+r"],
            Self::ThemePicker => &["ctrl+y"],
            Self::PluginBrowser => &["alt+p"],
            // `alt+s` for sessions: the roster of the ones `/resume` can load.
            // The picker is the only list that used to have no key, reachable
            // only as `/resume` with no argument, which made the most-used
            // surface the one with the longest path to it.
            Self::SessionPicker => &["alt+s"],
            // The diff pane is read top to bottom, so the file jumps sit on
            // the arrows' vertical neighbours and do not collide with the
            // transcript scrolling.
            Self::DiffPreviousFile => &["alt+up"],
            Self::DiffNextFile => &["alt+down"],
            // The pane's own left and right, on the arrows beside the file
            // jumps' up and down. Global rather than chat-scoped: the keys act
            // on the pane, not on the line being typed, and they do nothing at
            // all unless more than one view is open.
            Self::PanePreviousTab => &["alt+left"],
            Self::PaneNextTab => &["alt+right"],
            // The toggle a person reaches for when the answer is long. `ctrl+t`
            // is the transcript size control in most terminals' muscle memory.
            Self::TranscriptFullscreen => &["ctrl+t"],
            // The todo panel's switch. `ctrl+t` is the full-screen transcript,
            // so the neighbour that is not taken is `alt+t`: one key, the same
            // row as `alt+e`'s effort slider and `alt+v`'s attachments.
            Self::ToggleTodos => &["alt+t"],
            // The job panel. `ctrl+j` is a newline and `alt+j` is free; it is
            // the neighbour of the todo switch above, which is the list of
            // what this session means to do while this one is what is running.
            Self::JobsPanel => &["alt+j"],
            // Writing the prompt somewhere with room. `ctrl+x` is the shell's
            // "hand this off" prefix; it is the key alone rather than
            // `ctrl+x ctrl+e` because a prefix has to wait for its second
            // half, and handing the terminal over is not something to delay.
            // A person who wants the chord spells it in `keybindings.json`.
            Self::ExternalEditor => &["ctrl+x"],
            // The same key a shell's job control uses for "stop and keep it",
            // which is what stashing a half-written prompt is: taken off the
            // line and held, not thrown away.
            Self::StashInput => &["ctrl+s"],
            // Stop what is running and send this instead. `ctrl+g` is taken by
            // the agent roster and `ctrl+x` by the editor handover, so this
            // goes on the one remaining control letter next to `ctrl+s`:
            // set a line aside, or replace the turn that is running.
            Self::SendNow => &["ctrl+n"],
            // `alt+c` for configuration, beside the `alt+h` help page and the
            // `alt+p`/`alt+v` surfaces it sits with.
            Self::SettingsMenu => &["alt+c"],
            // The copy key most terminals use, and the one thing the transcript
            // was missing without a pointer. Not reserved: the terminal's own
            // ctrl+shift+c copies the terminal's selection, which the TUI's
            // repainting makes unreliable, so taking it here is the fix rather
            // than a collision.
            Self::CopyTranscript => &["ctrl+shift+c"],
            // The shift+arrow family every text surface has, plus the two
            // ends. A terminal that does not report SHIFT sends the plain
            // arrow, which keeps its scroll meaning, so nothing here can be
            // mistaken for scrolling.
            Self::SelectionExtendUp => &["shift+up"],
            Self::SelectionExtendDown => &["shift+down"],
            Self::SelectionExtendAll => &["shift+home"],
            Self::SelectionLast => &["shift+end"],
        }
    }
}

const ALL_ACTIONS: [Action; 50] = [
    Action::ToggleToolOutput,
    Action::ToggleThinking,
    Action::ModelPicker,
    Action::EffortSlider,
    Action::ToggleSplitPane,
    Action::CyclePermissionMode,
    Action::ImagePaste,
    Action::AttachmentBrowser,
    Action::VoiceInput,
    Action::Cancel,
    Action::HistorySearch,
    Action::ConfirmYes,
    Action::ConfirmNo,
    Action::PermissionAllow,
    Action::PermissionAllowSession,
    Action::PermissionDeny,
    Action::AutocompletePrevious,
    Action::AutocompleteNext,
    Action::AutocompleteAccept,
    Action::AutocompleteDismiss,
    Action::FileMentionAccept,
    Action::HistorySearchPrevious,
    Action::HistorySearchNext,
    Action::HistorySearchAccept,
    Action::HistorySearchCancel,
    Action::AgentsView,
    Action::Help,
    Action::Rewind,
    Action::ThemePicker,
    Action::PluginBrowser,
    Action::SessionPicker,
    Action::DiffPreviousFile,
    Action::DiffNextFile,
    Action::PanePreviousTab,
    Action::PaneNextTab,
    Action::TranscriptFullscreen,
    Action::ToggleTodos,
    Action::JobsPanel,
    Action::ListPrevious,
    Action::ListNext,
    Action::ListAccept,
    Action::ExternalEditor,
    Action::StashInput,
    Action::SendNow,
    Action::SettingsMenu,
    Action::CopyTranscript,
    Action::SelectionExtendUp,
    Action::SelectionExtendDown,
    Action::SelectionExtendAll,
    Action::SelectionLast,
];

/// A named key, for everything that is not a character.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NamedKey {
    /// Escape.
    Escape,
    /// Enter / Return.
    Enter,
    /// Tab.
    Tab,
    /// Space (a character in the terminal, a named key to a person).
    Space,
    /// Backspace.
    Backspace,
    /// Delete (forward delete).
    Delete,
    /// Up arrow.
    Up,
    /// Down arrow.
    Down,
    /// Left arrow.
    Left,
    /// Right arrow.
    Right,
    /// Home.
    Home,
    /// End.
    End,
    /// Page up.
    PageUp,
    /// Page down.
    PageDown,
    /// Insert.
    Insert,
}

impl NamedKey {
    /// The canonical spelling, used when printing the table back.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Escape => "esc",
            Self::Enter => "enter",
            Self::Tab => "tab",
            Self::Space => "space",
            Self::Backspace => "backspace",
            Self::Delete => "delete",
            Self::Up => "up",
            Self::Down => "down",
            Self::Left => "left",
            Self::Right => "right",
            Self::Home => "home",
            Self::End => "end",
            Self::PageUp => "pageup",
            Self::PageDown => "pagedown",
            Self::Insert => "insert",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        match name {
            "escape" | "esc" => Some(Self::Escape),
            "enter" | "return" => Some(Self::Enter),
            "tab" => Some(Self::Tab),
            "space" => Some(Self::Space),
            "backspace" => Some(Self::Backspace),
            "delete" | "del" => Some(Self::Delete),
            "up" => Some(Self::Up),
            "down" => Some(Self::Down),
            "left" => Some(Self::Left),
            "right" => Some(Self::Right),
            "home" => Some(Self::Home),
            "end" => Some(Self::End),
            "pageup" | "page_up" | "pgup" => Some(Self::PageUp),
            "pagedown" | "page_down" | "pgdn" => Some(Self::PageDown),
            "insert" | "ins" => Some(Self::Insert),
            _ => None,
        }
    }
}

/// What the key itself is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StrokeCode {
    /// A character key.
    Char(char),
    /// A named key.
    Named(NamedKey),
}

/// One keystroke: a key plus the modifiers held with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KeyStroke {
    /// The key.
    pub code: StrokeCode,
    /// Control held.
    pub ctrl: bool,
    /// Alt (Option on macOS) held.
    pub alt: bool,
    /// Shift held.
    ///
    /// A capital letter counts as shifted even when the terminal does not
    /// report the modifier, which most do not.
    pub shift: bool,
}

impl KeyStroke {
    /// Parse a keystroke such as `ctrl+shift+s`, `esc`, or `space`.
    ///
    /// # Errors
    ///
    /// Returns a description of what was wrong, suitable for a validation
    /// note.
    pub fn parse(text: &str) -> Result<Self, String> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Err("empty keystroke".into());
        }
        if trimmed.split_whitespace().count() > 1 {
            return Err(format!(
                "\"{trimmed}\" is a chord; parse it as a whole with Chord::parse"
            ));
        }
        // The last `+` separates the modifiers from the key, and a key may
        // itself be `+`, so everything after the final `+` is the key when
        // there is one — `ctrl++` is ctrl plus the key named `+`.
        let (modifier_text, key) = match trimmed.rsplit_once('+') {
            Some((modifiers, "")) => match modifiers.strip_suffix('+') {
                // `ctrl++`: the plus is the key, `ctrl` the modifier.
                Some(modifiers) => (modifiers, "+"),
                // A bare `+` is the plus key with no modifiers.
                None if modifiers.is_empty() => ("", "+"),
                // `ctrl+` names no key; the empty key reports that.
                None => (modifiers, ""),
            },
            Some((modifiers, key)) => (modifiers, key),
            // `+` on its own: no modifier text, the key is the plus itself.
            None => ("", trimmed),
        };
        let modifiers = Self::parse_modifiers(modifier_text, trimmed)?;
        let (code, shift) = Self::parse_key(key, trimmed)?;
        Ok(Self {
            code,
            shift: modifiers.shift || shift,
            ..modifiers
        })
    }

    /// Parse the modifier list of a keystroke.
    fn parse_modifiers(text: &str, full: &str) -> Result<Self, String> {
        let mut ctrl = false;
        let mut alt = false;
        let mut shift = false;
        if !text.is_empty() {
            for modifier in text.split('+') {
                if is_ctrl(modifier) {
                    ctrl = true;
                } else if is_alt(modifier) {
                    alt = true;
                } else if modifier.eq_ignore_ascii_case("shift") {
                    shift = true;
                } else if is_cmd(modifier) {
                    // Most terminals never deliver it; rejected with the
                    // reason so the config author knows why it would not fire.
                    return Err(format!(
                        "\"{modifier}\" (the command key) is not delivered by terminals"
                    ));
                } else {
                    return Err(format!("\"{modifier}\" is not a modifier (in \"{full}\")"));
                }
            }
        }
        Ok(Self {
            code: StrokeCode::Char('\0'),
            ctrl,
            alt,
            shift,
        })
    }

    /// Parse the key half, and whether the spelling implies shift.
    fn parse_key(key: &str, full: &str) -> Result<(StrokeCode, bool), String> {
        if key.is_empty() {
            return Err(format!("\"{full}\" names no key"));
        }
        if let Some(named) = NamedKey::parse(&key.to_ascii_lowercase()) {
            return Ok((StrokeCode::Named(named), false));
        }
        let mut chars = key.chars();
        match (chars.next(), chars.next()) {
            // A capital letter means the shifted key, whether or not the
            // person wrote `shift+`; most terminals do not report SHIFT.
            (Some(c), None) => Ok((StrokeCode::Char(c), c.is_uppercase())),
            _ => Err(format!("\"{key}\" is not a key name (in \"{full}\")")),
        }
    }

    /// The canonical spelling of this keystroke.
    #[must_use]
    pub fn display(&self) -> String {
        let mut out = String::new();
        if self.ctrl {
            out.push_str("ctrl+");
        }
        if self.alt {
            out.push_str("alt+");
        }
        if self.shift {
            out.push_str("shift+");
        }
        match self.code {
            StrokeCode::Char(c) => out.push(c),
            StrokeCode::Named(named) => out.push_str(named.as_str()),
        }
        out
    }

    /// The keystroke a key event is, as a value.
    ///
    /// The inverse of [`Self::matches`], with the same case folding: a
    /// terminal that reports `Ctrl+Shift+S` as `'S'` with only CONTROL
    /// produces the keystroke a config writes as `ctrl+shift+s`.
    #[must_use]
    pub const fn from_event(event: &KeyEvent) -> Self {
        let shift_modifier = event.modifiers.contains(KeyModifiers::SHIFT);
        let (code, shift) = match event.code {
            KeyCode::Char(c) => (
                StrokeCode::Char(c.to_ascii_lowercase()),
                shift_modifier || c.is_uppercase(),
            ),
            KeyCode::BackTab => (StrokeCode::Named(NamedKey::Tab), true),
            KeyCode::Esc => (StrokeCode::Named(NamedKey::Escape), shift_modifier),
            KeyCode::Enter => (StrokeCode::Named(NamedKey::Enter), shift_modifier),
            KeyCode::Tab => (StrokeCode::Named(NamedKey::Tab), shift_modifier),
            KeyCode::Backspace => (StrokeCode::Named(NamedKey::Backspace), shift_modifier),
            KeyCode::Delete => (StrokeCode::Named(NamedKey::Delete), shift_modifier),
            KeyCode::Up => (StrokeCode::Named(NamedKey::Up), shift_modifier),
            KeyCode::Down => (StrokeCode::Named(NamedKey::Down), shift_modifier),
            KeyCode::Left => (StrokeCode::Named(NamedKey::Left), shift_modifier),
            KeyCode::Right => (StrokeCode::Named(NamedKey::Right), shift_modifier),
            KeyCode::Home => (StrokeCode::Named(NamedKey::Home), shift_modifier),
            KeyCode::End => (StrokeCode::Named(NamedKey::End), shift_modifier),
            KeyCode::PageUp => (StrokeCode::Named(NamedKey::PageUp), shift_modifier),
            KeyCode::PageDown => (StrokeCode::Named(NamedKey::PageDown), shift_modifier),
            KeyCode::Insert => (StrokeCode::Named(NamedKey::Insert), shift_modifier),
            // A media or function key has no name in the table. It is
            // represented as a character no keyboard can produce, so it
            // compares equal to nothing rather than to a real key.
            _ => (StrokeCode::Char('\0'), shift_modifier),
        };
        Self {
            code,
            ctrl: event.modifiers.contains(KeyModifiers::CONTROL),
            alt: event.modifiers.contains(KeyModifiers::ALT),
            shift,
        }
    }

    /// Whether this keystroke is one the terminal has already claimed.
    ///
    /// `ctrl+c` and `ctrl+d` are the interrupt and exit keys before any
    /// application sees them (they are the shell's SIGINT and EOF), and
    /// `ctrl+m`, `ctrl+[`, `ctrl+i` and `ctrl+h` are indistinguishable from
    /// enter, escape, tab and backspace in most terminals. A binding on one of
    /// them would either never fire or fire twice.
    #[must_use]
    pub const fn is_reserved(&self) -> bool {
        if self.alt || self.shift {
            return false;
        }
        match self.code {
            StrokeCode::Char(c) => {
                self.ctrl && matches!(c.to_ascii_lowercase(), 'c' | 'd' | 'm' | '[' | 'i' | 'h')
            }
            StrokeCode::Named(_) => false,
        }
    }

    /// Whether a key event is this keystroke.
    ///
    /// Letters compare case-insensitively: a terminal may report
    /// `Ctrl+Shift+S` as `'S'` or `'s'` with `SHIFT`, depending on which
    /// keyboard-enhancement flags are on, and both are the same keystroke to
    /// the person pressing it.
    #[must_use]
    pub fn matches(&self, event: &KeyEvent) -> bool {
        if self.ctrl != event.modifiers.contains(KeyModifiers::CONTROL)
            || self.alt != event.modifiers.contains(KeyModifiers::ALT)
        {
            return false;
        }
        match (self.code, event.code) {
            (StrokeCode::Named(NamedKey::Tab), KeyCode::Tab)
            | (StrokeCode::Named(NamedKey::Space), KeyCode::Char(' ')) => {
                self.shift == event.modifiers.contains(KeyModifiers::SHIFT)
            }
            // `BackTab` *is* shift-tab; some terminals send it with the
            // modifier already stripped.
            (StrokeCode::Named(NamedKey::Tab), KeyCode::BackTab) => self.shift,
            (StrokeCode::Named(named), code) => {
                named_matches(named, code)
                    && self.shift == event.modifiers.contains(KeyModifiers::SHIFT)
            }
            (StrokeCode::Char(expected), KeyCode::Char(actual)) => {
                let shifted =
                    event.modifiers.contains(KeyModifiers::SHIFT) || actual.is_uppercase();
                expected.eq_ignore_ascii_case(&actual) && self.shift == shifted
            }
            _ => false,
        }
    }
}

fn named_matches(named: NamedKey, code: KeyCode) -> bool {
    match named {
        NamedKey::Escape => code == KeyCode::Esc,
        NamedKey::Enter => code == KeyCode::Enter,
        NamedKey::Tab => code == KeyCode::Tab,
        NamedKey::Space => code == KeyCode::Char(' '),
        NamedKey::Backspace => code == KeyCode::Backspace,
        NamedKey::Delete => code == KeyCode::Delete,
        NamedKey::Up => code == KeyCode::Up,
        NamedKey::Down => code == KeyCode::Down,
        NamedKey::Left => code == KeyCode::Left,
        NamedKey::Right => code == KeyCode::Right,
        NamedKey::Home => code == KeyCode::Home,
        NamedKey::End => code == KeyCode::End,
        NamedKey::PageUp => code == KeyCode::PageUp,
        NamedKey::PageDown => code == KeyCode::PageDown,
        NamedKey::Insert => code == KeyCode::Insert,
    }
}

/// How long a chord may pause between its keystrokes.
///
/// Long enough that a deliberate two-key sequence fits inside it, short enough
/// that a key which is also the start of a chord does not feel stuck.
pub const CHORD_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1500);

/// One binding as written: a single keystroke, or several pressed in order.
///
/// Chords exist because modifiers run out. `ctrl+x` alone hands the prompt to
/// an editor; `ctrl+x ctrl+e` is the shape an editor-driven agent's users
/// expect, and it needs a sequence rather than a wider modifier set no
/// terminal reliably reports.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Chord {
    strokes: Vec<KeyStroke>,
}

impl Chord {
    /// Parse a binding such as `ctrl+o`, `esc`, or `ctrl+x ctrl+e`.
    ///
    /// # Errors
    ///
    /// Returns a description of what was wrong, suitable for a validation
    /// note.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut strokes = Vec::new();
        for part in text.split_whitespace() {
            strokes.push(KeyStroke::parse(part)?);
        }
        if strokes.is_empty() {
            return Err("empty keystroke".into());
        }
        Ok(Self { strokes })
    }

    /// A chord of the one keystroke.
    #[must_use]
    pub fn single(stroke: KeyStroke) -> Self {
        Self {
            strokes: vec![stroke],
        }
    }

    /// The keystrokes, in the order they are pressed.
    #[must_use]
    pub fn strokes(&self) -> &[KeyStroke] {
        &self.strokes
    }

    /// How many keystrokes the chord has.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.strokes.len()
    }

    /// Whether the chord has no keystrokes. Never true for a parsed chord;
    /// present because a `len()` without it is a clippy lint.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.strokes.is_empty()
    }

    /// The canonical spelling: the keystrokes joined by a space.
    #[must_use]
    pub fn display(&self) -> String {
        self.strokes
            .iter()
            .map(KeyStroke::display)
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Whether two keystrokes mean the same key, as a terminal reports them.
///
/// The same folding [`KeyStroke::matches`] applies: `A` and `a` with SHIFT are
/// one key, and so are `space` and the space character. A chord compares
/// keystroke values rather than events, so this is the equality it uses.
fn strokes_agree(a: &KeyStroke, b: &KeyStroke) -> bool {
    if a.ctrl != b.ctrl || a.alt != b.alt || a.shift != b.shift {
        return false;
    }
    match (a.code, b.code) {
        (StrokeCode::Char(left), StrokeCode::Char(right)) => left.eq_ignore_ascii_case(&right),
        (left, right) => left == right,
    }
}

/// What a keystroke means, given the strokes of a chord already pressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SequenceResolution {
    /// The sequence is a binding.
    Action(Action),
    /// The sequence so far starts a binding that needs more keystrokes.
    Pending,
    /// Nothing is bound to the sequence.
    None,
}

/// What one key press did to the chord being typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChordStep {
    /// The key is not part of a chord; handle it as itself.
    Pass,
    /// The key extended a chord; more keystrokes are needed.
    Pending,
    /// The key completed a chord.
    Complete(Action),
}

/// The keystrokes of a chord pressed so far.
///
/// The REPL feeds every key through this before doing anything else, so a
/// chord's first strokes are held instead of acted on. A single-keystroke
/// binding is unaffected: nothing is pending and the key starts no chord, so
/// the press passes straight through.
#[derive(Debug, Clone, Default)]
pub struct ChordTracker {
    pending: Vec<KeyStroke>,
    /// The event that most recently extended the sequence. Kept so a timeout
    /// can replay it as what it means on its own.
    last: Option<KeyEvent>,
    /// The next press replays `last` and must not start a chord again.
    replay: bool,
}

impl ChordTracker {
    /// The keystrokes held so far, for display.
    #[must_use]
    pub fn pending(&self) -> &[KeyStroke] {
        &self.pending
    }

    /// Whether a sequence is being held.
    #[must_use]
    pub const fn is_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Feed one key press in.
    pub fn press(
        &mut self,
        bindings: &KeyBindings,
        context: Context,
        event: &KeyEvent,
    ) -> ChordStep {
        if self.replay {
            self.replay = false;
            return ChordStep::Pass;
        }
        let stroke = KeyStroke::from_event(event);
        // A key that both stands alone and starts a longer chord waits: the
        // longer chord could never be typed otherwise. The timeout replays it
        // as the binding it has by itself.
        let extends = bindings.extends_to_longer(context, &self.pending, &stroke);
        match bindings.resolve_sequence(context, &self.pending, &stroke) {
            SequenceResolution::Action(action) if !extends => {
                self.pending.clear();
                self.last = None;
                ChordStep::Complete(action)
            }
            SequenceResolution::Action(_) | SequenceResolution::Pending => {
                self.pending.push(stroke);
                self.last = Some(*event);
                ChordStep::Pending
            }
            // An unrelated key abandons the sequence and is handled as
            // itself; the held strokes are lost, the way a Vim prefix is lost
            // when the next key cannot follow it.
            SequenceResolution::None => {
                self.pending.clear();
                self.last = None;
                ChordStep::Pass
            }
        }
    }

    /// Drop the held sequence, returning the key to replay.
    ///
    /// Only a one-keystroke prefix is replayed: `ctrl+x` both opens the editor
    /// and starts `ctrl+x ctrl+e`, so when the second keystroke does not
    /// arrive the first has to mean what it means alone. Replaying the second
    /// stroke of a longer partial chord would be worse than dropping it — it
    /// would fire a binding the person never asked for.
    ///
    /// The caller handles the returned event as an ordinary key press, not
    /// through [`Self::press`]: the sequence is already over.
    pub fn expire(&mut self) -> Option<KeyEvent> {
        let single = self.pending.len() == 1;
        let last = self.last.take();
        self.pending.clear();
        if single {
            // The next press is the replay and must not be held again; it
            // would otherwise start the same chord a second time.
            self.replay = last.is_some();
            last
        } else {
            None
        }
    }

    /// Forget the sequence without replaying anything.
    pub fn clear(&mut self) {
        self.pending.clear();
        self.last = None;
        self.replay = false;
    }
}

const fn is_ctrl(modifier: &str) -> bool {
    modifier.eq_ignore_ascii_case("ctrl") || modifier.eq_ignore_ascii_case("control")
}

fn is_alt(modifier: &str) -> bool {
    ["alt", "opt", "option", "meta"]
        .iter()
        .any(|name| modifier.eq_ignore_ascii_case(name))
}

fn is_cmd(modifier: &str) -> bool {
    ["cmd", "command", "super", "win"]
        .iter()
        .any(|name| modifier.eq_ignore_ascii_case(name))
}

/// Whether a chord's first `pending.len() + 1` strokes are `pending` then
/// `stroke`.
///
/// `None` when they are not: a chord the sequence has already diverged from.
/// `Some(n)` gives how many strokes matched, so the caller can tell a complete
/// binding from a prefix of a longer one.
fn sequence_prefix_len(
    chord: &[KeyStroke],
    pending: &[KeyStroke],
    stroke: &KeyStroke,
) -> Option<usize> {
    let wanted = pending.len() + 1;
    if chord.len() < wanted {
        return None;
    }
    let combined = pending.iter().chain(std::iter::once(stroke));
    if chord
        .iter()
        .zip(combined)
        .all(|(bound, pressed)| strokes_agree(bound, pressed))
    {
        Some(wanted)
    } else {
        None
    }
}

/// The effective keymap: every action and the keystrokes that reach it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyBindings {
    bindings: std::collections::BTreeMap<Action, Vec<Chord>>,
}

impl Default for KeyBindings {
    fn default() -> Self {
        let mut bindings = std::collections::BTreeMap::new();
        for action in ALL_ACTIONS {
            let chords: Vec<Chord> = action
                .defaults()
                .iter()
                .filter_map(|text| Chord::parse(text).ok())
                .collect();
            bindings.insert(action, chords);
        }
        Self { bindings }
    }
}

/// What loading a `keybindings.json` produced.
#[derive(Debug, Clone)]
pub struct Loaded {
    /// The effective keymap.
    pub bindings: KeyBindings,
    /// Problems with the files, one line each.
    ///
    /// A malformed entry does not discard the rest of the file: a typo in one
    /// binding should cost that binding, not the whole configuration.
    pub notes: Vec<String>,
    /// Files that were read, in application order.
    pub sources: Vec<std::path::PathBuf>,
}

impl KeyBindings {
    /// The chords bound to an action.
    #[must_use]
    pub fn chords(&self, action: Action) -> &[Chord] {
        self.bindings
            .get(&action)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// The single-keystroke chords bound to an action.
    ///
    /// The callers that only print a key (the transcript footer, the help
    /// page) want the ones a person can press in one go; a chord needs both
    /// halves named to be usable.
    #[must_use]
    pub fn strokes(&self, action: Action) -> Vec<KeyStroke> {
        self.chords(action)
            .iter()
            .filter_map(|chord| match chord.strokes() {
                [one] => Some(*one),
                _ => None,
            })
            .collect()
    }

    /// The action a key performs in a context, if any.
    ///
    /// `Global` bindings are checked first, then the context's own, so a
    /// global shortcut cannot be shadowed by a palette key.
    ///
    /// Only single-keystroke bindings are considered: a chord needs the
    /// tracker. A key that is the *prefix* of a chord still resolves to
    /// whatever it means alone, which is what a caller without a tracker
    /// gets.
    #[must_use]
    pub fn resolve(&self, context: Context, event: &KeyEvent) -> Option<Action> {
        let stroke = KeyStroke::from_event(event);
        match self.resolve_sequence(context, &[], &stroke) {
            SequenceResolution::Action(action) => Some(action),
            // A prefix standing alone: the key is a binding's first half, not
            // a binding. The caller that asked without a tracker cannot wait
            // for the second, so it is told nothing.
            SequenceResolution::Pending | SequenceResolution::None => None,
        }
    }

    /// What pressing `stroke` after `pending` means in a context.
    ///
    /// The three answers the tracker needs from the table: a full binding, the
    /// start of one, or nothing at all. Checking the whole prefix is what
    /// makes `ctrl+x ctrl+e` mean the chord while `ctrl+x` alone still means
    /// the editor.
    ///
    /// A complete binding wins over a longer prefix here — `ctrl+x` resolves
    /// to the editor even when `ctrl+x ctrl+e` exists — because a caller that
    /// cannot wait for the second keystroke is asking about this one. The
    /// tracker asks [`Self::extends_to_longer`] separately when it needs the
    /// other answer.
    #[must_use]
    pub fn resolve_sequence(
        &self,
        context: Context,
        pending: &[KeyStroke],
        stroke: &KeyStroke,
    ) -> SequenceResolution {
        let wanted: Vec<Context> = match context {
            Context::Global => vec![Context::Global],
            // The file list walks and dismisses with the palette's keys and
            // adds Enter; it is a list with one extra decision, not a second
            // vocabulary.
            Context::FileMention => vec![Context::Global, Context::Autocomplete, context],
            _ => vec![Context::Global, context],
        };
        let mut complete = None;
        let mut prefix = false;
        for check in wanted {
            for (action, chords) in &self.bindings {
                if action.context() != check {
                    continue;
                }
                for chord in chords {
                    let strokes = chord.strokes();
                    let Some(prefix_len) = sequence_prefix_len(strokes, pending, stroke) else {
                        continue;
                    };
                    // The first context that has an answer wins, the same
                    // order `resolve` used: a global binding before the
                    // context's own.
                    if prefix_len == strokes.len() && complete.is_none() {
                        complete = Some(*action);
                    } else if prefix_len < strokes.len() {
                        prefix = true;
                    }
                }
            }
            if let Some(action) = complete {
                return SequenceResolution::Action(action);
            }
            if prefix {
                return SequenceResolution::Pending;
            }
        }
        SequenceResolution::None
    }

    /// Whether some binding in a context needs more keystrokes after
    /// `pending` then `stroke`.
    ///
    /// The tracker asks this before acting on a completed binding: a key that
    /// both stands alone and starts a longer chord has to wait, or the longer
    /// chord could never be typed.
    #[must_use]
    pub fn extends_to_longer(
        &self,
        context: Context,
        pending: &[KeyStroke],
        stroke: &KeyStroke,
    ) -> bool {
        let wanted: Vec<Context> = match context {
            Context::Global => vec![Context::Global],
            Context::FileMention => vec![Context::Global, Context::Autocomplete, context],
            _ => vec![Context::Global, context],
        };
        wanted.iter().any(|check| {
            self.bindings.iter().any(|(action, chords)| {
                action.context() == *check
                    && chords.iter().any(|chord| {
                        sequence_prefix_len(chord.strokes(), pending, stroke)
                            .is_some_and(|matched| matched < chord.len())
                    })
            })
        })
    }

    /// Bind a chord to an action, taking it from whoever held it.
    ///
    /// Returns the action that lost the key, if any, so the caller can say so:
    /// a silent move is how a config ends up doing nothing.
    pub fn assign(&mut self, action: Action, chord: Chord) -> Option<Action> {
        let context = action.context();
        let mut displaced = None;
        for (existing, chords) in &mut self.bindings {
            if existing.context() != context || *existing == action {
                continue;
            }
            if let Some(index) = chords.iter().position(|bound| *bound == chord) {
                chords.remove(index);
                displaced = Some(*existing);
            }
        }
        let chords = self.bindings.entry(action).or_default();
        if !chords.contains(&chord) {
            chords.push(chord);
        }
        displaced
    }

    /// Remove a chord from every action in a context.
    ///
    /// Returns the actions that lost it, so an unbind of a key nothing bound
    /// can be reported rather than silently accepted.
    pub fn unbind(&mut self, context: Context, chord: Chord) -> Vec<Action> {
        let mut removed = Vec::new();
        for (action, chords) in &mut self.bindings {
            if action.context() != context {
                continue;
            }
            if let Some(index) = chords.iter().position(|bound| *bound == chord) {
                chords.remove(index);
                removed.push(*action);
            }
        }
        removed
    }

    /// Apply one `{context: {keystroke: action}}` document on top of this map.
    fn apply_document(&mut self, document: &serde_json::Value, notes: &mut Vec<String>) {
        let Some(entries) = document.get("bindings").and_then(|value| value.as_array()) else {
            notes.push("\"bindings\" must be an array of {context, bindings} objects".into());
            return;
        };
        for (index, entry) in entries.iter().enumerate() {
            let Some(context_name) = entry.get("context").and_then(|value| value.as_str()) else {
                notes.push(format!("bindings[{index}] has no \"context\""));
                continue;
            };
            let Some(context) = Context::parse(context_name) else {
                let valid: Vec<&str> = Context::all().iter().map(|c| c.as_str()).collect();
                notes.push(format!(
                    "bindings[{index}]: unknown context \"{context_name}\" (valid: {})",
                    valid.join(", ")
                ));
                continue;
            };
            let Some(map) = entry.get("bindings").and_then(|value| value.as_object()) else {
                notes.push(format!(
                    "bindings[{index}] ({context_name}) has no \"bindings\" object"
                ));
                continue;
            };
            for (key_text, value) in map {
                self.apply_entry(context, key_text, value, notes);
            }
        }
    }

    fn apply_entry(
        &mut self,
        context: Context,
        key_text: &str,
        value: &serde_json::Value,
        notes: &mut Vec<String>,
    ) {
        let chord = match Chord::parse(key_text) {
            Ok(chord) => chord,
            Err(problem) => {
                notes.push(format!("{}: {problem}", context.as_str()));
                return;
            }
        };
        // Every stroke of a chord has to be pressable, not only its first:
        // `ctrl+x ctrl+c` never completes, so the binding would be dead.
        if let Some(reserved) = chord.strokes().iter().find(|stroke| stroke.is_reserved()) {
            notes.push(format!(
                "{}: {} is reserved by the terminal and cannot be bound",
                context.as_str(),
                reserved.display()
            ));
            return;
        }
        match value {
            serde_json::Value::Null => {
                let removed = self.unbind(context, chord.clone());
                if removed.is_empty() {
                    notes.push(format!(
                        "{}: {} was not bound to anything",
                        context.as_str(),
                        chord.display()
                    ));
                }
            }
            serde_json::Value::String(action_name) => {
                let Some(action) = Action::parse(action_name) else {
                    let valid: Vec<&str> = ALL_ACTIONS.iter().map(|a| a.as_str()).collect();
                    notes.push(format!(
                        "{}: unknown action \"{action_name}\" (valid: {})",
                        context.as_str(),
                        valid.join(", ")
                    ));
                    return;
                };
                if action.context() != context {
                    notes.push(format!(
                        "{}: {} belongs to the {} context; the binding was applied there",
                        context.as_str(),
                        action.as_str(),
                        action.context().as_str()
                    ));
                }
                let shadowed = self.chord_shadow(action, &chord);
                if let Some(displaced) = self.assign(action, chord.clone()) {
                    notes.push(format!(
                        "{}: {} moved from {} to {}",
                        context.as_str(),
                        chord.display(),
                        displaced.as_str(),
                        action.as_str()
                    ));
                }
                // A prefix the new binding needs is no longer reachable on its
                // own once the longer chord exists — `ctrl+x` cannot open the
                // editor *and* start `ctrl+x ctrl+e` at the same time. That is
                // not an error; it is the fact the person needs in order to
                // decide, so it is a note.
                if let Some(shadowed) = shadowed {
                    notes.push(format!(
                        "{}: {} now waits for the chord bound to {}",
                        context.as_str(),
                        chord.display(),
                        shadowed.as_str()
                    ));
                }
            }
            other => notes.push(format!(
                "{}: {} must be an action name or null, not {other}",
                context.as_str(),
                chord.display()
            )),
        }
    }

    /// The action whose binding this chord turns into a prefix, if any.
    fn chord_shadow(&self, action: Action, chord: &Chord) -> Option<Action> {
        if chord.len() < 2 {
            return None;
        }
        let first = chord.strokes()[0];
        let single = Chord::single(first);
        ALL_ACTIONS.into_iter().find(|existing| {
            *existing != action
                && existing.context() == action.context()
                && self.bindings.get(existing).is_some_and(|chords| {
                    chords
                        .iter()
                        .any(|bound| bound.len() == 1 && bound == &single)
                })
        })
    }

    /// Load and merge keybinding files over the defaults.
    ///
    /// Later files win, so the caller passes the user's file before the
    /// project's. A file that cannot be read or parsed is a note, not a
    /// failure: the defaults still apply.
    #[must_use]
    pub fn load(paths: &[&std::path::Path]) -> Loaded {
        let mut bindings = Self::default();
        let mut notes = Vec::new();
        let mut sources = Vec::new();
        for path in paths {
            if !path.exists() {
                continue;
            }
            let content = match std::fs::read_to_string(path) {
                Ok(content) => content,
                Err(error) => {
                    notes.push(format!("could not read {}: {error}", path.display()));
                    continue;
                }
            };
            let document: serde_json::Value = match serde_json::from_str(&content) {
                Ok(document) => document,
                Err(error) => {
                    notes.push(format!("{} is not valid JSON: {error}", path.display()));
                    continue;
                }
            };
            bindings.apply_document(&document, &mut notes);
            sources.push(path.to_path_buf());
        }
        Loaded {
            bindings,
            notes,
            sources,
        }
    }

    /// Every action with its keystrokes, for `/keybindings`.
    #[must_use]
    pub fn table(&self) -> Vec<(Action, Vec<String>)> {
        ALL_ACTIONS
            .iter()
            .map(|action| {
                (
                    *action,
                    self.chords(*action).iter().map(Chord::display).collect(),
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn stroke(text: &str) -> KeyStroke {
        KeyStroke::parse(text).expect("keystroke parses")
    }

    #[test]
    fn a_parsed_keystroke_matches_the_event_it_names() {
        assert!(stroke("ctrl+o").matches(&key(KeyCode::Char('o'), KeyModifiers::CONTROL)));
        assert!(stroke("esc").matches(&key(KeyCode::Esc, KeyModifiers::NONE)));
        assert!(stroke("shift+tab").matches(&key(KeyCode::BackTab, KeyModifiers::NONE)));
        assert!(stroke("shift+tab").matches(&key(KeyCode::Tab, KeyModifiers::SHIFT)));
        assert!(stroke("enter").matches(&key(KeyCode::Enter, KeyModifiers::NONE)));
        // Modifiers are part of the keystroke: ctrl+o is not plain o.
        assert!(!stroke("ctrl+o").matches(&key(KeyCode::Char('o'), KeyModifiers::NONE)));
        assert!(!stroke("o").matches(&key(KeyCode::Char('o'), KeyModifiers::CONTROL)));
    }

    /// A capital letter in the file means the shifted key even when the
    /// terminal does not report SHIFT — which most do not.
    #[test]
    fn a_capital_letter_is_the_shifted_key() {
        assert!(stroke("A").matches(&key(KeyCode::Char('A'), KeyModifiers::NONE)));
        assert!(stroke("A").matches(&key(KeyCode::Char('A'), KeyModifiers::SHIFT)));
        assert!(!stroke("a").matches(&key(KeyCode::Char('A'), KeyModifiers::NONE)));
        assert!(stroke("a").matches(&key(KeyCode::Char('a'), KeyModifiers::NONE)));
        // Ctrl chords are case-insensitive on the letter.
        assert!(stroke("ctrl+shift+s").matches(&key(KeyCode::Char('S'), KeyModifiers::CONTROL)));
        assert!(!stroke("ctrl+s").matches(&key(KeyCode::Char('S'), KeyModifiers::CONTROL)));
    }

    #[test]
    fn aliases_and_spellings_resolve_to_one_keystroke() {
        for (a, b) in [
            ("escape", "esc"),
            ("return", "enter"),
            ("control+space", "ctrl+space"),
            ("opt+x", "alt+x"),
            ("pgup", "pageup"),
        ] {
            assert_eq!(stroke(a), stroke(b), "{a} and {b} differ");
        }
        assert_eq!(stroke("ctrl+shift+s").display(), "ctrl+shift+s");
        assert_eq!(stroke("B").display(), "shift+B");
    }

    #[test]
    fn a_bad_keystroke_says_what_is_wrong() {
        assert!(KeyStroke::parse("").is_err());
        assert!(KeyStroke::parse("ctrl+").is_err());
        assert!(
            KeyStroke::parse("ctl+o")
                .unwrap_err()
                .contains("not a modifier")
        );
        assert!(
            KeyStroke::parse("ctrl+k ctrl+s")
                .unwrap_err()
                .contains("Chord::parse")
        );
        assert!(
            KeyStroke::parse("cmd+p")
                .unwrap_err()
                .contains("command key")
        );
    }

    #[test]
    fn reserved_keys_are_refused() {
        for reserved in ["ctrl+c", "ctrl+d", "ctrl+m", "ctrl+[", "ctrl+i", "ctrl+h"] {
            assert!(stroke(reserved).is_reserved(), "{reserved}");
        }
        for allowed in ["ctrl+o", "ctrl+v", "ctrl+shift+c", "esc", "ctrl+x"] {
            assert!(!stroke(allowed).is_reserved(), "{allowed}");
        }
    }

    fn chord(text: &str) -> Chord {
        Chord::parse(text).expect("chord parses")
    }

    #[test]
    fn a_chord_parses_into_its_keystrokes() {
        let parsed = chord("ctrl+x ctrl+e");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed.strokes(), [stroke("ctrl+x"), stroke("ctrl+e")]);
        // The canonical spelling is what `/keybindings` prints back, and it has
        // to round-trip or an edited file would read differently from the one
        // the page showed.
        assert_eq!(parsed.display(), "ctrl+x ctrl+e");
        assert_eq!(chord(&parsed.display()), parsed);
        // A single keystroke is a one-stroke chord, so one representation
        // covers both and the table has no second case.
        assert_eq!(chord("esc").len(), 1);
        assert_eq!(chord("esc").display(), "esc");
    }

    #[test]
    fn a_chord_with_a_bad_stroke_names_the_stroke() {
        // The message has to say which half was wrong: `ctrl+x ctl+e` is one
        // typo in a two-key binding, and "invalid chord" would not say where.
        let problem = Chord::parse("ctrl+x ctl+e").unwrap_err();
        assert!(problem.contains("ctl"), "{problem}");
        assert!(problem.contains("not a modifier"), "{problem}");
        assert!(Chord::parse("   ").is_err());
    }

    /// `ctrl+x` still means the editor while `ctrl+x ctrl+e` means the
    /// chord — the case the whole tracker exists for.
    #[test]
    fn a_prefix_key_waits_for_the_rest_of_its_chord() {
        let mut bindings = KeyBindings::default();
        bindings.assign(Action::ExternalEditor, chord("ctrl+x"));
        bindings.assign(Action::Help, chord("ctrl+x ctrl+e"));
        let mut tracker = ChordTracker::default();
        let x = key(KeyCode::Char('x'), KeyModifiers::CONTROL);

        assert_eq!(
            tracker.press(&bindings, Context::Chat, &x),
            ChordStep::Pending,
            "ctrl+x fired instead of waiting for the chord it starts"
        );
        assert_eq!(
            tracker.press(
                &bindings,
                Context::Chat,
                &key(KeyCode::Char('e'), KeyModifiers::CONTROL)
            ),
            ChordStep::Complete(Action::Help),
            "the second half did not complete the chord"
        );
        assert!(!tracker.is_pending(), "the sequence was left held");
    }

    #[test]
    fn the_timeout_replays_the_prefix_as_the_key_it_is() {
        let mut bindings = KeyBindings::default();
        bindings.assign(Action::ExternalEditor, chord("ctrl+x"));
        bindings.assign(Action::Help, chord("ctrl+x ctrl+e"));
        let mut tracker = ChordTracker::default();
        let x = key(KeyCode::Char('x'), KeyModifiers::CONTROL);

        assert_eq!(
            tracker.press(&bindings, Context::Chat, &x),
            ChordStep::Pending
        );
        assert_eq!(
            tracker.expire(),
            Some(x),
            "the held prefix was dropped instead of replayed, so ctrl+x stops \
             opening the editor once a chord is bound to it"
        );
        // The replay itself must not be held again: it would restart the same
        // chord and the key would never fire.
        assert_eq!(tracker.press(&bindings, Context::Chat, &x), ChordStep::Pass);
        assert_eq!(
            bindings.resolve(Context::Chat, &x),
            Some(Action::ExternalEditor),
            "the replayed key does not resolve to the binding it had alone"
        );
    }

    /// A second stroke that belongs to no binding abandons the sequence and is
    /// handled as itself. Vim's prefix behaves the same way.
    #[test]
    fn an_unrelated_key_abandons_the_chain() {
        let mut bindings = KeyBindings::default();
        bindings.assign(Action::Help, chord("ctrl+x ctrl+e"));
        let mut tracker = ChordTracker::default();
        assert_eq!(
            tracker.press(
                &bindings,
                Context::Chat,
                &key(KeyCode::Char('x'), KeyModifiers::CONTROL)
            ),
            ChordStep::Pending
        );
        assert_eq!(
            tracker.press(
                &bindings,
                Context::Chat,
                &key(KeyCode::Char('a'), KeyModifiers::NONE)
            ),
            ChordStep::Pass
        );
        assert!(!tracker.is_pending());
        assert_eq!(tracker.expire(), None, "a dropped sequence replayed");
    }

    /// A two-stroke prefix is not replayed: neither key means anything alone,
    /// and firing the second would run a binding the person never asked for.
    #[test]
    fn only_a_one_keystroke_prefix_is_replayed() {
        let mut bindings = KeyBindings::default();
        bindings.assign(Action::Help, chord("ctrl+x ctrl+e ctrl+h"));
        let mut tracker = ChordTracker::default();
        for character in ['x', 'e'] {
            assert_eq!(
                tracker.press(
                    &bindings,
                    Context::Chat,
                    &key(KeyCode::Char(character), KeyModifiers::CONTROL)
                ),
                ChordStep::Pending
            );
        }
        assert_eq!(tracker.expire(), None);
    }

    #[test]
    fn a_single_keystroke_binding_is_unaffected_by_the_tracker() {
        let bindings = KeyBindings::default();
        let mut tracker = ChordTracker::default();
        let ctrl_o = key(KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert_eq!(
            tracker.press(&bindings, Context::Chat, &ctrl_o),
            ChordStep::Complete(Action::ToggleToolOutput)
        );
        assert!(!tracker.is_pending());

        // A key bound to nothing is still nothing.
        assert_eq!(
            tracker.press(
                &bindings,
                Context::Chat,
                &key(KeyCode::Char('z'), KeyModifiers::CONTROL)
            ),
            ChordStep::Pass
        );
    }

    /// A chord in a file is loaded, printed back, and unbindable as a whole.
    #[test]
    fn a_chord_from_a_file_is_bound_and_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("keybindings.json");
        std::fs::write(
            &path,
            r#"{"bindings":[{"context":"Chat","bindings":{"ctrl+x ctrl+e":"app:help"}}]}"#,
        )
        .unwrap();
        let loaded = KeyBindings::load(&[path.as_path()]);
        assert_eq!(
            loaded
                .bindings
                .resolve_sequence(Context::Chat, &[stroke("ctrl+x")], &stroke("ctrl+e")),
            SequenceResolution::Action(Action::Help)
        );
        assert!(
            loaded
                .bindings
                .table()
                .iter()
                .any(|(action, chords)| *action == Action::Help
                    && chords.iter().any(|c| c == "ctrl+x ctrl+e")),
            "the chord is missing from the printed table: {:?}",
            loaded.bindings.table()
        );
        // The only note is the prefix warning the next test pins: `ctrl+x`
        // alone is the editor's, so the chord takes its first half.
        assert_eq!(loaded.notes.len(), 1, "{:?}", loaded.notes);
    }

    /// The longer chord takes the first key away from whatever held it, and
    /// the load says so rather than leaving a silently dead binding.
    #[test]
    fn binding_a_chord_reports_the_prefix_it_takes_over() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("keybindings.json");
        std::fs::write(
            &path,
            r#"{"bindings":[{"context":"Chat","bindings":{"ctrl+x ctrl+e":"app:help"}}]}"#,
        )
        .unwrap();
        let loaded = KeyBindings::load(&[path.as_path()]);
        assert!(
            loaded
                .notes
                .iter()
                .any(|note| note.contains("ctrl+x") && note.contains("chat:externalEditor")),
            "the note does not say which binding the chord takes over: {:?}",
            loaded.notes
        );
    }

    /// A chord with a reserved stroke never completes, so it is refused when
    /// the file is read rather than left as a key that silently does nothing.
    #[test]
    fn a_chord_ending_in_a_reserved_key_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("keybindings.json");
        std::fs::write(
            &path,
            r#"{"bindings":[{"context":"Chat","bindings":{"ctrl+x ctrl+c":"app:help"}}]}"#,
        )
        .unwrap();
        let loaded = KeyBindings::load(&[path.as_path()]);
        assert!(
            loaded
                .notes
                .iter()
                .any(|note| note.contains("ctrl+c") && note.contains("reserved")),
            "{:?}",
            loaded.notes
        );
        assert_eq!(
            loaded.bindings.resolve(
                Context::Chat,
                &key(KeyCode::Char('c'), KeyModifiers::CONTROL)
            ),
            None
        );
    }

    #[test]
    fn defaults_resolve_in_their_context() {
        let bindings = KeyBindings::default();
        assert_eq!(
            bindings.resolve(
                Context::Chat,
                &key(KeyCode::Char('r'), KeyModifiers::CONTROL)
            ),
            Some(Action::HistorySearch)
        );
        assert_eq!(
            bindings.resolve(
                Context::Confirmation,
                &key(KeyCode::Char('y'), KeyModifiers::NONE)
            ),
            Some(Action::ConfirmYes)
        );
        assert_eq!(
            bindings.resolve(
                Context::PermissionDialog,
                &key(KeyCode::Char('A'), KeyModifiers::NONE)
            ),
            Some(Action::PermissionAllowSession)
        );
        assert_eq!(
            bindings.resolve(
                Context::Autocomplete,
                &key(KeyCode::Tab, KeyModifiers::NONE)
            ),
            Some(Action::AutocompleteAccept)
        );
        // A confirmation key does nothing on the chat line.
        assert_eq!(
            bindings.resolve(Context::Chat, &key(KeyCode::Char('y'), KeyModifiers::NONE)),
            None
        );
    }

    /// Every list has a key. The session picker was the one that did not, so
    /// the most-used list was the only one reachable solely by typing a
    /// command; this pins the row of openers together.
    #[test]
    fn every_list_opener_has_a_key_of_its_own() {
        let bindings = KeyBindings::default();
        let openers = [
            Action::AgentsView,
            Action::Rewind,
            Action::ThemePicker,
            Action::PluginBrowser,
            Action::SessionPicker,
            Action::AttachmentBrowser,
        ];
        let mut seen: Vec<KeyStroke> = Vec::new();
        for action in openers {
            let strokes = bindings.strokes(action);
            assert_eq!(
                strokes.len(),
                1,
                "{} should have exactly one default key",
                action.as_str()
            );
            assert!(
                !seen.contains(&strokes[0]),
                "{} shares its key with another list opener",
                action.as_str()
            );
            seen.push(strokes[0]);
        }
        assert_eq!(
            bindings.resolve(Context::Chat, &key(KeyCode::Char('s'), KeyModifiers::ALT)),
            Some(Action::SessionPicker)
        );
    }

    fn document(entries: serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "bindings": entries })
    }

    fn apply(bindings: &mut KeyBindings, entries: serde_json::Value) -> Vec<String> {
        let mut notes = Vec::new();
        bindings.apply_document(&document(entries), &mut notes);
        notes
    }

    /// Moving a shortcut takes it from the action that held it; a key must
    /// never mean two things.
    #[test]
    fn assigning_a_key_moves_it() {
        let mut bindings = KeyBindings::default();
        let notes = apply(
            &mut bindings,
            serde_json::json!([{
                "context": "Global",
                "bindings": { "ctrl+j": "app:toggleToolOutput" }
            }]),
        );
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(
            bindings.resolve(
                Context::Chat,
                &key(KeyCode::Char('j'), KeyModifiers::CONTROL)
            ),
            Some(Action::ToggleToolOutput)
        );
        // The default still works: bindings are additive.
        assert_eq!(
            bindings.resolve(
                Context::Chat,
                &key(KeyCode::Char('o'), KeyModifiers::CONTROL)
            ),
            Some(Action::ToggleToolOutput)
        );

        let notes = apply(
            &mut bindings,
            serde_json::json!([{
                "context": "Global",
                "bindings": { "ctrl+o": "app:toggleSplitPane" }
            }]),
        );
        assert!(
            notes.iter().any(|note| note.contains("moved")),
            "the move was not reported: {notes:?}"
        );
        assert_eq!(
            bindings.resolve(
                Context::Chat,
                &key(KeyCode::Char('o'), KeyModifiers::CONTROL)
            ),
            Some(Action::ToggleSplitPane)
        );
    }

    #[test]
    fn null_unbinds_a_key() {
        let mut bindings = KeyBindings::default();
        let notes = apply(
            &mut bindings,
            serde_json::json!([{
                "context": "Confirmation",
                "bindings": { "esc": null }
            }]),
        );
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(
            bindings.resolve(
                Context::Confirmation,
                &key(KeyCode::Esc, KeyModifiers::NONE)
            ),
            None
        );
        // Other keys for the same action are untouched.
        assert_eq!(
            bindings.resolve(
                Context::Confirmation,
                &key(KeyCode::Char('n'), KeyModifiers::NONE)
            ),
            Some(Action::ConfirmNo)
        );

        // Unbinding a key nothing holds is said out loud.
        let notes = apply(
            &mut bindings,
            serde_json::json!([{
                "context": "Confirmation",
                "bindings": { "esc": null }
            }]),
        );
        assert!(
            notes.iter().any(|note| note.contains("not bound")),
            "{notes:?}"
        );
    }

    /// One bad entry costs that entry, not the file.
    #[test]
    fn a_bad_entry_is_a_note_and_the_rest_applies() {
        let mut bindings = KeyBindings::default();
        let notes = apply(
            &mut bindings,
            serde_json::json!([{
                "context": "Global",
                "bindings": {
                    "ctl+j": "app:toggleToolOutput",
                    "ctrl+c": "app:toggleToolOutput",
                    "ctrl+n": "app:noSuchAction",
                    "ctrl+b": 7,
                    "ctrl+j": "app:toggleSplitPane"
                }
            }]),
        );
        assert!(
            notes.iter().any(|note| note.contains("not a modifier")),
            "{notes:?}"
        );
        assert!(
            notes.iter().any(|note| note.contains("reserved")),
            "{notes:?}"
        );
        assert!(
            notes.iter().any(|note| note.contains("unknown action")),
            "{notes:?}"
        );
        assert!(
            notes.iter().any(|note| note.contains("must be an action")),
            "{notes:?}"
        );
        assert_eq!(
            bindings.resolve(
                Context::Chat,
                &key(KeyCode::Char('j'), KeyModifiers::CONTROL)
            ),
            Some(Action::ToggleSplitPane)
        );
    }

    /// An action filed under the wrong context is applied where it belongs and
    /// the mismatch is reported.
    #[test]
    fn an_action_in_the_wrong_context_is_placed_and_reported() {
        let mut bindings = KeyBindings::default();
        let notes = apply(
            &mut bindings,
            serde_json::json!([{
                "context": "Global",
                "bindings": { "ctrl+y": "confirm:yes" }
            }]),
        );
        assert!(
            notes
                .iter()
                .any(|note| note.contains("Confirmation context")),
            "{notes:?}"
        );
        assert_eq!(
            bindings.resolve(
                Context::Confirmation,
                &key(KeyCode::Char('y'), KeyModifiers::CONTROL)
            ),
            Some(Action::ConfirmYes)
        );
    }

    #[test]
    fn an_unknown_context_is_reported_and_skipped() {
        let mut bindings = KeyBindings::default();
        let notes = apply(
            &mut bindings,
            serde_json::json!([{
                "context": "chat room",
                "bindings": { "ctrl+o": "app:toggleToolOutput" }
            }]),
        );
        assert!(
            notes.iter().any(|note| note.contains("unknown context")),
            "{notes:?}"
        );
        assert_eq!(
            bindings.strokes(Action::ToggleToolOutput),
            vec![stroke("ctrl+o")]
        );
    }

    #[test]
    fn a_file_that_is_not_json_is_a_note() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keybindings.json");
        std::fs::write(&path, "{not json").unwrap();

        let loaded = KeyBindings::load(&[&path]);
        assert!(
            loaded
                .notes
                .iter()
                .any(|note| note.contains("not valid JSON"))
        );
        assert!(loaded.sources.is_empty());
        // The defaults are still there, so the session is usable.
        assert_eq!(
            loaded.bindings.strokes(Action::ToggleToolOutput),
            vec![stroke("ctrl+o")]
        );
    }

    #[test]
    fn later_files_win() {
        let user = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let user_file = user.path().join("keybindings.json");
        let project_file = project.path().join("keybindings.json");
        std::fs::write(
            &user_file,
            r#"{"bindings":[{"context":"Global","bindings":{"ctrl+o":"app:toggleSplitPane"}}]}"#,
        )
        .unwrap();
        std::fs::write(
            &project_file,
            r#"{"bindings":[{"context":"Global","bindings":{"ctrl+o":"app:toggleToolOutput"}}]}"#,
        )
        .unwrap();

        let loaded = KeyBindings::load(&[&user_file, &project_file]);
        assert_eq!(loaded.sources.len(), 2);
        assert_eq!(
            loaded.bindings.resolve(
                Context::Chat,
                &key(KeyCode::Char('o'), KeyModifiers::CONTROL)
            ),
            Some(Action::ToggleToolOutput)
        );
    }

    #[test]
    fn a_missing_file_is_not_a_note() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = KeyBindings::load(&[&dir.path().join("absent.json")]);
        assert!(loaded.notes.is_empty(), "{:?}", loaded.notes);
        assert!(loaded.sources.is_empty());
    }

    /// Enter must not be a palette key: a `/` suggestion that swallowed Enter
    /// made the line unsubmittable while a lookup was showing. In the `@` list
    /// it inserts the path instead, which is the one place it does not submit.
    #[test]
    fn enter_submits_from_the_palette_but_inserts_from_a_mention() {
        let bindings = KeyBindings::default();
        assert_eq!(
            bindings.resolve(
                Context::Autocomplete,
                &key(KeyCode::Enter, KeyModifiers::NONE)
            ),
            None,
            "Enter is bound inside the `/` palette and can never submit the line"
        );
        assert_eq!(
            bindings.resolve(
                Context::FileMention,
                &key(KeyCode::Enter, KeyModifiers::NONE)
            ),
            Some(Action::FileMentionAccept)
        );
        // The palette's keys are inherited, so Tab still completes a path.
        assert_eq!(
            bindings.resolve(Context::FileMention, &key(KeyCode::Tab, KeyModifiers::NONE)),
            Some(Action::AutocompleteAccept)
        );
        assert_eq!(
            bindings.resolve(Context::FileMention, &key(KeyCode::Esc, KeyModifiers::NONE)),
            Some(Action::AutocompleteDismiss)
        );
    }

    /// The table `/keybindings` prints covers every action, and each action
    /// belongs to a real context.
    #[test]
    fn the_table_is_complete() {
        let table = KeyBindings::default().table();
        assert_eq!(table.len(), Action::all().len());
        for (action, strokes) in table {
            assert!(!strokes.is_empty(), "{} has no keystrokes", action.as_str());
            for context in Context::all() {
                assert!(!context.as_str().is_empty());
            }
        }
    }

    #[test]
    fn send_now_is_a_chat_key_that_does_not_shadow_another() {
        let bindings = KeyBindings::default();
        let ctrl_n = key(KeyCode::Char('n'), KeyModifiers::CONTROL);
        assert_eq!(
            bindings.resolve(Context::Chat, &ctrl_n),
            Some(Action::SendNow)
        );
        // The other chat keys it sits beside still resolve to themselves, so
        // the new binding took a key nothing was using.
        assert_eq!(
            bindings.resolve(
                Context::Chat,
                &key(KeyCode::Char('s'), KeyModifiers::CONTROL)
            ),
            Some(Action::StashInput)
        );
        assert_eq!(
            bindings.resolve(
                Context::Chat,
                &key(KeyCode::Char('x'), KeyModifiers::CONTROL)
            ),
            Some(Action::ExternalEditor)
        );
    }
}
