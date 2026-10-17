//! Hooks: shell commands the harness runs around tool calls and at the end of
//! a turn — format-on-edit, lint feedback, policy checks, notifications.
//!
//! ```json
//! // ~/.harness/hooks.json, or <project>/.harness/hooks.json once trusted
//! {
//!   "pre_tool":  [{"match": "bash", "command": "./scripts/policy.sh"}],
//!   "post_tool": [{"match": "edit|write", "command": "cargo fmt 2>&1 | tail -5"}],
//!   "stop":      [{"command": "cargo check -q 2>&1 | tail -20"}]
//! }
//! ```
//!
//! Each command runs with `bash -c` in the workspace root, gets a JSON event
//! on stdin (`event`, `tool`, `args`, and for `post_tool` the `output` and
//! `is_error`), and has 60 s. Exit code 2 is the one that speaks to the model:
//!
//! - `pre_tool`: the call is blocked and stderr becomes its error result. A
//!   hook can only refuse — it never approves, so it cannot weaken the gate or
//!   the structural rules.
//! - `post_tool`: stderr is appended to the tool's result as feedback.
//! - `stop`: stderr becomes a follow-up prompt, so the model fixes what the
//!   hook found before the turn ends (capped per turn).
//!
//! Any other exit, a timeout, or a hook that fails to start is ignored: a
//! broken hook must not wedge the agent.
//!
//! Project hooks execute code from the repository, so they load only after
//! `harness trust` has recorded the project root in `~/.harness/trusted`.
//! User hooks always load.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

const HOOK_TIMEOUT: Duration = Duration::from_secs(60);
const FEEDBACK_CAP: usize = 4000;
/// `stop` hooks may send the model back at most this many times per turn.
pub const STOP_HOOK_ROUNDS: usize = 3;

#[derive(Debug, Clone, Deserialize)]
pub struct HookSpec {
    /// Regex over the tool name; absent matches every tool.
    #[serde(default, rename = "match")]
    pub matcher: Option<String>,
    pub command: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct HookFile {
    #[serde(default)]
    pub pre_tool: Vec<HookSpec>,
    #[serde(default)]
    pub post_tool: Vec<HookSpec>,
    #[serde(default)]
    pub stop: Vec<HookSpec>,
}

struct Compiled {
    matcher: Option<regex::Regex>,
    command: String,
}

impl Compiled {
    fn applies(&self, tool: &str) -> bool {
        self.matcher.as_ref().is_none_or(|re| re.is_match(tool))
    }
}

#[derive(Default)]
pub struct Hooks {
    pre_tool: Vec<Compiled>,
    post_tool: Vec<Compiled>,
    stop: Vec<Compiled>,
    root: PathBuf,
    /// Problems found while loading (bad JSON, bad regex) — surfaced once.
    pub warnings: Vec<String>,
}

/// What a hook run decided.
pub enum Verdict {
    Continue,
    /// Exit 2: the hook's message for the model.
    Feedback(String),
}

pub fn trusted_path() -> Option<PathBuf> {
    hasp_workspace::hasp_home().ok().map(|h| h.join("trusted"))
}

pub fn is_trusted(root: &Path) -> bool {
    trusted_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .is_some_and(|t| t.lines().any(|l| Path::new(l.trim()) == root))
}

/// Record `root` as trusted (idempotent).
pub fn trust(root: &Path) -> std::io::Result<bool> {
    if is_trusted(root) {
        return Ok(false);
    }
    let path = trusted_path().ok_or_else(|| std::io::Error::other("no home directory"))?;
    let mut text = std::fs::read_to_string(&path).unwrap_or_default();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&root.to_string_lossy());
    text.push('\n');
    std::fs::write(&path, text)?;
    Ok(true)
}

impl Hooks {
    /// User hooks, then (if the project is trusted) project hooks.
    pub fn load(root: &Path) -> Self {
        let mut files = Vec::new();
        if let Ok(home) = hasp_workspace::hasp_home() {
            files.push(home.join("hooks.json"));
        }
        let project = hasp_workspace::state_path(root, "hooks.json");
        let mut hooks = Hooks {
            root: root.to_path_buf(),
            ..Default::default()
        };
        if project.exists() {
            if is_trusted(root) {
                files.push(project);
            } else {
                hooks.warnings.push(format!(
                    "{} ignored: project hooks run code, so they load only after `hasp trust`",
                    project.display()
                ));
            }
        }
        for f in files {
            let Ok(text) = std::fs::read_to_string(&f) else {
                continue;
            };
            match serde_json::from_str::<HookFile>(&text) {
                Ok(file) => hooks.add(file, &f),
                Err(e) => hooks.warnings.push(format!("{}: {e}", f.display())),
            }
        }
        hooks
    }

    pub fn from_file(root: &Path, file: HookFile) -> Self {
        let mut hooks = Hooks {
            root: root.to_path_buf(),
            ..Default::default()
        };
        hooks.add(file, Path::new("<inline>"));
        hooks
    }

    fn add(&mut self, file: HookFile, src: &Path) {
        let mut compile = |specs: Vec<HookSpec>, into: &mut Vec<Compiled>| {
            for s in specs {
                let matcher = match s.matcher.as_deref().filter(|m| !m.is_empty()) {
                    None => None,
                    Some(m) => match regex::Regex::new(&format!("^(?:{m})$")) {
                        Ok(re) => Some(re),
                        Err(e) => {
                            self.warnings
                                .push(format!("{}: bad match `{m}`: {e}", src.display()));
                            continue;
                        }
                    },
                };
                into.push(Compiled {
                    matcher,
                    command: s.command,
                });
            }
        };
        let (mut pre, mut post, mut stop) = (Vec::new(), Vec::new(), Vec::new());
        compile(file.pre_tool, &mut pre);
        compile(file.post_tool, &mut post);
        compile(file.stop, &mut stop);
        self.pre_tool.extend(pre);
        self.post_tool.extend(post);
        self.stop.extend(stop);
    }

