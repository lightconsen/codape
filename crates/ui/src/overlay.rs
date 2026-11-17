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

/// Which half of the model picker keys move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerFocus {
    /// Left and right change the model.
    Model,
    /// Left and right change the reasoning effort.
    Effort,
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
}
