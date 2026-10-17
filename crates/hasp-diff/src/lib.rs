//! The harness's diff engine: one implementation for tool output, reviews and
//! the TUI, built for the diffs agents actually produce — whole files
//! rewritten, large refactors — without a size cap or a timeout.
//!
//! * **Lines: histogram diff when it is affordable, Myers when it is not**
//!   (`imara-diff`). Lines are interned to integers first, so comparisons are
//!   of ids, not strings. Histogram anchors on lines that are rare in the file
//!   (not `}` or blank lines), so moved and rewritten code splits the way a
//!   person reads it — but its cost grows with file length × number of
//!   separate changes (measured ≈1 ns per unit: a 200k-line file with 20k
//!   scattered edits took 4.2 s), which is exactly the shape of an agent's
//!   file-wide rename. Myers with git's heuristics stays near-linear (≈1 ms
//!   for that same diff). So Myers runs first, and histogram re-runs only
//!   within [`HISTOGRAM_BUDGET`]: every ordinary edit gets the readable diff,
//!   and no diff is ever slow. Either way the result is post-processed with
//!   git's indent heuristic, which slides each change to the most natural
//!   line boundary.
//! * **Words within a changed line: Myers** on word/space/punctuation tokens
//!   (histogram's rare-token anchoring is the wrong tool for small repeated
//!   tokens), so a one-token edit in a long line shows as that token.
//!
//! Nothing here truncates. Callers decide how much to *show* (a tool result
//! read by the model is capped; a review is not).

use std::ops::Range;

pub use imara_diff::Algorithm;
use imara_diff::{Diff, InternedInput};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Context,
    Add,
    Del,
}

/// One line of a hunk, with its 1-based number on each side it exists on.
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub kind: Kind,
    pub old: Option<u32>,
    pub new: Option<u32>,
    /// Without its line terminator.
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Hunk {
    /// 1-based start and length on each side, as in `@@ -a,b +c,d @@`.
    pub old_start: u32,
    pub old_len: u32,
    pub new_start: u32,
    pub new_len: u32,
    pub lines: Vec<Line>,
}

impl Hunk {
    pub fn header(&self) -> String {
        format!(
            "@@ -{},{} +{},{} @@",
            self.old_start, self.old_len, self.new_start, self.new_len
        )
    }
}

/// Line counts of a change.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    pub added: u32,
    pub removed: u32,
}

fn strip_eol(s: &str) -> &str {
    s.strip_suffix('\n')
        .map(|s| s.strip_suffix('\r').unwrap_or(s))
        .unwrap_or(s)
}

/// Largest (lines × separate changes) histogram is run on: ≈50 ms at the
/// measured ≈1 ns per unit. Above it the Myers result is kept.
pub const HISTOGRAM_BUDGET: u64 = 50_000_000;

/// The line diff, and which algorithm produced it.
fn line_diff(input: &InternedInput<&str>) -> (Diff, Algorithm) {
    let mut myers = Diff::compute(Algorithm::Myers, input);
    let units = (input.before.len() + input.after.len()) as u64 * myers.hunks().count() as u64;
    if units <= HISTOGRAM_BUDGET {
        let mut hist = Diff::compute(Algorithm::Histogram, input);
        hist.postprocess_lines(input);
        (hist, Algorithm::Histogram)
    } else {
        myers.postprocess_lines(input);
        (myers, Algorithm::Myers)
    }
}

/// Which algorithm [`hunks`] would use for these two texts.
pub fn algorithm_for(old: &str, new: &str) -> Algorithm {
    line_diff(&InternedInput::new(old, new)).1
}

/// The raw changed ranges (in line indices) between two texts.
fn changes(input: &InternedInput<&str>) -> Vec<(Range<u32>, Range<u32>)> {
    line_diff(input)
        .0
        .hunks()
        .map(|h| (h.before, h.after))
        .collect()
}

/// Where a line of `old` is in `new`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Moved {
    /// Unchanged, now at this (1-based) line.
    To(u32),
    /// The line itself was edited or removed; this is where its change is.
    Changed(u32),
}

