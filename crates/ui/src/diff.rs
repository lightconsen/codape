//! Unified-diff parsing and colouring for the split pane.
//!
//! Codape could already *produce* a diff — `/diff` runs `git diff` and prints
//! the text — but nothing in the interface rendered one as a diff. It arrived
//! as an undifferentiated block, so an added line and a context line looked
//! the same. This module turns unified diff text into lines with a kind
//! attached, which is the only thing the renderer needs to colour them.
//!
//! Deliberately small: it parses the format `git diff` emits, not the whole
//! unified-diff grammar. A hunk header it does not understand is still shown,
//! as plain text, because dropping a line is worse than not colouring it.

/// What a diff line is, which decides its colour and its gutter mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    /// A file header (`diff --git`, `index`, `---`, `+++`, `new file mode`).
    FileHeader,
    /// A hunk header (`@@ -1,4 +1,6 @@`).
    HunkHeader,
    /// A context line (unchanged).
    Context,
    /// An added line.
    Added,
    /// A removed line.
    Removed,
    /// `\ No newline at end of file`.
    NoNewline,
}

/// One line of a unified diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    /// What this line is.
    pub kind: DiffLineKind,
    /// The line, with its diff prefix kept (`+`, `-`, ` `).
    pub text: String,
}

/// Parse unified diff text into classified lines.
///
/// The classification reads only the first character, which is what makes it
/// safe: a line inside a hunk that happens to start with `+++` is context, not
/// a header. Being *inside* a hunk is tracked, and only `@@` ends it.
#[must_use]
pub fn parse_unified(text: &str) -> Vec<DiffLine> {
    let mut lines = Vec::new();
    let mut in_hunk = false;
    for raw in text.lines() {
        let (kind, text) = if raw.starts_with("@@") {
            in_hunk = true;
            (DiffLineKind::HunkHeader, raw)
        } else if raw.starts_with("diff --git")
            || raw.starts_with("index ")
            || raw.starts_with("new file mode")
            || raw.starts_with("deleted file mode")
            || raw.starts_with("old mode")
            || raw.starts_with("new mode")
            || raw.starts_with("similarity index")
            || raw.starts_with("rename from")
            || raw.starts_with("rename to")
            || raw.starts_with("--- ")
            || raw.starts_with("+++ ")
            || raw.starts_with("Binary files")
        {
            // A `---`/`+++` pair only appears between hunks, so it ends one.
            in_hunk = false;
            (DiffLineKind::FileHeader, raw)
        } else if in_hunk {
            match raw.as_bytes().first() {
                Some(b'+') => (DiffLineKind::Added, raw),
                Some(b'-') => (DiffLineKind::Removed, raw),
                Some(b'\\') => (DiffLineKind::NoNewline, raw),
                _ => (DiffLineKind::Context, raw),
            }
        } else {
            // Anything before the first hunk is header-ish; keep it visible.
            (DiffLineKind::FileHeader, raw)
        };
        lines.push(DiffLine {
            kind,
            text: text.to_string(),
        });
    }
    lines
}

/// How many lines each kind contributes, for the pane's title.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DiffStat {
    /// Added lines.
    pub added: usize,
    /// Removed lines.
    pub removed: usize,
    /// Files touched, counted from `diff --git` headers.
    pub files: usize,
}

impl DiffStat {
    /// The `+N -M` summary, as `git diff --stat` would put it.
    #[must_use]
    pub fn summary(&self) -> String {
        format!("{} file(s)  +{} -{}", self.files, self.added, self.removed)
    }
}

/// Count the lines in a parsed diff.
#[must_use]
pub fn stat(lines: &[DiffLine]) -> DiffStat {
    let mut out = DiffStat::default();
    for line in lines {
        match line.kind {
            DiffLineKind::Added => out.added += 1,
            DiffLineKind::Removed => out.removed += 1,
            DiffLineKind::FileHeader if line.text.starts_with("diff --git") => out.files += 1,
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "diff --git a/src/lib.rs b/src/lib.rs\n\
index 111..222 100644\n\
--- a/src/lib.rs\n\
+++ b/src/lib.rs\n\
@@ -1,3 +1,4 @@\n\
 fn one() {}\n\
-fn two() {}\n\
+fn two() { /* changed */ }\n\
+fn three() {}\n";

    #[test]
    fn classifies_each_line_by_its_prefix() {
        let lines = parse_unified(SAMPLE);
        let kinds: Vec<DiffLineKind> = lines.iter().map(|line| line.kind).collect();
        assert_eq!(
            kinds,
            vec![
                DiffLineKind::FileHeader,
                DiffLineKind::FileHeader,
                DiffLineKind::FileHeader,
                DiffLineKind::FileHeader,
                DiffLineKind::HunkHeader,
                DiffLineKind::Context,
                DiffLineKind::Removed,
                DiffLineKind::Added,
                DiffLineKind::Added,
            ]
        );
    }

    /// A context line inside a hunk can begin with `++`, which is not a file
    /// header. Tracking "inside a hunk" is what keeps the two apart.
    #[test]
    fn a_hunk_line_that_looks_like_a_header_stays_context() {
        // Inside a hunk the first byte is the diff's own mark, so a line whose
        // content is `+++ new` is written `++++ new`.
        let diff = "diff --git a/a b/a\n\
--- a/a\n\
+++ b/a\n\
@@ -1,1 +1,1 @@\n\
-+++ old\n\
++++ new\n";
        let lines = parse_unified(diff);
        assert_eq!(lines[4].kind, DiffLineKind::Removed);
        assert_eq!(lines[4].text, "-+++ old");
        assert_eq!(lines[5].kind, DiffLineKind::Added);
        assert_eq!(lines[5].text, "++++ new");
    }

    #[test]
    fn a_second_hunk_keeps_its_own_header() {
        let diff = "@@ -1 +1 @@\n-a\n+b\n@@ -9 +9 @@\n-c\n+d\n";
        let lines = parse_unified(diff);
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.kind == DiffLineKind::HunkHeader)
                .count(),
            2
        );
    }

    #[test]
    fn the_stat_counts_added_removed_and_files() {
        let stat = stat(&parse_unified(SAMPLE));
        assert_eq!(stat.added, 2);
        assert_eq!(stat.removed, 1);
        assert_eq!(stat.files, 1);
        assert_eq!(stat.summary(), "1 file(s)  +2 -1");
    }

    #[test]
    fn text_that_is_not_a_diff_is_kept_not_dropped() {
        // `/diff` in a directory without a repository prints a sentence. It
        // must not vanish just because it is not a diff.
        let lines = parse_unified("Not a git repository\n");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "Not a git repository");
    }
}
