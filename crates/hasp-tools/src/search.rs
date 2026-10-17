//! `grep` and `glob`: native, gitignore-aware search over the workspace —
//! ripgrep's own walker and matchers, in process. The model's most frequent
//! action is looking for things; through `bash` that costs an approval the
//! moment it pipes (`rg x | head` trips the metacharacter rule), a process
//! spawn, and unbounded output. Here it is read-only, confined, capped, and
//! safe to run in parallel.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use async_trait::async_trait;
use hasp_core::{Tool, ToolCtx, ToolOutput, ToolSpec};
use serde_json::{Value, json};

pub const GREP_TOOL_NAME: &str = "grep";
pub const GLOB_TOOL_NAME: &str = "glob";

const DEFAULT_HEAD_LIMIT: usize = 250;
const MAX_FILE_BYTES: u64 = 5_000_000;
const MAX_LINE_CHARS: usize = 400;
/// Hard ceiling on returned text, whatever `head_limit` says.
const MAX_OUTPUT_BYTES: usize = 48 * 1024;
/// Never descend into these, even when no ignore file says so.
const ALWAYS_SKIP: &[&str] = &[".git", ".hasp", ".harness", "node_modules"];

/// A glob in ripgrep's sense: without a `/` it matches the file name at any
/// depth (`*.rs`), with one it matches the path from the search root.
fn build_glob(pattern: &str) -> Result<(globset::GlobMatcher, bool), String> {
    let by_name = !pattern.contains('/');
    let glob = globset::GlobBuilder::new(pattern)
        .literal_separator(!by_name)
        .build()
        .map_err(|e| format!("bad glob `{pattern}`: {e}"))?;
    Ok((glob.compile_matcher(), by_name))
}

fn glob_matches(m: &(globset::GlobMatcher, bool), rel: &Path) -> bool {
    if m.1 {
        rel.file_name().is_some_and(|n| m.0.is_match(n))
    } else {
        m.0.is_match(rel)
    }
}

/// Files under `base`, gitignore-aware, symlinks not followed (confinement),
/// hidden files included except the always-skipped directories.
fn walk_files(base: &Path) -> impl Iterator<Item = PathBuf> {
    ignore::WalkBuilder::new(base)
        .hidden(false)
        .follow_links(false)
        .require_git(false)
        .filter_entry(|e| {
            !(e.file_type().is_some_and(|t| t.is_dir())
                && ALWAYS_SKIP.iter().any(|s| e.file_name() == *s))
        })
        .build()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
        .map(|e| e.into_path())
}

fn mtime(p: &Path) -> SystemTime {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// Workspace-relative display path.
fn rel_display(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .into_owned()
}

fn clip_line(line: &str) -> String {
    if line.chars().count() <= MAX_LINE_CHARS {
        return line.to_string();
    }
    let cut: String = line.chars().take(MAX_LINE_CHARS).collect();
    format!("{cut}… [line truncated]")
}

/// Resolve the optional `path` argument to a confined absolute path.
fn search_base(args: &Value, ctx: &ToolCtx<'_>) -> Result<PathBuf, String> {
    let rel = args["path"].as_str().unwrap_or(".");
    let rel = if rel.is_empty() { "." } else { rel };
    ctx.workspace.confine(rel).map_err(|e| e.to_string())
}

pub struct GrepTool;

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Files,
    Content,
    Count,
}

struct GrepArgs {
    re: regex::Regex,
    base: PathBuf,
    root: PathBuf,
    glob: Option<(globset::GlobMatcher, bool)>,
    mode: Mode,
    context: usize,
    head_limit: usize,
}

