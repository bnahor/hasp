//! `harness review`: review agent work in your editor, send the comments back
//! to the agent, and land what you approved. The model lives in
//! `harness-review`; this is the command line around it.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use harness_review::store::{State, Target};
use harness_review::{Reviews, Verdict};

const USAGE: &str = "\
harness review                      list reviews
harness review new [title] [--worktree NAME] [--no-edit]
                                    open a review of the workspace (or a child
                                    worktree) and start reading it
harness review edit [rN] [--all]    read what changed since your last look in
                                    $EDITOR: comment with `>>`, resolve, set a verdict
harness review show [rN] [--all]    print the review document
harness review comment [rN] [--at PATH[:LINE]] TEXT
harness review resolve [rN] N
harness review approve [rN] [--land]
harness review changes [rN]         request changes (then `send`)
harness review send [rN] [--print] [--yolo]
                                    hand the open comments to the agent (runs
                                    `harness -p` in the target, continuing its session)
harness review land [rN]            commit (workspace) or merge (worktree) the
                                    approved snapshot — exactly what you approved

rN defaults to the newest review that has not landed.";

/// The repository root and review target for a directory: inside a child
/// worktree (`<root>/.harness/worktrees/<name>`) that worktree, else the
/// workspace.
pub fn locate(dir: &Path) -> Result<(PathBuf, Target)> {
    let ws = harness_workspace::Workspace::resolve(dir)?;
    let here = ws.root().to_path_buf();
    let parts: Vec<_> = here.components().collect();
    for i in 0..parts.len().saturating_sub(2) {
        if parts[i].as_os_str() == ".harness" && parts[i + 1].as_os_str() == "worktrees" {
            let root: PathBuf = parts[..i].iter().collect();
            let name = parts[i + 2].as_os_str().to_string_lossy().into_owned();
            return Ok((root, Target::Worktree { name }));
        }
    }
    Ok((here, Target::Workspace))
}

struct Args {
    id: Option<String>,
    flags: Vec<String>,
    values: std::collections::HashMap<String, String>,
    words: Vec<String>,
}

impl Args {
    fn parse(argv: &[String], with_value: &[&str]) -> Result<Args> {
        let mut a = Args {
            id: None,
            flags: Vec::new(),
            values: Default::default(),
            words: Vec::new(),
        };
        let mut it = argv.iter();
        while let Some(s) = it.next() {
            if with_value.contains(&s.as_str()) {
                let v = it.next().with_context(|| format!("{s} needs a value"))?;
                a.values.insert(s.clone(), v.clone());
            } else if s.starts_with("--") {
                a.flags.push(s.clone());
            } else if a.id.is_none()
                && a.words.is_empty()
                && s.len() > 1
                && s.starts_with('r')
                && s[1..].chars().all(|c| c.is_ascii_digit())
            {
                a.id = Some(s.clone());
            } else {
                a.words.push(s.clone());
            }
        }
        Ok(a)
    }

    fn has(&self, f: &str) -> bool {
        self.flags.iter().any(|x| x == f)
    }
}

