//! Bash tool with the structural approval rules (codegraff §5.3):
//! - metacharacter rule: commands with shell metacharacters or control
//!   characters never prefix-match an approval (chaining smuggles commands)
//! - cwd-lock: auto-allowed commands must keep path arguments inside the
//!   workspace
//! - fat output becomes a 4 KB inline handle over an out-of-line artifact

use async_trait::async_trait;
use harness_core::{GateDecision, Tool, ToolCtx, ToolOutput, ToolSpec};
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;

pub const BASH_TOOL_NAME: &str = "bash";

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 600_000;
/// Output up to this size is returned whole.
const INLINE_LIMIT: usize = 16 * 1024;
const HEAD_BYTES: usize = 4 * 1024;
const TAIL_BYTES: usize = 10 * 1024;

/// Read-only commands auto-allowed before any approval. `find` is excluded:
/// `-exec`/`-delete` make it an execution tool, not a read tool.
pub fn seed_allowlist() -> Vec<&'static str> {
    vec![
        "ls", "cat", "head", "tail", "wc", "rg", "grep", "pwd", "diff", "which", "git", "cargo",
        "make", "node", "python3", "pytest",
    ]
}

pub fn is_simple(cmd: &str) -> bool {
    !cmd.chars().any(|c| {
        matches!(
            c,
            ';' | '|' | '&' | '>' | '<' | '$' | '`' | '\n' | '\r' | '\t' | '\0'
        )
    })
}

pub fn escapes_cwd(cmd: &str) -> bool {
    cmd.split_whitespace().skip(1).any(|tok| {
        tok.starts_with('/') || tok.starts_with('~') || tok.contains("..") || tok.starts_with("=/")
    })
}

pub struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: BASH_TOOL_NAME.into(),
            description: "Run a shell command in the project root and return its exit code and \
                          output. Long output keeps its head and tail with a pointer to the full \
                          copy. Prefer the grep, glob and read tools for searching and reading."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "timeout_ms": {"type": "integer", "description": "default 120000, max 600000"}
                },
                "required": ["command"]
            }),
        }
    }

    async fn execute(&self, args: Value, ctx: &mut ToolCtx<'_>) -> ToolOutput {
        let Some(cmd) = args["command"].as_str() else {
            return ToolOutput::error("missing `command`");
        };
        let timeout = std::time::Duration::from_millis(
            args["timeout_ms"]
                .as_u64()
                .unwrap_or(DEFAULT_TIMEOUT_MS)
                .min(MAX_TIMEOUT_MS),
        );
        if let GateDecision::Deny(reason) = ctx.gate.check_bash(cmd) {
            return ToolOutput::error(format!("command not approved: {reason}"));
        }
        let mut command = crate::sandbox::command(cmd, ctx.workspace.root());
        command
            .current_dir(ctx.workspace.root())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        // Own process group so a timeout or a cancel kills the whole tree,
        // not just bash.
        #[cfg(unix)]
        command.process_group(0);
        let mut child = match command.spawn() {
            Ok(c) => c,
            Err(e) => return ToolOutput::error(e.to_string()),
        };
        // Dropped with the future when the user cancels the turn: take the
        // command's children down too.
        let _group = GroupGuard(child.id());
        let out_task = tokio::spawn(read_all(child.stdout.take()));
        let err_task = tokio::spawn(read_all(child.stderr.take()));

        let (status, timed_out) = match tokio::time::timeout(timeout, child.wait()).await {
            Ok(Ok(status)) => (Some(status), false),
            Ok(Err(e)) => return ToolOutput::error(e.to_string()),
            Err(_) => {
                kill_group(child.id());
                let _ = child.start_kill();
                (None, true)
            }
        };
        // Readers finish at EOF; a grandchild that kept the pipe open must not
        // hold the result hostage.
        let grace = std::time::Duration::from_secs(2);
        let out = tokio::time::timeout(grace, out_task)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        let err = tokio::time::timeout(grace, err_task)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        let mut text = String::from_utf8_lossy(&out).into_owned();
        if !err.is_empty() {
            text.push_str("\n--- stderr ---\n");
            text.push_str(&String::from_utf8_lossy(&err));
        }
        let mut content = match (status, timed_out) {
            (Some(st), _) => format!("exit: {}\n", st.code().unwrap_or(-1)),
            _ => format!(
                "timed out after {} ms — output so far:\n",
                timeout.as_millis()
            ),
        };
        content.push_str(&shape_output(&text, ctx));
        ToolOutput {
            content,
            is_error: timed_out || !status.is_some_and(|s| s.success()),
            images: Vec::new(),
        }
    }
}

async fn read_all<R: tokio::io::AsyncRead + Unpin>(stream: Option<R>) -> Vec<u8> {
    let mut buf = Vec::new();
    if let Some(mut s) = stream {
        let _ = s.read_to_end(&mut buf).await;
    }
    buf
}

