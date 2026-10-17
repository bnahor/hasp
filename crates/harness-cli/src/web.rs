//! The desktop app's server (`harness app`, `harness web [port]`).
//!
//! Still just another client of the daemon: one daemon connection per open
//! project (the daemon keeps one resident session per workspace), each with
//! its own event stream, plus a few read-only views the daemon has no reason
//! to serve — the session list across every project (read from the session
//! journals), a finished transcript, and the working tree's changes (through
//! `harness-review` and `harness-diff`). Approvals, which the old page could
//! only deny, are asked in the page and answered back.
//!
//! Localhost only, and every route requires the token printed at startup, so a
//! stray page in another tab cannot drive a session.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use harness_core::session::SessionManager;
use harness_core::{AgentMessage, ContentBlock};
use harness_wire::Wire;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{TcpListener, TcpStream, UnixStream};
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};

const PAGE: &str = include_str!("../assets/app.html");

/// Lines of one file's diff sent to the page; the rest is summarized.
const DIFF_LINES_PER_FILE: usize = 4000;

/// Something for a project's connection to do.
enum Cmd {
    Call {
        method: String,
        params: Value,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    /// Answer a daemon permission request by its request id.
    Answer { id: u64, answer: String },
}

/// One open project: its daemon connection and its event stream.
struct Project {
    cmd: mpsc::UnboundedSender<Cmd>,
    events: broadcast::Sender<Value>,
    /// Permission questions not answered yet, re-sent to a page that
    /// (re)connects so an approval is never lost to a reload.
    asks: Arc<std::sync::Mutex<HashMap<u64, String>>>,
}

type Projects = Arc<Mutex<HashMap<PathBuf, Arc<Project>>>>;

async fn send(wr: &mut OwnedWriteHalf, msg: &Wire) -> Result<()> {
    wr.write_all(harness_wire::encode(msg).as_bytes()).await?;
    wr.flush().await?;
    Ok(())
}

/// Connect, initialize and open (or join) the project's resident session.
async fn open_project(root: &Path) -> Result<Arc<Project>> {
    let sock = crate::daemon::socket_path()?;
    crate::daemon::ensure_daemon(&sock, &[]).await?;
    let stream = UnixStream::connect(&sock)
        .await
        .with_context(|| format!("connecting to daemon at {}", sock.display()))?;
    let (rd, mut wr) = stream.into_split();
    let mut lines = BufReader::new(rd).lines();
    let mut next_id = 0u64;
    for (method, params) in [
        (harness_wire::METHOD_INITIALIZE, json!({"client": "app"})),
        (
            harness_wire::METHOD_SESSION_CONTINUE,
            json!({"cwd": root.to_string_lossy()}),
        ),
    ] {
        next_id += 1;
        let id = next_id;
        send(&mut wr, &Wire::request(id, method, params)).await?;
        let answered = tokio::time::timeout(std::time::Duration::from_secs(60), async {
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(Wire::Response {
                    id: rid,
                    result,
                    error,
                }) = harness_wire::decode(&line)
                    && rid == id
                {
                    return match (result, error) {
                        (None, Some(e)) => Err(e.message),
                        _ => Ok(()),
                    };
                }
            }
            Err("daemon closed before answering".to_string())
        })
        .await;
        match answered {
            Ok(Ok(())) => {}
            Ok(Err(e)) => bail!("daemon refused {method}: {e}"),
            Err(_) => bail!("daemon did not answer {method}"),
        }
    }

    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<Cmd>();
    let (events, _) = broadcast::channel::<Value>(1024);
    let asks: Arc<std::sync::Mutex<HashMap<u64, String>>> = Default::default();
    let project = Arc::new(Project {
        cmd: cmd_tx,
        events: events.clone(),
        asks: asks.clone(),
    });
    tokio::spawn(async move {
        let mut pending: HashMap<u64, oneshot::Sender<Result<Value, String>>> = HashMap::new();
        loop {
            tokio::select! {
                cmd = cmd_rx.recv() => match cmd {
                    None => return,
                    Some(Cmd::Call { method, params, reply }) => {
                        next_id += 1;
                        if send(&mut wr, &Wire::request(next_id, &method, params)).await.is_err() {
                            let _ = reply.send(Err("daemon connection lost".into()));
                            return;
                        }
                        pending.insert(next_id, reply);
                    }
                    Some(Cmd::Answer { id, answer }) => {
                        asks.lock().expect("asks").remove(&id);
                        let _ = send(&mut wr, &Wire::Response {
                            id,
                            result: Some(json!({"answer": answer})),
                            error: None,
                        }).await;
                        let _ = events.send(json!({"method": "permission/answered", "params": {"id": id}}));
                    }
                },
                line = lines.next_line() => {
                    let Ok(Some(line)) = line else {
                        let _ = events.send(json!({"method": "app/disconnected", "params": {}}));
                        return;
                    };
                    match harness_wire::decode(&line) {
                        Some(Wire::Response { id, result, error }) => {
                            if let Some(tx) = pending.remove(&id) {
                                let _ = tx.send(match (result, error) {
                                    (Some(r), _) => Ok(r),
                                    (None, Some(e)) => Err(e.message),
                                    _ => Ok(Value::Null),
                                });
                            }
                        }
                        Some(Wire::Notification { method, params }) => {
                            let _ = events.send(json!({"method": method, "params": params}));
                        }
                        Some(Wire::Request { id, method, params }) => {
                            let question = params["question"].as_str().unwrap_or("allow?").to_string();
                            asks.lock().expect("asks").insert(id, question.clone());
                            let _ = events.send(json!({
                                "method": method,
                                "params": {"id": id, "question": question},
                            }));
                        }
                        None => {}
                    }
                }
            }
        }
    });
    Ok(project)
}

