//! Agent Client Protocol over stdio (`--acp`): editors drive the harness.
//!
//! A translation layer, not a second implementation. ACP on the outside, our
//! own wire protocol to the daemon on the inside — so an editor session
//! inherits everything the daemon already does: resident sessions, MCP tools,
//! the approval gates, journals. One daemon connection per ACP session, because
//! the daemon holds one session per connection.
//!
//! Mapped: `initialize`, `authenticate`, `session/new`, `session/load`
//! (replays the journal), `session/prompt` (streaming), `session/cancel`, and
//! `session/request_permission` for gate questions. Tool traffic becomes
//! `tool_call` updates; thinking becomes `agent_thought_chunk`. ACP's
//! client-provided `fs/*` and `terminal/*` capabilities are not used — the
//! harness's own confined tools do that work — and notes (status chatter) are
//! dropped rather than shown as assistant text.

use anyhow::{Context, Result};
use hasp_wire::Wire;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Stdout};
use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::{Mutex, mpsc, oneshot};

/// How long an editor has to answer a permission question before we decline.
/// Nobody approves at their terminal forever, and the alternative is a wedged
/// turn in the editor.
const PERMISSION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// The daemon inherits our environment, so the provider/model it will use is
/// whatever this process was launched with.
fn provider_name_env() -> String {
    hasp_workspace::env_var("HASP_PROVIDER").unwrap_or_else(|_| "anthropic".into())
}

fn model_env() -> String {
    std::env::var("ANTHROPIC_MODEL")
        .unwrap_or_else(|_| hasp_provider::default_model(&provider_name_env()).to_string())
}

type Perms = Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>;

struct Session {
    lines: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    wr: OwnedWriteHalf,
    next_id: u64,
    id: String,
}

impl Session {
    async fn send(&mut self, method: &str, params: Value) -> Result<u64> {
        self.next_id += 1;
        let id = self.next_id;
        let line = hasp_wire::encode(&Wire::request(id, method, params));
        self.wr.write_all(line.as_bytes()).await?;
        self.wr.flush().await?;
        Ok(id)
    }
}

/// Writes ACP messages; one writer, so a mutex is enough.
#[derive(Clone)]
struct Out(Arc<Mutex<Stdout>>);

impl Out {
    async fn write(&self, msg: &Value) -> Result<()> {
        let mut line = serde_json::to_string(msg)?;
        line.push('\n');
        let mut out = self.0.lock().await;
        out.write_all(line.as_bytes()).await?;
        out.flush().await?;
        Ok(())
    }

    async fn ok(&self, id: Option<u64>, result: Value) -> Result<()> {
        self.write(&json!({"jsonrpc": "2.0", "id": id, "result": result}))
            .await
    }

    async fn err(&self, id: Option<u64>, code: i64, message: impl Into<String>) -> Result<()> {
        self.write(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": code, "message": message.into()}
        }))
        .await
    }

    async fn update(&self, session_id: &str, update: Value) -> Result<()> {
        self.write(&json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {"sessionId": session_id, "update": update}
        }))
        .await
    }
}

/// Turn one of our `session/update` notifications into ACP updates.
async fn forward_update(out: &Out, session_id: &str, kind: &str, body: &str) -> Result<()> {
    match kind {
        "delta" => {
            out.update(
                session_id,
                json!({"sessionUpdate": "agent_message_chunk",
                       "content": {"type": "text", "text": body}}),
            )
            .await
        }
        "thinking" => {
            out.update(
                session_id,
                json!({"sessionUpdate": "agent_thought_chunk",
                       "content": {"type": "text", "text": body}}),
            )
            .await
        }
        "tool" => {
            // The daemon sends this when a tool has finished, so the update
            // carries the result rather than an in-progress marker.
            let ev: Value = serde_json::from_str(body).unwrap_or(Value::Null);
            let name = ev["name"].as_str().unwrap_or("tool");
            let subject = ev["args_preview"].as_str().unwrap_or_default();
            let title = if subject.is_empty() {
                name.to_string()
            } else {
                format!("{name}: {subject}")
            };
            let output = ev["content_preview"].as_str().unwrap_or_default();
            let is_error = ev["is_error"].as_bool().unwrap_or(false);
            out.update(
                session_id,
                json!({
                    "sessionUpdate": "tool_call",
                    "toolCallId": format!("{name}-{}", ev["ms"].as_u64().unwrap_or(0)),
                    "title": title,
                    "kind": "other",
                    "status": if is_error { "failed" } else { "completed" },
                    "content": [{"type": "content",
                                 "content": {"type": "text", "text": output}}],
                }),
            )
            .await
        }
        // Notes are the harness talking to itself (status chatter); an editor
        // shows the conversation, so they are dropped rather than injected.
        _ => Ok(()),
    }
}

