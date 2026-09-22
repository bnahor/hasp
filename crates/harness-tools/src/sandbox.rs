//! OS-level sandbox for `bash`: `HARNESS_SANDBOX=off|workspace-write|read-only`.
//!
//! Approvals decide *whether* a command runs; the sandbox bounds *what it can
//! do* once it does — the layer that makes `--yolo` safe to leave running.
//! `workspace-write` lets commands write only inside the workspace, the temp
//! directories and any `HARNESS_SANDBOX_WRITABLE` (colon-separated) paths;
//! `read-only` allows temp writes only. `HARNESS_SANDBOX_NETWORK=deny` also
//! cuts network access. macOS uses `sandbox-exec` (Seatbelt), Linux uses
//! `bwrap` (bubblewrap). When the platform tool is missing the command is
//! refused with a clear message — never silently run unsandboxed.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    Off,
    WorkspaceWrite,
    ReadOnly,
}

impl Mode {
    pub fn from_env() -> Result<Self, String> {
        match std::env::var("HARNESS_SANDBOX").as_deref() {
            Err(_) | Ok("") | Ok("off") => Ok(Mode::Off),
            Ok("workspace-write") => Ok(Mode::WorkspaceWrite),
            Ok("read-only") => Ok(Mode::ReadOnly),
            Ok(other) => Err(format!(
                "HARNESS_SANDBOX={other} is not one of off, workspace-write, read-only"
            )),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::WorkspaceWrite => "workspace-write",
            Mode::ReadOnly => "read-only",
        }
    }
}

pub struct Policy {
    pub mode: Mode,
    pub writable: Vec<PathBuf>,
    pub network: bool,
}

impl Policy {
    pub fn from_env(root: &Path) -> Result<Self, String> {
        let mode = Mode::from_env()?;
        let mut writable = Vec::new();
        if mode == Mode::WorkspaceWrite {
            writable.push(root.to_path_buf());
            if let Ok(extra) = std::env::var("HARNESS_SANDBOX_WRITABLE") {
                writable.extend(
                    extra
                        .split(':')
                        .filter(|p| !p.is_empty())
                        .map(|p| canonical(Path::new(p))),
                );
            }
        }
        for tmp in temp_dirs() {
            if !writable.contains(&tmp) {
                writable.push(tmp);
            }
        }
        let network = std::env::var("HARNESS_SANDBOX_NETWORK").as_deref() != Ok("deny");
        Ok(Self {
            mode,
            writable,
            network,
        })
    }
}

fn canonical(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// Temp locations tools expect to write to (compilers, test runners).
fn temp_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![canonical(Path::new("/tmp"))];
    if let Ok(t) = std::env::var("TMPDIR") {
        dirs.push(canonical(Path::new(&t)));
    }
    #[cfg(target_os = "macos")]
    dirs.push(canonical(Path::new("/private/var/folders")));
    dirs.dedup();
    dirs
}

/// The command that runs `cmd` for the bash tool, sandboxed per the
/// environment.
pub fn command(cmd: &str, root: &Path) -> tokio::process::Command {
    let policy = match Policy::from_env(root) {
        Ok(p) => p,
        Err(e) => return refusal(&e),
    };
    wrap(cmd, &policy)
}

pub fn wrap(cmd: &str, policy: &Policy) -> tokio::process::Command {
    if policy.mode == Mode::Off {
        let mut c = tokio::process::Command::new("bash");
        c.arg("-c").arg(cmd);
        return c;
    }
    platform_wrap(cmd, policy)
}

/// A command that explains why nothing ran, exit 126 ("cannot execute").
fn refusal(why: &str) -> tokio::process::Command {
    let mut c = tokio::process::Command::new("bash");
    c.arg("-c")
        .arg("printf '%s\\n' \"$1\" >&2; exit 126")
        .arg("sandbox")
        .arg(format!("sandbox refused to run the command: {why}"));
    c
}

fn which(bin: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join(bin))
            .find(|p| p.is_file())
    })
}