fn run_grep(a: GrepArgs) -> String {
    let files: Vec<PathBuf> = if a.base.is_file() {
        vec![a.base.clone()]
    } else {
        walk_files(&a.base)
            .filter(|p| {
                a.glob
                    .as_ref()
                    .is_none_or(|g| glob_matches(g, p.strip_prefix(&a.base).unwrap_or(p)))
            })
            .collect()
    };
    // (file, numbered lines to show, match count)
    type Hit = (PathBuf, Vec<(usize, String)>, usize);
    let mut hits: Vec<Hit> = Vec::new();
    for path in files {
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_FILE_BYTES) {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        if bytes.iter().take(8192).any(|&b| b == 0) {
            continue; // binary
        }
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.lines().collect();
        let matched: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| a.re.is_match(l))
            .map(|(i, _)| i)
            .collect();
        if matched.is_empty() {
            continue;
        }
        let count = matched.len();
        let mut shown = Vec::new();
        if a.mode == Mode::Content {
            let mut last: Option<usize> = None;
            for &m in &matched {
                let from = m.saturating_sub(a.context);
                let to = (m + a.context).min(lines.len() - 1);
                for (i, text) in lines.iter().enumerate().take(to + 1).skip(from) {
                    if last.is_some_and(|l| i <= l) {
                        continue;
                    }
                    if last.is_some_and(|l| i > l + 1) {
                        shown.push((usize::MAX, String::new())); // group break
                    }
                    let sep = if i == m || matched.binary_search(&i).is_ok() {
                        ':'
                    } else {
                        '-'
                    };
                    shown.push((i + 1, format!("{sep}{}", clip_line(text))));
                    last = Some(i);
                }
            }
        }
        hits.push((path, shown, count));
    }
    // Most recently modified first: what is being worked on is usually what
    // is being looked for.
    hits.sort_by_key(|(p, _, _)| std::cmp::Reverse(mtime(p)));

    if hits.is_empty() {
        return "no matches".into();
    }
    let total_files = hits.len();
    let mut out = String::new();
    let mut emitted = 0usize;
    let mut truncated = false;
    'files: for (path, shown, count) in &hits {
        let rel = rel_display(&a.root, path);
        match a.mode {
            Mode::Files => {
                if emitted >= a.head_limit || out.len() > MAX_OUTPUT_BYTES {
                    truncated = true;
                    break;
                }
                out.push_str(&rel);
                out.push('\n');
                emitted += 1;
            }
            Mode::Count => {
                if emitted >= a.head_limit || out.len() > MAX_OUTPUT_BYTES {
                    truncated = true;
                    break;
                }
                out.push_str(&format!("{rel}:{count}\n"));
                emitted += 1;
            }
            Mode::Content => {
                for (n, line) in shown {
                    if emitted >= a.head_limit || out.len() > MAX_OUTPUT_BYTES {
                        truncated = true;
                        break 'files;
                    }
                    if *n == usize::MAX {
                        out.push_str("--\n");
                        continue;
                    }
                    out.push_str(&format!("{rel}:{n}{line}\n"));
                    emitted += 1;
                }
            }
        }
    }
    if truncated {
        out.push_str(&format!(
            "[results truncated at {emitted} {}; {total_files} file(s) matched — narrow the pattern, \
             `path` or `glob`, or raise head_limit]\n",
            if a.mode == Mode::Content { "lines" } else { "entries" }
        ));
    }
    out.trim_end().to_string()
}