/// Read from the daemon until `want` comes back, dispatching whatever arrives
/// in the meantime: streamed updates go to the editor, permission questions
/// become ACP requests whose answers travel back over the socket.
async fn await_reply(
    session: &mut Session,
    want: u64,
    out: &Out,
    perms: &Perms,
    next_acp_req: &mut u64,
) -> Result<Value> {
    loop {
        let line = match session.lines.next_line().await? {
            Some(l) => l,
            None => anyhow::bail!("daemon connection closed"),
        };
        let Some(msg) = hasp_wire::decode(&line) else {
            continue;
        };
        match msg {
            Wire::Response { id, result, error } => {
                if id != want {
                    continue;
                }
                return match (result, error) {
                    (Some(r), _) => Ok(r),
                    (None, Some(e)) => anyhow::bail!("{}", e.message),
                    _ => Ok(Value::Null),
                };
            }
            Wire::Notification { method, params } if method == "session/update" => {
                let kind = params["kind"].as_str().unwrap_or_default().to_string();
                let body = params["text"].as_str().unwrap_or_default().to_string();
                forward_update(out, &session.id, &kind, &body).await?;
            }
            Wire::Request { id, method, params } if method == "permission/request" => {
                let question = params["question"]
                    .as_str()
                    .unwrap_or("approve?")
                    .to_string();
                *next_acp_req += 1;
                let acp_id = *next_acp_req;
                let (tx, rx) = oneshot::channel();
                perms.lock().await.insert(acp_id, tx);
                out.write(&json!({
                    "jsonrpc": "2.0",
                    "id": acp_id,
                    "method": "session/request_permission",
                    "params": {
                        "sessionId": session.id,
                        "toolCall": {"toolCallId": format!("perm-{acp_id}"), "title": question},
                        "options": [
                            {"optionId": "allow", "name": "Allow once", "kind": "allow_once"},
                            {"optionId": "allow_always", "name": "Always allow", "kind": "allow_always"},
                            {"optionId": "reject", "name": "Reject", "kind": "reject_once"}
                        ]
                    }
                }))
                .await?;
                let answer = match tokio::time::timeout(PERMISSION_TIMEOUT, rx).await {
                    Ok(Ok(v)) => match v["result"]["outcome"]["optionId"].as_str() {
                        Some("allow") => 'y',
                        Some("allow_always") => 'a',
                        _ => 'n',
                    },
                    // No answer (or a closed editor): deny, like a detached
                    // daemon does.
                    _ => 'n',
                };
                let reply = hasp_wire::encode(&Wire::Response {
                    id,
                    result: Some(json!({"answer": answer.to_string()})),
                    error: None,
                });
                session.wr.write_all(reply.as_bytes()).await?;
                session.wr.flush().await?;
            }
            _ => {}
        }
    }
}

fn prompt_text(params: &Value) -> String {
    params["prompt"]
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| b["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// ACP `image` content blocks, in the daemon wire's shape.
fn prompt_images(params: &Value) -> Vec<Value> {
    params["prompt"]
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b["type"] == "image")
                .filter_map(|b| {
                    Some(json!({
                        "mediaType": b["mimeType"].as_str()?,
                        "data": b["data"].as_str()?,
                    }))
                })
                .collect()
        })
        .unwrap_or_default()
}