async fn project(projects: &Projects, root: &str) -> Result<Arc<Project>> {
    if root.is_empty() {
        bail!("root required");
    }
    // Resolve to the workspace root so two paths into one repo share one entry.
    let ws = harness_workspace::Workspace::resolve(Path::new(root))?;
    let key = ws.root().to_path_buf();
    let mut map = projects.lock().await;
    if let Some(p) = map.get(&key)
        && !p.cmd.is_closed()
    {
        return Ok(p.clone());
    }
    let p = open_project(&key).await?;
    map.insert(key, p.clone());
    Ok(p)
}

async fn call(p: &Project, method: &str, params: Value) -> Result<Value, String> {
    let (tx, rx) = oneshot::channel();
    p.cmd
        .send(Cmd::Call {
            method: method.to_string(),
            params,
            reply: tx,
        })
        .map_err(|_| "project connection is gone".to_string())?;
    // A turn can run for hours (goals, autonomous mode); everything else is quick.
    let limit = if method == harness_wire::METHOD_SESSION_PROMPT {
        24 * 3600
    } else {
        600
    };
    match tokio::time::timeout(std::time::Duration::from_secs(limit), rx).await {
        Ok(Ok(r)) => r,
        Ok(Err(_)) => Err("project connection ended".into()),
        Err(_) => Err("timed out".into()),
    }
}

// ---- read-only views ----

/// Every saved session, newest first, grouped by project root.
fn list_projects(keep: &str) -> Result<Value> {
    let root = harness_workspace::sessions_root()?;
    let mut sessions: Vec<(i64, Value, String)> = Vec::new();
    for dir in std::fs::read_dir(&root).into_iter().flatten().flatten() {
        for f in std::fs::read_dir(dir.path())
            .into_iter()
            .flatten()
            .flatten()
        {
            let path = f.path();
            if path.extension().is_none_or(|e| e != "jsonl") {
                continue;
            }
            let modified = f
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            if let Some((summary, ws_root)) = summarize(&path, modified) {
                sessions.push((modified, summary, ws_root));
            }
        }
    }
    sessions.sort_by_key(|s| std::cmp::Reverse(s.0));
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<Value>> = HashMap::new();
    for (_, s, ws_root) in sessions {
        if !groups.contains_key(&ws_root) {
            order.push(ws_root.clone());
        }
        let g = groups.entry(ws_root).or_default();
        if g.len() < 50 {
            g.push(s);
        }
    }
    let projects: Vec<Value> = order
        .into_iter()
        .filter(|r| {
            // The launch project, and worktrees made from it, stay listed
            // even where scratch directories are otherwise hidden.
            let kept = r == keep
                || worktree_of(Path::new(r)).is_some_and(|(p, _)| p.to_string_lossy() == keep);
            Path::new(r).is_dir() && (kept || !is_scratch(Path::new(r)))
        })
        .map(|r| {
            let name = Path::new(&r)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| r.clone());
            let sessions = groups.remove(&r).unwrap_or_default();
            let wt = worktree_of(Path::new(&r));
            json!({
                "root": r,
                "name": name,
                "sessions": sessions,
                "parent": wt.as_ref().map(|(p, _)| p.display().to_string()),
            })
        })
        .collect();
    Ok(json!({"projects": projects}))
}

