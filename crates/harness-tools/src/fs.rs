//! File tools with structural confinement: relative paths only, no `..`,
//! no symlink escape. Confinement is never bypassable by approvals.

use async_trait::async_trait;
use harness_core::{GateDecision, Tool, ToolCtx, ToolOutput, ToolSpec};
use serde_json::{Value, json};

pub const READ_TOOL_NAME: &str = "read";
pub const WRITE_TOOL_NAME: &str = "write";
pub const EDIT_TOOL_NAME: &str = "edit";

const READ_ARTIFACT_THRESHOLD: usize = 64 * 1024;
/// Most text a single `read` returns; the rest is reachable with `offset`.
const READ_MAX_BYTES: usize = 100 * 1024;
const READ_MAX_LINE_BYTES: usize = 2000;

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

    fn concurrency_safe(&self) -> bool {
        true
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
        if abs.is_dir() {
            return list_dir(&abs, path).await;
        }
        let bytes = match tokio::fs::read(&abs).await {
            Ok(b) => b,
            Err(e) => return ToolOutput::error(e.to_string()),
        };
        if bytes.iter().take(8192).any(|&b| b == 0) {
            return ToolOutput::error(format!(
                "{path} looks binary ({} bytes); not shown as text",
                bytes.len()
            ));
        }
        let text = String::from_utf8_lossy(&bytes);
        let all_lines: Vec<&str> = text.lines().collect();
        let total = all_lines.len();
        let start = (offset as usize - 1).min(total);
        let mut end = (start + limit as usize).min(total);
        let mut body = String::new();
        let mut clipped_lines = 0;
        for (i, line) in all_lines[start..end].iter().enumerate() {
            // A byte budget as well as a line budget: 2000 lines of minified
            // code would otherwise put megabytes into the context.
            if body.len() > READ_MAX_BYTES {
                end = start + i;
                break;
            }
            let shown = if line.len() > READ_MAX_LINE_BYTES {
                clipped_lines += 1;
                let mut cut = READ_MAX_LINE_BYTES;
                while !line.is_char_boundary(cut) {
                    cut -= 1;
                }
                format!("{}… [line truncated, {} bytes]", &line[..cut], line.len())
            } else {
                line.to_string()
            };
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str(&format!("{:>6} | {}", start + i + 1, shown));
        }
        if total == 0 {
            body.push_str("(empty file)");
        }
        if end < total {
            body.push_str(&format!(
                "\n... ({} of {} lines shown; call again with offset {})",
                end - start,
                total,
                end + 1
            ));
        }
        if clipped_lines > 0 {
            body.push_str(&format!(
                "\n[{clipped_lines} long line(s) truncated; use grep with a narrow pattern to see them]"
            ));
        }
        if bytes.len() > READ_ARTIFACT_THRESHOLD
            && let Ok(rel) = ctx.artifacts.store("read", &bytes)
        {
            body.push_str(&format!(
                "\n[full output stored at {rel}; page it with the read tool]"
            ));
        }
        ToolOutput::text(body)
    }
}

/// Reading a directory lists it — models ask for one often enough that an
/// error would just cost a round trip.
async fn list_dir(abs: &std::path::Path, shown: &str) -> ToolOutput {
    let mut entries = match tokio::fs::read_dir(abs).await {
        Ok(e) => e,
        Err(e) => return ToolOutput::error(e.to_string()),
    };
    let mut names = Vec::new();
    while let Ok(Some(e)) = entries.next_entry().await {
        let mut name = e.file_name().to_string_lossy().into_owned();
        if e.file_type().await.is_ok_and(|t| t.is_dir()) {
            name.push('/');
        }
        names.push(name);
    }
    names.sort();
    let total = names.len();
    names.truncate(500);
    let mut out = format!(
        "{shown} is a directory ({total} entries):\n{}",
        names.join("\n")
    );
    if total > 500 {
        out.push_str("\n[… more entries; use glob to narrow]");
    }
    ToolOutput::text(out)
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
        if let Some(parent) = abs.parent()
            && let Err(e) = tokio::fs::create_dir_all(parent).await
        {
            return ToolOutput::error(e.to_string());
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
        if old.is_empty() {
            return ToolOutput::error("`old_text` is empty; use write to create a file");
        }
        let count = text.matches(old).count();
        let (updated, count, note) = if count == 0 {
            match fuzzy_replace(&text, old, new) {
                Ok((updated, note)) => (updated, 1, note),
                Err(e) => return ToolOutput::error(e),
            }
        } else {
            if count > 1 && !replace_all {
                return ToolOutput::error(format!(
                    "`old_text` matches {count} times; provide more surrounding context or set replace_all"
                ));
            }
            let updated = if replace_all {
                text.replace(old, new)
            } else {
                text.replacen(old, new, 1)
            };
            (updated, count, "")
        };
        if let GateDecision::Deny(reason) = ctx.gate.check_write(path) {
            return ToolOutput::error(format!("edit not approved: {reason}"));
        }
        match tokio::fs::write(&abs, &updated).await {
            Ok(_) => ToolOutput::text(format!(
                "replaced {count} occurrence(s) in {path}{note}\n{}",
                crate::diff::unified(&text, &updated, path)
            )),
            Err(e) => ToolOutput::error(e.to_string()),
        }
    }
}

