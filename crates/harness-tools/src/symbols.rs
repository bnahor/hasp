//! `symbol_search`: find definitions across the workspace — the answer to
//! "where is X defined?" without the model paging through files. Honestly a
//! per-call scan (regexes per language), not a graph or an index: no stale
//! cache, nothing to invalidate, and fast enough at workspace scale. Exposed
//! wherever tools are, including `--mcp-serve`, so other agents get it too.

use async_trait::async_trait;
use harness_core::{Tool, ToolCtx, ToolOutput, ToolSpec};
use serde_json::{Value, json};

const MAX_FILES: usize = 4000;
const MAX_FILE_BYTES: u64 = 1_000_000;
const MAX_RESULTS: usize = 50;

pub const SYMBOL_SEARCH_TOOL_NAME: &str = "symbol_search";

pub struct SymbolSearchTool;

/// (kind, pattern) pairs per file extension. Patterns match a single line and
/// capture the symbol name in group 1.
fn patterns_for(ext: &str) -> Option<&'static [(&'static str, &'static str)]> {
    const RUST: &[(&str, &str)] = &[
        (
            "fn",
            r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:const\s+)?(?:async\s+)?(?:unsafe\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)",
        ),
        (
            "struct",
            r"^\s*(?:pub(?:\([^)]*\))?\s+)?struct\s+([A-Za-z_][A-Za-z0-9_]*)",
        ),
        (
            "enum",
            r"^\s*(?:pub(?:\([^)]*\))?\s+)?enum\s+([A-Za-z_][A-Za-z0-9_]*)",
        ),
        (
            "trait",
            r"^\s*(?:pub(?:\([^)]*\))?\s+)?trait\s+([A-Za-z_][A-Za-z0-9_]*)",
        ),
        ("mod", r"^\s*(?:pub\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)"),
    ];
    const PY: &[(&str, &str)] = &[
        ("def", r"^\s*(?:async\s+)?def\s+([A-Za-z_][A-Za-z0-9_]*)"),
        ("class", r"^\s*class\s+([A-Za-z_][A-Za-z0-9_]*)"),
    ];
    const JS: &[(&str, &str)] = &[
        (
            "fn",
            r"^\s*(?:export\s+)?(?:default\s+)?(?:async\s+)?function\s*\*?\s+([A-Za-z_$][A-Za-z0-9_$]*)",
        ),
        (
            "class",
            r"^\s*(?:export\s+)?(?:default\s+)?class\s+([A-Za-z_$][A-Za-z0-9_$]*)",
        ),
        (
            "const",
            r"^\s*(?:export\s+)?const\s+([A-Za-z_$][A-Za-z0-9_$]*)\s*=\s*(?:async\s*)?(?:\([^)]*\)|[A-Za-z_$][A-Za-z0-9_$]*)\s*=>",
        ),
    ];
    const GO: &[(&str, &str)] = &[
        (
            "func",
            r"^\s*func\s+(?:\([^)]*\)\s*)?([A-Za-z_][A-Za-z0-9_]*)",
        ),
        ("type", r"^\s*type\s+([A-Za-z_][A-Za-z0-9_]*)\s"),
    ];
    match ext {
        "rs" => Some(RUST),
        "py" => Some(PY),
        "js" | "jsx" | "ts" | "tsx" | "mjs" | "mts" => Some(JS),
        "go" => Some(GO),
        _ => None,
    }
}