/// Throwaway directories (tests, one-off experiments) under the system temp
/// dir: real projects do not live there, and they would bury the ones that do.
fn is_scratch(root: &Path) -> bool {
    let tmp = std::env::temp_dir();
    let tmp = tmp.canonicalize().unwrap_or(tmp);
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    root.starts_with(&tmp)
        || [
            "/tmp",
            "/private/tmp",
            "/var/folders",
            "/private/var/folders",
        ]
        .iter()
        .any(|t| root.starts_with(t))
}

/// A session's sidebar row, without parsing the whole journal: the header,
/// its latest name, and the first thing the user asked as a fallback title.
fn summarize(path: &Path, modified: i64) -> Option<(Value, String)> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next()?).ok()?;
    let ws_root = header["workspace"]["root"].as_str()?.to_string();
    let mut name: Option<String> = None;
    let mut title: Option<String> = None;
    let mut messages = 0usize;
    for line in lines {
        if line.contains("\"type\":\"session_info\"") {
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                name = v["name"].as_str().map(String::from);
            }
        } else if line.contains("\"type\":\"message\"") {
            messages += 1;
            if title.is_none()
                && line.contains("\"role\":\"user\"")
                && let Ok(v) = serde_json::from_str::<Value>(line)
            {
                title = v["message"]["content"]
                    .as_str()
                    .map(|t| t.lines().next().unwrap_or("").chars().take(80).collect());
            }
        }
    }
    if messages == 0 {
        return None;
    }
    let title = name.or(title).unwrap_or_else(|| "Untitled".into());
    Some((
        json!({
            "id": header["id"],
            "path": path.display().to_string(),
            "title": title,
            "branch": header["workspace"]["branch"],
            "modified": modified,
            "messages": messages,
        }),
        ws_root,
    ))
}

/// A saved session's conversation, structured for the transcript view.
fn transcript(path: &Path) -> Result<Value> {
    let s = SessionManager::open(path)?;
    let mut items = Vec::new();
    let mut calls: HashMap<String, (String, Value)> = HashMap::new();
    for m in s.context_messages() {
        match m {
            AgentMessage::User {
                content,
                images,
                ts,
            } => items
                .push(json!({"kind": "user", "text": content, "images": images.len(), "ts": ts})),
            AgentMessage::Assistant {
                content, model, ts, ..
            } => {
                for b in content {
                    match b {
                        ContentBlock::Text { text } if !text.trim().is_empty() => items.push(
                            json!({"kind": "assistant", "text": text, "model": model, "ts": ts}),
                        ),
                        ContentBlock::Thinking { thinking, .. } if !thinking.trim().is_empty() => {
                            items.push(json!({"kind": "thinking", "text": thinking}))
                        }
                        ContentBlock::ToolCall {
                            id,
                            name,
                            arguments,
                        } => {
                            calls.insert(id, (name, arguments));
                        }
                        _ => {}
                    }
                }
            }
            AgentMessage::ToolResult {
                tool_call_id,
                tool_name,
                content,
                is_error,
                ts,
                ..
            } => {
                let args = calls
                    .remove(&tool_call_id)
                    .map(|(_, a)| a)
                    .unwrap_or(Value::Null);
                items.push(json!({
                    "kind": "tool",
                    "name": tool_name,
                    "subject": subject_of(&args),
                    "output": clip(&content, 20_000),
                    "isError": is_error,
                    "ts": ts,
                }));
            }
        }
    }
    Ok(json!({"id": s.header.id, "items": items}))
}

fn subject_of(args: &Value) -> String {
    for k in ["command", "path", "pattern", "url", "query"] {
        if let Some(s) = args[k].as_str() {
            return s.to_string();
        }
    }
    match args {
        Value::Null => String::new(),
        other => clip(&other.to_string(), 200),
    }
}

fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… ({} more bytes)", &s[..end], s.len() - end)
}

/// The working tree against HEAD, as the review engine diffs it.
/// A worktree this harness made — `<parent>/.harness/worktrees/<name>` — as
/// its parent root and name.
fn worktree_of(root: &Path) -> Option<(PathBuf, String)> {
    let name = root.file_name()?.to_str()?.to_string();
    let worktrees = root.parent()?;
    let dot = worktrees.parent()?;
    if worktrees.file_name()? != "worktrees" || dot.file_name()? != ".harness" {
        return None;
    }
    Some((dot.parent()?.to_path_buf(), name))
}

/// Read-only, bounded browsing through the same confinement as agent tools.
fn browse(root: &Path, rel: &str, preview: bool) -> Result<Value> {
    use std::io::Read;

    if root.as_os_str().is_empty() || !root.is_dir() {
        bail!("a workspace directory is required");
    }
    let ws = harness_workspace::Workspace::resolve(root)?;
    let path = ws.confine(rel)?;
    if preview {
        const LIMIT: u64 = 256 * 1024;
        let meta = std::fs::metadata(&path)?;
        if !meta.is_file() {
            bail!("only regular files can be previewed");
        }
        let mut bytes = Vec::new();
        std::fs::File::open(&path)?
            .take(LIMIT + 1)
            .read_to_end(&mut bytes)?;
        let truncated = bytes.len() as u64 > LIMIT;
        bytes.truncate(LIMIT as usize);
        let text = std::str::from_utf8(&bytes);
        // A bounded read can split the final UTF-8 character.
        let binary = bytes.contains(&0)
            || text
                .as_ref()
                .is_err_and(|e| !truncated || e.error_len().is_some());
        return Ok(json!({
            "path": rel, "size": meta.len(), "binary": binary, "truncated": truncated,
            "text": if binary { None } else { Some(String::from_utf8_lossy(&bytes)) },
        }));
    }
    const LIMIT: usize = 5000;
    let mut entries = Vec::new();
    let mut truncated = false;
    for entry in std::fs::read_dir(&path)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name == ".git" {
            continue;
        }
        if entries.len() == LIMIT {
            truncated = true;
            break;
        }
        let relative = Path::new(rel).join(name).to_string_lossy().to_string();
        // Do not follow links outside the workspace, or expose special files.
        let Ok(confined) = ws.confine(&relative) else {
            continue;
        };
        let Ok(meta) = std::fs::metadata(confined) else {
            continue;
        };
        if !meta.is_file() && !meta.is_dir() {
            continue;
        }
        entries.push(json!({"name": name, "path": relative, "directory": meta.is_dir()}));
    }
    entries.sort_by(|a, b| {
        b["directory"]
            .as_bool()
            .cmp(&a["directory"].as_bool())
            .then_with(|| a["name"].as_str().cmp(&b["name"].as_str()))
    });
    Ok(json!({"entries": entries, "truncated": truncated}))
}

/// The working tree against HEAD — or, in a worktree, against where its
/// branch left the parent, so committed work shows too — as the review
/// engine diffs it.
fn changes(root: &Path) -> Result<Value> {
    use harness_review::{diff, git};
    let head = match git::head(root) {
        Ok(h) => h,
        Err(_) => return Ok(json!({"files": [], "note": "not a git repository with a commit"})),
    };
    let wt = worktree_of(root);
    let base = match &wt {
        Some((parent, name)) => {
            git::git(parent, &["merge-base", "HEAD", &format!("harness/{name}")]).unwrap_or(head)
        }
        None => head,
    };
    let snap = git::snapshot(root, "harness app: working tree")?;
    let files = diff::between(root, &base, &snap)?;
    let out: Vec<Value> = files
        .iter()
        .map(|f| {
            let mut shown = 0usize;
            let hunks: Vec<Value> = f
                .hunks
                .iter()
                .map_while(|h| {
                    if shown >= DIFF_LINES_PER_FILE {
                        return None;
                    }
                    let lines: Vec<Value> = h
                        .lines
                        .iter()
                        .take(DIFF_LINES_PER_FILE - shown)
                        .map(|l| {
                            let kind = match l.kind {
                                diff::Kind::Add => "add",
                                diff::Kind::Del => "del",
                                diff::Kind::Context => "ctx",
                            };
                            json!([kind, l.old, l.new, l.text])
                        })
                        .collect();
                    shown += lines.len();
                    Some(json!({"header": h.header(), "lines": lines}))
                })
                .collect();
            json!({
                "path": f.path,
                "note": f.note,
                "added": f.stats.added,
                "removed": f.stats.removed,
                "hunks": hunks,
                "truncated": shown >= DIFF_LINES_PER_FILE,
            })
        })
        .collect();
    let worktree = wt.map(|(parent, name)| {
        let branch = git::git(&parent, &["symbolic-ref", "--quiet", "--short", "HEAD"]).ok();
        json!({"parent": parent.display().to_string(), "name": name, "into": branch})
    });
    Ok(json!({"files": out, "worktree": worktree}))
}

