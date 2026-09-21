//! Line diff for tool output.
//!
//! A coding agent's most useful tool report is the change itself, so `write` and
//! `edit` return a compact unified diff. The model reads it, and the TUI colours
//! `+`/`-`/`@@` lines. Common prefix/suffix are trimmed before the LCS so the
//! quadratic step only ever sees the changed middle.

/// Lines of context kept around each change.
const CONTEXT: usize = 3;
/// Refuse to run the quadratic pass on more than this many cells.
const MAX_DP_CELLS: usize = 4_000_000;
/// Cap on emitted diff lines, so a rewrite cannot flood the transcript.
pub const MAX_DIFF_LINES: usize = 80;

enum Op<'a> {
    Keep(&'a str),
    Del(&'a str),
    Ins(&'a str),
}

/// Unified diff of two texts. `path` is only used for the file headers.
pub fn unified(old: &str, new: &str, path: &str) -> String {
    if old == new {
        return format!("{path}: unchanged");
    }
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();

    let mut pre = 0;
    while pre < a.len() && pre < b.len() && a[pre] == b[pre] {
        pre += 1;
    }
    let mut suf = 0;
    while suf < a.len() - pre && suf < b.len() - pre && a[a.len() - 1 - suf] == b[b.len() - 1 - suf]
    {
        suf += 1;
    }
    let mid_a = &a[pre..a.len() - suf];
    let mid_b = &b[pre..b.len() - suf];
    if mid_a.len().saturating_mul(mid_b.len()) > MAX_DP_CELLS {
        return format!(
            "--- {path}\n+++ {path}\n{} → {} lines (too large to diff)\n",
            a.len(),
            b.len()
        );
    }

    // Whole-file rows, so line numbers and surrounding context are correct.
    let mut rows: Vec<(char, &str, usize, usize)> = Vec::new();
    let (mut oldno, mut newno) = (1usize, 1usize);
    for line in &a[..pre] {
        rows.push((' ', line, oldno, newno));
        oldno += 1;
        newno += 1;
    }
    for op in lcs_ops(mid_a, mid_b) {
        match op {
            Op::Keep(t) => {
                rows.push((' ', t, oldno, newno));
                oldno += 1;
                newno += 1;
            }
            Op::Del(t) => {
                rows.push(('-', t, oldno, newno));
                oldno += 1;
            }
            Op::Ins(t) => {
                rows.push(('+', t, oldno, newno));
                newno += 1;
            }
        }
    }
    for line in &a[a.len() - suf..] {
        rows.push((' ', line, oldno, newno));
        oldno += 1;
        newno += 1;
    }

    let changed: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| r.0 != ' ')
        .map(|(i, _)| i)
        .collect();
    if changed.is_empty() {
        return format!("{path}: unchanged");
    }

    let mut out = format!("--- {path}\n+++ {path}\n");
    let mut emitted = 0usize;
    let mut i = 0usize;
    while i < changed.len() {
        let start = changed[i].saturating_sub(CONTEXT);
        let mut last = changed[i];
        let mut j = i;
        // Merge changes that are within two context blocks of each other.
        while j + 1 < changed.len() && changed[j + 1] - last <= 2 * CONTEXT {
            last = changed[j + 1];
            j += 1;
        }
        let end = (last + CONTEXT + 1).min(rows.len());
        let hunk = &rows[start..end];
        let old_count = hunk.iter().filter(|r| r.0 != '+').count();
        let new_count = hunk.iter().filter(|r| r.0 != '-').count();
        let old_start = hunk.first().map_or(1, |r| r.2);
        let new_start = hunk.first().map_or(1, |r| r.3);
        out.push_str(&format!(
            "@@ -{old_start},{old_count} +{new_start},{new_count} @@\n"
        ));
        let mut truncated = false;
        for row in hunk {
            if emitted >= MAX_DIFF_LINES {
                truncated = true;
                break;
            }
            out.push_str(&format!("{}{}\n", row.0, row.1));
            emitted += 1;
        }
        if truncated {
            out.push_str("… diff truncated\n");
            break;
        }
        i = j + 1;
    }
    out.trim_end().to_string()
}

/// Longest common subsequence walk, producing keep/delete/insert ops.
fn lcs_ops<'a>(a: &[&'a str], b: &[&'a str]) -> Vec<Op<'a>> {
    let (n, m) = (a.len(), b.len());
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if a[i] == b[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (0usize, 0usize);
    let mut ops = Vec::with_capacity(n + m);
    while i < n && j < m {
        if a[i] == b[j] {
            ops.push(Op::Keep(a[i]));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            ops.push(Op::Del(a[i]));
            i += 1;
        } else {
            ops.push(Op::Ins(b[j]));
            j += 1;
        }
    }
    while i < n {
        ops.push(Op::Del(a[i]));
        i += 1;
    }
    while j < m {
        ops.push(Op::Ins(b[j]));
        j += 1;
    }
    ops
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
