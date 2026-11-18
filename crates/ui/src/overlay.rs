//! Overlay and modal rendering — permission dialogs, notifications, command palette.

/// An overlay layer rendered on top of the main UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Overlay {
    /// Permission approval dialog.
    PermissionDialog {
        /// Tool name requesting permission.
        tool: String,
        /// Tool arguments.
        args: String,
        /// Risk summary or explanation for the user.
        risk_summary: String,
        /// Whether this is a one-time or always-allow request.
        always_option: bool,
    },
    /// A transient notification.
    Notification {
        /// Notification message.
        message: String,
        /// Severity level.
        level: NotificationLevel,
    },
    /// Command palette.
    CommandPalette {
        /// Current search/filter query.
        query: String,
        /// Available command labels.
        commands: Vec<String>,
        /// Currently selected index.
        selected: usize,
    },
    /// `@path` completion list.
    ///
    /// Separate from [`Overlay::CommandPalette`] because choosing an entry
    /// inserts text into the input line rather than running a command, and the
    /// list is filtered as the mention is typed.
    FileMention {
        /// The text typed after the `@`, without the `@`.
        query: String,
        /// Matching project-relative paths.
        files: Vec<String>,
        /// Currently selected index.
        selected: usize,
    },
    /// Generic detail viewer used for plan, activity, failure, and notification inspection.
    Detail {
        /// Overlay title.
        title: String,
        /// Overlay body content.
        body: String,
        /// Rows the body is scrolled down.
        ///
        /// The box grows to hold the body, but only up to the screen: a body
        /// longer than that has to be reachable rather than clipped, and the
        /// help page is longer than any terminal. Kept here rather than in
        /// `UiState` because the position belongs to the body being read: the
        /// next `Detail` to open is a different document and starts at the
        /// top, and the state that replaces this one would have to remember to
        /// reset it.
        scroll: u16,
    },
    /// Confirmation dialog.
    Confirm {
        /// Confirmation prompt.
        message: String,
    },
    /// Model and reasoning-effort chooser.
    ///
    /// A modal list rather than a `/model <name>` argument because the two
    /// settings are one decision — a model with no effort level and an effort
    /// level the model does not have are both mistakes — and because the set
    /// of choices is small enough to show.
    ModelPicker(ModelPicker),
    /// The reasoning-effort slider on its own.
    ///
    /// Separate from [`Overlay::ModelPicker`] rather than a focus preset of it
    /// because the two are reached for at different moments: the picker is
    /// "which model", and the slider is "this is taking too long" or "think
    /// harder about this one" in the middle of a turn. Opening the picker for
    /// the second means tabs and a model list to walk past.
    EffortSlider(EffortSlider),
    /// A one-line text field over the transcript.
    ///
    /// The overlays so far all *choose* from a fixed set. Naming an agent is
    /// the first thing in the interface that needs a string the session cannot
    /// enumerate, and putting it on the input line instead would clear
    /// whatever the person was writing when they pressed the key.
    TextInput(TextInput),
    /// A filterable list of choices supplied by the session.
    ///
    /// One overlay for every surface that is "a titled list you narrow by
    /// typing and pick from": the agent roster, the rewind points, the
    /// theme list, the installed plugins and their marketplaces. The rows are
    /// the session's; the interaction is [`crate::picker::Picker`]'s.
    List(crate::picker::Picker),
}

/// The choices in an open [`Overlay::ModelPicker`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelPicker {
    /// Model ids, current one included.
    pub models: Vec<String>,
    /// Index into [`Self::models`] of the model in use.
    pub model_index: usize,
    /// The session's effort, as its wire spelling.
    pub effort: Option<String>,
    /// Whether the person is adjusting the model or the effort.
    pub focus: PickerFocus,
}

/// The one-line text field behind [`Overlay::TextInput`].
///
/// Deliberately not the transcript's `InputBuffer`: that one carries history,
/// a cursor, vim mode and mention completion, all of which belong to writing a
/// prompt. A field that opens pre-filled with a name needs none of it, and
/// reusing the input line would make the two share a draft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextInput {
    /// What the dialog is asking for, drawn above the field.
    pub prompt: String,
    /// The field's own text, typed or pre-filled.
    pub text: String,
    /// What an accepted string means. Opaque here, like a [`crate::picker::PickerItem`] key.
    pub key: String,
}

