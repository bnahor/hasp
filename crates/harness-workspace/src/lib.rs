//! Workspace identity resolution and structural path confinement
//! (research/03-architecture.md §3).

use std::path::{Component, Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrustLevel {
    FirstUse,
    Trusted,
}

/// Canonical identity of the checkout a session is bound to. Resolved once at
/// session start, pinned in the session header, verified on resume.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceIdentity {
    pub root: PathBuf,
    pub is_linked_worktree: bool,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub trust: TrustLevel,
}

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    #[error("path escapes the workspace root: {0}")]
    EscapesRoot(String),
    #[error("absolute paths are not allowed: {0}")]
    Absolute(String),
    #[error("`..` components are not allowed: {0}")]
    ParentDir(String),
    #[error("{0}")]
    Io(String),
}

impl From<std::io::Error> for WorkspaceError {
    fn from(e: std::io::Error) -> Self {
        WorkspaceError::Io(e.to_string())
    }
}

#[derive(Clone)]
pub struct Workspace {
    identity: WorkspaceIdentity,
}

/// Counted at session start and compared on resume (world-state drift).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DirtyState {
    pub modified: usize,
    pub untracked: usize,
}

impl Workspace {
    pub fn resolve(cwd: &Path) -> Result<Self, WorkspaceError> {
        Ok(Self {
            identity: resolve_identity(cwd)?,
        })
    }

    pub fn identity(&self) -> &WorkspaceIdentity {
        &self.identity
    }

