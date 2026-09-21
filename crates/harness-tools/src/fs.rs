//! File tools with structural confinement: relative paths only, no `..`,
//! no symlink escape. Confinement is never bypassable by approvals.

use async_trait::async_trait;
use harness_core::{GateDecision, Tool, ToolCtx, ToolOutput, ToolSpec};
use serde_json::{Value, json};

pub const READ_TOOL_NAME: &str = "read";
pub const WRITE_TOOL_NAME: &str = "write";
pub const EDIT_TOOL_NAME: &str = "edit";

const READ_ARTIFACT_THRESHOLD: usize = 64 * 1024;

pub struct ReadTool;

#[async_trait]
impl Tool for ReadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: READ_TOOL_NAME.into(),
            description: "Read a text file from the project. Returns a numbered line window; \
                          large files get a pointer to a stored full copy."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Path relative to the project root"},
                    "offset": {"type": "integer", "description": "1-based first line to return (default 1)"},
                    "limit": {"type": "integer", "description": "Max lines to return (default 2000)"}
                },
                "required": ["path"]
            }),
        }
    }

    async fn execute(&self, args: Value, ctx: &mut ToolCtx<'_>) -> ToolOutput {
        let Some(path) = args["path"].as_str() else {
            return ToolOutput::error("missing `path`");
        };
        let offset = args["offset"].as_u64().unwrap_or(1).max(1);
        let limit = args["limit"].as_u64().unwrap_or(2000);
        let abs = match ctx.workspace.confine(path) {
            Ok(p) => p,
            Err(e) => return ToolOutput::error(e.to_string()),
        };
        let bytes = match tokio::fs::read(&abs).await {
            Ok(b) => b,
            Err(e) => return ToolOutput::error(e.to_string()),
        };
        let text = String::from_utf8_lossy(&bytes);
        let all_lines: Vec<&str> = text.lines().collect();
        let total = all_lines.len();
        let start = (offset as usize - 1).min(total);
        let end = (start + limit as usize).min(total);
        let mut body = all_lines[start..end]
            .iter()
            .enumerate()
            .map(|(i, l)| format!("{:>6} | {}", start + i + 1, l))
            .collect::<Vec<_>>()
            .join("\n");
        if end < total {
            body.push_str(&format!(
                "\n... ({} of {} lines shown; call again with offset {})",
                end - start,
                total,
                end + 1
            ));
        }
        if bytes.len() > READ_ARTIFACT_THRESHOLD {
            if let Ok(rel) = ctx.artifacts.store("read", &bytes) {
                body.push_str(&format!(
                    "\n[full output stored at {rel}; page it with the read tool]"
                ));
            }
        }
        ToolOutput::text(body)
    }
}

pub struct WriteTool;

#[async_trait]
impl Tool for WriteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: WRITE_TOOL_NAME.into(),
            description: "Create or overwrite a file with the given content.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["path", "content"]
            }),
        }
    }

    async fn execute(&self, args: Value, ctx: &mut ToolCtx<'_>) -> ToolOutput {
        let (Some(path), Some(content)) = (args["path"].as_str(), args["content"].as_str()) else {
            return ToolOutput::error("missing `path` or `content`");
        };
        let abs = match ctx.workspace.confine(path) {
            Ok(p) => p,
            Err(e) => return ToolOutput::error(e.to_string()),
        };
        if let GateDecision::Deny(reason) = ctx.gate.check_write(path) {
            return ToolOutput::error(format!("write not approved: {reason}"));
        }
        if let Some(parent) = abs.parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                return ToolOutput::error(e.to_string());
            }
        }
        // Read what was there so the report is the change itself; a missing file
        // simply makes every line an addition.
        let before = tokio::fs::read_to_string(&abs).await.unwrap_or_default();
        match tokio::fs::write(&abs, content).await {
            Ok(_) => ToolOutput::text(format!(
                "wrote {} bytes to {path}\n{}",
                content.len(),
                crate::diff::unified(&before, content, path)
            )),
            Err(e) => ToolOutput::error(e.to_string()),
        }
    }
}

pub struct EditTool;