impl TextInput {
    /// Open a field asking `prompt`, pre-filled with `initial`.
    #[must_use]
    pub fn new(prompt: impl Into<String>, initial: impl Into<String>, key: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            text: initial.into(),
            key: key.into(),
        }
    }

    /// Append a character.
    pub fn push(&mut self, character: char) {
        self.text.push(character);
    }

    /// Remove the last character.
    pub fn backspace(&mut self) {
        self.text.pop();
    }

    /// The one-line rendering shown in the dialog.
    #[must_use]
    pub fn render(&self) -> String {
        format!(
            "{prompt}\n\n{text}█\n\n[enter] save  [esc] cancel",
            prompt = self.prompt,
            text = self.text
        )
    }
}

/// Which half of the model picker keys move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerFocus {
    /// Left and right change the model.
    Model,
    /// Left and right change the reasoning effort.
    Effort,
}

/// The reasoning-effort slider, on its own.
///
/// Holds nothing but the level being chosen: the session already knows the
/// current one, and the slider is opened from it, applied on Enter and
/// dropped on Esc. Effort is a named level rather than a number, so the
/// "slider" is a row of levels with the chosen one marked — the meaning of
/// `high` is not the meaning of `xhigh` divided in two, and a continuous
/// control would imply it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffortSlider {
    /// The level being pointed at, as its wire spelling.
    pub effort: Option<String>,
}

impl EffortSlider {
    /// Which track position a level sits at.
    const LEVELS: [Option<&'static str>; 6] = ModelPicker::EFFORTS;

    /// Open on the session's current level.
    #[must_use]
    pub fn new(effort: Option<String>) -> Self {
        // An unknown spelling — a settings.json edited by hand — opens as
        // "off" rather than as nothing selected: the slider has to show a
        // position, and the level the session would actually send is the one
        // `ReasoningEffort::parse` accepts.
        let effort = effort.filter(|level| {
            Self::LEVELS
                .iter()
                .any(|candidate| candidate.as_deref() == Some(level.as_str()))
        });
        Self { effort }
    }

    /// Move the slider by `delta`, clamping at both ends.
    ///
    /// Clamping, not wrapping — the same reason the picker's effort half
    /// clamps: "off" and "max" are opposite ends of a cost trade-off, and a
    /// keypress at one end should not land on the other.
    pub fn move_by(&mut self, delta: isize) {
        let position = Self::LEVELS
            .iter()
            .position(|level| *level == self.effort.as_deref())
            .unwrap_or(0);
        let pos = isize::try_from(position).unwrap_or(0);
        let len = isize::try_from(Self::LEVELS.len()).unwrap_or(isize::MAX);
        let next = usize::try_from((pos + delta).clamp(0, len - 1)).unwrap_or(0);
        self.effort = Self::LEVELS[next].map(str::to_string);
    }

    /// The one-line rendering shown in the dialog.
    ///
    /// The track is drawn as a position among six, because that is what tells
    /// a person pressing Right whether there is further to go — the same
    /// question a scroll bar answers, and the reason it is a slider rather
    /// than a word that changes.
    #[must_use]
    pub fn render(&self) -> String {
        use std::fmt::Write as _;

        let position = Self::LEVELS
            .iter()
            .position(|level| *level == self.effort.as_deref())
            .unwrap_or(0);
        let mut out = String::from("Reasoning effort\n\n");
        let mut track = String::new();
        for (index, level) in Self::LEVELS.iter().enumerate() {
            let mark = if index == position { "◆" } else { "◇" };
            let _ = write!(track, "{mark}{} ", level.unwrap_or("off"));
        }
        let _ = writeln!(out, "{}", track.trim_end());
        let arrows = "--".repeat(position);
        let _ = writeln!(
            out,
            "  {arrows}^ {}",
            self.effort.as_deref().unwrap_or("off")
        );
        out.push_str("\n[←/→] change  [enter] apply  [esc] cancel");
        out
    }
}

impl ModelPicker {
    /// The effort levels, cheapest first, including "off".
    ///
    /// `None` is listed first because a session that never set an effort is
    /// the common case and has to stay reachable.
    pub const EFFORTS: [Option<&'static str>; 6] = [
        None,
        Some("low"),
        Some("medium"),
        Some("high"),
        Some("xhigh"),
        Some("max"),
    ];

    /// Build a picker with `current` selected.
    #[must_use]
    pub fn new(models: Vec<String>, current: &str, effort: Option<String>) -> Self {
        let model_index = models.iter().position(|m| m == current).unwrap_or(0);
        Self {
            models,
            model_index,
            effort,
            focus: PickerFocus::Model,
        }
    }

    /// The model the picker is pointing at.
    #[must_use]
    pub fn selected_model(&self) -> Option<&str> {
        self.models.get(self.model_index).map(String::as_str)
    }

