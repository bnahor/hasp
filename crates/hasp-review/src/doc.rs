//! The review document: a diff you can edit in any editor.
//!
//! ```text
//! verdict: pending            # pending | approve | changes
//! >> a general comment goes before the first file
//!
//! == src/lib.rs
//! @@ -10,6 +10,8 @@ fn parse
//!     10 │     let x = 1;
//! +   11 │     let y = 2;
//! >> why y? use a constant
//! -   12 │     old();
//! << #3 open: an earlier comment (change `open` to `resolved` to close it)
//! ```
//!
//! Lines starting with `#` are instructions and ignored. A `>> ` line is a
//! new comment on the diff line above it (consecutive `>>` lines are one
//! comment); on the `== path` line it is about the whole file, before any file
//! it is about the whole change. Earlier comments appear as `<< #n open: …`;
//! changing `open` to `resolved` resolves them.

use crate::diff::{FileDiff, Kind};
use crate::store::{Comment, Side};

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Pending,
    Approve,
    Changes,
}

impl Verdict {
    pub fn label(&self) -> &'static str {
        match self {
            Verdict::Pending => "pending",
            Verdict::Approve => "approve",
            Verdict::Changes => "changes",
        }
    }
}

/// A comment typed into the document, anchored to what it was written under.
#[derive(Debug, Clone, PartialEq)]
pub struct NewComment {
    pub path: Option<String>,
    pub line: Option<u32>,
    pub side: Side,
    /// The diff line it was written under, for context in feedback.
    pub quote: Option<String>,
    pub text: String,
}

#[derive(Debug, Default, PartialEq)]
pub struct Parsed {
    pub verdict: Option<Verdict>,
    pub comments: Vec<NewComment>,
    pub resolved: Vec<u32>,
}

pub struct Header<'a> {
    pub id: &'a str,
    pub title: &'a str,
    pub target: String,
    pub showing: String,
    pub verdict: &'a str,
}

const GUTTER: char = '│';

/// Render `files` with `open` comments shown next to the lines they are on.
pub fn render(h: &Header<'_>, files: &[FileDiff], open: &[Comment]) -> String {
    let mut out = String::new();
    out.push_str(&format!("# hasp review {} — {}\n", h.id, h.title));
    out.push_str(&format!(
        "# target: {}\n# showing: {}\n",
        h.target, h.showing
    ));
    out.push_str(
        "#\n# Comment on a line by adding a line below it that starts with \">> \".\n\
         # \">> \" under a \"== file\" line is about that file; before the first file, about\n\
         # the whole change. Change \"open\" to \"resolved\" on a \"<< #n\" line to close it.\n\
         # Set the verdict (pending | approve | changes), save, and close the editor.\n\
         # Lines starting with \"#\" are ignored.\n#\n",
    );
    out.push_str(&format!("verdict: {}\n", h.verdict));
    let mut placed = vec![false; open.len()];
    for (i, c) in open.iter().enumerate() {
        if c.path.is_none() {
            out.push_str(&comment_line(c));
            placed[i] = true;
        }
    }
    if files.is_empty() {
        out.push_str("\n# (no changes to show)\n");
    }
    for f in files {
        out.push('\n');
        out.push_str(&format!("== {}", f.path));
        if let Some(note) = &f.note {
            out.push_str(&format!("  ({note})"));
        }
        out.push('\n');
        for (i, c) in open.iter().enumerate() {
            if !placed[i] && c.path.as_deref() == Some(&f.path) && c.line.is_none() {
                out.push_str(&comment_line(c));
                placed[i] = true;
            }
        }
        for hunk in &f.hunks {
            out.push_str(&hunk.header());
            out.push('\n');
            for l in &hunk.lines {
                let (mark, num, side) = match l.kind {
                    Kind::Add => ('+', l.new, Side::New),
                    Kind::Del => ('-', l.old, Side::Old),
                    Kind::Context => (' ', l.new, Side::New),
                };
                out.push_str(&format!(
                    "{mark}{:>5} {GUTTER} {}\n",
                    num.map(|n| n.to_string()).unwrap_or_default(),
                    l.text
                ));
                for (i, c) in open.iter().enumerate() {
                    if !placed[i]
                        && c.path.as_deref() == Some(&f.path)
                        && c.line == num
                        && c.side == side
                    {
                        out.push_str(&comment_line(c));
                        placed[i] = true;
                    }
                }
            }
        }
    }
    // Comments whose line is not in this diff (it did not change since you
    // last looked) are still open: list them so they are not forgotten.
    let rest: Vec<&Comment> = open
        .iter()
        .enumerate()
        .filter(|(i, _)| !placed[*i])
        .map(|(_, c)| c)
        .collect();
    if !rest.is_empty() {
        out.push_str("\n== (earlier comments on lines not shown here)\n");
        for c in rest {
            out.push_str(&comment_line(c));
        }
    }
    out
}

fn comment_line(c: &Comment) -> String {
    let at = match (&c.path, c.line) {
        (Some(p), Some(l)) => format!(" ({p}:{l})"),
        (Some(p), None) => format!(" ({p})"),
        _ => String::new(),
    };
    let mut s = format!("<< #{} open{at}: ", c.n);
    s.push_str(&c.text.replace('\n', "\n<<    "));
    s.push('\n');
    s
}

