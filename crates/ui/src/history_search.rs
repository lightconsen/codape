//! Reverse incremental history search (Ctrl+R).
//!
//! The input buffer's Up/Down recall walks history one entry at a time, which
//! is right for `the last prompt` and useless for `that command I typed twenty
//! turns ago`. Ctrl+R opens this: every keystroke narrows the list, the newest
//! match is highlighted, Enter puts it on the input line and Esc puts back
//! whatever was there before.
//!
//! The search holds its own copy of the candidate list at open time. History
//! only grows at submit, so a snapshot cannot go stale in a way anyone can
//! notice, and it keeps this type independent of the input buffer.

/// How many matches the renderer shows at once.
pub const VISIBLE_MATCHES: usize = 4;

/// An open reverse search over the input history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistorySearch {
    /// What has been typed since the search opened.
    query: String,
    /// Matching history entries, newest first.
    matches: Vec<String>,
    /// Which match is highlighted.
    selected: usize,
    /// The input text to restore if the search is cancelled.
    original: String,
}

impl HistorySearch {
    /// Open a search over `history`, remembering `original` for Esc.
    ///
    /// The newest entry is highlighted first, which is what makes a bare
    /// Ctrl+R a convenient "the command before this one".
    #[must_use]
    pub fn new(history: &[String], original: String) -> Self {
        let mut search = Self {
            query: String::new(),
            matches: Vec::new(),
            selected: 0,
            original,
        };
        search.refresh(history);
        search
    }

    /// The current query.
    #[must_use]
    pub fn query(&self) -> &str {
        &self.query
    }

    /// The matching entries, newest first.
    #[must_use]
    pub fn matches(&self) -> &[String] {
        &self.matches
    }

    /// Which match is highlighted.
    #[must_use]
    pub const fn selected(&self) -> usize {
        self.selected
    }

    /// The highlighted entry, if anything matched.
    #[must_use]
    pub fn current(&self) -> Option<&str> {
        self.matches.get(self.selected).map(String::as_str)
    }

    /// The text to put back on Esc.
    #[must_use]
    pub fn original(&self) -> &str {
        &self.original
    }

    /// Add a character to the query and re-filter.
    pub fn push_char(&mut self, c: char, history: &[String]) {
        self.query.push(c);
        self.refresh(history);
    }

    /// Remove the last character of the query and re-filter.
    pub fn pop_char(&mut self, history: &[String]) {
        self.query.pop();
        self.refresh(history);
    }

    /// Move the highlight by `delta`, clamped to the match list.
    ///
    /// Clamped rather than wrapped: at either end the highlight simply stays,
    /// so holding Down does not jump back to the newest entry and Enter never
    /// picks something the person did not see highlighted.
    pub fn move_selection(&mut self, delta: isize) {
        if self.matches.is_empty() {
            return;
        }
        let last = self.matches.len() - 1;
        let moved = self.selected.saturating_add_signed(delta).min(last);
        self.selected = moved;
    }

    /// Recompute the matches for the current query.
    ///
    /// Matching is case-insensitive substring, which is what people expect
    /// from Ctrl+R: a fuzzy match that reorders history would make "the one
    /// before this one" unpredictable.
    fn refresh(&mut self, history: &[String]) {
        let needle = self.query.to_lowercase();
        self.matches = history
            .iter()
            .rev()
            .filter(|entry| needle.is_empty() || entry.to_lowercase().contains(&needle))
            .cloned()
            .collect();
        self.selected = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history() -> Vec<String> {
        vec![
            "cargo test".into(),
            "git status".into(),
            "cargo build --release".into(),
            "ls".into(),
        ]
    }

    #[test]
    fn a_fresh_search_highlights_the_newest_entry() {
        let search = HistorySearch::new(&history(), "draft".into());
        assert_eq!(search.query(), "");
        assert_eq!(search.matches().len(), 4);
        assert_eq!(search.current(), Some("ls"));
        assert_eq!(search.original(), "draft");
    }

    #[test]
    fn typing_narrows_to_substring_matches_newest_first() {
        let mut search = HistorySearch::new(&history(), String::new());
        for c in "CARGO".chars() {
            search.push_char(c, &history());
        }
        assert_eq!(
            search.matches(),
            [
                "cargo build --release".to_string(),
                "cargo test".to_string()
            ]
        );
        assert_eq!(search.current(), Some("cargo build --release"));
    }

    #[test]
    fn backspace_widens_the_search_again() {
        let mut search = HistorySearch::new(&history(), String::new());
        for c in "lsx".chars() {
            search.push_char(c, &history());
        }
        assert!(search.matches().is_empty());
        search.pop_char(&history());
        assert_eq!(search.matches(), ["ls".to_string()]);
    }

    #[test]
    fn selection_clamps_instead_of_wrapping() {
        let mut search = HistorySearch::new(&history(), String::new());
        search.move_selection(-1);
        assert_eq!(search.selected(), 0, "selection wrapped off the top");
        for _ in 0..10 {
            search.move_selection(1);
        }
        assert_eq!(search.selected(), 3, "selection wrapped off the bottom");
        assert_eq!(search.current(), Some("cargo test"));
    }

    #[test]
    fn a_query_that_matches_nothing_has_no_current_entry() {
        let mut search = HistorySearch::new(&history(), String::new());
        for c in "zzz".chars() {
            search.push_char(c, &history());
        }
        assert_eq!(search.current(), None);
        // Moving in an empty list must not panic or invent an index.
        search.move_selection(1);
        assert_eq!(search.selected(), 0);
    }
}