/// Follow 1-based `line` of `old` into `new` — how a comment stays on the
/// code it was written about while the agent keeps editing the file.
pub fn map_line(old: &str, new: &str, line: u32) -> Moved {
    let at = line.saturating_sub(1);
    let mut shift: i64 = 0;
    for (b, a) in changes(&InternedInput::new(old, new)) {
        if b.start <= at && at < b.end {
            return Moved::Changed(a.start + 1);
        }
        if b.end > at {
            break;
        }
        // Wholly before the line (an insertion at the line counts): shift.
        shift += (a.end - a.start) as i64 - (b.end - b.start) as i64;
    }
    Moved::To((at as i64 + shift).max(0) as u32 + 1)
}

/// Added/removed line counts, without building hunks.
pub fn stats(old: &str, new: &str) -> Stats {
    let input = InternedInput::new(old, new);
    let diff = line_diff(&input).0;
    Stats {
        added: diff.count_additions(),
        removed: diff.count_removals(),
    }
}

/// Hunks with `context` unchanged lines around each change; changes closer
/// than twice that are merged into one hunk, as `diff -U` does.
pub fn hunks(old: &str, new: &str, context: u32) -> Vec<Hunk> {
    if old == new {
        return Vec::new();
    }
    let input = InternedInput::new(old, new);
    let changes = changes(&input);
    let before = |i: u32| strip_eol(input.interner[input.before[i as usize]]);
    let after = |i: u32| strip_eol(input.interner[input.after[i as usize]]);
    let (n_old, n_new) = (input.before.len() as u32, input.after.len() as u32);

    let mut out: Vec<Hunk> = Vec::new();
    let mut i = 0;
    while i < changes.len() {
        // Group changes whose gap is small enough to share context.
        let mut j = i;
        while j + 1 < changes.len() && changes[j + 1].0.start - changes[j].0.end <= 2 * context {
            j += 1;
        }
        let first = &changes[i];
        let last = &changes[j];
        let lead = context.min(first.0.start);
        let (mut o, mut n) = (first.0.start - lead, first.1.start - lead);
        let old_end = (last.0.end + context).min(n_old);
        let mut lines = Vec::new();
        for (before_r, after_r) in &changes[i..=j] {
            while o < before_r.start {
                lines.push(Line {
                    kind: Kind::Context,
                    old: Some(o + 1),
                    new: Some(n + 1),
                    text: before(o).to_string(),
                });
                o += 1;
                n += 1;
            }
            for k in before_r.clone() {
                lines.push(Line {
                    kind: Kind::Del,
                    old: Some(k + 1),
                    new: None,
                    text: before(k).to_string(),
                });
            }
            for k in after_r.clone() {
                lines.push(Line {
                    kind: Kind::Add,
                    old: None,
                    new: Some(k + 1),
                    text: after(k).to_string(),
                });
            }
            o = before_r.end;
            n = after_r.end;
        }
        while o < old_end && n < n_new {
            lines.push(Line {
                kind: Kind::Context,
                old: Some(o + 1),
                new: Some(n + 1),
                text: before(o).to_string(),
            });
            o += 1;
            n += 1;
        }
        let old_start = first.0.start - lead;
        let new_start = first.1.start - lead;
        let old_len = lines.iter().filter(|l| l.kind != Kind::Add).count() as u32;
        let new_len = lines.iter().filter(|l| l.kind != Kind::Del).count() as u32;
        out.push(Hunk {
            // `diff -U` convention: an empty side starts at the line before.
            old_start: if old_len == 0 {
                old_start
            } else {
                old_start + 1
            },
            old_len,
            new_start: if new_len == 0 {
                new_start
            } else {
                new_start + 1
            },
            new_len,
            lines,
        });
        i = j + 1;
    }
    out
}

/// A unified diff for a tool result. At most `max_lines` diff lines are
/// emitted (the model reads this); the header always gives the full size.
pub fn unified(old: &str, new: &str, path: &str, max_lines: usize) -> String {
    if old == new {
        return format!("{path}: unchanged");
    }
    let hs = hunks(old, new, 3);
    let (added, removed) =
        hs.iter()
            .flat_map(|h| &h.lines)
            .fold((0, 0), |(a, r), l| match l.kind {
                Kind::Add => (a + 1, r),
                Kind::Del => (a, r + 1),
                Kind::Context => (a, r),
            });
    let mut out = format!("--- {path}\n+++ {path}\n");
    let mut emitted = 0usize;
    'outer: for h in &hs {
        out.push_str(&h.header());
        out.push('\n');
        for l in &h.lines {
            if emitted >= max_lines {
                out.push_str(&format!(
                    "… diff truncated: {added} added, {removed} removed lines in total\n"
                ));
                break 'outer;
            }
            let mark = match l.kind {
                Kind::Add => '+',
                Kind::Del => '-',
                Kind::Context => ' ',
            };
            out.push(mark);
            out.push_str(&l.text);
            out.push('\n');
            emitted += 1;
        }
    }
    out
}