/// Fallbacks for an `old_text` with no exact match, in order of strictness,
/// each accepted only when it identifies exactly one place: the same text with
/// the file's CRLF line endings, then the same lines ignoring trailing
/// whitespace, then ignoring indentation too. The replaced span is the file's
/// own lines, so nothing outside the match moves.
fn fuzzy_replace(text: &str, old: &str, new: &str) -> Result<(String, &'static str), String> {
    if text.contains("\r\n") && !old.contains('\r') {
        let (old_crlf, new_crlf) = (old.replace('\n', "\r\n"), new.replace('\n', "\r\n"));
        match text.matches(&old_crlf).count() {
            1 => {
                return Ok((
                    text.replacen(&old_crlf, &new_crlf, 1),
                    " (matched with CRLF line endings)",
                ));
            }
            n if n > 1 => {
                return Err(format!(
                    "`old_text` matches {n} times (with CRLF line endings); add surrounding context"
                ));
            }
            _ => {}
        }
    }
    let old_lines: Vec<&str> = old.trim_end_matches('\n').split('\n').collect();
    // Byte span of each line of the file, without its terminator.
    let mut spans = Vec::new();
    let mut pos = 0;
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches('\n').trim_end_matches('\r');
        spans.push((pos, pos + body.len()));
        pos += line.len();
    }
    let line = |i: usize| &text[spans[i].0..spans[i].1];
    type Pass = (fn(&str) -> &str, &'static str);
    let passes: [Pass; 2] = [
        (str::trim_end, " (matched ignoring trailing whitespace)"),
        (
            str::trim,
            " (matched ignoring indentation — check the result's indentation)",
        ),
    ];
    for (norm, note) in passes {
        let k = old_lines.len();
        if k == 0 || k > spans.len() || old_lines.iter().all(|l| norm(l).is_empty()) {
            break;
        }
        let hits: Vec<usize> = (0..=spans.len() - k)
            .filter(|&s| (0..k).all(|j| norm(line(s + j)) == norm(old_lines[j])))
            .collect();
        match hits.as_slice() {
            [s] => {
                let (from, to) = (spans[*s].0, spans[*s + k - 1].1);
                // The span stops before the last line's terminator, so a
                // trailing newline in `old`/`new` is already accounted for.
                let replacement = match old.ends_with('\n') {
                    true => new.strip_suffix('\n').unwrap_or(new),
                    false => new,
                };
                let mut out = String::with_capacity(text.len() + new.len());
                out.push_str(&text[..from]);
                out.push_str(replacement);
                out.push_str(&text[to..]);
                return Ok((out, note));
            }
            [] => continue,
            many => {
                return Err(format!(
                    "`old_text` not found exactly, and matches {} places when whitespace is \
                     ignored; add surrounding context",
                    many.len()
                ));
            }
        }
    }
    Err(
        "`old_text` not found in file. Re-read the file (it may have changed) and copy the \
         text exactly, including whitespace."
            .into(),
    )
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

    #[test]
    fn fuzzy_edit_tolerates_whitespace_and_crlf_but_only_when_unique() {
        let text = "fn a() {\n    let x = 1;   \n    x\n}\n";
        let (out, note) =
            fuzzy_replace(text, "    let x = 1;\n    x", "    let x = 2;\n    x").unwrap();
        assert_eq!(out, "fn a() {\n    let x = 2;\n    x\n}\n");
        assert!(note.contains("trailing"));

        let (out, note) = fuzzy_replace(text, "let x = 1;", "    let y = 1;").unwrap();
        assert_eq!(out, "fn a() {\n    let y = 1;\n    x\n}\n");
        assert!(note.contains("indentation"));

        let crlf = "a\r\nb\r\nc\r\n";
        let (out, _) = fuzzy_replace(crlf, "a\nb", "a\nB").unwrap();
        assert_eq!(out, "a\r\nB\r\nc\r\n");

        assert!(
            fuzzy_replace("  a b\n    a b\n", "a b\t", "y")
                .unwrap_err()
                .contains("2 places")
        );
        assert!(
            fuzzy_replace("abc\n", "zzz", "y")
                .unwrap_err()
                .contains("not found")
        );
    }
}