/// Seatbelt profile string literal.
fn sb_quote(p: &Path) -> String {
    let s = p.to_string_lossy();
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

pub fn seatbelt_profile(policy: &Policy) -> String {
    let mut profile = String::from(
        "(version 1)\n(allow default)\n(deny file-write*)\n\
         (allow file-write* (literal \"/dev/null\") (literal \"/dev/zero\") \
         (literal \"/dev/dtracehelper\") (regex #\"^/dev/tty\") (regex #\"^/dev/fd/\"))\n",
    );
    for w in &policy.writable {
        profile.push_str(&format!("(allow file-write* (subpath {}))\n", sb_quote(w)));
    }
    if !policy.network {
        profile.push_str("(deny network*)\n(allow network* (remote unix-socket))\n");
    }
    profile
}

#[cfg(target_os = "macos")]
fn platform_wrap(cmd: &str, policy: &Policy) -> tokio::process::Command {
    let Some(exe) = which("sandbox-exec") else {
        return refusal(&format!(
            "HARNESS_SANDBOX={} needs sandbox-exec, which is not on PATH",
            policy.mode.label()
        ));
    };
    let mut c = tokio::process::Command::new(exe);
    c.arg("-p")
        .arg(seatbelt_profile(policy))
        .arg("bash")
        .arg("-c")
        .arg(cmd);
    c
}

#[cfg(target_os = "linux")]
fn platform_wrap(cmd: &str, policy: &Policy) -> tokio::process::Command {
    let Some(exe) = which("bwrap") else {
        return refusal(&format!(
            "HARNESS_SANDBOX={} needs bubblewrap (bwrap), which is not on PATH",
            policy.mode.label()
        ));
    };
    let mut c = tokio::process::Command::new(exe);
    c.args(["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc"]);
    for w in &policy.writable {
        if w.exists() {
            c.arg("--bind").arg(w).arg(w);
        }
    }
    if !policy.network {
        c.arg("--unshare-net");
    }
    c.args(["--die-with-parent", "--", "bash", "-c", cmd]);
    c
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn platform_wrap(_cmd: &str, policy: &Policy) -> tokio::process::Command {
    refusal(&format!(
        "HARNESS_SANDBOX={} is not supported on this platform",
        policy.mode.label()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(mode: Mode, root: &Path) -> Policy {
        let mut writable = temp_dirs();
        if mode == Mode::WorkspaceWrite {
            writable.insert(0, root.to_path_buf());
        }
        Policy {
            mode,
            writable,
            network: true,
        }
    }

    #[test]
    fn profile_allows_only_listed_write_roots() {
        let p = seatbelt_profile(&Policy {
            mode: Mode::WorkspaceWrite,
            writable: vec![PathBuf::from("/work/a \"b\"")],
            network: false,
        });
        assert!(p.contains("(deny file-write*)"));
        assert!(p.contains(r#"(subpath "/work/a \"b\"")"#), "{p}");
        assert!(p.contains("(deny network*)"));
    }

    /// Runs the real platform sandbox when it is installed; skipped (not
    /// failed) where it is not, since CI images differ.
    #[tokio::test]
    async fn workspace_write_confines_writes() {
        let tool = if cfg!(target_os = "macos") {
            "sandbox-exec"
        } else {
            "bwrap"
        };
        if which(tool).is_none() {
            eprintln!("skipping: {tool} not installed");
            return;
        }
        // Both outside the temp dirs, which every mode leaves writable.
        let home = std::env::var("HOME").unwrap();
        let ws = tempfile::tempdir_in(&home).unwrap();
        let root = canonical(ws.path());
        let outside = tempfile::tempdir_in(&home).unwrap();
        let outside_file = outside.path().join("escape.txt");
        let script = format!(
            "echo in > inside.txt && echo out > '{}'",
            outside_file.display()
        );
        let out = wrap(&script, &policy(Mode::WorkspaceWrite, &root))
            .current_dir(&root)
            .output()
            .await
            .unwrap();
        assert!(root.join("inside.txt").exists(), "workspace write allowed");
        assert!(
            !outside_file.exists(),
            "write outside the workspace blocked"
        );
        assert!(!out.status.success());

        let out = wrap("echo x > ro.txt", &policy(Mode::ReadOnly, &root))
            .current_dir(&root)
            .output()
            .await
            .unwrap();
        assert!(!out.status.success());
        assert!(
            !root.join("ro.txt").exists(),
            "read-only blocks workspace writes"
        );
    }
}