#[async_trait]
impl Tool for EditTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: EDIT_TOOL_NAME.into(),
            description: "Replace exact text in a file. `old_text` must match exactly once \
                          unless `replace_all` is set."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old_text": {"type": "string"},
                    "new_text": {"type": "string"},
                    "replace_all": {"type": "boolean"}
                },
                "required": ["path", "old_text", "new_text"]
            }),
        }
    }

    async fn execute(&self, args: Value, ctx: &mut ToolCtx<'_>) -> ToolOutput {
        let (Some(path), Some(old), Some(new)) = (
            args["path"].as_str(),
            args["old_text"].as_str(),
            args["new_text"].as_str(),
        ) else {
            return ToolOutput::error("missing `path`, `old_text` or `new_text`");
        };
        let replace_all = args["replace_all"].as_bool().unwrap_or(false);
        let abs = match ctx.workspace.confine(path) {
            Ok(p) => p,
            Err(e) => return ToolOutput::error(e.to_string()),
        };
        let text = match tokio::fs::read_to_string(&abs).await {
            Ok(t) => t,
            Err(e) => return ToolOutput::error(e.to_string()),
        };
        let count = text.matches(old).count();
        if count == 0 {
            return ToolOutput::error("`old_text` not found in file");
        }
        if count > 1 && !replace_all {
            return ToolOutput::error(format!(
                "`old_text` matches {count} times; provide more surrounding context or set replace_all"
            ));
        }
        if let GateDecision::Deny(reason) = ctx.gate.check_write(path) {
            return ToolOutput::error(format!("edit not approved: {reason}"));
        }
        let updated = if replace_all {
            text.replace(old, new)
        } else {
            text.replacen(old, new, 1)
        };
        match tokio::fs::write(&abs, &updated).await {
            Ok(_) => ToolOutput::text(format!(
                "replaced {count} occurrence(s) in {path}\n{}",
                crate::diff::unified(&text, &updated, path)
            )),
            Err(e) => ToolOutput::error(e.to_string()),
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

    #[tokio::test]
    async fn read_returns_numbered_lines_and_windows() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "a\nb\nc\n").unwrap();
        let mut gate = AllowGate;
        let mut ctx = ctx_in(tmp.path(), &mut gate);
        let out = ReadTool
            .execute(json!({"path": "f.txt", "offset": 2, "limit": 1}), &mut ctx)
            .await;
        assert!(out.content.contains("b"));
        assert!(!out.content.contains(" a\n"));
    }

    #[tokio::test]
    async fn read_rejects_absolute_paths_structurally() {
        let tmp = tempfile::tempdir().unwrap();
        let mut gate = AllowGate;
        let mut ctx = ctx_in(tmp.path(), &mut gate);
        let out = ReadTool
            .execute(json!({"path": "/etc/passwd"}), &mut ctx)
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("absolute"));
    }

    #[tokio::test]
    async fn write_goes_through_gate_and_creates_files() {
        let tmp = tempfile::tempdir().unwrap();
        let mut gate = AllowGate;
        let mut ctx = ctx_in(tmp.path(), &mut gate);
        let out = WriteTool
            .execute(json!({"path": "src/new.txt", "content": "hi"}), &mut ctx)
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("src/new.txt")).unwrap(),
            "hi"
        );

        let mut deny = DenyGate;
        let mut ctx = ctx_in(tmp.path(), &mut deny);
        let out = WriteTool
            .execute(json!({"path": "no.txt", "content": "x"}), &mut ctx)
            .await;
        assert!(out.is_error);
        assert!(!tmp.path().join("no.txt").exists());
    }

    #[tokio::test]
    async fn edit_enforces_unique_match() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "x ax\nx ax\n").unwrap();
        let mut gate = AllowGate;
        let mut ctx = ctx_in(tmp.path(), &mut gate);

        let ambiguous = EditTool
            .execute(
                json!({"path": "f.txt", "old_text": "ax", "new_text": "b"}),
                &mut ctx,
            )
            .await;
        assert!(ambiguous.is_error);
        assert!(ambiguous.content.contains("2 times"));

        let unique = EditTool
            .execute(
                json!({"path": "f.txt", "old_text": "x ax\nx ax", "new_text": "one"}),
                &mut ctx,
            )
            .await;
        assert!(!unique.is_error, "{}", unique.content);
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("f.txt")).unwrap(),
            "one\n"
        );

        std::fs::write(tmp.path().join("g.txt"), "n n n").unwrap();
        let all = EditTool
            .execute(
                json!({"path": "g.txt", "old_text": "n", "new_text": "m", "replace_all": true}),
                &mut ctx,
            )
            .await;
        assert!(!all.is_error);
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("g.txt")).unwrap(),
            "m m m"
        );
    }
}