/// A new worktree off `root`'s HEAD, for a session that runs beside the ones
/// already working there.
fn create_worktree(root: &Path, name: &str) -> Result<Value> {
    let ws = harness_workspace::Workspace::resolve(root)?;
    if worktree_of(ws.root()).is_some() {
        bail!("already in a worktree — make it from the main checkout");
    }
    let name: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    let name = if name.is_empty() {
        format!("wt-{}", harness_core::session::now_ms() % 100_000)
    } else {
        name
    };
    let wt = ws.create_worktree(&name)?;
    Ok(json!({
        "root": wt.root().display().to_string(),
        "name": name,
        "branch": format!("harness/{name}"),
    }))
}

/// Land what the Changes pane shows, through the review engine: committed on
/// HEAD in a workspace, merged into the parent from a worktree. The click is
/// the approval, so the snapshot taken now is exactly what lands.
fn land(root: &Path, message: &str) -> Result<Value> {
    use harness_review::store::Target;
    use harness_review::{Reviews, Verdict, git};
    let root = root.canonicalize()?;
    let (repo, target) = match worktree_of(&root) {
        Some((parent, name)) => (parent, Target::Worktree { name }),
        None => (root.clone(), Target::Workspace),
    };
    let message = if message.trim().is_empty() {
        "Changes from harness"
    } else {
        message.trim()
    };
    let reviews = Reviews::new(&repo);
    let s = reviews.open(message, target)?;
    if git::tree_of(&repo, s.latest())? == git::tree_of(&repo, &s.base)? {
        bail!("nothing to land — no changes since {}", git::short(&s.base));
    }
    reviews.verdict(&s, Verdict::Approve)?;
    let s = reviews.load(&s.id)?;
    let commit = reviews.land(&s)?;
    Ok(json!({"commit": commit, "into": repo.display().to_string(), "review": s.id}))
}

/// Remove a worktree. Safe without asking when it holds nothing that has not
/// landed: its state now is the snapshot its last review merged. Anything
/// else needs `discard` — the caller has confirmed losing it. The branch
/// stays either way, so committed work is never lost.
fn remove_worktree(root: &Path, discard: bool) -> Result<Value> {
    use harness_review::store::Target;
    use harness_review::{Reviews, git};
    let root = root.canonicalize()?;
    let Some((parent, name)) = worktree_of(&root) else {
        bail!("{} is not a harness worktree", root.display());
    };
    let target = Target::Worktree { name: name.clone() };
    let landed = Reviews::new(&parent)
        .list()
        .into_iter()
        .rev()
        .find(|s| s.target == target && s.landed.is_some())
        .map(|s| s.latest().to_string());
    let now = git::snapshot(&root, "harness app: before removing")?;
    let clean = match &landed {
        Some(l) => git::tree_of(&parent, l)? == git::tree_of(&parent, &now)?,
        None => git::tree_of(&parent, &now)? == git::tree_of(&parent, &format!("harness/{name}"))?,
    };
    let merged = harness_workspace::Workspace::resolve(&parent)?.worktree_merged(&name);
    if !clean && !discard {
        bail!("{name} has changes that have not landed — land them first, or discard them");
    }
    let dir = root.display().to_string();
    git::git(&parent, &["worktree", "remove", "--force", &dir])?;
    Ok(json!({
        "removed": name,
        "merged": merged || landed.is_some(),
        "discarded": !clean,
        "branch": format!("harness/{name}"),
    }))
}

