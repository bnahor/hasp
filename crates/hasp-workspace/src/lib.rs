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
            STATE_DIR,
            LEGACY_STATE_DIR,
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

    /// Workspace-local state directory (`<root>/.hasp`), created on demand.
    pub fn state_dir(&self) -> Result<PathBuf, WorkspaceError> {
        let dir = self.identity.root.join(STATE_DIR);
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

    /// Session-level worktree mode: create `<root>/.hasp/worktrees/<name>`
    /// on branch `hasp/<name>` from HEAD and resolve it as the new workspace.
    pub fn create_worktree(&self, name: &str) -> Result<Workspace, WorkspaceError> {
        let dir = self
            .identity
            .root
            .join(STATE_DIR)
            .join("worktrees")
            .join(name);
        if self.worktree_dir(name).exists() {
            return Err(WorkspaceError::Io(format!(
                "worktree {name} already exists"
            )));
        }
        let branch = format!("{BRANCH_PREFIX}{name}");
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

    /// Names of the worktrees this tool created (`.hasp/worktrees/<name>`, or
    /// `.harness/worktrees/<name>` from before the rename), sorted so listings
    /// are stable.
    pub fn child_worktrees(&self) -> Vec<String> {
        let mut names: Vec<String> = state_dirs(&self.identity.root)
            .iter()
            .filter_map(|d| std::fs::read_dir(d.join("worktrees")).ok())
            .flat_map(|entries| entries.flatten())
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// Where a child worktree lives: under `.hasp/`, or `.harness/` for one
    /// made before the rename.
    pub fn worktree_dir(&self, name: &str) -> PathBuf {
        state_path(&self.identity.root, &format!("worktrees/{name}"))
    }

    /// A child worktree's branch: `hasp/<name>`, or `harness/<name>` for one
    /// made before the rename.
    pub fn worktree_branch(&self, name: &str) -> String {
        worktree_branch_in(&self.identity.root, name)
    }

    /// Whether a child worktree's branch is already contained in `HEAD` — i.e.
    /// removing the worktree cannot lose committed work. Uncommitted changes are
    /// a separate matter: `git worktree remove` refuses those on its own.
    pub fn worktree_merged(&self, name: &str) -> bool {
        let branch = self.worktree_branch(name);
        Command::new("git")
            .args(["merge-base", "--is-ancestor", &branch, "HEAD"])
            .current_dir(&self.identity.root)
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false)
    }

    pub fn remove_worktree(&self, name: &str) -> Result<(), WorkspaceError> {
        let dir = self.worktree_dir(name);
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

/// The project state directory (`<root>/.hasp`): traces, worktrees, reviews,
/// project skills, commands, hooks and MCP config.
pub const STATE_DIR: &str = ".hasp";
/// Its name before the rename. Still read, never written.
pub const LEGACY_STATE_DIR: &str = ".harness";
/// Branch prefix of worktrees this tool creates (`hasp/<name>`).
pub const BRANCH_PREFIX: &str = "hasp/";
/// The prefix older worktrees were created with.
pub const LEGACY_BRANCH_PREFIX: &str = "harness/";

/// `std::env::var` for this tool's variables: `HASP_X`, falling back to the
/// pre-rename `HARNESS_X` so existing shell configs keep working.
pub fn env_var(name: &str) -> Result<String, std::env::VarError> {
    match std::env::var(name) {
        Err(std::env::VarError::NotPresent) => match name.strip_prefix("HASP_") {
            Some(rest) => std::env::var(format!("HARNESS_{rest}")),
            None => Err(std::env::VarError::NotPresent),
        },
        other => other,
    }
}

/// The branch of child worktree `name` in the repository at `root`: the
/// current prefix, unless only the pre-rename branch exists.
pub fn worktree_branch_in(root: &Path, name: &str) -> String {
    let new = format!("{BRANCH_PREFIX}{name}");
    let old = format!("{LEGACY_BRANCH_PREFIX}{name}");
    let exists = |b: &str| {
        Command::new("git")
            .args(["rev-parse", "--verify", "-q", &format!("refs/heads/{b}")])
            .current_dir(root)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    };
    if !exists(&new) && exists(&old) {
        old
    } else {
        new
    }
}

/// A project's state directories, current name first. Reads try both; writes
/// go to the first.
pub fn state_dirs(root: &Path) -> [PathBuf; 2] {
    [root.join(STATE_DIR), root.join(LEGACY_STATE_DIR)]
}

/// The first of `state_dirs(root)/<rel>` that exists, else the current one.
pub fn state_path(root: &Path, rel: &str) -> PathBuf {
    let [new, old] = state_dirs(root);
    let (new, old) = (new.join(rel), old.join(rel));
    if !new.exists() && old.exists() {
        old
    } else {
        new
    }
}

/// `~/.hasp`. The first call migrates this tool's own files from the
/// pre-rename `~/.harness` (see [`migrate_legacy_home`]).
pub fn hasp_home() -> Result<PathBuf, WorkspaceError> {
    let home =
        std::env::home_dir().ok_or_else(|| WorkspaceError::Io("no home directory".into()))?;
    let dir = home.join(".hasp");
    if !dir.exists() {
        std::fs::create_dir_all(&dir).map_err(WorkspaceError::from)?;
        migrate_legacy_home(&home.join(".harness"), &dir);
    }
    Ok(dir)
}

/// What this tool kept in `~/.harness` before the rename. That directory is
/// shared with other tools (graff and its desktop app keep their own files
/// there), so it is never moved: only these entries are copied, once.
pub const LEGACY_HOME_ENTRIES: &[&str] = &[
    "sessions",
    "credentials.json",
    "providers.json",
    "agents.json",
    "models.json",
    "hooks.json",
    "trusted",
    "mcp.json",
    "environments",
    "skills",
    "commands",
    "checkout",
];

/// Copy [`LEGACY_HOME_ENTRIES`] from `old` into `new` (which must be empty).
/// Best effort: a failure leaves that entry behind and says so, and the old
/// copy stays where it was either way.
pub fn migrate_legacy_home(old: &Path, new: &Path) {
    if !old.is_dir() {
        return;
    }
    let mut copied = Vec::new();
    for name in LEGACY_HOME_ENTRIES {
        let from = old.join(name);
        if !from.exists() {
            continue;
        }
        match copy_tree(&from, &new.join(name)) {
            Ok(()) => copied.push(*name),
            Err(e) => eprintln!("▪ could not copy ~/.harness/{name} to ~/.hasp: {e}"),
        }
    }
    if !copied.is_empty() {
        let _ = std::fs::write(
            new.join(".migrated-from-harness"),
            format!("{}\n", copied.join("\n")),
        );
        eprintln!(
            "▪ moved to ~/.hasp: copied {} from ~/.harness (left in place)",
            copied.join(", ")
        );
    }
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(from)?;
    if meta.is_dir() {
        std::fs::create_dir_all(to)?;
        for e in std::fs::read_dir(from)? {
            let e = e?;
            copy_tree(&e.path(), &to.join(e.file_name()))?;
        }
        Ok(())
    } else if meta.file_type().is_symlink() {
        // Symlinks stay links; following them could copy anything.
        #[cfg(unix)]
        std::os::unix::fs::symlink(std::fs::read_link(from)?, to)?;
        Ok(())
    } else {
        // `copy` carries the permission bits, so credentials stay 0600.
        std::fs::copy(from, to).map(|_| ())
    }
}

pub fn sessions_root() -> Result<PathBuf, WorkspaceError> {
    hasp_home().map(|h| h.join("sessions"))
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
                .contains(".hasp/worktrees/feature")
        );
        assert!(wt.identity().is_linked_worktree);
        assert_eq!(wt.identity().branch.as_deref(), Some("hasp/feature"));
        ws.remove_worktree("feature").unwrap();
        assert!(
            !tmp.path()
                .join(".hasp")
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

    /// Worktrees made before the rename (`.harness/worktrees/x` on branch
    /// `harness/x`) are still found, merged-checked and removed.
    #[test]
    fn pre_rename_worktrees_are_still_managed() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let git = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(dir)
                    .status()
                    .unwrap()
                    .success(),
                "git {args:?}"
            );
        };
        git(&["init", "--quiet"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        git(&["add", "."]);
        git(&["commit", "--quiet", "-m", "init"]);
        let old = dir.join(".harness/worktrees/old");
        git(&[
            "worktree",
            "add",
            "-q",
            "-b",
            "harness/old",
            &old.to_string_lossy(),
        ]);

        let ws = Workspace::resolve(dir).unwrap();
        ws.create_worktree("new").unwrap();
        assert_eq!(
            ws.child_worktrees(),
            vec!["new".to_string(), "old".to_string()]
        );
        assert_eq!(ws.worktree_branch("old"), "harness/old");
        assert_eq!(ws.worktree_branch("new"), "hasp/new");
        assert_eq!(
            ws.worktree_dir("old"),
            ws.root().join(".harness/worktrees/old")
        );
        assert!(ws.worktree_merged("old"));
        ws.remove_worktree("old").unwrap();
        assert!(!old.exists());
    }

    #[test]
    fn env_vars_fall_back_to_their_pre_rename_names() {
        // Unique names: tests run in parallel and share the environment.
        unsafe {
            std::env::set_var("HARNESS_ZZ_RENAME_TEST", "old");
        }
        assert_eq!(env_var("HASP_ZZ_RENAME_TEST").unwrap(), "old");
        unsafe {
            std::env::set_var("HASP_ZZ_RENAME_TEST", "new");
        }
        assert_eq!(
            env_var("HASP_ZZ_RENAME_TEST").unwrap(),
            "new",
            "the new name wins"
        );
        assert!(env_var("HASP_ZZ_NEVER_SET").is_err());
    }

    /// `~/.harness` is shared with other tools: only this tool's own entries
    /// are copied, the rest is never touched, and nothing is moved.
    #[test]
    fn migration_copies_only_our_files_and_moves_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let old = tmp.path().join(".harness");
        let new = tmp.path().join(".hasp");
        std::fs::create_dir_all(old.join("sessions/ws-key")).unwrap();
        std::fs::write(old.join("sessions/ws-key/1.jsonl"), "{}").unwrap();
        std::fs::write(old.join("credentials.json"), "{}").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                old.join("credentials.json"),
                std::fs::Permissions::from_mode(0o600),
            )
            .unwrap();
        }
        // Another tool's files in the same directory.
        std::fs::create_dir_all(old.join("bin")).unwrap();
        std::fs::write(old.join("bin/graff"), "binary").unwrap();
        std::fs::write(old.join("ui-settings.json"), "{\"theirs\":1}").unwrap();
        std::fs::create_dir_all(&new).unwrap();

        migrate_legacy_home(&old, &new);

        assert!(new.join("sessions/ws-key/1.jsonl").exists());
        assert!(new.join("credentials.json").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(new.join("credentials.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "credentials keep their permissions");
        }
        assert!(!new.join("bin").exists(), "not ours");
        assert!(!new.join("ui-settings.json").exists(), "not ours");
        assert!(
            old.join("sessions/ws-key/1.jsonl").exists(),
            "copied, not moved"
        );
        assert!(old.join("bin/graff").exists());
        assert!(new.join(".migrated-from-harness").exists());
    }
}