    pub fn is_empty(&self) -> bool {
        self.pre_tool.is_empty() && self.post_tool.is_empty() && self.stop.is_empty()
    }

    pub fn has_stop(&self) -> bool {
        !self.stop.is_empty()
    }

    pub async fn pre_tool(&self, tool: &str, args: &Value) -> Verdict {
        let event = serde_json::json!({"event": "pre_tool", "tool": tool, "args": args});
        self.run_all(
            self.pre_tool.iter().filter(|h| h.applies(tool)),
            &event,
            true,
        )
        .await
    }

    pub async fn post_tool(
        &self,
        tool: &str,
        args: &Value,
        output: &str,
        is_error: bool,
    ) -> Verdict {
        let event = serde_json::json!({
            "event": "post_tool", "tool": tool, "args": args,
            "output": output, "is_error": is_error,
        });
        self.run_all(
            self.post_tool.iter().filter(|h| h.applies(tool)),
            &event,
            false,
        )
        .await
    }

    pub async fn stop(&self) -> Verdict {
        let event = serde_json::json!({"event": "stop"});
        self.run_all(self.stop.iter(), &event, false).await
    }

    /// Runs matching hooks in order; `first_wins` stops at the first exit 2
    /// (a block), otherwise every hook's feedback is collected.
    async fn run_all<'a>(
        &self,
        hooks: impl Iterator<Item = &'a Compiled>,
        event: &Value,
        first_wins: bool,
    ) -> Verdict {
        let mut feedback = Vec::new();
        for h in hooks {
            if let Some(msg) = run_one(&h.command, &self.root, event).await {
                if first_wins {
                    return Verdict::Feedback(msg);
                }
                feedback.push(msg);
            }
        }
        if feedback.is_empty() {
            Verdict::Continue
        } else {
            Verdict::Feedback(feedback.join("\n"))
        }
    }
}

/// `Some(stderr)` when the hook exited 2.
async fn run_one(command: &str, root: &Path, event: &Value) -> Option<String> {
    use tokio::io::AsyncWriteExt;
    let mut cmd = tokio::process::Command::new("bash");
    cmd.arg("-c")
        .arg(command)
        .current_dir(root)
        .env("HASP_PROJECT_DIR", root)
        // The pre-rename name, for hook scripts written against it.
        .env("HARNESS_PROJECT_DIR", root)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd.spawn().ok()?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(event.to_string().as_bytes()).await;
    }
    let out = tokio::time::timeout(HOOK_TIMEOUT, child.wait_with_output())
        .await
        .ok()?
        .ok()?;
    if out.status.code() != Some(2) {
        return None;
    }
    let mut msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if msg.is_empty() {
        msg = format!("hook `{command}` objected (exit 2) without a message");
    }
    if msg.len() > FEEDBACK_CAP {
        let mut cut = FEEDBACK_CAP;
        while !msg.is_char_boundary(cut) {
            cut -= 1;
        }
        msg.truncate(cut);
        msg.push_str("… [hook output truncated]");
    }
    Some(msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn hooks(root: &Path, file: Value) -> Hooks {
        Hooks::from_file(root, serde_json::from_value(file).unwrap())
    }

    #[tokio::test]
    async fn exit_two_speaks_other_exits_are_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let h = hooks(
            tmp.path(),
            json!({
                "pre_tool": [
                    {"match": "bash", "command": "grep -q 'rm -rf' && { echo 'no rm -rf' >&2; exit 2; }; exit 0"},
                    {"match": "bash", "command": "exit 1"}
                ],
                "post_tool": [{"match": "edit|write", "command": "cat > seen.json; echo 'lint: bad' >&2; exit 2"}]
            }),
        );
        let blocked = h.pre_tool("bash", &json!({"command": "rm -rf /"})).await;
        assert!(matches!(blocked, Verdict::Feedback(m) if m == "no rm -rf"));
        assert!(matches!(
            h.pre_tool("bash", &json!({"command": "ls"})).await,
            Verdict::Continue
        ));
        // matcher is anchored: `bash` does not match `bash_like`
        assert!(matches!(
            h.pre_tool("bash_like", &json!({"command": "rm -rf"})).await,
            Verdict::Continue
        ));
        let fb = h
            .post_tool("edit", &json!({"path": "a"}), "ok", false)
            .await;
        assert!(matches!(fb, Verdict::Feedback(m) if m == "lint: bad"));
        let seen: Value =
            serde_json::from_str(&std::fs::read_to_string(tmp.path().join("seen.json")).unwrap())
                .unwrap();
        assert_eq!(seen["tool"], "edit");
        assert_eq!(seen["output"], "ok");
        assert!(matches!(
            h.post_tool("read", &json!({}), "", false).await,
            Verdict::Continue
        ));
    }

    #[test]
    fn bad_regex_is_a_warning_not_a_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let h = hooks(
            tmp.path(),
            json!({"pre_tool": [{"match": "(", "command": "true"}]}),
        );
        assert!(h.is_empty());
        assert_eq!(h.warnings.len(), 1);
    }
}