#[async_trait]
impl Tool for GrepTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: GREP_TOOL_NAME.into(),
            description: "Search file contents with a regular expression (Rust regex syntax), \
                          respecting .gitignore. Prefer this over running grep/rg through bash. \
                          Default output lists matching files, most recently modified first; \
                          `output_mode: \"content\"` shows matching lines as path:line:text."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Regular expression to search for"},
                    "path": {"type": "string", "description": "File or directory to search, relative to the project root (default: the root)"},
                    "glob": {"type": "string", "description": "Only search files matching this glob, e.g. \"*.rs\" or \"src/**/*.ts\""},
                    "output_mode": {"type": "string", "enum": ["files_with_matches", "content", "count"], "description": "Default files_with_matches"},
                    "context": {"type": "integer", "description": "Lines of context around each match (content mode)"},
                    "case_insensitive": {"type": "boolean"},
                    "head_limit": {"type": "integer", "description": "Max entries or lines to return (default 250)"}
                },
                "required": ["pattern"]
            }),
        }
    }

    fn concurrency_safe(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, ctx: &mut ToolCtx<'_>) -> ToolOutput {
        let Some(pattern) = args["pattern"].as_str() else {
            return ToolOutput::error("missing `pattern`");
        };
        let re = match regex::RegexBuilder::new(pattern)
            .case_insensitive(args["case_insensitive"].as_bool().unwrap_or(false))
            .size_limit(10 << 20)
            .build()
        {
            Ok(r) => r,
            Err(e) => return ToolOutput::error(format!("bad regex: {e}")),
        };
        let base = match search_base(&args, ctx) {
            Ok(b) => b,
            Err(e) => return ToolOutput::error(e),
        };
        if !base.exists() {
            return ToolOutput::error(format!(
                "no such path: {}",
                args["path"].as_str().unwrap_or(".")
            ));
        }
        let glob = match args["glob"].as_str().filter(|g| !g.is_empty()) {
            Some(g) => match build_glob(g) {
                Ok(m) => Some(m),
                Err(e) => return ToolOutput::error(e),
            },
            None => None,
        };
        let mode = match args["output_mode"].as_str() {
            Some("content") => Mode::Content,
            Some("count") => Mode::Count,
            _ => Mode::Files,
        };
        let a = GrepArgs {
            re,
            base,
            root: ctx.workspace.root().to_path_buf(),
            glob,
            mode,
            context: args["context"].as_u64().unwrap_or(0).min(20) as usize,
            head_limit: args["head_limit"]
                .as_u64()
                .map(|n| n.max(1) as usize)
                .unwrap_or(DEFAULT_HEAD_LIMIT),
        };
        match tokio::task::spawn_blocking(move || run_grep(a)).await {
            Ok(out) => ToolOutput::text(out),
            Err(e) => ToolOutput::error(format!("search failed: {e}")),
        }
    }
}

