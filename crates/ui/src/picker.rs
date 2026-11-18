//! A filterable modal list — the one interaction the TUI's surfaces were
//! missing.
//!
//! The TUI could already *show* things: a plan, a diff, a notification, a
//! permission dialog. What it could not do was offer a list and take a choice
//! from it. Every such surface was therefore reachable only by typing a slash
//! command with the answer spelled out — `/rewind 42`, `/theme dark`,
//! `/agents follow <id>` — which is exactly the kind of thing a person has to
//! look up rather than recognise.
//!
//! Almost every one of those lists is the same interaction: a titled set of
//! rows, a filter you type into, a highlight you move, one row you accept.
//! They differ only in what a row *is*. So this module owns the interaction
//! once, generically, and each surface supplies its rows.
//!
//! Deliberately not a widget hierarchy. A `Picker` is data: [`PickerItem`]
//! rows plus a query and a highlight. Rendering it is `terminal.rs`'s job and
//! acting on the choice is the session's, which keeps this module free of both
//! ratatui and `codape-core`.

use std::fmt::Write as _;

/// One row a [`Picker`] can offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerItem {
    /// What accepting this row means. Opaque here: the surface that built the
    /// list is the only thing that can interpret it, and it must survive a
    /// round trip through the picker unchanged.
    pub key: String,
    /// The row as drawn.
    pub label: String,
    /// Right-aligned annotation — a status, a size, a shortcut. Empty is fine.
    pub detail: String,
    /// Whether the row is offered at all.
    ///
    /// A greyed row is still shown: a list that silently omits the one entry
    /// someone is looking for is worse than a list that says why it cannot be
    /// chosen. Accepting a disabled row is refused rather than ignored.
    pub enabled: bool,
}

impl PickerItem {
    /// A plain selectable row.
    #[must_use]
    pub fn new(key: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            label: label.into(),
            detail: String::new(),
            enabled: true,
        }
    }

    /// Attach the right-aligned annotation.
    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }

    /// Mark the row unavailable, with the reason as its annotation.
    #[must_use]
    pub fn disabled(mut self, reason: impl Into<String>) -> Self {
        self.enabled = false;
        self.detail = reason.into();
        self
    }

    /// Whether `query` matches this row.
    ///
    /// Case-insensitive substring over the label and the key. Not fuzzy: a
    /// list this short is faster to narrow by substring, and a fuzzy match
    /// that highlights the wrong row costs more than it saves.
    #[must_use]
    pub fn matches(&self, query: &str) -> bool {
        if query.is_empty() {
            return true;
        }
        let needle = query.to_ascii_lowercase();
        self.label.to_ascii_lowercase().contains(&needle)
            || self.key.to_ascii_lowercase().contains(&needle)
    }
}

/// An open modal list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picker {
    /// The box title.
    pub title: String,
    /// Every row, in display order; the filter is applied on top.
    pub items: Vec<PickerItem>,
    /// The filter as typed.
    pub query: String,
    /// Index into [`Self::visible`], not into `items` — the two differ as soon
    /// as someone types.
    pub selected: usize,
}

impl Picker {
    /// A picker over `items`, highlighted on the first selectable row.
    #[must_use]
    pub fn new(title: impl Into<String>, items: Vec<PickerItem>) -> Self {
        let mut picker = Self {
            title: title.into(),
            items,
            query: String::new(),
            selected: 0,
        };
        picker.select_first_enabled();
        picker
    }