    /// Move the model selection by `delta`, clamping at both ends.
    pub fn move_model(&mut self, delta: isize) {
        if self.models.is_empty() {
            return;
        }
        let len = isize::try_from(self.models.len()).unwrap_or(isize::MAX);
        let pos = isize::try_from(self.model_index).unwrap_or(0);
        self.model_index = usize::try_from((pos + delta).clamp(0, len - 1)).unwrap_or(0);
    }

    /// Move the effort by `delta`, clamping at both ends.
    ///
    /// Clamping rather than wrapping: "max" and "off" are far apart in meaning
    /// and a keypress at the end of the list should not jump between them.
    pub fn move_effort(&mut self, delta: isize) {
        let levels = Self::EFFORTS;
        let position = levels
            .iter()
            .position(|level| *level == self.effort.as_deref())
            .unwrap_or(0);
        let pos = isize::try_from(position).unwrap_or(0);
        let len = isize::try_from(levels.len()).unwrap_or(isize::MAX);
        let next = usize::try_from((pos + delta).clamp(0, len - 1)).unwrap_or(0);
        self.effort = levels[next].map(str::to_string);
    }

    /// The one-line rendering shown in the dialog.
    #[must_use]
    pub fn render(&self) -> String {
        use std::fmt::Write as _;

        let mut out = String::from("Model\n");
        for (index, model) in self.models.iter().enumerate() {
            let marker = if index == self.model_index {
                "▸"
            } else {
                " "
            };
            let focus = if self.focus == PickerFocus::Model && index == self.model_index {
                " ◂"
            } else {
                ""
            };
            let _ = writeln!(out, "{marker} {model}{focus}");
        }
        let effort = self.effort.as_deref().unwrap_or("off");
        let focus = if self.focus == PickerFocus::Effort {
            " ◂"
        } else {
            ""
        };
        let _ = writeln!(out, "\nEffort: {effort}{focus}");
        out.push_str("\n[tab] model/effort  [←/→] change  [enter] apply  [esc] cancel");
        out
    }
}

/// Notification severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationLevel {
    /// Informational.
    Info,
    /// Warning.
    Warning,
    /// Error.
    Error,
}

/// Overlay manager — stack of overlays.
#[derive(Debug, Default)]
pub struct OverlayStack {
    overlays: Vec<Overlay>,
}

