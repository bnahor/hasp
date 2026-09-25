//! Reviews of agent work, done in your editor.
//!
//! A review follows one target — the main working tree, or a child worktree an
//! agent works in — through rounds:
//!
//! 1. Every look starts by committing the target as it is now (a *snapshot*,
//!    made with a private index, so the user's staging area and branches never
//!    move). Snapshots with the same tree are the same state.
//! 2. The reviewer reads a document (see [`doc`]) of only what changed since
//!    the snapshot they last reviewed — not the whole change again — adds
//!    `>>` comments under any line, resolves earlier ones, sets a verdict.
//! 3. Open comments go back to the agent as feedback. Each comment stays on
//!    the code it was written about as the file changes under it.
//! 4. An approved snapshot lands: committed (workspace) or merged (worktree),
//!    exactly as approved — never newer, unreviewed work.
//!
//! State is an append-only event log per review (see [`store`]); every
//! snapshot is kept alive by a ref under `refs/harness/reviews/`.

pub mod diff;
pub mod doc;
pub mod git;
pub mod store;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::diff::FileDiff;
use crate::store::{Comment, Event, Side, State, Store, Target, now};

pub use crate::doc::Verdict;

/// The reviews of one workspace (a git repository root).
pub struct Reviews {
    root: PathBuf,
    store: Store,
}

/// A rendered review document and what it shows.
pub struct Document {
    pub text: String,
    pub from: String,
    pub to: String,
    pub files: Vec<FileDiff>,
}

/// What applying an edited document recorded.
#[derive(Debug, Default, PartialEq)]
pub struct Applied {
    pub comments: usize,
    pub resolved: usize,
    pub verdict: Option<Verdict>,
}