/// Read a (possibly edited) document back.
pub fn parse(doc: &str) -> Parsed {
    let mut p = Parsed::default();
    let mut path: Option<String> = None;
    // The line a new comment would attach to: (line, side, quote); `None`
    // after a `== file` header means "the whole file".
    let mut anchor: Option<(u32, Side, String)> = None;
    let mut pending: Option<NewComment> = None;
    let flush = |pending: &mut Option<NewComment>, p: &mut Parsed| {
        if let Some(c) = pending.take()
            && !c.text.trim().is_empty()
        {
            p.comments.push(c);
        }
    };
    for raw in doc.lines() {
        let line = raw.trim_end();
        if let Some(text) = line.strip_prefix(">>") {
            let text = text.strip_prefix(' ').unwrap_or(text);
            match &mut pending {
                Some(c) => {
                    c.text.push('\n');
                    c.text.push_str(text);
                }
                None => {
                    pending = Some(NewComment {
                        path: path.clone(),
                        line: anchor.as_ref().map(|a| a.0),
                        side: anchor.as_ref().map(|a| a.1).unwrap_or(Side::New),
                        quote: anchor.as_ref().map(|a| a.2.clone()),
                        text: text.to_string(),
                    })
                }
            }
            continue;
        }
        flush(&mut pending, &mut p);
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if let Some(v) = line.strip_prefix("verdict:") {
            let v = v
                .split('#')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            p.verdict = match v.as_str() {
                "approve" | "approved" | "lgtm" => Some(Verdict::Approve),
                "changes" | "request changes" | "reject" => Some(Verdict::Changes),
                "pending" | "" => Some(Verdict::Pending),
                _ => p.verdict.take(),
            };
            continue;
        }
        if let Some(rest) = line.strip_prefix("<< #") {
            let mut parts = rest.splitn(2, ' ');
            if let (Some(n), Some(tail)) = (parts.next(), parts.next())
                && tail.trim_start().starts_with("resolved")
                && let Ok(n) = n.parse()
            {
                p.resolved.push(n);
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("== ") {
            let name = rest.split("  (").next().unwrap_or(rest).trim();
            path = (!name.starts_with('(')).then(|| name.to_string());
            anchor = None;
            continue;
        }
        if line.starts_with("@@") {
            continue;
        }
        if let Some((gutter, text)) = line.split_once(GUTTER) {
            let mark = gutter.chars().next().unwrap_or(' ');
            if let Ok(n) = gutter[mark.len_utf8()..].trim().parse::<u32>() {
                let side = if mark == '-' { Side::Old } else { Side::New };
                let text = text.strip_prefix(' ').unwrap_or(text);
                anchor = Some((n, side, text.to_string()));
            }
        }
    }
    flush(&mut pending, &mut p);
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff;

    fn files() -> Vec<FileDiff> {
        vec![
            diff::file(
                "src/a.rs",
                "let x = 1;\nlet y = 2;\ndone();\n",
                "let x = 1;\nlet y = 3;\ndone();\n",
                None,
            ),
            diff::file("new.txt", "", "hello\n", Some("new file".into())),
        ]
    }

    fn old_comment(n: u32, path: Option<&str>, line: Option<u32>) -> Comment {
        Comment {
            n,
            path: path.map(String::from),
            line,
            side: Side::New,
            quote: None,
            text: format!("earlier {n}"),
            commit: "c".into(),
            resolved: false,
            sent: false,
        }
    }

    #[test]
    fn a_review_round_trips_through_the_document() {
        let files = files();
        let header = Header {
            id: "r-1",
            title: "t",
            target: "workspace".into(),
            showing: "everything".into(),
            verdict: "pending",
        };
        let open = vec![
            old_comment(1, Some("src/a.rs"), Some(2)),
            old_comment(2, Some("gone.rs"), Some(9)),
        ];
        let doc = render(&header, &files, &open);
        assert!(
            doc.contains("+    2 │ let y = 3;\n<< #1 open (src/a.rs:2): earlier 1"),
            "{doc}"
        );
        assert!(doc.contains("== new.txt  (new file)"), "{doc}");
        assert!(doc.contains("earlier comments on lines not shown"), "{doc}");

        // The reviewer comments, resolves one, and sets a verdict.
        let edited = doc
            .replace("verdict: pending", "verdict: changes")
            .replace(
                "+    2 │ let y = 3;\n",
                "+    2 │ let y = 3;\n>> why 3?\n>> say so in a comment\n",
            )
            .replace(
                "-    2 │ let y = 2;\n",
                "-    2 │ let y = 2;\n>> this was right\n",
            )
            .replace(
                "== new.txt  (new file)\n",
                "== new.txt  (new file)\n>> needed?\n",
            )
            .replace("<< #2 open", "<< #2 resolved")
            .replace(
                "verdict: changes\n",
                "verdict: changes\n>> overall: close\n",
            );
        let p = parse(&edited);
        assert_eq!(p.verdict, Some(Verdict::Changes));
        assert_eq!(p.resolved, vec![2]);
        assert_eq!(p.comments.len(), 4, "{:#?}", p.comments);
        assert_eq!(p.comments[0].path, None, "general");
        assert_eq!(p.comments[0].text, "overall: close");
        let old = &p.comments[1];
        assert_eq!(
            (old.path.as_deref(), old.line, old.side),
            (Some("src/a.rs"), Some(2), Side::Old)
        );
        let new = &p.comments[2];
        assert_eq!((new.line, new.side), (Some(2), Side::New));
        assert_eq!(new.text, "why 3?\nsay so in a comment");
        assert_eq!(new.quote.as_deref(), Some("let y = 3;"));
        let file = &p.comments[3];
        assert_eq!((file.path.as_deref(), file.line), (Some("new.txt"), None));
    }

    #[test]
    fn an_untouched_document_records_nothing() {
        let files = files();
        let header = Header {
            id: "r-1",
            title: "t",
            target: "w".into(),
            showing: "s".into(),
            verdict: "pending",
        };
        let p = parse(&render(&header, &files, &[]));
        assert_eq!(p.verdict, Some(Verdict::Pending));
        assert!(p.comments.is_empty() && p.resolved.is_empty());
    }
}