/// Output that fits is returned whole; otherwise the head and — the part that
/// usually matters, where the error and the test summary are — the tail, with
/// the full text stored as an artifact the model can page.
fn shape_output(text: &str, ctx: &ToolCtx<'_>) -> String {
    if text.len() <= INLINE_LIMIT {
        return text.to_string();
    }
    let head = floor_boundary(text, HEAD_BYTES);
    let tail = ceil_boundary(text, text.len() - TAIL_BYTES);
    let omitted = text[head..tail].lines().count();
    let pointer = match ctx.artifacts.store("bash", text.as_bytes()) {
        Ok(rel) => format!("full output at {rel} — page it with the read tool"),
        Err(e) => format!("full output could not be stored: {e}"),
    };
    format!(
        "{}\n[... {omitted} lines omitted; {pointer} ...]\n{}",
        &text[..head],
        &text[tail..]
    )
}

fn floor_boundary(s: &str, mut i: usize) -> usize {
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_boundary(s: &str, mut i: usize) -> usize {
    while !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

#[cfg(unix)]
fn kill_group(pid: Option<u32>) {
    if let Some(pid) = pid {
        let _ = std::process::Command::new("kill")
            .args(["-KILL", &format!("-{pid}")])
            .stderr(std::process::Stdio::null())
            .status();
    }
}

#[cfg(not(unix))]
fn kill_group(_: Option<u32>) {}

/// Kills the command's process group when dropped. A finished group is
/// already gone, so the kill is a harmless no-op then.
struct GroupGuard(Option<u32>);

impl Drop for GroupGuard {
    fn drop(&mut self) {
        kill_group(self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::{ArtifactStore, Gate};
    use harness_workspace::Workspace;

    struct AllowGate;
    impl Gate for AllowGate {
        fn check_bash(&mut self, _: &str) -> GateDecision {
            GateDecision::Allow
        }
        fn check_write(&mut self, _: &str) -> GateDecision {
            GateDecision::Allow
        }
    }

    struct DenyGate;
    impl Gate for DenyGate {
        fn check_bash(&mut self, _: &str) -> GateDecision {
            GateDecision::Deny("no".into())
        }
        fn check_write(&mut self, _: &str) -> GateDecision {
            GateDecision::Deny("no".into())
        }
    }

    fn ctx_in<'a>(path: &std::path::Path, gate: &'a mut dyn Gate) -> ToolCtx<'a> {
        let ws: &'static Workspace = Box::leak(Box::new(Workspace::resolve(path).unwrap()));
        let artifacts: &'static ArtifactStore = Box::leak(Box::new(
            ArtifactStore::new(
                path.join(".harness").join("artifacts").join("test"),
                "test".into(),
            )
            .unwrap(),
        ));
        ToolCtx {
            workspace: ws,
            gate,
            artifacts,
            bus: None,
        }
    }

    #[test]
    fn metacharacter_and_cwd_rules() {
        assert!(is_simple("cargo test"));
        assert!(!is_simple("ls; rm -rf /"));
        assert!(!is_simple("echo `whoami`"));
        assert!(!is_simple("cat a\nrm b"));
        assert!(escapes_cwd("cat /etc/passwd"));
        assert!(escapes_cwd("cat ~/secret"));
        assert!(escapes_cwd("cat ../outside"));
        assert!(!escapes_cwd("rg pattern src"));
    }

    #[tokio::test]
    async fn runs_command_and_reports_exit() {
        let tmp = tempfile::tempdir().unwrap();
        let mut gate = AllowGate;
        let mut ctx = ctx_in(tmp.path(), &mut gate);
        let out = BashTool
            .execute(json!({"command": "printf 'hello'"}), &mut ctx)
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("hello"));

        let fail = BashTool
            .execute(json!({"command": "exit 3"}), &mut ctx)
            .await;
        assert!(fail.is_error);
        assert!(fail.content.contains("exit: 3"));
    }

    #[tokio::test]
    async fn denial_becomes_error_result() {
        let tmp = tempfile::tempdir().unwrap();
        let mut gate = DenyGate;
        let mut ctx = ctx_in(tmp.path(), &mut gate);
        let out = BashTool.execute(json!({"command": "ls"}), &mut ctx).await;
        assert!(out.is_error);
        assert!(out.content.contains("not approved"));
    }

    #[tokio::test]
    async fn fat_output_becomes_handle() {
        let tmp = tempfile::tempdir().unwrap();
        let mut gate = AllowGate;
        let mut ctx = ctx_in(tmp.path(), &mut gate);
        let out = BashTool
            .execute(
                json!({"command": "seq 1 20000", "timeout_ms": 30000}),
                &mut ctx,
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content
                .contains("full output at .harness/artifacts/test/"),
            "{}",
            out.content
        );
        assert!(out.content.contains("\n1\n2\n"), "head kept");
        assert!(out.content.trim_end().ends_with("20000"), "tail kept");
    }

    #[tokio::test]
    async fn non_utf8_output_and_timeouts_keep_what_was_printed() {
        let tmp = tempfile::tempdir().unwrap();
        let mut gate = AllowGate;
        let mut ctx = ctx_in(tmp.path(), &mut gate);
        let out = BashTool
            .execute(json!({"command": "printf 'ok\\377done'"}), &mut ctx)
            .await;
        assert!(
            out.content.contains("ok") && out.content.contains("done"),
            "{}",
            out.content
        );

        let out = BashTool
            .execute(
                json!({"command": "echo started; sleep 5", "timeout_ms": 300}),
                &mut ctx,
            )
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("timed out"), "{}", out.content);
        assert!(
            out.content.contains("started"),
            "partial output kept: {}",
            out.content
        );
    }
}