pub async fn run(argv: &[String]) -> Result<()> {
    let (root, here) = locate(&std::env::current_dir()?)?;
    let reviews = Reviews::new(&root);
    let sub = argv.first().map(String::as_str);
    let rest = argv.get(1..).unwrap_or_default();
    match sub {
        None | Some("list") => list(&reviews),
        Some("new") => {
            let a = Args::parse(rest, &["--worktree"])?;
            let target = match a.values.get("--worktree") {
                Some(name) => Target::Worktree { name: name.clone() },
                None => here,
            };
            let title = if a.words.is_empty() {
                default_title(&reviews, &target)
            } else {
                a.words.join(" ")
            };
            let s = reviews.open(&title, target)?;
            println!(
                "opened review {} — {} ({})",
                s.id,
                s.title,
                s.target.label()
            );
            if a.has("--no-edit") {
                return Ok(());
            }
            edit(&reviews, &s.id, false)
        }
        Some("edit") => {
            let a = Args::parse(rest, &[])?;
            let s = reviews.pick(a.id.as_deref())?;
            edit(&reviews, &s.id, a.has("--all"))
        }
        Some("show") => {
            let a = Args::parse(rest, &[])?;
            let s = reviews.pick(a.id.as_deref())?;
            let (s, _) = reviews.refresh(&s.id)?;
            print!("{}", reviews.document(&s, a.has("--all"))?.text);
            Ok(())
        }
        Some("comment") => {
            let a = Args::parse(rest, &["--at"])?;
            let s = reviews.pick(a.id.as_deref())?;
            if a.words.is_empty() {
                bail!("usage: harness review comment [rN] [--at PATH[:LINE]] TEXT");
            }
            let (s, _) = reviews.refresh(&s.id)?;
            let (path, line) = match a.values.get("--at") {
                None => (None, None),
                Some(at) => match at.rsplit_once(':') {
                    Some((p, l)) if l.parse::<u32>().is_ok() => (Some(p), l.parse().ok()),
                    _ => (Some(at.as_str()), None),
                },
            };
            let n = reviews.comment(&s, path, line, &a.words.join(" "))?;
            println!("{}: comment #{n} added", s.id);
            Ok(())
        }
        Some("resolve") => {
            let a = Args::parse(rest, &[])?;
            let s = reviews.pick(a.id.as_deref())?;
            let n = a
                .words
                .first()
                .and_then(|w| w.trim_start_matches('#').parse().ok())
                .context("usage: harness review resolve [rN] N")?;
            reviews.resolve(&s, n)?;
            println!("{}: #{n} resolved", s.id);
            Ok(())
        }
        Some("approve") => {
            let a = Args::parse(rest, &[])?;
            let s = reviews.pick(a.id.as_deref())?;
            let (s, moved) = reviews.refresh(&s.id)?;
            if moved {
                bail!(
                    "{} changed since you last looked — `harness review edit {}` first",
                    s.target.label(),
                    s.id
                );
            }
            reviews.verdict(&s, Verdict::Approve)?;
            println!(
                "{}: approved at {}",
                s.id,
                harness_review::git::short(s.latest())
            );
            if a.has("--land") {
                land(&reviews, &s.id)?;
            }
            Ok(())
        }
        Some("changes") => {
            let a = Args::parse(rest, &[])?;
            let s = reviews.pick(a.id.as_deref())?;
            let (s, _) = reviews.refresh(&s.id)?;
            reviews.verdict(&s, Verdict::Changes)?;
            println!(
                "{}: changes requested — `harness review send` hands them to the agent",
                s.id
            );
            Ok(())
        }
        Some("send") => {
            let a = Args::parse(rest, &[])?;
            let s = reviews.pick(a.id.as_deref())?;
            send(&reviews, &s, a.has("--print"), a.has("--yolo"))
        }
        Some("land") => {
            let a = Args::parse(rest, &[])?;
            let s = reviews.pick(a.id.as_deref())?;
            land(&reviews, &s.id)
        }
        Some("-h" | "--help" | "help") => {
            println!("{USAGE}");
            Ok(())
        }
        Some(other) => bail!("unknown review subcommand: {other}\n\n{USAGE}"),
    }
}

fn default_title(reviews: &Reviews, target: &Target) -> String {
    let n = reviews
        .list()
        .iter()
        .filter(|s| &s.target == target)
        .count()
        + 1;
    match target {
        Target::Workspace => format!("workspace changes #{n}"),
        Target::Worktree { name } => format!("{name} #{n}"),
    }
}

fn list(reviews: &Reviews) -> Result<()> {
    let all = reviews.list();
    if all.is_empty() {
        println!("no reviews — `harness review new` starts one");
        return Ok(());
    }
    println!(
        "{:<5} {:<18} {:<5} {:<24} title",
        "id", "status", "open", "target"
    );
    for s in all {
        let target = match &s.target {
            Target::Workspace => "workspace".to_string(),
            Target::Worktree { name } => format!("worktree {name}"),
        };
        println!(
            "{:<5} {:<18} {:<5} {:<24} {}",
            s.id,
            s.status(),
            s.open_comments().count(),
            target,
            s.title
        );
    }
    Ok(())
}

/// Snapshot, open the document in `$EDITOR`, record what the reviewer did.
/// Returns the state afterwards, or `None` if the editor was abandoned.
pub fn review_in_editor(reviews: &Reviews, id: &str, all: bool) -> Result<Option<(State, String)>> {
    let (s, _) = reviews.refresh(id)?;
    let d = reviews.document(&s, all)?;
    let Some(edited) = crate::tui::edit_text(&d.text) else {
        return Ok(None);
    };
    let applied = reviews.apply(&s, &d, &edited)?;
    let s = reviews.load(id)?;
    let mut summary = format!("{}: ", s.id);
    let mut parts = Vec::new();
    if applied.comments > 0 {
        parts.push(format!(
            "{} new comment{}",
            applied.comments,
            plural(applied.comments)
        ));
    }
    if applied.resolved > 0 {
        parts.push(format!("{} resolved", applied.resolved));
    }
    if parts.is_empty() {
        parts.push("reviewed".into());
    }
    summary.push_str(&parts.join(", "));
    summary.push_str(&format!(
        " · {} · {} open",
        s.status(),
        s.open_comments().count()
    ));
    Ok(Some((s, summary)))
}

/// What `/review` in the TUI did.
pub struct TuiReview {
    pub summary: String,
    /// Open comments as a prompt, when the reviewer asked for changes.
    pub feedback: Option<String>,
}

