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
const INLINE_LIMIT: usize = 4 * 1024;

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
            description: "Run a shell command in the project root and return its combined \
                          output. Long output is truncated with a pointer to the full copy."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "timeout_ms": {"type": "integer", "description": "default 120000"}
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
            args["timeout_ms"].as_u64().unwrap_or(DEFAULT_TIMEOUT_MS),
        );
        if let GateDecision::Deny(reason) = ctx.gate.check_bash(cmd) {
            return ToolOutput::error(format!("command not approved: {reason}"));
        }
        let mut command = tokio::process::Command::new("bash");
        command
            .arg("-c")
            .arg(cmd)
            .current_dir(ctx.workspace.root())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        // Own process group so a timeout kills the whole tree, not just bash.
        #[cfg(unix)]
        command.process_group(0);
        let mut child = match command.spawn() {
            Ok(c) => c,
            Err(e) => return ToolOutput::error(e.to_string()),
        };
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let out_task = tokio::spawn(async move {
            let mut buf = String::new();
            if let Some(mut s) = stdout {
                let _ = s.read_to_string(&mut buf).await;
            }
            buf
        });
        let err_task = tokio::spawn(async move {
            let mut buf = String::new();
            if let Some(mut s) = stderr {
                let _ = s.read_to_string(&mut buf).await;
            }
            buf
        });

        match tokio::time::timeout(timeout, child.wait()).await {
            Ok(Ok(status)) => {
                let mut out = out_task.await.unwrap_or_default();
                let err = err_task.await.unwrap_or_default();
                if !err.is_empty() {
                    out.push_str("\n--- stderr ---\n");
                    out.push_str(&err);
                }
                let mut content = format!("exit: {}\n", status.code().unwrap_or(-1));
                if out.len() > INLINE_LIMIT {
                    match ctx.artifacts.store("bash", out.as_bytes()) {
                        Ok(rel) => {
                            let head = String::from_utf8_lossy(&out.as_bytes()[..INLINE_LIMIT]);
                            content.push_str(&head);
                            content.push_str(&format!(
                                "\n[... truncated; full output at {rel} — page it with the read tool]"
                            ));
                        }
                        Err(e) => {
                            content.push_str(&format!("\n(full output could not be stored: {e})"))
                        }
                    }
                } else {
                    content.push_str(&out);
                }
                ToolOutput {
                    content,
                    is_error: !status.success(),
                }
            }
            Ok(Err(e)) => ToolOutput::error(e.to_string()),
            Err(_) => {
                #[cfg(unix)]
                if let Some(pid) = child.id() {
                    let _ = std::process::Command::new("kill")
                        .args(["-TERM", &format!("-{pid}")])
                        .status();
                }
                let _ = child.start_kill();
                ToolOutput::error(format!(
                    "command timed out after {} ms",
                    timeout.as_millis()
                ))
            }
        }
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
                json!({"command": "seq 1 2000", "timeout_ms": 30000}),
                &mut ctx,
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content
                .contains("[... truncated; full output at .harness/artifacts/test/")
        );
    }
}
