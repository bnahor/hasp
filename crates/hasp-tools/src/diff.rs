//! Line diff for tool output.
//!
//! A coding agent's most useful tool report is the change itself, so `write` and
//! `edit` return a compact unified diff. The model reads it, and the TUI colours
//! `+`/`-`/`@@` lines. The diff itself is `hasp-diff`: near-linear on any
//! input, so a whole-file rewrite by the agent costs milliseconds.

/// Cap on emitted diff lines, so a rewrite cannot flood the transcript.
pub const MAX_DIFF_LINES: usize = 80;

/// Unified diff of two texts. `path` is only used for the file headers.
pub fn unified(old: &str, new: &str, path: &str) -> String {
    let mut d = hasp_diff::unified(old, new, path, MAX_DIFF_LINES);
    d.truncate(d.trim_end_matches('\n').len());
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_an_appended_line_with_context() {
        let d = unified("a\nb\nc\n", "a\nb\nc\nd\n", "f.txt");
        assert!(
            d.starts_with("--- f.txt\n+++ f.txt\n@@ -1,3 +1,4 @@\n"),
            "{d}"
        );
        assert!(d.ends_with("+d"), "added line last: {d}");
        assert!(d.contains("\n c\n"), "context line kept: {d}");
    }

    #[test]
    fn reports_a_changed_line() {
        let d = unified("one\ntwo\nthree\n", "one\nTWO\nthree\n", "f.txt");
        assert!(d.contains("\n-two\n"), "{d}");
        assert!(d.contains("\n+TWO\n"), "{d}");
    }

    #[test]
    fn identical_text_is_not_a_diff() {
        assert_eq!(unified("x\n", "x\n", "f.txt"), "f.txt: unchanged");
    }

    #[test]
    fn a_new_file_is_all_additions() {
        let d = unified("", "hello\nworld\n", "new.txt");
        assert!(d.contains("\n+hello\n"), "{d}");
        assert!(d.ends_with("+world"), "{d}");
        assert!(!d.contains("\n-"), "{d}");
    }

    #[test]
    fn long_files_still_produce_a_bounded_diff() {
        let old: String = (0..5000).map(|i| format!("line {i}\n")).collect();
        let new = old.replace("line 2500", "LINE 2500");
        let d = unified(&old, &new, "big.txt");
        assert!(d.lines().count() <= MAX_DIFF_LINES + 8, "{d}");
        assert!(d.contains("+LINE 2500"), "{d}");
    }

    #[test]
    fn distant_changes_get_separate_hunks() {
        let old: String = (0..60).map(|i| format!("l{i}\n")).collect();
        let new = old.replace("l5\n", "L5\n").replace("l50\n", "L50\n");
        let d = unified(&old, &new, "f.txt");
        assert_eq!(d.matches("@@").count(), 4, "two hunks: {d}");
    }
}