fn models() -> Value {
    let list: Vec<Value> = harness_provider::catalog::load()
        .into_iter()
        .map(|m| {
            json!({
                "id": m.id,
                "window": m.context_window,
                "input": m.input_per_mtok,
                "output": m.output_per_mtok,
            })
        })
        .collect();
    json!({"models": list})
}

// ---- HTTP ----

fn token() -> String {
    // Localhost CSRF guard: unguessable enough for a page in another tab.
    let mut bytes = [0u8; 16];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut bytes))
        .is_err()
    {
        let seed = harness_core::session::now_ms() as u64 ^ (std::process::id() as u64) << 32;
        bytes[..8].copy_from_slice(&seed.to_le_bytes());
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

struct Request {
    method: String,
    path: String,
    query: HashMap<String, String>,
    token: Option<String>,
    body: Vec<u8>,
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(b);
                    i += 2;
                } else {
                    out.push(b'%');
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn read_request(stream: &mut TcpStream) -> Result<Request> {
    let mut buf = Vec::with_capacity(4096);
    let mut byte = [0u8; 1];
    // Headers end at CRLFCRLF; read a byte at a time so no body is swallowed.
    while !buf.ends_with(b"\r\n\r\n") {
        if buf.len() > 64 * 1024 {
            bail!("request head too large");
        }
        if stream.read(&mut byte).await? == 0 {
            bail!("connection closed");
        }
        buf.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&buf).to_string();
    let mut lines = head.lines();
    let mut parts = lines.next().unwrap_or_default().split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    let mut content_length = 0usize;
    let mut token = None;
    for line in lines {
        let lower = line.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        } else if lower.starts_with("x-harness-token:") {
            token = line.split_once(':').map(|(_, v)| v.trim().to_string());
        }
    }
    // Images come through here; 64 MiB is several screenshots.
    if content_length > 64 * 1024 * 1024 {
        bail!("request body too large");
    }
    let mut body = vec![0u8; content_length];
    if !body.is_empty() {
        stream.read_exact(&mut body).await?;
    }
    let (path, q) = target.split_once('?').unwrap_or((target.as_str(), ""));
    let query = q
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), url_decode(v)))
        .collect();
    Ok(Request {
        method,
        path: path.to_string(),
        query,
        token,
        body,
    })
}

async fn respond(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &str,
) -> Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\n\
         cache-control: no-store\r\nx-content-type-options: nosniff\r\nconnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

async fn json_reply(stream: &mut TcpStream, r: Result<Value, String>) -> Result<()> {
    let body = match r {
        Ok(v) => json!({"ok": true, "result": v}),
        Err(e) => json!({"ok": false, "error": e}),
    };
    respond(stream, "200 OK", "application/json", &body.to_string()).await
}

/// Bind and serve until killed. Returns never; `on_ready` gets the URL.
pub async fn serve_with(port: u16, on_ready: impl FnOnce(&str)) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| format!("binding 127.0.0.1:{port}"))?;
    let bound = listener.local_addr()?.port();
    let token = Arc::new(token());
    let cwd = std::env::current_dir()?;
    let start = harness_workspace::Workspace::resolve(&cwd)
        .map(|w| w.root().display().to_string())
        .unwrap_or_default();
    let url = format!(
        "http://127.0.0.1:{bound}/?token={token}&root={}",
        start
            .replace('%', "%25")
            .replace('&', "%26")
            .replace(' ', "%20")
    );
    on_ready(&url);
    let projects: Projects = Default::default();
    loop {
        let (mut stream, _) = listener.accept().await?;
        let token = token.clone();
        let projects = projects.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(&mut stream, &token, &projects).await {
                let _ = respond(
                    &mut stream,
                    "400 Bad Request",
                    "text/plain",
                    &format!("{e}"),
                )
                .await;
            }
        });
    }
}

/// `harness web [port]` — serve and print the URL.
pub async fn serve(port: u16) -> Result<()> {
    serve_with(port, |url| {
        use std::io::Write;
        // A closed pipe (`harness web | head -1`) must not kill the server.
        let mut out = std::io::stdout().lock();
        let _ = writeln!(
            out,
            "harness web → {url}\n  localhost only · token required"
        );
        let _ = out.flush();
    })
    .await
}