/// Word/space/punctuation tokens of one line.
fn word_tokens(s: &str) -> Vec<&str> {
    #[derive(PartialEq, Clone, Copy)]
    enum C {
        Word,
        Space,
        Other,
    }
    let class = |c: char| {
        if c.is_alphanumeric() || c == '_' {
            C::Word
        } else if c.is_whitespace() {
            C::Space
        } else {
            C::Other
        }
    };
    let mut out = Vec::new();
    let mut start = 0;
    let mut prev: Option<C> = None;
    for (i, c) in s.char_indices() {
        let k = class(c);
        // Punctuation is one token per character; words and spaces are runs.
        if let Some(p) = prev
            && (p != k || k == C::Other)
        {
            out.push(&s[start..i]);
            start = i;
        }
        prev = Some(k);
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// Byte ranges that differ between two versions of a line: the removed
/// spans of `old` and the added spans of `new`, for highlighting.
pub fn intraline(old: &str, new: &str) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    let (a, b) = (word_tokens(old), word_tokens(new));
    let mut input: InternedInput<&str> = InternedInput::default();
    input.update_before(a.iter().copied());
    input.update_after(b.iter().copied());
    let mut diff = Diff::compute(Algorithm::Myers, &input);
    diff.postprocess_no_heuristic(&input);
    let offsets = |toks: &[&str]| {
        let mut acc = Vec::with_capacity(toks.len() + 1);
        let mut o = 0;
        acc.push(0);
        for t in toks {
            o += t.len();
            acc.push(o);
        }
        acc
    };
    let (oa, ob) = (offsets(&a), offsets(&b));
    let mut del = Vec::new();
    let mut add = Vec::new();
    for h in diff.hunks() {
        if !h.before.is_empty() {
            del.push(oa[h.before.start as usize]..oa[h.before.end as usize]);
        }
        if !h.after.is_empty() {
            add.push(ob[h.after.start as usize]..ob[h.after.end as usize]);
        }
    }
    (del, add)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(hs: &[Hunk]) -> String {
        let mut s = String::new();
        for h in hs {
            s.push_str(&h.header());
            s.push('\n');
            for l in &h.lines {
                s.push(match l.kind {
                    Kind::Add => '+',
                    Kind::Del => '-',
                    Kind::Context => ' ',
                });
                s.push_str(&l.text);
                s.push('\n');
            }
        }
        s
    }

    #[test]
    fn matches_the_unified_format_git_produces() {
        let old = "a\nb\nc\nd\ne\nf\ng\nh\n";
        let new = "a\nb\nc\nD\ne\nf\ng\nh\nI\n";
        assert_eq!(
            render(&hunks(old, new, 3)),
            "@@ -1,8 +1,9 @@\n a\n b\n c\n-d\n+D\n e\n f\n g\n h\n+I\n"
        );
        // Far-apart changes get separate hunks with their own line numbers.
        let old: String = (1..=40).map(|i| format!("l{i}\n")).collect();
        let new = old.replace("l5\n", "L5\n").replace("l35\n", "L35\n");
        let hs = hunks(&old, &new, 3);
        assert_eq!(hs.len(), 2);
        assert_eq!(hs[1].header(), "@@ -32,7 +32,7 @@");
        let l = hs[1].lines.iter().find(|l| l.kind == Kind::Add).unwrap();
        assert_eq!((l.new, l.text.as_str()), (Some(35), "L35"));
    }

    #[test]
    fn pure_insertions_and_new_files_follow_the_empty_side_convention() {
        assert_eq!(render(&hunks("", "x\ny\n", 3)), "@@ -0,0 +1,2 @@\n+x\n+y\n");
        assert_eq!(render(&hunks("x\n", "", 3)), "@@ -1,1 +0,0 @@\n-x\n");
        assert!(hunks("same\n", "same\n", 3).is_empty());
    }

    #[test]
    fn histogram_keeps_a_moved_function_readable() {
        // Myers tends to match the braces and blank lines of two different
        // functions; histogram anchors on the unique lines and shows one
        // function removed and one added whole.
        let old = "fn a() {\n    one();\n}\n\nfn b() {\n    two();\n}\n";
        let new = "fn b() {\n    two();\n}\n\nfn a() {\n    one();\n}\n";
        let d = render(&hunks(old, new, 0));
        assert!(d.contains("-fn a() {\n-    one();\n-}\n-\n"), "{d}");
        assert!(!d.contains("-}\n+}"), "no brace-for-brace pairing: {d}");
    }

    #[test]
    fn a_large_rewrite_diffs_quickly_and_completely() {
        // 200k lines with 20k scattered changes — what the old quadratic
        // implementation refused ("too large to diff").
        let old: String = (0..200_000)
            .map(|i| format!("let v{i} = compute({i});\n"))
            .collect();
        let new: String = (0..200_000)
            .map(|i| {
                if i % 10 == 0 {
                    format!("let v{i} = compute_fast({i});\n")
                } else {
                    format!("let v{i} = compute({i});\n")
                }
            })
            .collect();
        let t = std::time::Instant::now();
        let s = stats(&old, &new);
        let hs = hunks(&old, &new, 3);
        let took = t.elapsed();
        assert_eq!(
            s,
            Stats {
                added: 20_000,
                removed: 20_000
            }
        );
        assert_eq!(
            hs.iter()
                .flat_map(|h| &h.lines)
                .filter(|l| l.kind == Kind::Add)
                .count(),
            20_000
        );
        assert!(took < std::time::Duration::from_secs(1), "{took:?}");
        assert_eq!(algorithm_for(&old, &new), Algorithm::Myers, "over budget");
    }

    #[test]
    fn a_line_is_followed_through_later_edits() {
        let old = "a\nb\nc\nd\ne\n";
        let new = "new\na\nB\nc\ne\n";
        assert_eq!(map_line(old, new, 1), Moved::To(2));
        assert_eq!(map_line(old, new, 2), Moved::Changed(3));
        assert_eq!(map_line(old, new, 3), Moved::To(4));
        assert!(matches!(map_line(old, new, 4), Moved::Changed(_)));
        assert_eq!(map_line(old, new, 5), Moved::To(5));
    }

    #[test]
    fn ordinary_edits_get_the_readable_algorithm() {
        let old: String = (0..2_000).map(|i| format!("line {i}\n")).collect();
        let new = old
            .replace("line 7\n", "changed\n")
            .replace("line 900\n", "x\n");
        assert_eq!(algorithm_for(&old, &new), Algorithm::Histogram);
        // A whole-file rewrite is one change, however large.
        let rewrite: String = (0..50_000).map(|i| format!("new {i}\n")).collect();
        let before: String = (0..50_000).map(|i| format!("old {i}\n")).collect();
        assert_eq!(algorithm_for(&before, &rewrite), Algorithm::Histogram);
    }

    #[test]
    fn unified_caps_what_it_shows_but_reports_the_whole() {
        let old: String = (0..500).map(|i| format!("{i}\n")).collect();
        let new: String = (0..500).map(|i| format!("x{i}\n")).collect();
        let u = unified(&old, &new, "f", 80);
        assert!(u.lines().count() <= 84, "{}", u.lines().count());
        assert!(u.contains("500 added, 500 removed lines in total"), "{u}");
        assert_eq!(unified("a\n", "a\n", "f", 80), "f: unchanged");
    }

    #[test]
    fn intraline_marks_only_the_changed_token() {
        let (del, add) = intraline(
            "    let total = price * qty;",
            "    let total = price * quantity;",
        );
        // "    let total = price * " is 24 bytes: `qty` / `quantity` follow.
        assert_eq!(del, vec![24..27]);
        assert_eq!(add, vec![24..32]);
        let (del, add) = intraline("same", "same");
        assert!(del.is_empty() && add.is_empty());
    }
}