impl OverlayStack {
    /// Create a new empty overlay stack.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            overlays: Vec::new(),
        }
    }

    /// Push an overlay on top.
    pub fn push(&mut self, overlay: Overlay) {
        self.overlays.push(overlay);
    }

    /// Pop the top overlay.
    pub fn pop(&mut self) -> Option<Overlay> {
        self.overlays.pop()
    }

    /// Clear all overlays.
    pub fn clear(&mut self) {
        self.overlays.clear();
    }

    /// Drop the inline completion palette, if it is the top overlay.
    ///
    /// A command that opens a dialog is submitted from the input line, which
    /// means the `/` palette listing that very command is up when the command
    /// runs. Left there it draws over the dialog and, worse, counts as an
    /// overlay to every `is_active` check behind it. Removes only the palette:
    /// anything else on the stack was put there deliberately and is not this
    /// command's to dismiss.
    pub fn pop_palette(&mut self) {
        if matches!(
            self.overlays.last(),
            Some(Overlay::CommandPalette { .. } | Overlay::FileMention { .. })
        ) {
            self.overlays.pop();
        }
    }

    /// Get the top overlay.
    #[must_use]
    pub fn top(&self) -> Option<&Overlay> {
        self.overlays.last()
    }

    /// The top overlay, mutably, for updating a dialog in place.
    pub fn top_mut(&mut self) -> Option<&mut Overlay> {
        self.overlays.last_mut()
    }

    /// Check if any overlay is active.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        !self.overlays.is_empty()
    }

    /// Check whether an overlay is up that *owns the keyboard*.
    ///
    /// This is what a slash command's "should I open a dialog?" question
    /// means, and it is deliberately not [`Self::is_active`]: the `/` and `@`
    /// completion palettes are overlays but not dialogs. They are drawn from
    /// the input line and disappear when it stops matching, so while someone is
    /// typing `/resume` the palette for the word they are still writing is
    /// necessarily up. Asking `is_active` there made a command that opens a
    /// list think a dialog was already showing and fall through to its other
    /// behaviour — `bare /resume` resumed the newest session, and the same
    /// check sat in front of bare `/theme` and `/plugin`.
    ///
    /// Both inline palettes are excluded: `@path` puts `FileMention` up for the
    /// same reason, and a command line never contains one.
    #[must_use]
    pub fn has_modal(&self) -> bool {
        self.overlays.iter().any(|overlay| {
            !matches!(
                overlay,
                Overlay::CommandPalette { .. } | Overlay::FileMention { .. }
            )
        })
    }

    /// Count active overlays.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.overlays.len()
    }

    /// Check if there are no active overlays.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.overlays.is_empty()
    }

    /// Update the command palette query and filter commands.
    pub fn update_palette_query(&mut self, _query: &str) {
        if let Some(Overlay::CommandPalette {
            commands, selected, ..
        }) = self.overlays.last_mut()
        {
            // Filter would happen here; for now just update query
            let _ = (commands, selected);
        }
    }

    /// Move selection in command palette.
    pub fn move_palette_selection(&mut self, delta: isize) {
        if let Some(Overlay::CommandPalette {
            commands, selected, ..
        }) = self.overlays.last_mut()
        {
            if commands.is_empty() {
                return;
            }
            let len = isize::try_from(commands.len()).unwrap_or(isize::MAX);
            let pos = isize::try_from(*selected).unwrap_or(isize::MAX);
            *selected = usize::try_from((pos + delta).rem_euclid(len)).unwrap_or(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_and_pop() {
        let mut stack = OverlayStack::new();
        assert!(!stack.is_active());

        stack.push(Overlay::Notification {
            message: "Done".into(),
            level: NotificationLevel::Info,
        });
        assert!(stack.is_active());
        assert_eq!(stack.len(), 1);

        let popped = stack.pop();
        assert!(popped.is_some());
        assert!(!stack.is_active());
    }

    #[test]
    fn the_completion_palette_is_up_but_is_not_a_dialog() {
        // The distinction the slash-command arms depend on: while someone
        // types `/resume` the palette for that word is up, and a command that
        // asks "is a dialog showing?" must not answer yes for it.
        let mut stack = OverlayStack::new();
        stack.push(Overlay::CommandPalette {
            query: "/resume".into(),
            commands: vec!["/resume - Resume a previous session".into()],
            selected: 0,
        });
        assert!(stack.is_active(), "it is still an overlay");
        assert!(!stack.has_modal(), "but it does not own the keyboard");

        stack.push(Overlay::List(crate::picker::Picker::new("Sessions", vec![])));
        assert!(stack.has_modal());
    }

    #[test]
    fn popping_the_palette_leaves_a_dialog_underneath() {
        let mut stack = OverlayStack::new();
        stack.push(Overlay::List(crate::picker::Picker::new("Sessions", vec![])));
        // Nothing to pop: the top is not a palette, and a command must not be
        // able to dismiss a dialog it did not open.
        stack.pop_palette();
        assert_eq!(stack.len(), 1);

        stack.push(Overlay::FileMention {
            query: "src".into(),
            files: vec!["src/lib.rs".into()],
            selected: 0,
        });
        stack.pop_palette();
        assert_eq!(stack.len(), 1);
        assert!(matches!(stack.top(), Some(Overlay::List(_))));
    }

    #[test]
    fn top_returns_last() {
        let mut stack = OverlayStack::new();
        stack.push(Overlay::Notification {
            message: "first".into(),
            level: NotificationLevel::Info,
        });
        stack.push(Overlay::Notification {
            message: "second".into(),
            level: NotificationLevel::Warning,
        });
        assert_eq!(stack.len(), 2);
        if let Some(Overlay::Notification { message, .. }) = stack.top() {
            assert_eq!(message, "second");
        } else {
            panic!("expected notification");
        }
    }

    #[test]
    fn clear_all() {
        let mut stack = OverlayStack::new();
        stack.push(Overlay::Notification {
            message: "a".into(),
            level: NotificationLevel::Info,
        });
        stack.push(Overlay::Notification {
            message: "b".into(),
            level: NotificationLevel::Error,
        });
        stack.clear();
        assert!(!stack.is_active());
    }

    #[test]
    fn permission_dialog() {
        let mut stack = OverlayStack::new();
        stack.push(Overlay::PermissionDialog {
            tool: "Bash".into(),
            args: "rm -rf /tmp/test".into(),
            risk_summary: "Destructive shell command".into(),
            always_option: true,
        });
        if let Some(Overlay::PermissionDialog {
            tool,
            always_option,
            ..
        }) = stack.top()
        {
            assert_eq!(tool, "Bash");
            assert!(*always_option);
        } else {
            panic!("expected permission dialog");
        }
    }

    #[test]
    fn palette_selection_wraps() {
        let mut stack = OverlayStack::new();
        stack.push(Overlay::CommandPalette {
            query: String::new(),
            commands: vec!["a".into(), "b".into(), "c".into()],
            selected: 0,
        });
        stack.move_palette_selection(-1); // wrap to end
        if let Some(Overlay::CommandPalette { selected, .. }) = stack.top() {
            assert_eq!(*selected, 2);
        }
        stack.move_palette_selection(1); // wrap to start
        if let Some(Overlay::CommandPalette { selected, .. }) = stack.top() {
            assert_eq!(*selected, 0);
        }
    }

    #[test]
    fn empty_palette_no_crash() {
        let mut stack = OverlayStack::new();
        stack.push(Overlay::CommandPalette {
            query: String::new(),
            commands: Vec::new(),
            selected: 0,
        });
        stack.move_palette_selection(1); // should not panic
    }

    fn picker() -> ModelPicker {
        ModelPicker::new(
            vec!["opus".into(), "sonnet".into(), "haiku".into()],
            "sonnet",
            Some("high".into()),
        )
    }

    #[test]
    fn the_picker_opens_on_the_model_in_use() {
        let picker = picker();
        assert_eq!(picker.selected_model(), Some("sonnet"));
    }

    /// Both ends clamp: "max" and "off" are not neighbours in meaning, so a
    /// keypress at either end must not wrap into the other.
    #[test]
    fn picker_ends_clamp_instead_of_wrapping() {
        let mut picker = picker();
        picker.move_model(10);
        assert_eq!(picker.selected_model(), Some("haiku"));
        picker.move_model(-10);
        assert_eq!(picker.selected_model(), Some("opus"));

        assert_eq!(picker.effort.as_deref(), Some("high"));
        picker.move_effort(10);
        assert_eq!(picker.effort.as_deref(), Some("max"));
        picker.move_effort(-10);
        assert_eq!(picker.effort, None, "the first entry is off");
        picker.move_effort(-1);
        assert_eq!(picker.effort, None, "off is the floor");
    }

    #[test]
    fn the_picker_body_names_the_current_choices() {
        let rendered = picker().render();
        assert!(rendered.contains("opus"), "{rendered}");
        assert!(rendered.contains("haiku"), "{rendered}");
        assert!(rendered.contains("Effort: high"), "{rendered}");
        assert!(rendered.contains("[tab]"), "no key hint: {rendered}");
    }

    #[test]
    fn the_slider_opens_on_the_session_level() {
        let slider = EffortSlider::new(Some("xhigh".into()));
        assert_eq!(slider.effort.as_deref(), Some("xhigh"));
        assert_eq!(EffortSlider::new(None).effort, None, "off is the default");
    }

    /// A spelling the levels do not have — a hand-edited `settings.json` —
    /// opens at "off" rather than at nothing: the dialog has to show where the
    /// handle is, and a handle past the end of the track shows nothing.
    #[test]
    fn an_unknown_level_opens_as_off() {
        assert_eq!(EffortSlider::new(Some("turbo".into())).effort, None);
    }

    #[test]
    fn the_slider_ends_clamp() {
        let mut slider = EffortSlider::new(Some("max".into()));
        slider.move_by(1);
        assert_eq!(slider.effort.as_deref(), Some("max"), "max is the ceiling");
        let mut slider = EffortSlider::new(None);
        slider.move_by(-1);
        assert_eq!(slider.effort, None, "off is the floor");
        slider.move_by(2);
        assert_eq!(slider.effort.as_deref(), Some("medium"));
    }

    /// The track shows a position, not just the word: that is what tells a
    /// person pressing Right whether there is further to go.
    #[test]
    fn the_slider_draws_where_the_handle_is() {
        let rendered = EffortSlider::new(Some("high".into())).render();
        assert!(rendered.contains("◆high"), "{rendered}");
        assert!(!rendered.contains("◆off"), "two handles: {rendered}");
        assert!(rendered.contains("[←/→]"), "no key hint: {rendered}");
    }

    #[test]
    fn a_text_field_opens_on_what_is_there_and_edits_in_place() {
        let mut field = TextInput::new("Rename this agent", "auth scout", "rename:a1");
        assert_eq!(field.text, "auth scout");
        field.backspace();
        field.push('!');
        assert_eq!(field.text, "auth scou!");
        let rendered = field.render();
        assert!(rendered.contains("Rename this agent"), "{rendered}");
        assert!(rendered.contains("auth scou!"), "{rendered}");
        assert!(rendered.contains("[enter] save"), "{rendered}");
    }

    /// The field starts empty when there is nothing to pre-fill, and
    /// backspacing an empty field is not an underflow.
    #[test]
    fn an_empty_text_field_backspaces_to_nothing() {
        let mut field = TextInput::new("Group", "", "group:a1");
        field.backspace();
        assert_eq!(field.text, "");
    }
}
