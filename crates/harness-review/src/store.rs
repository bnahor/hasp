//! Reviews on disk: one append-only JSONL file per review under
//! `.harness/reviews/`. Like sessions, a review is never rewritten — every
//! decision is an event, and the current state is a fold over them.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// What a review is looking at.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Target {
    /// The main working tree.
    Workspace,
    /// A child worktree (`.harness/worktrees/<name>`, branch `harness/<name>`).
    Worktree { name: String },
}

impl Target {
    pub fn label(&self) -> String {
        match self {
            Target::Workspace => "workspace".into(),
            Target::Worktree { name } => format!("worktree {name} (branch harness/{name})"),
        }
    }
}

/// Which side of the diff a line comment is on: the new text, or a line the
/// change deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    New,
    Old,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Open {
        id: String,
        title: String,
        target: Target,
        /// What the change is measured against.
        base: String,
        at: u64,
    },
    /// A new state of the target, committed with [`crate::git::snapshot`].
    Snapshot {
        commit: String,
        at: u64,
    },
    /// The reviewer has seen everything up to this snapshot.
    Reviewed {
        commit: String,
        at: u64,
    },
    Comment {
        n: u32,
        path: Option<String>,
        line: Option<u32>,
        side: Side,
        quote: Option<String>,
        text: String,
        /// The snapshot the comment was written against; its line number is
        /// a line of this commit.
        commit: String,
        at: u64,
    },
    Resolve {
        n: u32,
        at: u64,
    },
    Verdict {
        verdict: String,
        commit: String,
        at: u64,
    },
    /// Comments handed to the agent as feedback.
    Sent {
        ns: Vec<u32>,
        at: u64,
    },
    /// The approved snapshot landed (merged or committed) as this commit.
    Landed {
        commit: String,
        at: u64,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Comment {
    pub n: u32,
    pub path: Option<String>,
    pub line: Option<u32>,
    pub side: Side,
    pub quote: Option<String>,
    pub text: String,
    pub commit: String,
    pub resolved: bool,
    pub sent: bool,
}

/// A review's current state.
#[derive(Debug, Clone, PartialEq)]
pub struct State {
    pub id: String,
    pub title: String,
    pub target: Target,
    pub base: String,
    pub opened: u64,
    pub snapshots: Vec<String>,
    pub reviewed: Option<String>,
    pub comments: Vec<Comment>,
    /// Latest verdict and the snapshot it was given on.
    pub verdict: Option<(String, String)>,
    pub landed: Option<String>,
}

impl State {
    pub fn latest(&self) -> &str {
        self.snapshots
            .last()
            .map(String::as_str)
            .unwrap_or(&self.base)
    }

    pub fn open_comments(&self) -> impl Iterator<Item = &Comment> {
        self.comments.iter().filter(|c| !c.resolved)
    }

    /// The verdict, if it was given on the latest snapshot — a verdict on an
    /// older state says nothing about what the agent did since.
    pub fn current_verdict(&self) -> Option<&str> {
        self.verdict
            .as_ref()
            .filter(|(_, at)| at == self.latest())
            .map(|(v, _)| v.as_str())
    }

    /// One word for listings.
    pub fn status(&self) -> &'static str {
        if self.landed.is_some() {
            return "landed";
        }
        match self.current_verdict() {
            Some("approve") => "approved",
            Some("changes") if self.reviewed.as_deref() == Some(self.latest()) => {
                "changes requested"
            }
            _ if self.reviewed.as_deref() != Some(self.latest()) => "needs review",
            _ if self.open_comments().next().is_some() => "open comments",
            _ => "reviewed",
        }
    }

    fn fold(events: &[Event]) -> Result<State> {
        let Some(Event::Open {
            id,
            title,
            target,
            base,
            at,
        }) = events.first()
        else {
            bail!("review file does not start with an open event");
        };
        let mut s = State {
            id: id.clone(),
            title: title.clone(),
            target: target.clone(),
            base: base.clone(),
            opened: *at,
            snapshots: Vec::new(),
            reviewed: None,
            comments: Vec::new(),
            verdict: None,
            landed: None,
        };
        for e in &events[1..] {
            match e {
                Event::Open { .. } => bail!("review file has a second open event"),
                Event::Snapshot { commit, .. } => s.snapshots.push(commit.clone()),
                Event::Reviewed { commit, .. } => s.reviewed = Some(commit.clone()),
                Event::Comment {
                    n,
                    path,
                    line,
                    side,
                    quote,
                    text,
                    commit,
                    ..
                } => s.comments.push(Comment {
                    n: *n,
                    path: path.clone(),
                    line: *line,
                    side: *side,
                    quote: quote.clone(),
                    text: text.clone(),
                    commit: commit.clone(),
                    resolved: false,
                    sent: false,
                }),
                Event::Resolve { n, .. } => {
                    if let Some(c) = s.comments.iter_mut().find(|c| c.n == *n) {
                        c.resolved = true;
                    }
                }
                Event::Verdict {
                    verdict, commit, ..
                } => s.verdict = Some((verdict.clone(), commit.clone())),
                Event::Sent { ns, .. } => {
                    for c in s.comments.iter_mut().filter(|c| ns.contains(&c.n)) {
                        c.sent = true;
                    }
                }
                Event::Landed { commit, .. } => s.landed = Some(commit.clone()),
            }
        }
        Ok(s)
    }
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The reviews of one workspace.
pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn new(root: &Path) -> Store {
        Store {
            dir: root.join(".harness").join("reviews"),
        }
    }

    fn file(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.jsonl"))
    }

    /// The next free id: `r1`, `r2`, …
    pub fn next_id(&self) -> String {
        let max = self
            .ids()
            .iter()
            .filter_map(|id| id.strip_prefix('r')?.parse::<u32>().ok())
            .max()
            .unwrap_or(0);
        format!("r{}", max + 1)
    }

    fn ids(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|e| {
                e.file_name()
                    .to_string_lossy()
                    .strip_suffix(".jsonl")
                    .map(String::from)
            })
            .collect()
    }

    /// Start a review file. Fails if the id is taken.
    pub fn create(&self, open: &Event) -> Result<()> {
        let Event::Open { id, .. } = open else {
            bail!("a review starts with an open event");
        };
        std::fs::create_dir_all(&self.dir)?;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.file(id))
            .with_context(|| format!("creating review {id}"))?;
        writeln!(f, "{}", serde_json::to_string(open)?)?;
        Ok(())
    }

    pub fn append(&self, id: &str, events: &[Event]) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        let mut buf = String::new();
        for e in events {
            buf.push_str(&serde_json::to_string(e)?);
            buf.push('\n');
        }
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(self.file(id))
            .with_context(|| format!("no review {id}"))?;
        // One write, so a crash leaves whole lines.
        f.write_all(buf.as_bytes())?;
        Ok(())
    }

    pub fn events(&self, id: &str) -> Result<Vec<Event>> {
        let text =
            std::fs::read_to_string(self.file(id)).with_context(|| format!("no review {id}"))?;
        let mut out = Vec::new();
        for (i, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str(line) {
                Ok(e) => out.push(e),
                // A torn last line (crash mid-write) is dropped, not fatal.
                Err(_) if i + 1 == text.lines().count() => break,
                Err(e) => bail!("review {id} line {}: {e}", i + 1),
            }
        }
        Ok(out)
    }

    pub fn load(&self, id: &str) -> Result<State> {
        State::fold(&self.events(id)?)
    }

    /// Every review, oldest first.
    pub fn list(&self) -> Vec<State> {
        let mut all: Vec<State> = self
            .ids()
            .iter()
            .filter_map(|id| self.load(id).ok())
            .collect();
        all.sort_by_key(|s| {
            (
                s.id.strip_prefix('r')
                    .and_then(|n| n.parse::<u32>().ok())
                    .unwrap_or(u32::MAX),
                s.id.clone(),
            )
        });
        all
    }

    /// The newest review of `target` that has not landed.
    pub fn open_for(&self, target: &Target) -> Option<State> {
        self.list()
            .into_iter()
            .rev()
            .find(|s| &s.target == target && s.landed.is_none())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_is_a_fold_over_appended_events() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path());
        assert_eq!(store.next_id(), "r1");
        store
            .create(&Event::Open {
                id: "r1".into(),
                title: "t".into(),
                target: Target::Workspace,
                base: "b".into(),
                at: 1,
            })
            .unwrap();
        assert_eq!(store.next_id(), "r2");
        store
            .append(
                "r1",
                &[
                    Event::Snapshot {
                        commit: "s1".into(),
                        at: 2,
                    },
                    Event::Comment {
                        n: 1,
                        path: Some("a.rs".into()),
                        line: Some(3),
                        side: Side::New,
                        quote: None,
                        text: "hm".into(),
                        commit: "s1".into(),
                        at: 3,
                    },
                    Event::Reviewed {
                        commit: "s1".into(),
                        at: 3,
                    },
                    Event::Verdict {
                        verdict: "changes".into(),
                        commit: "s1".into(),
                        at: 3,
                    },
                ],
            )
            .unwrap();
        let s = store.load("r1").unwrap();
        assert_eq!(s.status(), "changes requested");
        assert_eq!(s.open_comments().count(), 1);

        // The agent answers with a new state: the old verdict no longer applies.
        store
            .append(
                "r1",
                &[
                    Event::Sent { ns: vec![1], at: 4 },
                    Event::Snapshot {
                        commit: "s2".into(),
                        at: 5,
                    },
                ],
            )
            .unwrap();
        let s = store.load("r1").unwrap();
        assert_eq!(s.status(), "needs review");
        assert!(s.comments[0].sent);
        store
            .append(
                "r1",
                &[
                    Event::Resolve { n: 1, at: 6 },
                    Event::Reviewed {
                        commit: "s2".into(),
                        at: 6,
                    },
                    Event::Verdict {
                        verdict: "approve".into(),
                        commit: "s2".into(),
                        at: 6,
                    },
                ],
            )
            .unwrap();
        let s = store.load("r1").unwrap();
        assert_eq!(s.status(), "approved");
        assert_eq!(s.open_comments().count(), 0);
        assert_eq!(store.open_for(&Target::Workspace).unwrap().id, "r1");
    }

    #[test]
    fn a_torn_last_line_is_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path());
        store
            .create(&Event::Open {
                id: "r1".into(),
                title: "t".into(),
                target: Target::Workspace,
                base: "b".into(),
                at: 1,
            })
            .unwrap();
        let path = tmp.path().join(".harness/reviews/r1.jsonl");
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("{\"event\":\"snapsh");
        std::fs::write(&path, text).unwrap();
        assert!(store.load("r1").unwrap().snapshots.is_empty());
    }
}