    /// Relative file paths under the root, for `@file` completion. Skips VCS,
    /// build and cache directories and stops after `limit` entries, so a huge
    /// tree cannot stall a keystroke.
    pub fn list_files(&self, limit: usize) -> Vec<String> {
        const SKIP: &[&str] = &[
            ".git",
            ".harness",
            "target",
            "node_modules",
            "dist",
            "build",
            "out",
            ".venv",
            "venv",
            "__pycache__",
            ".next",
            ".cache",
            ".mypy_cache",
            ".pytest_cache",
            "vendor",
        ];
        let root = self.root();
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                if out.len() >= limit {
                    out.sort();
                    return out;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                // `file_type` does not follow symlinks, so a link cannot start a
                // walk that loops.
                if kind.is_dir() {
                    if SKIP.contains(&name.as_str()) {
                        continue;
                    }
                    stack.push(entry.path());
                } else if kind.is_file()
                    && let Ok(rel) = entry.path().strip_prefix(root)
                {
                    out.push(rel.to_string_lossy().replace('\\', "/"));
                }
            }
        }
        out.sort();
        out
    }

    pub fn root(&self) -> &Path {
        &self.identity.root
    }

    /// Workspace-local state directory (`<root>/.harness`), created on demand.
    pub fn state_dir(&self) -> Result<PathBuf, WorkspaceError> {
        let dir = self.identity.root.join(".harness");
        std::fs::create_dir_all(&dir).map_err(WorkspaceError::from)?;
        Ok(dir)
    }

    pub fn dirty_state(&self) -> Option<DirtyState> {
        let out = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&self.identity.root)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let mut state = DirtyState {
            modified: 0,
            untracked: 0,
        };
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if line.starts_with("??") {
                state.untracked += 1;
            } else if !line.trim().is_empty() {
                state.modified += 1;
            }
        }
        Some(state)
    }

    /// Session-level worktree mode: create `<root>/.harness/worktrees/<name>`
    /// on branch `harness/<name>` from HEAD and resolve it as the new workspace.
    pub fn create_worktree(&self, name: &str) -> Result<Workspace, WorkspaceError> {
        let dir = self
            .identity
            .root
            .join(".harness")
            .join("worktrees")
            .join(name);
        if dir.exists() {
            return Err(WorkspaceError::Io(format!(
                "worktree {name} already exists"
            )));
        }
        let branch = format!("harness/{name}");
        let out = Command::new("git")
            .args(["worktree", "add", "-b", &branch, &dir.to_string_lossy()])
            .current_dir(&self.identity.root)
            .output()
            .map_err(|e| WorkspaceError::Io(e.to_string()))?;
        if !out.status.success() {
            return Err(WorkspaceError::Io(format!(
                "git worktree add failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Workspace::resolve(&dir)
    }

    /// Names of the worktrees this harness created (`.harness/worktrees/<name>`),
    /// sorted so listings are stable.
    pub fn child_worktrees(&self) -> Vec<String> {
        let dir = self.identity.root.join(".harness").join("worktrees");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        names
    }

    /// Whether a child worktree's branch is already contained in `HEAD` — i.e.
    /// removing the worktree cannot lose committed work. Uncommitted changes are
    /// a separate matter: `git worktree remove` refuses those on its own.
    pub fn worktree_merged(&self, name: &str) -> bool {
        let branch = format!("harness/{name}");
        Command::new("git")
            .args(["merge-base", "--is-ancestor", &branch, "HEAD"])
            .current_dir(&self.identity.root)
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false)
    }

    pub fn remove_worktree(&self, name: &str) -> Result<(), WorkspaceError> {
        let dir = self
            .identity
            .root
            .join(".harness")
            .join("worktrees")
            .join(name);
        let out = Command::new("git")
            .args(["worktree", "remove", &dir.to_string_lossy()])
            .current_dir(&self.identity.root)
            .output()
            .map_err(|e| WorkspaceError::Io(e.to_string()))?;
        if !out.status.success() {
            return Err(WorkspaceError::Io(format!(
                "git worktree remove failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(())
    }

    pub fn session_key(&self) -> String {
        self.identity
            .root
            .to_string_lossy()
            .replace(['/', '\\'], "-")
            .trim_start_matches('-')
            .to_string()
    }

    /// Structural confinement: relative paths only, no `..`, no symlink
    /// escape. Applies in every mode; approvals cannot lift it.
    pub fn confine(&self, rel: &str) -> Result<PathBuf, WorkspaceError> {
        let rel_path = Path::new(rel);
        if rel_path.is_absolute() {
            // Name the relative form when the path is inside the root, so the
            // model's retry is one call, not a guess.
            let hint = rel_path
                .strip_prefix(&self.identity.root)
                .ok()
                .filter(|r| !r.as_os_str().is_empty())
                .map(|r| format!(" (inside the project: use `{}`)", r.display()))
                .unwrap_or_default();
            return Err(WorkspaceError::Absolute(format!("{rel}{hint}")));
        }
        if rel_path
            .components()
            .any(|c| matches!(c, Component::ParentDir))
        {
            return Err(WorkspaceError::ParentDir(rel.to_string()));
        }
        let target = self.identity.root.join(rel_path);
        let canonical = if target.exists() {
            target.canonicalize().map_err(WorkspaceError::from)?
        } else {
            // New file: canonicalize the deepest existing ancestor, then
            // re-join the remaining components. Still catches symlink escapes:
            // an existing symlink component resolves outside the root.
            let mut ancestor = target.parent();
            while let Some(a) = ancestor {
                if a.exists() {
                    break;
                }
                ancestor = a.parent();
            }
            let base = ancestor.ok_or_else(|| WorkspaceError::EscapesRoot(rel.to_string()))?;
            let canonical_base = base.canonicalize().map_err(WorkspaceError::from)?;
            let suffix = target
                .strip_prefix(base)
                .map_err(|_| WorkspaceError::EscapesRoot(rel.to_string()))?;
            canonical_base.join(suffix)
        };
        if !canonical.starts_with(&self.identity.root) {
            return Err(WorkspaceError::EscapesRoot(rel.to_string()));
        }
        Ok(canonical)
    }
}

fn resolve_identity(cwd: &Path) -> Result<WorkspaceIdentity, WorkspaceError> {
    let cwd = cwd.canonicalize().map_err(WorkspaceError::from)?;
    let root = git(&cwd, &["rev-parse", "--show-toplevel"])
        .map(PathBuf::from)
        .unwrap_or(cwd);
    let root = root.canonicalize().map_err(WorkspaceError::from)?;
    let is_linked_worktree = match (
        git(&root, &["rev-parse", "--git-dir"]),
        git(&root, &["rev-parse", "--git-common-dir"]),
    ) {
        (Some(dir), Some(common)) => dir != common,
        _ => false,
    };
    let branch = git(&root, &["symbolic-ref", "--quiet", "--short", "HEAD"]);
    let head = git(&root, &["rev-parse", "HEAD"]).map(|h| h[..12.min(h.len())].to_string());
    Ok(WorkspaceIdentity {
        root,
        is_linked_worktree,
        branch,
        head,
        trust: TrustLevel::FirstUse,
    })
}

fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}

pub fn harness_home() -> Result<PathBuf, WorkspaceError> {
    let home =
        std::env::home_dir().ok_or_else(|| WorkspaceError::Io("no home directory".into()))?;
    let dir = home.join(".harness");
    std::fs::create_dir_all(&dir).map_err(WorkspaceError::from)?;
    Ok(dir)
}

pub fn sessions_root() -> Result<PathBuf, WorkspaceError> {
    harness_home().map(|h| h.join("sessions"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_plain_directory_without_git() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = Workspace::resolve(tmp.path()).unwrap();
        assert_eq!(ws.root(), tmp.path().canonicalize().unwrap());
        assert!(!ws.identity().is_linked_worktree);
        assert!(ws.identity().branch.is_none());
    }

    #[test]
    fn resolves_git_repository() {
        let tmp = tempfile::tempdir().unwrap();
        for args in [
            vec!["init", "--quiet"],
            vec!["config", "user.email", "t@t"],
            vec!["config", "user.name", "t"],
        ] {
            Command::new("git")
                .args(&args)
                .current_dir(tmp.path())
                .status()
                .unwrap();
        }
        std::fs::write(tmp.path().join("a.txt"), "x").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(tmp.path())
            .status()
            .unwrap();
        Command::new("git")
            .args(["commit", "--quiet", "-m", "init"])
            .current_dir(tmp.path())
            .status()
            .unwrap();
        let ws = Workspace::resolve(tmp.path()).unwrap();
        assert!(ws.identity().branch.is_some());
        assert!(ws.identity().head.is_some());
    }

    #[test]
    fn an_absolute_path_inside_the_root_is_refused_with_its_relative_form() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = Workspace::resolve(tmp.path()).unwrap();
        let inside = ws.root().join("app/regex.txt");
        let err = ws
            .confine(&inside.to_string_lossy())
            .unwrap_err()
            .to_string();
        assert!(err.contains("use `app/regex.txt`"), "{err}");
        let outside = ws.confine("/etc/passwd").unwrap_err().to_string();
        assert!(!outside.contains("use `"), "{outside}");
    }

    #[test]
    fn confinement_rejects_absolute_and_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = Workspace::resolve(tmp.path()).unwrap();
        assert!(matches!(
            ws.confine("/etc/passwd"),
            Err(WorkspaceError::Absolute(_))
        ));
        assert!(matches!(
            ws.confine("../x"),
            Err(WorkspaceError::ParentDir(_))
        ));
        assert!(ws.confine("src/main.rs").is_ok());
    }

    #[test]
    fn confinement_rejects_symlink_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), tmp.path().join("link")).unwrap();
        let ws = Workspace::resolve(tmp.path()).unwrap();
        assert!(matches!(
            ws.confine("link/file.txt"),
            Err(WorkspaceError::EscapesRoot(_))
        ));
    }

    #[test]
    fn dirty_state_counts_and_worktrees_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        for args in [
            vec!["init", "--quiet"],
            vec!["config", "user.email", "t@t"],
            vec!["config", "user.name", "t"],
        ] {
            Command::new("git")
                .args(&args)
                .current_dir(tmp.path())
                .status()
                .unwrap();
        }
        std::fs::write(tmp.path().join("a.txt"), "x").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(tmp.path())
            .status()
            .unwrap();
        Command::new("git")
            .args(["commit", "--quiet", "-m", "init"])
            .current_dir(tmp.path())
            .status()
            .unwrap();

        let ws = Workspace::resolve(tmp.path()).unwrap();
        assert_eq!(
            ws.dirty_state(),
            Some(DirtyState {
                modified: 0,
                untracked: 0
            })
        );
        std::fs::write(tmp.path().join("a.txt"), "changed").unwrap();
        std::fs::write(tmp.path().join("new.txt"), "x").unwrap();
        assert_eq!(
            ws.dirty_state(),
            Some(DirtyState {
                modified: 1,
                untracked: 1
            })
        );

        let wt = ws.create_worktree("feature").unwrap();
        assert!(
            wt.root()
                .to_string_lossy()
                .contains(".harness/worktrees/feature")
        );
        assert!(wt.identity().is_linked_worktree);
        assert_eq!(wt.identity().branch.as_deref(), Some("harness/feature"));
        ws.remove_worktree("feature").unwrap();
        assert!(
            !tmp.path()
                .join(".harness")
                .join("worktrees")
                .join("feature")
                .exists()
        );
    }

    #[test]
    fn lists_child_worktrees_and_says_whether_they_are_merged() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let git = |args: &[&str], cwd: &std::path::Path| {
            let ok = std::process::Command::new("git")
                .args(args)
                .current_dir(cwd)
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&["init", "--quiet"], dir);
        git(&["config", "user.email", "t@t"], dir);
        git(&["config", "user.name", "t"], dir);
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        git(&["add", "."], dir);
        git(&["commit", "--quiet", "-m", "init"], dir);

        let ws = Workspace::resolve(dir).unwrap();
        assert!(ws.child_worktrees().is_empty(), "nothing yet");

        let child = ws.create_worktree("sa-001-ab").unwrap();
        assert_eq!(ws.child_worktrees(), vec!["sa-001-ab"]);
        // A fresh worktree has no commits of its own: nothing to lose.
        assert!(ws.worktree_merged("sa-001-ab"));

        // Commit inside the child: now it holds work the root does not have.
        std::fs::write(child.root().join("b.txt"), "y").unwrap();
        git(&["add", "."], child.root());
        git(&["commit", "--quiet", "-m", "child work"], child.root());
        assert!(
            !ws.worktree_merged("sa-001-ab"),
            "unmerged work must be visible as such"
        );

        ws.remove_worktree("sa-001-ab").unwrap();
        assert!(ws.child_worktrees().is_empty(), "swept");
    }
}