/// `harness app` — serve on a free port and open it in an app window.
pub async fn app() -> Result<()> {
    serve_with(0, |url| {
        eprintln!("harness app → {url}");
        if let Err(e) = open_window(url) {
            eprintln!("could not open a window ({e}); open the URL above in a browser");
        }
    })
    .await
}

/// A chromeless app window: a Chromium-family browser's `--app` mode when one
/// is installed (own window, no tabs or address bar), else the default browser.
fn open_window(url: &str) -> Result<()> {
    let profile = harness_workspace::harness_home()?.join("app-window");
    let app_flag = format!("--app={url}");
    let profile_flag = format!("--user-data-dir={}", profile.display());
    #[cfg(target_os = "macos")]
    {
        for app in [
            "Google Chrome",
            "Microsoft Edge",
            "Brave Browser",
            "Chromium",
            "Arc",
        ] {
            if Path::new(&format!("/Applications/{app}.app")).exists() {
                std::process::Command::new("open")
                    .args(["-na", app, "--args", &app_flag, &profile_flag])
                    .spawn()?;
                return Ok(());
            }
        }
        std::process::Command::new("open").arg(url).spawn()?;
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        for bin in [
            "google-chrome",
            "chromium",
            "chromium-browser",
            "microsoft-edge",
            "brave-browser",
        ] {
            if std::process::Command::new(bin)
                .args([&app_flag, &profile_flag])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .is_ok()
            {
                return Ok(());
            }
        }
        std::process::Command::new("xdg-open").arg(url).spawn()?;
        Ok(())
    }
}

