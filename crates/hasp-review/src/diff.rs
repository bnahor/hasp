//! The diff a reviewer reads, between two commits.
//!
//! git decides *which* files changed (its tree comparison is exact and fast);
//! the contents come out of one `git cat-file --batch` process and are diffed
//! by `hasp-diff`, so a review of an agent's 20,000-line change costs
//! milliseconds and never falls back to "diff too large to show".

use std::io::{Read as _, Write as _};
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

pub use hasp_diff::{Hunk, Kind, Stats};

use crate::git;

#[derive(Debug, Clone)]
pub struct FileDiff {
    pub path: String,
    /// "new file", "deleted", "binary …": what the hunks alone do not say.
    pub note: Option<String>,
    pub hunks: Vec<Hunk>,
    pub stats: Stats,
}

/// Lines of context around each change in a review.
pub const CONTEXT: u32 = 3;

/// Diff two texts as one file.
pub fn file(path: &str, old: &str, new: &str, note: Option<String>) -> FileDiff {
    FileDiff {
        path: path.to_string(),
        note,
        hunks: hasp_diff::hunks(old, new, CONTEXT),
        stats: hasp_diff::stats(old, new),
    }
}

/// Paths that differ between two commits, with git's status letter.
pub fn changed(dir: &Path, from: &str, to: &str) -> Result<Vec<(char, String)>> {
    let out = git::git(
        dir,
        &[
            "diff",
            "--no-renames",
            "--no-ext-diff",
            "--name-status",
            "-z",
            from,
            to,
            "--",
            ".",
            ":(exclude).hasp",
            ":(exclude).harness",
        ],
    )?;
    let mut parts = out.split('\0').filter(|s| !s.is_empty());
    let mut files = Vec::new();
    while let (Some(status), Some(path)) = (parts.next(), parts.next()) {
        files.push((status.chars().next().unwrap_or('M'), path.to_string()));
    }
    Ok(files)
}

/// Every changed file between `from` and `to`, diffed.
pub fn between(dir: &Path, from: &str, to: &str) -> Result<Vec<FileDiff>> {
    let files = changed(dir, from, to)?;
    let mut specs = Vec::with_capacity(files.len() * 2);
    for (_, path) in &files {
        specs.push(format!("{from}:{path}"));
        specs.push(format!("{to}:{path}"));
    }
    let mut blobs = cat_files(dir, &specs)?.into_iter();
    let mut out = Vec::with_capacity(files.len());
    for (status, path) in files {
        let old = blobs.next().flatten().unwrap_or_default();
        let new = blobs.next().flatten().unwrap_or_default();
        out.push(file_from_bytes(&path, status, &old, &new));
    }
    Ok(out)
}

fn file_from_bytes(path: &str, status: char, old: &[u8], new: &[u8]) -> FileDiff {
    let texts = (text(old), text(new));
    let (Some(o), Some(n)) = texts else {
        return FileDiff {
            path: path.to_string(),
            note: Some(format!("binary, {} → {} bytes", old.len(), new.len())),
            hunks: Vec::new(),
            stats: Stats::default(),
        };
    };
    let note = match status {
        'A' => Some("new file".to_string()),
        'D' => Some("deleted".to_string()),
        'T' => Some("type changed".to_string()),
        _ => None,
    };
    file(path, o, n, note)
}

/// UTF-8 without NULs, or `None` for binary content.
fn text(bytes: &[u8]) -> Option<&str> {
    if bytes[..bytes.len().min(8000)].contains(&0) {
        return None;
    }
    std::str::from_utf8(bytes).ok()
}

/// Read `<rev>:<path>` blobs in one git process. `None` where the object does
/// not exist (the file is absent on that side).
pub fn cat_files(dir: &Path, specs: &[String]) -> Result<Vec<Option<Vec<u8>>>> {
    if specs.is_empty() {
        return Ok(Vec::new());
    }
    let mut child = Command::new("git")
        .args(["cat-file", "--batch"])
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("running git cat-file")?;
    let mut stdin = child.stdin.take().context("git stdin")?;
    let input: String = specs.iter().map(|s| format!("{s}\n")).collect();
    // Written from a thread: git answers while we are still asking, and a
    // full stdout pipe would otherwise deadlock both sides.
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(input.as_bytes());
    });
    let mut raw = Vec::new();
    child
        .stdout
        .take()
        .context("git stdout")?
        .read_to_end(&mut raw)?;
    let _ = writer.join();
    child.wait()?;

    let mut out = Vec::with_capacity(specs.len());
    let mut at = 0;
    for spec in specs {
        let nl = raw[at..]
            .iter()
            .position(|&b| b == b'\n')
            .with_context(|| format!("git cat-file: no answer for {spec}"))?;
        let header = String::from_utf8_lossy(&raw[at..at + nl]).into_owned();
        at += nl + 1;
        let fields: Vec<&str> = header.split(' ').collect();
        match fields.as_slice() {
            [_, "blob", size] => {
                let size: usize = size.parse().context("git cat-file size")?;
                if at + size > raw.len() {
                    bail!("git cat-file: short read for {spec}");
                }
                out.push(Some(raw[at..at + size].to_vec()));
                at += size + 1;
            }
            // A submodule or directory: nothing a line diff can show.
            [_, _, size] => {
                let size: usize = size.parse().unwrap_or(0);
                out.push(None);
                at += size + 1;
            }
            _ => out.push(None), // "<spec> missing"
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        git::git(d, &["init", "-q"]).unwrap();
        for (k, v) in [("user.name", "t"), ("user.email", "t@t")] {
            git::git(d, &["config", k, v]).unwrap();
        }
        tmp
    }

    #[test]
    fn diffs_every_kind_of_change_between_commits() {
        let tmp = repo();
        let d = tmp.path();
        std::fs::write(d.join("keep.txt"), "one\ntwo\nthree\n").unwrap();
        std::fs::write(d.join("gone.txt"), "bye\n").unwrap();
        git::git(d, &["add", "-A"]).unwrap();
        git::git(d, &["commit", "-qm", "base"]).unwrap();
        let base = git::head(d).unwrap();

        std::fs::write(d.join("keep.txt"), "one\nTWO\nthree\n").unwrap();
        std::fs::remove_file(d.join("gone.txt")).unwrap();
        std::fs::write(d.join("new.txt"), "hello\n").unwrap();
        std::fs::write(d.join("img.bin"), [0u8, 1, 2, 3]).unwrap();
        let snap = git::snapshot(d, "s").unwrap();

        let files = between(d, &base, &snap).unwrap();
        let by = |p: &str| files.iter().find(|f| f.path == p).unwrap();
        assert_eq!(files.len(), 4);
        assert_eq!(
            by("keep.txt").stats,
            Stats {
                added: 1,
                removed: 1
            }
        );
        assert_eq!(by("gone.txt").note.as_deref(), Some("deleted"));
        assert_eq!(by("new.txt").note.as_deref(), Some("new file"));
        assert!(by("img.bin").note.as_deref().unwrap().starts_with("binary"));
    }
}