#[async_trait]
impl Tool for SymbolSearchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: SYMBOL_SEARCH_TOOL_NAME.into(),
            description: "Find where symbols (functions, types, classes) are defined in \
                          this workspace. Rust, Python, JavaScript/TypeScript and Go. \
                          Scans on demand — results are always fresh, output capped."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Name substring, case-insensitive"},
                    "kind": {"type": "string", "description": "Optional kind filter (fn, struct, class, …)"}
                },
                "required": ["query"]
            }),
        }
    }

    async fn execute(&self, args: Value, ctx: &mut ToolCtx<'_>) -> ToolOutput {
        let Some(query) = args["query"].as_str() else {
            return ToolOutput::error("missing `query`");
        };
        if query.is_empty() {
            return ToolOutput::error("`query` is empty");
        }
        let kind = args["kind"]
            .as_str()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let needle = query.to_ascii_lowercase();

        // Compile once per (ext, pattern): the same regex is reused per line.
        let mut cache: std::collections::HashMap<String, Vec<(String, regex::Regex)>> =
            std::collections::HashMap::new();
        let mut hits: Vec<String> = Vec::new();
        let mut files = 0usize;
        let mut scanned = 0usize;
        'files: for rel in ctx.workspace.list_files(MAX_FILES) {
            files += 1;
            let ext = rel.rsplit('.').next().unwrap_or_default().to_string();
            let Some(pats) = patterns_for(&ext) else {
                continue;
            };
            let compiled = cache.entry(ext.clone()).or_insert_with(|| {
                pats.iter()
                    .filter_map(|(k, p)| regex::Regex::new(p).ok().map(|r| (k.to_string(), r)))
                    .collect()
            });
            if compiled.is_empty() {
                continue;
            }
            let Ok(meta) = std::fs::metadata(ctx.workspace.root().join(&rel)) else {
                continue;
            };
            if meta.len() > MAX_FILE_BYTES {
                continue;
            }
            let Ok(source) = std::fs::read_to_string(ctx.workspace.root().join(&rel)) else {
                continue;
            };
            scanned += 1;
            for (lineno, line) in source.lines().enumerate() {
                for (kind_name, re) in compiled.iter() {
                    if !kind.is_empty() && kind_name.as_str() != kind {
                        continue;
                    }
                    if let Some(caps) = re.captures(line) {
                        let Some(name) = caps.get(1) else { continue };
                        if name.as_str().to_ascii_lowercase().contains(&needle) {
                            hits.push(format!(
                                "{rel}:{}  {:<7} {}",
                                lineno + 1,
                                kind_name.as_str(),
                                name.as_str()
                            ));
                            if hits.len() >= MAX_RESULTS {
                                break 'files;
                            }
                        }
                    }
                }
            }
        }
        if hits.is_empty() {
            return ToolOutput::text(format!(
                "no definitions matching “{query}” ({files} files listed, {scanned} scanned)"
            ));
        }
        let n = hits.len();
        let mut out = hits.join("\n");
        if n == MAX_RESULTS {
            out.push_str(&format!("\n… capped at {MAX_RESULTS} — refine the query"));
        }
        ToolOutput::text(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::ArtifactStore;
    use harness_workspace::Workspace;

    fn ctx_for(dir: &std::path::Path) -> (ArtifactStore, Workspace) {
        (
            ArtifactStore::new(dir.join(".harness/artifacts/t"), "t".into()).unwrap(),
            Workspace::resolve(dir).unwrap(),
        )
    }

    #[tokio::test]
    async fn finds_definitions_across_languages_and_kinds() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("a.rs"),
            "pub fn alpha_one() {}\nfn helper() {}\npub struct Widget {};\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("b.py"),
            "def alpha_two():\n    pass\nclass Other:\n    pass\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("c.ts"),
            "export const alphaThree = (x) => x + 1;\n",
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("node_modules/pkg")).unwrap();
        std::fs::write(
            tmp.path().join("node_modules/pkg/d.rs"),
            "fn alpha_hidden() {}\n",
        )
        .unwrap();

        let (store, ws) = ctx_for(tmp.path());
        let mut c = harness_core::ToolCtx {
            workspace: &ws,
            gate: &mut NoGate,
            artifacts: &store,
            bus: None,
        };
        let out = SymbolSearchTool
            .execute(json!({"query": "alpha"}), &mut c)
            .await;
        assert!(!out.is_error);
        assert!(
            out.content.contains("a.rs:1  fn      alpha_one"),
            "{}",
            out.content
        );
        assert!(
            out.content.contains("b.py:1  def     alpha_two"),
            "{}",
            out.content
        );
        assert!(
            out.content.contains("c.ts:1  const   alphaThree"),
            "{}",
            out.content
        );
        assert!(!out.content.contains("alpha_hidden"), "ignore dirs skipped");
        assert!(!out.content.contains("helper"), "non-matches excluded");

        let out = SymbolSearchTool
            .execute(json!({"query": "widget", "kind": "struct"}), &mut c)
            .await;
        assert!(out.content.contains("Widget"), "{}", out.content);
    }

    struct NoGate;
    impl harness_core::Gate for NoGate {
        fn check_bash(&mut self, _cmd: &str) -> harness_core::GateDecision {
            harness_core::GateDecision::Allow
        }
        fn check_write(&mut self, _path: &str) -> harness_core::GateDecision {
            harness_core::GateDecision::Allow
        }
    }
}
