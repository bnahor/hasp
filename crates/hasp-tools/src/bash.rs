//! Bash tool with the structural approval rules (codegraff §5.3):
//! - metacharacter rule: commands with shell metacharacters or control
//!   characters never prefix-match an approval (chaining smuggles commands)
//! - cwd-lock: auto-allowed commands must keep path arguments inside the
//!   workspace
//! - fat output becomes a 4 KB inline handle over an out-of-line artifact

use async_trait::async_trait;
use hasp_core::{GateDecision, Tool, ToolCtx, ToolOutput, ToolSpec};
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
/// Commands the gate may run without asking, *when* `read_only_invocation`
/// also accepts the arguments. Interpreters and build tools (python3, node,
/// make, cargo, pytest...) are deliberately absent: `python3 -c ...` or a
/// build script can write anything, so they need an approval or an explicit
/// `bash.allow` entry in ~/.hasp/policy.json.
pub fn seed_allowlist() -> Vec<&'static str> {
    vec![
        "cat", "diff", "git", "grep", "head", "ls", "pwd", "rg", "tail", "wc", "which",
    ]
}

/// Split a simple command line into words the way a POSIX shell would for the
/// quoting we allow (single quotes, double quotes, backslash). `None` when the
/// quoting is unbalanced: anything we can't parse is not read-only.
pub fn split_words(cmd: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = cmd.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        ch => cur.push(ch),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => cur.push(chars.next()?),
                        ch => cur.push(ch),
                    }
                }
            }
            '\\' => {
                in_word = true;
                cur.push(chars.next()?);
            }
            ' ' => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            ch => {
                in_word = true;
                cur.push(ch);
            }
        }
    }
    if in_word {
        words.push(cur);
    }
    Some(words)
}

/// git subcommands that only read the repository.
const GIT_READ: &[&str] = &[
    "blame", "branch", "describe", "diff", "log", "ls-files", "remote", "rev-parse", "shortlog",
    "show", "status",
];

/// Flags that make an otherwise read-only tool write files or run programs.
fn forbidden_flag(tool: &str, arg: &str) -> bool {
    let starts = |p: &str| arg == p || arg.starts_with(&format!("{p}="));
    match tool {
        // --pre / --hostname-bin run arbitrary programs; -z runs decompressors.
        "rg" => starts("--pre") || starts("--pre-glob") || starts("--hostname-bin")
            || arg == "-z" || arg == "--search-zip",
        "git" => {
            starts("--output") || starts("--ext-diff") || arg == "--textconv"
                || starts("--exec") || starts("--upload-pack") || starts("--receive-pack")
                || starts("--config") || starts("--open-files-in-pager") || arg == "-c"
                || arg.starts_with("-O") || starts("--git-dir") || starts("--work-tree")
                || starts("--exec-path") || starts("--paginate")
        }
        _ => false,
    }
}