pub async fn serve() -> Result<()> {
    let out = Out(Arc::new(Mutex::new(tokio::io::stdout())));
    let perms: Perms = Arc::new(Mutex::new(HashMap::new()));
    let (tx, mut rx) = mpsc::unbounded_channel::<Value>();

    // stdin: requests in, permission answers back. A separate task so the
    // handler below can await daemon replies without holding up the editor.
    {
        let perms = perms.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(tokio::io::stdin()).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if msg.get("method").is_none() {
                    if let Some(id) = msg["id"].as_u64()
                        && let Some(tx) = perms.lock().await.remove(&id)
                    {
                        let _ = tx.send(msg);
                    }
                    continue;
                }
                if tx.send(msg).is_err() {
                    break;
                }
            }
        });
    }

    let mut sessions: Vec<Session> = Vec::new();
    let mut next_acp_req = 0u64;
    let mut initialized = false;

    while let Some(msg) = rx.recv().await {
        let id = msg["id"].as_u64();
        let method = msg["method"].as_str().unwrap_or_default().to_string();
        let params = msg["params"].clone();
        match method.as_str() {
            "initialize" => {
                initialized = true;
                out.ok(
                    id,
                    json!({
                        "protocolVersion": 1,
                        "agentCapabilities": {
                            "loadSession": true,
                            "promptCapabilities": {
                                "image": true, "audio": false, "embeddedContext": false
                            }
                        },
                        "agentInfo": {
                            "name": "hasp",
                            "version": env!("CARGO_PKG_VERSION")
                        },
                        "authMethods": []
                    }),
                )
                .await?;
            }
            "authenticate" => out.ok(id, json!({})).await?,
            "session/new" | "session/load" => {
                if !initialized {
                    out.err(id, -32002, "initialize first").await?;
                    continue;
                }
                let cwd = params["cwd"]
                    .as_str()
                    .map(std::path::PathBuf::from)
                    .unwrap_or(std::env::current_dir()?);
                let open = if method == "session/new" {
                    hasp_wire::METHOD_SESSION_NEW
                } else {
                    hasp_wire::METHOD_SESSION_CONTINUE
                };
                match open_session(&cwd, open, &out, &perms, &mut next_acp_req).await {
                    Ok(mut session) => {
                        let sid = session.id.clone();
                        // Replaying the journal is what `load` promises.
                        if method == "session/load"
                            && let Err(e) =
                                replay(&mut session, &out, &perms, &mut next_acp_req).await
                        {
                            // A failed replay must not fail the open; the
                            // editor simply starts without history.
                            eprintln!("acp: history replay failed: {e}");
                        }
                        sessions.push(session);
                        out.ok(id, json!({"sessionId": sid})).await?;
                    }
                    Err(e) => out.err(id, -32000, format!("opening session: {e}")).await?,
                }
            }
            "session/prompt" => {
                let Some(sid) = params["sessionId"].as_str().map(str::to_string) else {
                    out.err(id, -32602, "sessionId required").await?;
                    continue;
                };
                let Some(session) = sessions.iter_mut().find(|s| s.id == sid) else {
                    out.err(id, -32602, "unknown sessionId").await?;
                    continue;
                };
                let prompt = prompt_text(&params);
                let images = prompt_images(&params);
                let wire_id = session
                    .send(
                        hasp_wire::METHOD_SESSION_PROMPT,
                        json!({"prompt": prompt, "images": images}),
                    )
                    .await?;
                match await_reply(session, wire_id, &out, &perms, &mut next_acp_req).await {
                    Ok(result) => {
                        let stop = if result["aborted"].as_bool().unwrap_or(false) {
                            "cancelled"
                        } else {
                            "end_turn"
                        };
                        out.ok(id, json!({"stopReason": stop})).await?;
                    }
                    Err(e) => out.err(id, -32000, format!("turn failed: {e}")).await?,
                }
            }
            "session/cancel" => {
                if let Some(sid) = params["sessionId"].as_str()
                    && let Some(session) = sessions.iter_mut().find(|s| s.id == sid)
                {
                    session
                        .send(hasp_wire::METHOD_SESSION_CANCEL, json!({}))
                        .await?;
                }
                // ACP cancel is a notification; no reply either way.
            }
            other => {
                out.err(id, -32601, format!("method not supported: {other}"))
                    .await?;
            }
        }
    }
    Ok(())
}

async fn open_session(
    cwd: &std::path::Path,
    open_method: &str,
    out: &Out,
    perms: &Perms,
    next_acp_req: &mut u64,
) -> Result<Session> {
    let sock = crate::daemon::socket_path()?;
    let env = vec![
        ("HASP_PROVIDER".to_string(), provider_name_env()),
        ("ANTHROPIC_MODEL".to_string(), model_env()),
    ];
    crate::daemon::ensure_daemon(&sock, &env).await?;
    let stream = UnixStream::connect(&sock)
        .await
        .with_context(|| format!("connecting to daemon at {}", sock.display()))?;
    let (rd, wr) = stream.into_split();
    let mut session = Session {
        lines: BufReader::new(rd).lines(),
        wr,
        next_id: 0,
        id: String::new(),
    };
    let init = session
        .send(hasp_wire::METHOD_INITIALIZE, json!({"client": "acp"}))
        .await?;
    await_reply(&mut session, init, out, perms, next_acp_req).await?;
    let open = session
        .send(open_method, json!({"cwd": cwd.to_string_lossy()}))
        .await?;
    let opened = await_reply(&mut session, open, out, perms, next_acp_req).await?;
    session.id = opened["sessionId"].as_str().unwrap_or("?").to_string();
    Ok(session)
}

/// `session/load`: put the conversation the editor missed on screen.
async fn replay(
    session: &mut Session,
    out: &Out,
    perms: &Perms,
    next_acp_req: &mut u64,
) -> Result<()> {
    let id = session
        .send(hasp_wire::METHOD_SESSION_HISTORY, json!({}))
        .await?;
    let history = await_reply(session, id, out, perms, next_acp_req).await?;
    for m in history["messages"].as_array().into_iter().flatten() {
        let text = m["text"].as_str().unwrap_or_default();
        if text.is_empty() {
            continue;
        }
        let kind = if m["role"].as_str() == Some("user") {
            "user_message_chunk"
        } else {
            "agent_message_chunk"
        };
        out.update(
            &session.id.clone(),
            json!({"sessionUpdate": kind, "content": {"type": "text", "text": text}}),
        )
        .await?;
    }
    Ok(())
}