impl Reviews {
    pub fn new(root: &Path) -> Reviews {
        Reviews {
            root: root.to_path_buf(),
            store: Store::new(root),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The directory holding a target's files.
    pub fn target_dir(&self, t: &Target) -> PathBuf {
        match t {
            Target::Workspace => self.root.clone(),
            Target::Worktree { name } => self.root.join(".harness").join("worktrees").join(name),
        }
    }

    pub fn list(&self) -> Vec<State> {
        self.store.list()
    }

    pub fn load(&self, id: &str) -> Result<State> {
        self.store.load(id)
    }

    /// `id`, or else the newest review that has not landed.
    pub fn pick(&self, id: Option<&str>) -> Result<State> {
        match id {
            Some(id) => self.load(id),
            None => self
                .list()
                .into_iter()
                .rev()
                .find(|s| s.landed.is_none())
                .context("no open review — start one with `harness review new`"),
        }
    }

    /// Open a review of `target`, measured from where it diverged.
    pub fn open(&self, title: &str, target: Target) -> Result<State> {
        let dir = self.target_dir(&target);
        if !dir.is_dir() {
            bail!("{} does not exist", dir.display());
        }
        let base = match &target {
            Target::Workspace => git::head(&self.root)
                .context("reviews need a commit to measure from — commit once first")?,
            Target::Worktree { name } => git::git(
                &self.root,
                &["merge-base", "HEAD", &format!("harness/{name}")],
            )
            .with_context(|| format!("finding where harness/{name} diverged"))?,
        };
        let id = self.store.next_id();
        git::keep(&self.root, &id, "base", &base)?;
        self.store.create(&Event::Open {
            id: id.clone(),
            title: title.to_string(),
            target,
            base,
            at: now(),
        })?;
        Ok(self.refresh(&id)?.0)
    }

    /// Snapshot the target; record it if it differs from the latest state.
    /// Returns the state and whether anything new was recorded.
    pub fn refresh(&self, id: &str) -> Result<(State, bool)> {
        let s = self.load(id)?;
        if s.landed.is_some() {
            return Ok((s, false));
        }
        let dir = self.target_dir(&s.target);
        let commit = git::snapshot(&dir, &format!("harness review {id}: {}", s.title))?;
        if git::tree_of(&self.root, &commit)? == git::tree_of(&self.root, s.latest())? {
            return Ok((s, false));
        }
        let label = format!("s{}", s.snapshots.len() + 1);
        git::keep(&self.root, id, &label, &commit)?;
        self.store
            .append(id, &[Event::Snapshot { commit, at: now() }])?;
        Ok((self.load(id)?, true))
    }

    /// The document for the next look: what changed since the last reviewed
    /// snapshot, or (with `all`, or on the first look) since the base.
    pub fn document(&self, s: &State, all: bool) -> Result<Document> {
        let to = s.latest().to_string();
        let from = match (&s.reviewed, all) {
            (Some(r), false) => r.clone(),
            _ => s.base.clone(),
        };
        let files = if from == to {
            Vec::new()
        } else {
            diff::between(&self.root, &from, &to)?
        };
        let showing = if from == s.base {
            format!(
                "everything since {} ({})",
                git::short(&from),
                summary(&files)
            )
        } else if from == to {
            format!(
                "nothing new since your last review of {} — `--all` shows the whole change",
                git::short(&to)
            )
        } else {
            format!(
                "only what changed since your last review: {} → {} ({})",
                git::short(&from),
                git::short(&to),
                summary(&files)
            )
        };
        let open = self.follow(s, &from, &to)?;
        let verdict = s.current_verdict().unwrap_or("pending").to_string();
        let text = doc::render(
            &doc::Header {
                id: &s.id,
                title: &s.title,
                target: s.target.label(),
                showing,
                verdict: &verdict,
            },
            &files,
            &open,
        );
        Ok(Document {
            text,
            from,
            to,
            files,
        })
    }

    /// Open comments, with their lines moved to where that code is in `to`.
    fn follow(&self, s: &State, from: &str, to: &str) -> Result<Vec<Comment>> {
        let open: Vec<Comment> = s.open_comments().cloned().collect();
        let moving: Vec<&Comment> = open
            .iter()
            .filter(|c| c.side == Side::New && c.line.is_some() && c.commit != to)
            .collect();
        let mut specs = Vec::new();
        for c in &moving {
            let p = c.path.as_deref().unwrap_or_default();
            specs.push(format!("{}:{p}", c.commit));
            specs.push(format!("{to}:{p}"));
        }
        let blobs = diff::cat_files(&self.root, &specs)?;
        let mut followed = std::collections::HashMap::new();
        for (i, c) in moving.iter().enumerate() {
            let text =
                |b: &Option<Vec<u8>>| b.as_ref().map(|b| String::from_utf8_lossy(b).into_owned());
            let moved = match (text(&blobs[2 * i]), text(&blobs[2 * i + 1])) {
                (Some(old), Some(new)) => {
                    Some(harness_diff::map_line(&old, &new, c.line.unwrap_or(1)))
                }
                _ => None,
            };
            followed.insert(c.n, moved);
        }
        Ok(open
            .into_iter()
            .map(|mut c| {
                match followed.get(&c.n) {
                    Some(Some(harness_diff::Moved::To(l))) => c.line = Some(*l),
                    Some(Some(harness_diff::Moved::Changed(l))) => {
                        c.line = Some(*l);
                        c.text = format!("(this line has changed since) {}", c.text);
                    }
                    // The file is gone: the comment is about the file now.
                    Some(None) => c.line = None,
                    None => {}
                }
                // A deleted line can only be shown against the text it was
                // deleted from.
                if c.side == Side::Old && c.commit != from {
                    c.line = None;
                }
                c
            })
            .collect())
    }

    /// Record an edited document: new comments, resolutions, the verdict, and
    /// that everything in it has now been reviewed.
    pub fn apply(&self, s: &State, d: &Document, edited: &str) -> Result<Applied> {
        let parsed = doc::parse(edited);
        let at = now();
        let first = s.comments.iter().map(|c| c.n).max().unwrap_or(0) + 1;
        let mut events = Vec::new();
        let mut applied = Applied::default();
        for (n, c) in (first..).zip(parsed.comments) {
            applied.comments += 1;
            events.push(Event::Comment {
                n,
                path: c.path,
                line: c.line,
                side: c.side,
                quote: c.quote,
                text: c.text,
                commit: if c.side == Side::Old {
                    d.from.clone()
                } else {
                    d.to.clone()
                },
                at,
            });
        }
        for r in parsed.resolved {
            if s.open_comments().any(|c| c.n == r) {
                applied.resolved += 1;
                events.push(Event::Resolve { n: r, at });
            }
        }
        events.push(Event::Reviewed {
            commit: d.to.clone(),
            at,
        });
        if let Some(v) = parsed.verdict.filter(|v| *v != Verdict::Pending) {
            events.push(Event::Verdict {
                verdict: v.label().into(),
                commit: d.to.clone(),
                at,
            });
            applied.verdict = Some(v);
        }
        self.store.append(&s.id, &events)?;
        Ok(applied)
    }

    /// Comment on the latest snapshot without opening the document.
    pub fn comment(
        &self,
        s: &State,
        path: Option<&str>,
        line: Option<u32>,
        text: &str,
    ) -> Result<u32> {
        let n = s.comments.iter().map(|c| c.n).max().unwrap_or(0) + 1;
        self.store.append(
            &s.id,
            &[Event::Comment {
                n,
                path: path.map(String::from),
                line,
                side: Side::New,
                quote: None,
                text: text.to_string(),
                commit: s.latest().to_string(),
                at: now(),
            }],
        )?;
        Ok(n)
    }

    pub fn resolve(&self, s: &State, n: u32) -> Result<()> {
        if !s.open_comments().any(|c| c.n == n) {
            bail!("review {} has no open comment #{n}", s.id);
        }
        self.store.append(&s.id, &[Event::Resolve { n, at: now() }])
    }

    /// Give a verdict on the latest snapshot (which counts as reviewing it).
    pub fn verdict(&self, s: &State, v: Verdict) -> Result<()> {
        let at = now();
        let commit = s.latest().to_string();
        self.store.append(
            &s.id,
            &[
                Event::Reviewed {
                    commit: commit.clone(),
                    at,
                },
                Event::Verdict {
                    verdict: v.label().into(),
                    commit,
                    at,
                },
            ],
        )
    }

    /// The open comments as a prompt for the agent, and their numbers.
    pub fn feedback(&self, s: &State) -> Option<(String, Vec<u32>)> {
        let open: Vec<&Comment> = s.open_comments().collect();
        if open.is_empty() {
            return None;
        }
        let mut out = format!(
            "Review {} of your work (\"{}\"): the reviewer {}. Address every comment below \
             — change the code, or answer if you disagree — then stop so they can look again.\n",
            s.id,
            s.title,
            if s.current_verdict() == Some("changes") {
                "requests changes"
            } else {
                "left comments"
            }
        );
        for c in &open {
            out.push_str(&format!("\n#{}", c.n));
            match (&c.path, c.line) {
                (Some(p), Some(l)) => {
                    let side = if c.side == Side::Old {
                        " (a line you removed)"
                    } else {
                        ""
                    };
                    out.push_str(&format!(" {p}:{l}{side}"));
                }
                (Some(p), None) => out.push_str(&format!(" {p}")),
                _ => out.push_str(" (the change as a whole)"),
            }
            if c.sent {
                out.push_str(" — still open from an earlier round");
            }
            out.push('\n');
            if let Some(q) = &c.quote {
                out.push_str(&format!("    > {}\n", q.trim()));
            }
            for l in c.text.lines() {
                out.push_str(&format!("    {l}\n"));
            }
        }
        Some((out, open.iter().map(|c| c.n).collect()))
    }

    pub fn mark_sent(&self, s: &State, ns: Vec<u32>) -> Result<()> {
        self.store.append(&s.id, &[Event::Sent { ns, at: now() }])
    }

    /// Land the approved snapshot: commit it in the workspace, or merge it
    /// from the worktree. Refuses if the target moved on since the approval,
    /// so only what was reviewed ever lands.
    pub fn land(&self, s: &State) -> Result<String> {
        if s.landed.is_some() {
            bail!("review {} has already landed", s.id);
        }
        if s.current_verdict() != Some("approve") {
            bail!("review {} is not approved (status: {})", s.id, s.status());
        }
        let (fresh, moved) = self.refresh(&s.id)?;
        if moved {
            bail!(
                "the {} changed after you approved it — review the new state first",
                fresh.target.label()
            );
        }
        let approved = s.latest();
        let message = format!("{}\n\nReviewed-in: harness review {}", s.title, s.id);
        let commit = match &s.target {
            Target::Workspace => {
                // The approved snapshot sits on the HEAD it was taken from;
                // if HEAD moved since, what was reviewed is not what would land.
                let parent = git::git(&self.root, &["rev-parse", &format!("{approved}^")])?;
                if parent != git::head(&self.root)? {
                    bail!("HEAD moved since the review began — open a new review of the workspace");
                }
                let tree = git::tree_of(&self.root, approved)?;
                git::commit_tree_on_head(&self.root, &tree, &message)?
            }
            Target::Worktree { .. } => {
                git::git(
                    &self.root,
                    &["merge", "--no-ff", "-q", "-m", &message, approved],
                )?;
                git::head(&self.root)?
            }
        };
        self.store.append(
            &s.id,
            &[Event::Landed {
                commit: commit.clone(),
                at: now(),
            }],
        )?;
        Ok(commit)
    }
}

fn summary(files: &[FileDiff]) -> String {
    let (a, r) = files
        .iter()
        .fold((0, 0), |(a, r), f| (a + f.stats.added, r + f.stats.removed));
    format!(
        "{} file{}, +{a} −{r}",
        files.len(),
        if files.len() == 1 { "" } else { "s" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        git::git(d, &["init", "-q", "-b", "main"]).unwrap();
        for (k, v) in [("user.name", "t"), ("user.email", "t@t")] {
            git::git(d, &["config", k, v]).unwrap();
        }
        std::fs::write(d.join(".gitignore"), ".harness/\n").unwrap();
        std::fs::write(d.join("a.rs"), "fn a() {\n    one();\n}\n").unwrap();
        git::git(d, &["add", "-A"]).unwrap();
        git::git(d, &["commit", "-qm", "base"]).unwrap();
        tmp
    }

    #[test]
    fn a_full_review_round_of_the_workspace() {
        let tmp = repo();
        let d = tmp.path();
        let reviews = Reviews::new(d);
        std::fs::write(d.join("a.rs"), "fn a() {\n    two();\n}\n").unwrap();
        let s = reviews.open("swap one for two", Target::Workspace).unwrap();
        assert_eq!(s.id, "r1");
        assert_eq!(s.status(), "needs review");

        // First look: everything, with a comment and a request for changes.
        let d1 = reviews.document(&s, false).unwrap();
        assert!(d1.text.contains("+    2 │     two();"), "{}", d1.text);
        let edited = d1
            .text
            .replace("verdict: pending", "verdict: changes")
            .replace(
                "+    2 │     two();\n",
                "+    2 │     two();\n>> call three() instead\n",
            );
        let applied = reviews.apply(&s, &d1, &edited).unwrap();
        assert_eq!(applied.comments, 1);
        let s = reviews.load("r1").unwrap();
        assert_eq!(s.status(), "changes requested");

        // Feedback for the agent quotes the line.
        let (prompt, ns) = reviews.feedback(&s).unwrap();
        assert!(
            prompt.contains("#1 a.rs:2") && prompt.contains("> two();"),
            "{prompt}"
        );
        reviews.mark_sent(&s, ns).unwrap();

        // The agent inserts a line above and edits the commented one.
        std::fs::write(d.join("a.rs"), "// doc\nfn a() {\n    three();\n}\n").unwrap();
        let (s, moved) = reviews.refresh("r1").unwrap();
        assert!(moved);
        assert_eq!(s.status(), "needs review");
        let d2 = reviews.document(&s, false).unwrap();
        assert!(
            d2.text.contains("only what changed since your last review"),
            "{}",
            d2.text
        );
        assert!(
            !d2.text.contains("-    2 │     one();"),
            "the first round is not repeated: {}",
            d2.text
        );
        assert!(
            d2.text.contains("+    3 │     three();\n<< #1 open (a.rs:3): (this line has changed since) call three() instead"),
            "{}",
            d2.text
        );

        // Resolve and approve; then land it.
        let edited = d2
            .text
            .replace("<< #1 open", "<< #1 resolved")
            .replace("verdict: pending", "verdict: approve");
        let applied = reviews.apply(&s, &d2, &edited).unwrap();
        assert_eq!(
            (applied.resolved, applied.verdict),
            (1, Some(Verdict::Approve))
        );
        let s = reviews.load("r1").unwrap();
        assert_eq!(s.status(), "approved");
        assert!(reviews.feedback(&s).is_none());
        let landed = reviews.land(&s).unwrap();
        assert_eq!(git::head(d).unwrap(), landed);
        assert!(
            git::git(d, &["log", "-1", "--format=%B"])
                .unwrap()
                .contains("Reviewed-in: harness review r1")
        );
        assert_eq!(reviews.load("r1").unwrap().status(), "landed");
    }

    #[test]
    fn nothing_lands_that_was_not_approved() {
        let tmp = repo();
        let d = tmp.path();
        let reviews = Reviews::new(d);
        std::fs::write(d.join("a.rs"), "changed\n").unwrap();
        let s = reviews.open("t", Target::Workspace).unwrap();
        assert!(reviews.land(&s).is_err(), "not approved");
        reviews.verdict(&s, Verdict::Approve).unwrap();
        // The agent keeps working after the approval.
        std::fs::write(d.join("a.rs"), "changed again\n").unwrap();
        let s = reviews.load("r1").unwrap();
        let err = reviews.land(&s).unwrap_err().to_string();
        assert!(err.contains("changed after you approved"), "{err}");
        assert_eq!(reviews.load("r1").unwrap().status(), "needs review");
    }

    #[test]
    fn a_worktree_review_merges_exactly_the_approved_snapshot() {
        let tmp = repo();
        let d = tmp.path();
        let wt = d.join(".harness/worktrees/feat");
        git::git(
            d,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "harness/feat",
                &wt.to_string_lossy(),
            ],
        )
        .unwrap();
        // Uncommitted work in the child counts: the snapshot captures it.
        std::fs::write(wt.join("b.rs"), "new\n").unwrap();
        let reviews = Reviews::new(d);
        let s = reviews
            .open(
                "feature",
                Target::Worktree {
                    name: "feat".into(),
                },
            )
            .unwrap();
        let doc = reviews.document(&s, false).unwrap();
        assert!(doc.text.contains("== b.rs  (new file)"), "{}", doc.text);
        reviews.verdict(&s, Verdict::Approve).unwrap();
        let s = reviews.load("r1").unwrap();
        reviews.land(&s).unwrap();
        assert_eq!(std::fs::read_to_string(d.join("b.rs")).unwrap(), "new\n");
    }
}