/// Whether `cmd` is a simple invocation of a read-only tool with arguments
/// that keep it read-only: the narrow set the gate runs without asking.
/// Structural checks (`is_simple`, `escapes_cwd`) and the OS sandbox still
/// apply on top; this only decides "no prompt needed".
pub fn read_only_invocation(cmd: &str) -> bool {
    if !is_simple(cmd) {
        return false;
    }
    let Some(words) = split_words(cmd) else {
        return false;
    };
    let Some((tool, args)) = words.split_first() else {
        return false;
    };
    if !seed_allowlist().contains(&tool.as_str()) {
        return false;
    }
    if args.iter().any(|a| forbidden_flag(tool, a)) {
        return false;
    }
    if tool == "git" {
        // No global options before the subcommand (-c, -C, --git-dir ... all
        // change what runs or where), and only reading subcommands.
        let Some(sub) = args.first() else {
            return false;
        };
        if !GIT_READ.contains(&sub.as_str()) {
            return false;
        }
        let rest = &args[1..];
        return match sub.as_str() {
            // listing only: no names (which would create/rename/delete)
            "branch" => rest.iter().all(|a| {
                matches!(
                    a.as_str(),
                    "-a" | "-r" | "-v" | "-vv" | "--list" | "--all" | "--remotes" | "--show-current"
                )
            }),
            "remote" => rest.iter().all(|a| a == "-v"),
            _ => true,
        };
    }
    true
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

#[derive(Default)]
pub struct BashTool {
    jobs: std::sync::Arc<crate::jobs::JobTable>,
}

impl BashTool {
    /// A bash tool whose background jobs land in `jobs` (shared with the
    /// `job` tool that manages them).
    pub fn with_jobs(jobs: std::sync::Arc<crate::jobs::JobTable>) -> Self {
        Self { jobs }
    }
}

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
                    "timeout_ms": {"type": "integer", "description": "default 120000, max 600000"},
                    "background": {"type": "boolean", "description": "Start it as a background job and return at once (dev servers, watchers, long builds); manage it with the job tool"}
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
        if args["background"].as_bool().unwrap_or(false) {
            return match self.jobs.start(cmd, ctx) {
                Ok(msg) => ToolOutput::text(msg),
                Err(e) => ToolOutput::error(e),
            };
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

fn kill_group(pid: Option<u32>) {
    if let Some(pid) = pid {
        hasp_core::process::signal_group(pid, hasp_core::process::SIGKILL);
    }
}

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
    use hasp_core::{ArtifactStore, Gate};
    use hasp_workspace::Workspace;

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
                path.join(".hasp").join("artifacts").join("test"),
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

    #[test]
    fn read_only_invocations_are_narrow() {
        for ok in [
            "ls -la src",
            "cat README.md",
            "rg -n 'fn main' crates",
            "grep -rn TODO src",
            "git status",
            "git log --oneline -5",
            "git diff HEAD~1 -- src",
            "git branch -a",
            "git remote -v",
            "wc -l \"a file.txt\"",
        ] {
            assert!(read_only_invocation(ok), "{ok}");
        }
        for bad in [
            // the review probe: an interpreter is not read-only
            "python3 -c \"open('probe.txt','w').write('harmless')\"",
            "node -e \"require('fs').writeFileSync('x','y')\"",
            "cargo build",
            "make",
            "pytest",
            "sh -c 'echo hi'",
            // read-only tools turned into writers or launchers
            "rg --pre ./evil.sh pattern",
            "rg --pre=./evil.sh pattern",
            "rg --hostname-bin ./evil.sh x",
            "git log --output=owned.txt",
            "git diff --ext-diff",
            "git -c core.pager=./evil log",
            "git -C other status",
            "git commit -m x",
            "git push",
            "git checkout -- .",
            "git branch new-branch",
            "git branch -D main",
            "git remote add x y",
            // unparseable or shell features
            "cat 'unterminated",
            "cat a > b",
            "ls; rm -rf x",
            "",
        ] {
            assert!(!read_only_invocation(bad), "{bad}");
        }
    }

    #[test]
    fn splits_like_a_shell() {
        assert_eq!(
            split_words(r#"a 'b c' "d \"e\"" f\ g"#).unwrap(),
            vec!["a", "b c", "d \"e\"", "f g"]
        );
        assert!(split_words("a 'b").is_none());
    }

    #[tokio::test]
    async fn runs_command_and_reports_exit() {
        let tmp = tempfile::tempdir().unwrap();
        let mut gate = AllowGate;
        let mut ctx = ctx_in(tmp.path(), &mut gate);
        let out = BashTool::default()
            .execute(json!({"command": "printf 'hello'"}), &mut ctx)
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("hello"));

        let fail = BashTool::default()
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
        let out = BashTool::default()
            .execute(json!({"command": "ls"}), &mut ctx)
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("not approved"));
    }

    #[tokio::test]
    async fn fat_output_becomes_handle() {
        let tmp = tempfile::tempdir().unwrap();
        let mut gate = AllowGate;
        let mut ctx = ctx_in(tmp.path(), &mut gate);
        let out = BashTool::default()
            .execute(
                json!({"command": "seq 1 20000", "timeout_ms": 30000}),
                &mut ctx,
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.contains("full output at .hasp/artifacts/test/"),
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
        let out = BashTool::default()
            .execute(json!({"command": "printf 'ok\\377done'"}), &mut ctx)
            .await;
        assert!(
            out.content.contains("ok") && out.content.contains("done"),
            "{}",
            out.content
        );

        let out = BashTool::default()
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