/// `/review [--all] [new]` for the session rooted at `root`: continue the
/// open review of that target (opening one if there is none, or with `new`),
/// then read it in the editor.
pub fn tui_review(root: &Path, args: &str) -> Result<TuiReview> {
    let (repo, target) = locate(root)?;
    let reviews = Reviews::new(&repo);
    let words: Vec<&str> = args.split_whitespace().collect();
    let existing = reviews
        .list()
        .into_iter()
        .rev()
        .find(|s| s.target == target && s.landed.is_none());
    let s = match existing {
        Some(s) if !words.contains(&"new") => s,
        _ => {
            let title = default_title(&reviews, &target);
            reviews.open(&title, target)?
        }
    };
    let Some((s, summary)) = review_in_editor(&reviews, &s.id, words.contains(&"--all"))? else {
        return Ok(TuiReview {
            summary: format!("{}: editor exited without saving — nothing recorded", s.id),
            feedback: None,
        });
    };
    let mut feedback = None;
    if s.current_verdict() == Some("changes")
        && let Some((prompt, ns)) = reviews.feedback(&s)
    {
        reviews.mark_sent(&s, ns)?;
        feedback = Some(prompt);
    }
    let summary = match s.status() {
        "approved" => format!("{summary} — `harness review land {}` commits it", s.id),
        _ => summary,
    };
    Ok(TuiReview { summary, feedback })
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn edit(reviews: &Reviews, id: &str, all: bool) -> Result<()> {
    match review_in_editor(reviews, id, all)? {
        None => println!("editor exited without saving — nothing recorded"),
        Some((s, summary)) => {
            println!("{summary}");
            match s.status() {
                "approved" => println!("next: `harness review land {}`", s.id),
                _ if s.open_comments().any(|c| !c.sent) => {
                    println!(
                        "next: `harness review send {}` hands the comments to the agent",
                        s.id
                    )
                }
                _ => {}
            }
        }
    }
    Ok(())
}

fn send(reviews: &Reviews, s: &State, print_only: bool, yolo: bool) -> Result<()> {
    let Some((prompt, ns)) = reviews.feedback(s) else {
        println!("{}: no open comments to send", s.id);
        return Ok(());
    };
    if print_only {
        print!("{prompt}");
        return Ok(());
    }
    reviews.mark_sent(s, ns)?;
    let dir = reviews.target_dir(&s.target);
    println!(
        "{}: sending {} comment(s) to the agent in {}",
        s.id,
        s.open_comments().count(),
        dir.display()
    );
    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.current_dir(&dir).arg("-p").arg(&prompt);
    if yolo {
        cmd.arg("--yolo");
    }
    let status = cmd.status().context("running the agent")?;
    if !status.success() {
        bail!("the agent exited with {status}");
    }
    let (s, moved) = reviews.refresh(&s.id)?;
    if moved {
        println!(
            "{}: the agent made changes — `harness review edit {}` shows only those",
            s.id, s.id
        );
    } else {
        println!("{}: the agent changed no files", s.id);
    }
    Ok(())
}

fn land(reviews: &Reviews, id: &str) -> Result<()> {
    let s = reviews.load(id)?;
    let commit = reviews.land(&s)?;
    println!(
        "{}: landed as {} on {}",
        s.id,
        harness_review::git::short(&commit),
        harness_review::git::git(reviews.root(), &["rev-parse", "--abbrev-ref", "HEAD"])
            .unwrap_or_else(|_| "HEAD".into())
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_take_a_review_id_only_in_front() {
        let a = Args::parse(&["r3".into(), "--all".into()], &[]).unwrap();
        assert_eq!((a.id.as_deref(), a.has("--all")), (Some("r3"), true));
        let a = Args::parse(
            &["--at".into(), "a.rs:4".into(), "rename".into(), "r2".into()],
            &["--at"],
        )
        .unwrap();
        assert_eq!(a.id, None);
        assert_eq!(a.words, vec!["rename", "r2"]);
        assert_eq!(a.values["--at"], "a.rs:4");
    }

    #[test]
    fn a_child_worktree_is_its_own_target() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().canonicalize().unwrap();
        let git = |dir: &Path, args: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(dir)
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
        };
        git(&d, &["init", "-q"]);
        git(
            &d,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "x",
            ],
        );
        let wt = d.join(".harness/worktrees/feat");
        git(
            &d,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "harness/feat",
                &wt.to_string_lossy(),
            ],
        );
        assert_eq!(locate(&d).unwrap(), (d.clone(), Target::Workspace));
        assert_eq!(
            locate(&wt).unwrap(),
            (
                d.clone(),
                Target::Worktree {
                    name: "feat".into()
                }
            )
        );
    }
}