    /// The rows the filter currently admits, as indices into [`Self::items`].
    #[must_use]
    pub fn visible(&self) -> Vec<usize> {
        self.items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.matches(&self.query))
            .map(|(index, _)| index)
            .collect()
    }

    /// The highlighted row, if the filter admits any.
    #[must_use]
    pub fn selected_item(&self) -> Option<&PickerItem> {
        let visible = self.visible();
        visible
            .get(self.selected)
            .and_then(|index| self.items.get(*index))
    }

    /// Move the highlight by `delta`, wrapping at both ends.
    ///
    /// Wrapping, unlike the model picker's effort list: these rows are peers
    /// of unknown length, and a list you can type into is one people page
    /// through in both directions.
    pub fn move_selection(&mut self, delta: isize) {
        let len = self.visible().len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        let len_signed = isize::try_from(len).unwrap_or(isize::MAX);
        let pos = isize::try_from(self.selected).unwrap_or(0);
        self.selected = usize::try_from((pos + delta).rem_euclid(len_signed)).unwrap_or(0);
    }

    /// Type a character into the filter.
    ///
    /// Every keystroke re-filters, so the highlight has to be re-clamped: the
    /// row it was on may not be admitted any more. It lands on the first match
    /// rather than staying at the same offset, because the offsets of two
    /// different result sets are not comparable.
    pub fn push_query(&mut self, character: char) {
        self.query.push(character);
        self.select_first_enabled();
    }

    /// Remove the last character of the filter.
    pub fn pop_query(&mut self) {
        self.query.pop();
        self.select_first_enabled();
    }

    /// Replace the filter wholesale.
    pub fn set_query(&mut self, query: impl Into<String>) {
        self.query = query.into();
        self.select_first_enabled();
    }

    /// The answer: the highlighted row, when it and its choice are usable.
    ///
    /// Returns the row's `key`. `None` covers both "nothing matches" and "the
    /// highlighted row is disabled", because the caller's next move is the
    /// same in each case — say which it was and leave the dialog open.
    #[must_use]
    pub fn accept(&self) -> Option<&str> {
        self.selected_item()
            .filter(|item| item.enabled)
            .map(|item| item.key.as_str())
    }

    /// Why [`Self::accept`] would refuse, for the message left on screen.
    #[must_use]
    pub fn refusal(&self) -> Option<String> {
        let Some(item) = self.selected_item() else {
            return Some(format!("Nothing matches \"{}\"", self.query));
        };
        if item.enabled {
            return None;
        }
        let reason = if item.detail.is_empty() {
            String::new()
        } else {
            format!(": {}", item.detail)
        };
        Some(format!("{} is unavailable{reason}", item.label))
    }

    /// Highlight the first enabled row the filter admits.
    fn select_first_enabled(&mut self) {
        let visible = self.visible();
        self.selected = visible
            .iter()
            .position(|index| self.items.get(*index).is_some_and(|item| item.enabled))
            .unwrap_or(0);
    }

    /// The dialog body as drawn.
    ///
    /// The filter line is always present — even when empty — because a list
    /// that looks static is one people do not try typing into.
    #[must_use]
    pub fn render(&self, max_rows: usize) -> String {
        let visible = self.visible();
        let mut out = String::new();
        let _ = writeln!(out, "Filter: {}", self.query);
        out.push('\n');

        if visible.is_empty() {
            out.push_str("  (nothing matches)\n");
        }

        // Keep the highlight on screen. Scrolling by a window rather than
        // always starting at zero is what makes a long agent roster usable.
        let start = if self.selected >= max_rows {
            self.selected + 1 - max_rows
        } else {
            0
        };
        for (offset, index) in visible.iter().enumerate().skip(start).take(max_rows) {
            let Some(item) = self.items.get(*index) else {
                continue;
            };
            let marker = if offset == self.selected { "▸" } else { " " };
            let label = if item.enabled {
                item.label.clone()
            } else {
                format!("{} (unavailable)", item.label)
            };
            if item.detail.is_empty() {
                let _ = writeln!(out, "{marker} {label}");
            } else {
                let _ = writeln!(out, "{marker} {label}  ·  {}", item.detail);
            }
        }

        let hidden = visible.len().saturating_sub(start + max_rows);
        if hidden > 0 {
            let _ = writeln!(out, "  … {hidden} more");
        }
        out.push_str("\n[↑/↓] move  [type] filter  [enter] choose  [esc] cancel");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picker() -> Picker {
        Picker::new(
            "Agents",
            vec![
                PickerItem::new("a1", "Explore · auth flow").with_detail("running"),
                PickerItem::new("a2", "Plan · migration").with_detail("done"),
                PickerItem::new("a3", "Explore · tests").disabled("finished, no transcript"),
            ],
        )
    }

    #[test]
    fn the_filter_narrows_and_the_highlight_follows() {
        let mut picker = picker();
        picker.set_query("plan");
        let visible = picker.visible();
        assert_eq!(visible.len(), 1);
        assert_eq!(picker.selected_item().map(|i| i.key.as_str()), Some("a2"));
        assert_eq!(picker.accept(), Some("a2"));
    }

    #[test]
    fn the_filter_is_case_insensitive_and_matches_the_key_too() {
        let mut picker = picker();
        picker.set_query("EXPLORE");
        assert_eq!(picker.visible().len(), 2, "two Explore rows");
        picker.set_query("a3");
        assert_eq!(picker.visible().len(), 1, "keys are matched as well");
    }

    #[test]
    fn a_disabled_row_is_shown_but_cannot_be_accepted() {
        let mut picker = picker();
        picker.set_query("tests");
        assert_eq!(picker.visible().len(), 1, "the row is still listed");
        assert_eq!(picker.accept(), None);
        assert!(
            picker.refusal().is_some_and(|r| r.contains("unavailable")),
            "{:?}",
            picker.refusal()
        );
    }

    #[test]
    fn the_highlight_skips_a_disabled_row_on_open() {
        let picker = Picker::new(
            "t",
            vec![
                PickerItem::new("x", "first").disabled("nope"),
                PickerItem::new("y", "second"),
            ],
        );
        assert_eq!(picker.selected_item().map(|i| i.key.as_str()), Some("y"));
    }

    #[test]
    fn an_empty_result_explains_itself() {
        let mut picker = picker();
        picker.set_query("nothing like this");
        assert!(picker.visible().is_empty());
        assert_eq!(picker.accept(), None);
        assert!(
            picker
                .refusal()
                .is_some_and(|r| r.contains("Nothing matches"))
        );
        // Moving in an empty list must not panic or produce an impossible index.
        picker.move_selection(3);
        assert_eq!(picker.selected, 0);
    }

    #[test]
    fn selection_wraps_in_both_directions() {
        let mut picker = picker();
        picker.set_query("");
        picker.move_selection(-1);
        assert_eq!(picker.selected, 2, "wraps to the end");
        picker.move_selection(1);
        assert_eq!(picker.selected, 0, "wraps to the start");
    }

    #[test]
    fn typing_and_backspacing_re_filter() {
        let mut picker = picker();
        for c in "plan".chars() {
            picker.push_query(c);
        }
        assert_eq!(picker.visible().len(), 1);
        picker.pop_query();
        assert_eq!(picker.query, "pla");
        assert_eq!(picker.visible().len(), 1, "still matches Plan");
        picker.pop_query();
        picker.pop_query();
        picker.pop_query();
        assert!(picker.query.is_empty());
        assert_eq!(picker.visible().len(), 3, "the whole list is back");
    }

    #[test]
    fn the_body_names_the_filter_and_the_keys() {
        let rendered = picker().render(10);
        assert!(rendered.contains("Filter:"), "{rendered}");
        assert!(rendered.contains("[enter] choose"), "{rendered}");
        assert!(rendered.contains("Explore · auth flow"), "{rendered}");
    }

    /// A highlight below the window must be scrolled into view, or the row the
    /// person just moved to is invisible.
    #[test]
    fn the_window_follows_the_highlight() {
        let items: Vec<PickerItem> = (0..20)
            .map(|i| PickerItem::new(format!("k{i}"), format!("row {i}")))
            .collect();
        let mut picker = Picker::new("t", items);
        picker.move_selection(15);
        let rendered = picker.render(5);
        assert!(rendered.contains("row 15"), "{rendered}");
        assert!(
            !rendered.contains("row 0"),
            "scrolled past the top: {rendered}"
        );
        assert!(
            rendered.contains("more"),
            "reports the hidden rows: {rendered}"
        );
    }
}