pub struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: GLOB_TOOL_NAME.into(),
            description: "Find files by name pattern, respecting .gitignore; most recently \
                          modified first. A pattern without `/` matches file names at any depth \
                          (\"*.rs\"); with one it matches paths from `path` (\"src/**/*.test.ts\")."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string"},
                    "path": {"type": "string", "description": "Directory to search from, relative to the project root (default: the root)"},
                    "head_limit": {"type": "integer", "description": "Max paths to return (default 250)"}
                },
                "required": ["pattern"]
            }),
        }
    }

    fn concurrency_safe(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, ctx: &mut ToolCtx<'_>) -> ToolOutput {
        let Some(pattern) = args["pattern"].as_str() else {
            return ToolOutput::error("missing `pattern`");
        };
        let matcher = match build_glob(pattern) {
            Ok(m) => m,
            Err(e) => return ToolOutput::error(e),
        };
        let base = match search_base(&args, ctx) {
            Ok(b) => b,
            Err(e) => return ToolOutput::error(e),
        };
        let limit = args["head_limit"]
            .as_u64()
            .map(|n| n.max(1) as usize)
            .unwrap_or(DEFAULT_HEAD_LIMIT);
        let root = ctx.workspace.root().to_path_buf();
        let found = tokio::task::spawn_blocking(move || {
            let mut files: Vec<PathBuf> = walk_files(&base)
                .filter(|p| glob_matches(&matcher, p.strip_prefix(&base).unwrap_or(p)))
                .collect();
            files.sort_by_key(|p| std::cmp::Reverse(mtime(p)));
            let total = files.len();
            let mut out: Vec<String> = files
                .iter()
                .take(limit)
                .map(|p| rel_display(&root, p))
                .collect();
            if total > limit {
                out.push(format!(
                    "[{total} files matched; showing the {limit} most recent]"
                ));
            }
            out
        })
        .await;
        match found {
            Ok(v) if v.is_empty() => ToolOutput::text("no files matched"),
            Ok(v) => ToolOutput::text(v.join("\n")),
            Err(e) => ToolOutput::error(format!("glob failed: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hasp_core::{ArtifactStore, RefuseGate};
    use hasp_workspace::Workspace;

    fn ctx_in<'a>(path: &Path, gate: &'a mut RefuseGate) -> ToolCtx<'a> {
        let ws: &'static Workspace = Box::leak(Box::new(Workspace::resolve(path).unwrap()));
        let artifacts: &'static ArtifactStore = Box::leak(Box::new(
            ArtifactStore::new(path.join(".hasp/artifacts/test"), "test".into()).unwrap(),
        ));
        ToolCtx {
            workspace: ws,
            gate,
            artifacts,
            bus: None,
        }
    }

    fn fixture() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path();
        std::fs::create_dir_all(p.join("src/deep")).unwrap();
        std::fs::create_dir_all(p.join("target")).unwrap();
        std::fs::write(p.join(".gitignore"), "target/\n").unwrap();
        std::fs::write(p.join("src/lib.rs"), "fn alpha() {}\nfn beta() {}\n").unwrap();
        std::fs::write(p.join("src/deep/mod.rs"), "// alpha here\nlet x = 1;\n").unwrap();
        std::fs::write(p.join("target/gen.rs"), "fn alpha() {}\n").unwrap();
        std::fs::write(p.join("notes.md"), "ALPHA in caps\n").unwrap();
        std::fs::write(p.join("blob.bin"), b"alpha\0\x01\x02").unwrap();
        tmp
    }

    #[tokio::test]
    async fn grep_respects_gitignore_skips_binaries_and_filters_by_glob() {
        let tmp = fixture();
        let mut gate = RefuseGate;
        let mut ctx = ctx_in(tmp.path(), &mut gate);
        let out = GrepTool
            .execute(json!({"pattern": "alpha"}), &mut ctx)
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("src/lib.rs"));
        assert!(out.content.contains("src/deep/mod.rs"));
        assert!(
            !out.content.contains("target/"),
            "gitignored: {}",
            out.content
        );
        assert!(!out.content.contains("blob.bin"), "binary: {}", out.content);
        assert!(
            !out.content.contains("notes.md"),
            "case-sensitive by default"
        );

        let out = GrepTool
            .execute(
                json!({"pattern": "alpha", "case_insensitive": true, "glob": "*.md"}),
                &mut ctx,
            )
            .await;
        assert_eq!(out.content, "notes.md");
    }

    #[tokio::test]
    async fn grep_content_mode_numbers_lines_with_context() {
        let tmp = fixture();
        let mut gate = RefuseGate;
        let mut ctx = ctx_in(tmp.path(), &mut gate);
        let out = GrepTool
            .execute(
                json!({"pattern": "beta", "output_mode": "content", "context": 1, "path": "src/lib.rs"}),
                &mut ctx,
            )
            .await;
        assert_eq!(
            out.content,
            "src/lib.rs:1-fn alpha() {}\nsrc/lib.rs:2:fn beta() {}"
        );
    }

    #[tokio::test]
    async fn grep_is_confined_and_limited() {
        let tmp = fixture();
        let mut gate = RefuseGate;
        let mut ctx = ctx_in(tmp.path(), &mut gate);
        let out = GrepTool
            .execute(json!({"pattern": "x", "path": "../"}), &mut ctx)
            .await;
        assert!(out.is_error);
        let out = GrepTool
            .execute(
                json!({"pattern": ".", "output_mode": "content", "head_limit": 1}),
                &mut ctx,
            )
            .await;
        assert!(
            out.content.contains("results truncated at 1 lines"),
            "{}",
            out.content
        );
    }

    #[tokio::test]
    async fn glob_matches_names_anywhere_or_paths_from_base() {
        let tmp = fixture();
        let mut gate = RefuseGate;
        let mut ctx = ctx_in(tmp.path(), &mut gate);
        let out = GlobTool.execute(json!({"pattern": "*.rs"}), &mut ctx).await;
        let mut lines: Vec<&str> = out.content.lines().collect();
        lines.sort();
        assert_eq!(lines, vec!["src/deep/mod.rs", "src/lib.rs"]);

        let out = GlobTool
            .execute(json!({"pattern": "deep/*.rs", "path": "src"}), &mut ctx)
            .await;
        assert_eq!(out.content, "src/deep/mod.rs");
    }
}
