//! The git plumbing reviews stand on. Every state a reviewer looks at is a
//! real git commit, so diffs between rounds are ordinary `git diff`s and
//! nothing about a review depends on the working tree staying still.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

pub fn git(dir: &Path, args: &[&str]) -> Result<String> {
    git_env(dir, args, &[])
}

fn git_env(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.args(args).current_dir(dir);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().context("running git")?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

/// Commit the working tree of `dir` as it is right now — tracked changes and
/// untracked files, minus ignored ones and the harness's own `.harness/` —
/// without touching the real index or any branch. Returns the commit id.
///
/// A temporary index does the work: seeded from HEAD, updated with
/// `add -A`, written out as a tree. The user's staging area never moves.
pub fn snapshot(dir: &Path, message: &str) -> Result<String> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let index = std::env::temp_dir().join(format!(
        "harness-review-index-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let idx = index.to_string_lossy().into_owned();
    let env = [("GIT_INDEX_FILE", idx.as_str())];
    let head = git(dir, &["rev-parse", "--verify", "-q", "HEAD"]).ok();
    let result = (|| {
        if head.is_some() {
            git_env(dir, &["read-tree", "HEAD"], &env)?;
        }
        git_env(dir, &["add", "-A", "--", "."], &env)?;
        // Removed after the add, not excluded in it: naming a gitignored
        // `.harness` in a pathspec makes `git add` fail.
        git_env(
            dir,
            &[
                "rm",
                "-r",
                "-q",
                "--cached",
                "--ignore-unmatch",
                "--",
                ".harness",
            ],
            &env,
        )?;
        let tree = git_env(dir, &["write-tree"], &env)?;
        let mut args = vec!["commit-tree", tree.as_str(), "-m", message];
        if let Some(h) = &head {
            args.extend(["-p", h.as_str()]);
        }
        git_env(
            dir,
            &args,
            &[
                ("GIT_AUTHOR_NAME", "harness"),
                ("GIT_AUTHOR_EMAIL", "harness@localhost"),
                ("GIT_COMMITTER_NAME", "harness"),
                ("GIT_COMMITTER_EMAIL", "harness@localhost"),
            ],
        )
    })();
    let _ = std::fs::remove_file(&index);
    result
}

/// Commit `tree` on top of HEAD as the user and move the current branch
/// there. The index is then reset to the new HEAD; the working tree is not
/// touched.
pub fn commit_tree_on_head(dir: &Path, tree: &str, message: &str) -> Result<String> {
    let head = head(dir)?;
    let commit = git(dir, &["commit-tree", tree, "-p", &head, "-m", message])?;
    git(
        dir,
        &[
            "update-ref",
            "-m",
            "harness review: land",
            "HEAD",
            &commit,
            &head,
        ],
    )?;
    git(dir, &["reset", "-q"])?;
    Ok(commit)
}

/// Keep `commit` alive (reachable, so `git gc` leaves it) under a review ref.
pub fn keep(dir: &Path, review: &str, label: &str, commit: &str) -> Result<()> {
    git(
        dir,
        &[
            "update-ref",
            &format!("refs/harness/reviews/{review}/{label}"),
            commit,
        ],
    )
    .map(|_| ())
}

/// Tree ids are what decide "has anything changed": two snapshots of the
/// same content are different commits with the same tree.
pub fn tree_of(dir: &Path, commit: &str) -> Result<String> {
    git(dir, &["rev-parse", &format!("{commit}^{{tree}}")])
}

pub fn head(dir: &Path) -> Result<String> {
    git(dir, &["rev-parse", "HEAD"])
}

pub fn short(id: &str) -> &str {
    &id[..id.len().min(10)]
}