async fn handle(stream: &mut TcpStream, token: &str, projects: &Projects) -> Result<()> {
    let req = read_request(stream).await?;
    let supplied = req
        .token
        .clone()
        .or_else(|| req.query.get("token").cloned())
        .unwrap_or_default();
    if supplied != token {
        return respond(stream, "401 Unauthorized", "text/plain", "bad token").await;
    }
    let body: Value = if req.body.is_empty() {
        json!({})
    } else {
        serde_json::from_slice(&req.body).context("body must be JSON")?
    };
    let q = |k: &str| req.query.get(k).cloned().unwrap_or_default();
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") => respond(stream, "200 OK", "text/html; charset=utf-8", PAGE).await,
        ("GET", "/health") => respond(stream, "200 OK", "application/json", r#"{"ok":true}"#).await,
        ("GET", "/api/projects") => {
            let keep = q("root");
            let r = tokio::task::spawn_blocking(move || list_projects(&keep)).await?;
            json_reply(stream, r.map_err(|e| e.to_string())).await
        }
        ("GET", "/api/transcript") => {
            let path = PathBuf::from(q("path"));
            let r = tokio::task::spawn_blocking(move || transcript(&path)).await?;
            json_reply(stream, r.map_err(|e| e.to_string())).await
        }
        ("GET", "/api/changes") => {
            let root = PathBuf::from(q("root"));
            let r = tokio::task::spawn_blocking(move || changes(&root)).await?;
            json_reply(stream, r.map_err(|e| e.to_string())).await
        }
        ("GET", "/api/files" | "/api/file") => {
            let root = PathBuf::from(q("root"));
            let path = q("path");
            let preview = req.path == "/api/file";
            let r = tokio::task::spawn_blocking(move || browse(&root, &path, preview)).await?;
            json_reply(stream, r.map_err(|e| e.to_string())).await
        }
        ("GET", "/api/skills") => {
            let root = PathBuf::from(q("root"));
            let r = tokio::task::spawn_blocking(move || -> Result<Value> {
                if root.as_os_str().is_empty() || !root.is_dir() {
                    bail!("a workspace directory is required");
                }
                let ws = harness_workspace::Workspace::resolve(&root)?;
                let skills: Vec<_> = harness_core::extensions::skills(ws.root())
                    .into_iter()
                    .map(|s| json!({"name": s.name, "description": s.description}))
                    .collect();
                Ok(json!({"skills": skills}))
            })
            .await?;
            json_reply(stream, r.map_err(|e| e.to_string())).await
        }
        ("GET", "/api/models") => json_reply(stream, Ok(models())).await,
        ("POST", "/api/worktree") => {
            let root = PathBuf::from(body["root"].as_str().unwrap_or_default());
            let name = body["name"].as_str().unwrap_or_default().to_string();
            let r = tokio::task::spawn_blocking(move || create_worktree(&root, &name)).await?;
            json_reply(stream, r.map_err(|e| e.to_string())).await
        }
        ("POST", "/api/land") => {
            let root = PathBuf::from(body["root"].as_str().unwrap_or_default());
            let message = body["message"].as_str().unwrap_or_default().to_string();
            let r = tokio::task::spawn_blocking(move || land(&root, &message)).await?;
            json_reply(stream, r.map_err(|e| format!("{e:#}"))).await
        }
        ("POST", "/api/worktree/remove") => {
            let root = PathBuf::from(body["root"].as_str().unwrap_or_default());
            let key = root.canonicalize().unwrap_or_else(|_| root.clone());
            let discard = body["discard"].as_bool().unwrap_or(false);
            let r = tokio::task::spawn_blocking(move || remove_worktree(&root, discard)).await?;
            if r.is_ok() {
                // Its daemon connection goes with it.
                projects.lock().await.remove(&key);
            }
            json_reply(stream, r.map_err(|e| e.to_string())).await
        }
        ("POST", "/api/rpc") => {
            let root = body["root"].as_str().unwrap_or_default().to_string();
            let method = body["method"].as_str().unwrap_or_default().to_string();
            let params = body.get("params").cloned().unwrap_or(json!({}));
            let r = match project(projects, &root).await {
                Ok(p) => call(&p, &method, params).await,
                Err(e) => Err(e.to_string()),
            };
            json_reply(stream, r).await
        }
        ("POST", "/api/permission") => {
            let root = body["root"].as_str().unwrap_or_default().to_string();
            let r = match project(projects, &root).await {
                Ok(p) => {
                    let id = body["id"].as_u64().unwrap_or_default();
                    let answer = match body["answer"].as_str() {
                        Some(a @ ("y" | "a" | "n")) => a.to_string(),
                        _ => "n".to_string(),
                    };
                    p.cmd
                        .send(Cmd::Answer { id, answer })
                        .map(|_| json!({"answered": id}))
                        .map_err(|_| "project connection is gone".to_string())
                }
                Err(e) => Err(e.to_string()),
            };
            json_reply(stream, r).await
        }
        ("GET", "/events") => {
            let p = project(projects, &q("root")).await?;
            let mut rx = p.events.subscribe();
            let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                        cache-control: no-store\r\nconnection: keep-alive\r\n\r\n";
            stream.write_all(head.as_bytes()).await?;
            // Questions still waiting for an answer, for a page that reloaded.
            let waiting: Vec<(u64, String)> = p
                .asks
                .lock()
                .expect("asks")
                .iter()
                .map(|(k, v)| (*k, v.clone()))
                .collect();
            for (id, question) in waiting {
                let v = json!({"method": "permission/request", "params": {"id": id, "question": question}});
                stream
                    .write_all(format!("data: {v}\n\n").as_bytes())
                    .await?;
            }
            stream.flush().await?;
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(20));
            loop {
                tokio::select! {
                    ev = rx.recv() => match ev {
                        Ok(v) => {
                            stream.write_all(format!("data: {v}\n\n").as_bytes()).await?;
                            stream.flush().await?;
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    },
                    _ = tick.tick() => {
                        stream.write_all(b": ping\n\n").await?;
                        stream.flush().await?;
                    }
                }
            }
            Ok(())
        }
        _ => respond(stream, "404 Not Found", "text/plain", "no such route").await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_values_are_decoded() {
        assert_eq!(url_decode("%2FUsers%2Fme%20x+y"), "/Users/me x y");
        assert_eq!(url_decode("100%"), "100%");
    }

    #[test]
    fn worktrees_are_recognized_by_their_place() {
        assert_eq!(
            worktree_of(Path::new("/w/repo/.harness/worktrees/wt-1")),
            Some((PathBuf::from("/w/repo"), "wt-1".into()))
        );
        assert_eq!(worktree_of(Path::new("/w/repo")), None);
        assert_eq!(worktree_of(Path::new("/w/repo/worktrees/x")), None);
    }

    #[test]
    fn tokens_are_long_and_differ() {
        let (a, b) = (token(), token());
        assert_eq!(a.len(), 32);
        assert_ne!(a, b);
    }
}
