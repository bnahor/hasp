//! TUI backends: the embedded engine (`--no-daemon`) and the wire client
//! (default, daemon-backed). Both speak the same JSON method contract the
//! daemon defines — one code path in the TUI, and the engine implementation
//! doubles as the reference for every response shape.

use anyhow::{Result, anyhow, bail};
use hasp_core::Sink;
use hasp_core::engine::{Canceller, Engine, TurnSink, post_turn_check_shared, run_turn_shared};
use hasp_core::session::SessionManager;
use hasp_core::{AgentMessage, ContentBlock};
use hasp_provider::catalog;
use hasp_wire::Wire;
use hasp_workspace::Workspace;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::gate::PromptRequest;
use crate::tui::{DoneKind, UiEvent};

pub trait Backend: Send {
    /// Fire a turn; completion arrives as `UiEvent::Done`.
    fn submit_prompt(&mut self, prompt: String);
    /// Fire a compaction; completion arrives as `UiEvent::Done`.
    fn submit_compact(&mut self);
    /// Cancel the in-flight turn.
    fn cancel(&mut self);
    /// Blocking call for non-provider operations (UI thread, under block_in_place).
    fn call(&mut self, method: &str, params: Value) -> Result<Value>;
}

// ---- embedded (reference implementation) ----

/// Context size for the UI: the provider's own count when it still describes
/// the context, else the estimate, plus which of the two it was.
pub fn ctx_json(eng: &Engine) -> Value {
    let (tokens, measured) = eng.context_tokens();
    json!({"tokens": tokens, "measured": measured})
}

/// Recent user/assistant texts for replay when a session opens. The full
/// transcript stays in the journal; this is the tail the UI shows.
pub fn history_json(session: &SessionManager) -> Value {
    // Tool traffic is not replayed — only the words exchanged.
    let texts: Vec<(bool, String)> = session
        .context_messages()
        .iter()
        .filter_map(|m| match m {
            AgentMessage::User { content, .. } => Some((true, content.clone())),
            AgentMessage::Assistant { content, .. } => Some((
                false,
                content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            )),
            AgentMessage::ToolResult { .. } => None,
        })
        .collect();
    let total = texts.len();
    let shown: Vec<Value> = texts
        .iter()
        .skip(total.saturating_sub(HISTORY_LIMIT))
        .map(|(is_user, text)| {
            json!({
                "role": if *is_user { "user" } else { "assistant" },
                "text": text,
            })
        })
        .collect();
    json!({"total": total, "messages": shown})
}

/// How many prior messages the UI replays on open.
pub const HISTORY_LIMIT: usize = 30;

/// Shared shape for `goal/set` and `goal/status` in both backends.
pub fn goal_status(eng: &Engine) -> Value {
    let goal = eng.session.goal();
    let auto = eng.session.autonomous();
    json!({
        "goal": goal.as_ref().map(|g| json!({
            "text": g.text,
            "budget": g.budget_tokens,
            "paused": g.paused,
            "completed": g.completed,
        })),
        "autonomous": auto.as_ref().filter(|a| a.on).map(|a| json!({
            "maxTurns": a.max_turns,
            "maxTokens": a.max_tokens,
            "gate": a.gate,
        })),
    })
}

pub struct EngineBackend {
    engine: Arc<Mutex<Engine>>,
    handle: tokio::runtime::Handle,
    event_tx: std::sync::mpsc::Sender<UiEvent>,
    canceller: Canceller,
    worktree_stack: Vec<(Workspace, String)>,
    /// Held for a whole turn; the engine lock only for moments (see
    /// `hasp_core::engine`), so UI calls answer mid-turn instead of
    /// freezing the interface until the model finishes.
    turn: Arc<Mutex<()>>,
}

impl EngineBackend {
    pub fn new(
        engine: Arc<Mutex<Engine>>,
        handle: tokio::runtime::Handle,
        event_tx: std::sync::mpsc::Sender<UiEvent>,
        canceller: Canceller,
    ) -> Self {
        Self {
            engine,
            handle,
            event_tx,
            canceller,
            worktree_stack: Vec::new(),
            turn: Arc::new(Mutex::new(())),
        }
    }
}

impl Backend for EngineBackend {
    fn submit_prompt(&mut self, prompt: String) {
        self.canceller.reset();
        let engine = self.engine.clone();
        let turn = self.turn.clone();
        let tx = self.event_tx.clone();
        self.handle.spawn(async move {
            let _turn = turn.lock().await;
            let cell: &Mutex<Engine> = &engine;
            let mut sink = EventSink { tx: tx.clone() };
            let mut res = run_turn_shared(cell, &prompt, Vec::new(), &mut sink).await;
            // Goals and autonomous mode continue the work without a new prompt.
            while res.is_ok() {
                match post_turn_check_shared(cell, &mut sink).await {
                    Some(cont) => res = run_turn_shared(cell, &cont, Vec::new(), &mut sink).await,
                    None => break,
                }
            }
            let _ = match res {
                Ok(stats) => tx.send(UiEvent::Done(Ok(DoneKind::Turn(stats)))),
                Err(e) => tx.send(UiEvent::Done(Err(e.to_string()))),
            };
        });
    }

    fn submit_compact(&mut self) {
        let engine = self.engine.clone();
        let turn = self.turn.clone();
        let tx = self.event_tx.clone();
        self.handle.spawn(async move {
            let _turn = turn.lock().await;
            let res = hasp_core::engine::compact_shared(&*engine).await;
            let _ = match res {
                Ok(s) => tx.send(UiEvent::Done(Ok(DoneKind::Compact(s)))),
                Err(e) => tx.send(UiEvent::Done(Err(e.to_string()))),
            };
        });
    }

    fn cancel(&mut self) {
        self.canceller.cancel();
    }

    fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        // Requests that swap or rewind the session wait for no turn: they are
        // refused while one runs, and hold the turn lock while they work.
        let restructures = matches!(
            method,
            "session/new" | "session/load" | "session/worktree" | "session/worktree-exit"
        ) || (method == "session/branch" && params["target"].is_string());
        let _turn = if restructures {
            match self.turn.clone().try_lock_owned() {
                Ok(g) => Some(g),
                Err(_) => bail!("a turn is running — wait for it, or cancel it (esc), then retry"),
            }
        } else {
            None
        };
        let mut eng = self.engine.blocking_lock();
        match method {
            "session/new" => {
                let s = SessionManager::create(&eng.workspace)?;
                let reply = json!({"sessionId": s.header.id, "path": s.path.as_ref().map(|p| p.display().to_string()).unwrap_or_default()});
                eng.session = s;
                Ok(reply)
            }
            "session/load" => {
                let path = params["path"]
                    .as_str()
                    .ok_or_else(|| anyhow!("path required"))?;
                let s = SessionManager::open(PathBuf::from(path).as_path())?;
                let reply = json!({"sessionId": s.header.id, "path": path});
                eng.session = s;
                Ok(reply)
            }
            "session/history" => Ok(history_json(&eng.session)),
            "session/stats" => {
                let totals = eng.session.totals();
                Ok(json!({
                    "sessionId": eng.session.header.id,
                    "model": eng.model,
                    "root": eng.workspace.root().display().to_string(),
                    "branch": eng.workspace.identity().branch,
                    "window": eng.provider.context_window(),
                    "entries": eng.session.entries().len(),
                    "input": totals.total_in(),
                    "cacheRead": totals.cache_read,
                    "cacheWrite": totals.cache_write,
                    "output": totals.output,
                    "estTokens": eng.context_est_tokens(),
                    "ctx": ctx_json(&eng),
                }))
            }
            "session/name" => {
                let name = params["name"]
                    .as_str()
                    .ok_or_else(|| anyhow!("name required"))?;
                eng.session.append_session_info(name);
                Ok(json!({"named": true}))
            }
            "model/set" => {
                let model = params["model"]
                    .as_str()
                    .ok_or_else(|| anyhow!("model required"))?;
                // Dynamic providers serve model ids the static catalog has
                // never heard of; their list comes from the endpoint.
                if catalog::find(&catalog::load(), model).is_none()
                    && !hasp_provider::is_dynamic(eng.provider.name())
                {
                    return Err(anyhow!("unknown model {model}"));
                }
                eng.model = model.to_string();
                eng.max_tokens = hasp_provider::max_output_for(model);
                let provider = eng.provider.name();
                eng.session.append_model_change(provider, model);
                Ok(json!({"model": model}))
            }
            "goal/set" => {
                let action = params["action"].as_str().unwrap_or("set");
                let text = params["text"].as_str().unwrap_or_default();
                let budget = params["budget"].as_u64();
                eng.set_goal(action, text, budget);
                Ok(goal_status(&eng))
            }
            "goal/status" => Ok(goal_status(&eng)),
            "autonomous/set" => {
                let on = params["on"].as_bool().unwrap_or(false);
                eng.set_autonomous(
                    on,
                    params["maxTurns"].as_u64(),
                    params["maxTokens"].as_u64(),
                    params["gate"].as_str(),
                );
                Ok(json!({"on": on}))
            }
            "heartbeat/set" | "heartbeat/list" => Err(anyhow!(
                "heartbeats need daemon mode (they must fire while no client is attached)"
            )),
            "workspace/files" => {
                let limit = params["limit"].as_u64().unwrap_or(4000) as usize;
                Ok(json!({"files": eng.workspace.list_files(limit)}))
            }
            "session/listings" => {
                let list = SessionManager::list_for(&eng.workspace)?;
                Ok(Value::Array(
                    list.iter()
                        .map(|s| {
                            json!({
                                "createdMs": s.created_ms,
                                "id": s.id,
                                "name": s.name,
                                "entries": s.message_entries,
                                "path": s.path.display().to_string(),
                            })
                        })
                        .collect(),
                ))
            }
            "session/branch" => match params["target"].as_str() {
                None => Ok(Value::Array(
                    eng.session
                        .branch_points()
                        .into_iter()
                        .map(|(id, label)| json!({"id": id, "label": label}))
                        .collect(),
                )),
                Some(target) => Ok(json!({"moved": eng.session.branch(target)})),
            },
            "session/fork" => {
                let fork = eng.session.clone_session(&eng.workspace)?;
                Ok(json!({
                    "sessionId": fork.header.id,
                    "path": fork.path.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
                }))
            }
            "session/worktree" => {
                let name: String = params["name"]
                    .as_str()
                    .unwrap_or_default()
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
                    .collect();
                let name = if name.is_empty() {
                    format!("wt-{}", hasp_core::session::now_ms() % 100_000)
                } else {
                    name
                };
                let ws2 = eng.workspace.create_worktree(&name)?;
                let session = SessionManager::create(&ws2)?;
                let reply = json!({
                    "name": name,
                    "root": ws2.root().display().to_string(),
                    "sessionId": session.header.id,
                });
                self.worktree_stack.push((eng.workspace.clone(), name));
                eng.workspace = ws2;
                eng.session = session;
                Ok(reply)
            }
            "session/worktree-exit" => {
                let Some((old_ws, name)) = self.worktree_stack.pop() else {
                    return Err(anyhow!("not in a worktree"));
                };
                if params["remove"].as_bool().unwrap_or(false) {
                    old_ws.remove_worktree(&name).map_err(|e| anyhow!("{e}"))?;
                }
                let session = SessionManager::open_latest(&old_ws)?.unwrap_or_else(|| {
                    SessionManager::create(&old_ws).expect("session dir writable")
                });
                let reply = json!({
                    "root": old_ws.root().display().to_string(),
                    "sessionId": session.header.id,
                });
                eng.workspace = old_ws;
                eng.session = session;
                Ok(reply)
            }
            other => Err(anyhow!("unknown method {other}")),
        }
    }
}

struct EventSink {
    tx: std::sync::mpsc::Sender<UiEvent>,
}

impl Sink for EventSink {
    fn on_thinking(&mut self, text: &str) {
        let _ = self.tx.send(UiEvent::Thinking(text.to_string()));
    }

    fn on_delta(&mut self, text: &str) {
        let _ = self.tx.send(UiEvent::Delta(text.to_string()));
    }
}

impl TurnSink for EventSink {
    fn on_note(&mut self, note: &str) {
        let _ = self.tx.send(UiEvent::Note(note.to_string()));
    }

    fn on_tool(&mut self, ev: &hasp_core::engine::ToolEvent) {
        let _ = self.tx.send(UiEvent::Tool(ev.clone()));
    }
}

// ---- wire (daemon-backed) ----

enum Cmd {
    Call {
        method: String,
        params: Value,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    Prompt {
        text: String,
    },
    Compact,
    Cancel,
    AnswerPermission {
        id: u64,
        answer: char,
    },
}

pub struct WireBackend {
    cmd_tx: mpsc::UnboundedSender<Cmd>,
}

impl WireBackend {
    /// Connects, performs the initialize + session-open handshake, then runs
    /// the connection actor. The session lives exactly as long as this
    /// connection — `open_method` is `session/continue` or `session/new`.
    pub async fn spawn(
        socket: PathBuf,
        open_method: &str,
        cwd: &std::path::Path,
        event_tx: std::sync::mpsc::Sender<UiEvent>,
        prompt_tx: std::sync::mpsc::Sender<PromptRequest>,
    ) -> Result<Self> {
        let stream = UnixStream::connect(&socket)
            .await
            .map_err(|e| anyhow!("connect: {e}"))?;
        let (rd, mut wr) = stream.into_split();
        let mut rd = BufReader::new(rd);

        let init = hasp_wire::encode(&Wire::request(
            1,
            hasp_wire::METHOD_INITIALIZE,
            json!({"client": "tui", "protocol": hasp_wire::PROTOCOL_VERSION}),
        ));
        wr.write_all(init.as_bytes()).await?;
        wr.flush().await?;
        read_response(&mut rd, 1).await?;

        let open = hasp_wire::encode(&Wire::request(
            2,
            open_method,
            json!({"cwd": cwd.to_string_lossy(), "environment": crate::session_environment()}),
        ));
        wr.write_all(open.as_bytes()).await?;
        wr.flush().await?;
        let opened = read_response(&mut rd, 2).await?;
        let sid = opened["sessionId"].as_str().unwrap_or("?").to_string();
        let _ = event_tx.send(UiEvent::Note(format!(
            "▪ session {sid} via daemon at {}",
            socket.display()
        )));
        if opened["adopted"].as_bool() == Some(true) {
            let _ = event_tx.send(UiEvent::Note(
                "▪ attached to the running session — it kept going while you were away".into(),
            ));
        }

        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        tokio::spawn(actor(
            rd,
            wr,
            3,
            cmd_rx,
            cmd_tx.clone(),
            event_tx,
            prompt_tx,
        ));
        Ok(Self { cmd_tx })
    }
}

async fn read_response(
    rd: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
    id: u64,
) -> Result<Value> {
    loop {
        let mut line = String::new();
        if rd.read_line(&mut line).await? == 0 {
            return Err(anyhow!("daemon closed during handshake"));
        }
        if let Some(Wire::Response {
            id: rid,
            result,
            error,
        }) = hasp_wire::decode(&line)
            && rid == id
        {
            if let Some(e) = error {
                return Err(anyhow!("{}: {}", e.code, e.message));
            }
            return Ok(result.unwrap_or(Value::Null));
        }
    }
}

impl Backend for WireBackend {
    fn submit_prompt(&mut self, prompt: String) {
        let _ = self.cmd_tx.send(Cmd::Prompt { text: prompt });
    }

    fn submit_compact(&mut self) {
        let _ = self.cmd_tx.send(Cmd::Compact);
    }

    fn cancel(&mut self) {
        let _ = self.cmd_tx.send(Cmd::Cancel);
    }

    fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(Cmd::Call {
                method: method.to_string(),
                params,
                reply: tx,
            })
            .map_err(|_| anyhow!("connection actor stopped"))?;
        match rx.blocking_recv() {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(anyhow!("{e}")),
            Err(_) => Err(anyhow!("connection actor dropped the call")),
        }
    }
}

async fn write_msg(wr: &mut tokio::net::unix::OwnedWriteHalf, msg: &Wire) {
    let _ = wr.write_all(hasp_wire::encode(msg).as_bytes()).await;
    let _ = wr.flush().await;
}

async fn actor(
    mut rd: BufReader<tokio::net::unix::OwnedReadHalf>,
    mut wr: tokio::net::unix::OwnedWriteHalf,
    mut next_id: u64,
    mut cmd_rx: mpsc::UnboundedReceiver<Cmd>,
    cmd_tx: mpsc::UnboundedSender<Cmd>,
    event_tx: std::sync::mpsc::Sender<UiEvent>,
    prompt_tx: std::sync::mpsc::Sender<PromptRequest>,
) {
    let mut pending: HashMap<u64, oneshot::Sender<Result<Value, String>>> = HashMap::new();
    let mut turn_id: Option<u64> = None;
    let mut compact_id: Option<u64> = None;

    loop {
        let mut line = String::new();
        tokio::select! {
            read = rd.read_line(&mut line) => {
                match read {
                    Ok(0) | Err(_) => {
                        let _ = event_tx.send(UiEvent::Done(Err("connection to daemon lost".into())));
                        for (_, tx) in pending.drain() { let _ = tx.send(Err("daemon disconnected".into())); }
                        break;
                    }
                    Ok(_) => {
                        let Some(msg) = hasp_wire::decode(&line) else { continue };
                        match msg {
                            Wire::Response { id, result, error } => {
                                if Some(id) == turn_id {
                                    turn_id = None;
                                    let _ = event_tx.send(UiEvent::Done(match error {
                                        None => Ok(DoneKind::Turn(parse_turn(result.as_ref()))),
                                        Some(e) => Err(format!("{}: {}", e.code, e.message)),
                                    }));
                                } else if Some(id) == compact_id {
                                    compact_id = None;
                                    let _ = event_tx.send(UiEvent::Done(match error {
                                        None => Ok(DoneKind::Compact(parse_compact(result.as_ref()))),
                                        Some(e) => Err(format!("{}: {}", e.code, e.message)),
                                    }));
                                } else if let Some(tx) = pending.remove(&id) {
                                    let _ = tx.send(match error {
                                        None => Ok(result.unwrap_or(Value::Null)),
                                        Some(e) => Err(format!("{}: {}", e.code, e.message)),
                                    });
                                }
                            }
                            Wire::Request { id, method, params } => {
                                if method == hasp_wire::METHOD_PERMISSION_REQUEST {
                                    let (respond, rx) = std::sync::mpsc::channel::<char>();
                                    let question =
                                        params["question"].as_str().unwrap_or("approve?").to_string();
                                    let _ = prompt_tx.send(PromptRequest { question, respond });
                                    // A watcher thread bridges the UI's answer
                                    // back into this actor's command stream.
                                    let cmd_tx = cmd_tx.clone();
                                    std::thread::spawn(move || {
                                        if let Ok(answer) = rx.recv() {
                                            let _ = cmd_tx.send(Cmd::AnswerPermission { id, answer });
                                        }
                                    });
                                }
                            }
                            Wire::Notification { method, params } => {
                                if method == hasp_wire::METHOD_SESSION_UPDATE {
                                    let text = params["text"].as_str().unwrap_or_default().to_string();
                                    match params["kind"].as_str().unwrap_or("") {
                                        "delta" => { let _ = event_tx.send(UiEvent::Delta(text)); }
                                        // Tool events ride the update channel as a
                                        // serialized payload; anything unparsable
                                        // still shows up as a plain note.
                                        "thinking" => {
                                            let _ = event_tx.send(UiEvent::Thinking(text));
                                        }
                                        "tool" => match serde_json::from_str(&text) {
                                            Ok(ev) => { let _ = event_tx.send(UiEvent::Tool(ev)); }
                                            Err(_) => { let _ = event_tx.send(UiEvent::Note(text)); }
                                        },
                                        _ => { let _ = event_tx.send(UiEvent::Note(text)); }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { break };
                next_id += 1;
                let id = next_id;
                match cmd {
                    Cmd::Call { method, params, reply } => {
                        pending.insert(id, reply);
                        write_msg(&mut wr, &Wire::request(id, &method, params)).await;
                    }
                    Cmd::Prompt { text } => {
                        turn_id = Some(id);
                        write_msg(&mut wr, &Wire::request(id, hasp_wire::METHOD_SESSION_PROMPT, json!({"prompt": text}))).await;
                    }
                    Cmd::Compact => {
                        compact_id = Some(id);
                        write_msg(&mut wr, &Wire::request(id, hasp_wire::METHOD_SESSION_COMPACT, json!({}))).await;
                    }
                    Cmd::Cancel => {
                        write_msg(&mut wr, &Wire::request(id, hasp_wire::METHOD_SESSION_CANCEL, json!({}))).await;
                    }
                    Cmd::AnswerPermission { id, answer } => {
                        write_msg(&mut wr, &Wire::ok(id, json!({"answer": answer.to_string()}))).await;
                    }
                }
            }
        }
    }
}

fn parse_turn(r: Option<&Value>) -> hasp_core::engine::TurnStats {
    let v = r.cloned().unwrap_or_default();
    hasp_core::engine::TurnStats {
        samples: v["samples"].as_u64().unwrap_or(0) as usize,
        tool_calls: v["toolCalls"].as_u64().unwrap_or(0) as usize,
        usage: hasp_core::Usage {
            input: v["input"].as_u64().unwrap_or(0),
            cache_read: v["cacheRead"].as_u64().unwrap_or(0),
            cache_write: v["cacheWrite"].as_u64().unwrap_or(0),
            output: v["output"].as_u64().unwrap_or(0),
        },
        ms: v["ms"].as_u64().unwrap_or(0) as u128,
        aborted: v["aborted"].as_bool().unwrap_or(false),
    }
}

fn parse_compact(r: Option<&Value>) -> hasp_core::compaction::CompactStats {
    let v = r.cloned().unwrap_or_default();
    hasp_core::compaction::CompactStats {
        tokens_before: v["tokensBefore"].as_u64().unwrap_or(0),
        summary_tokens: v["summaryTokens"].as_u64().unwrap_or(0),
        retained_messages: v["retained"].as_u64().unwrap_or(0) as usize,
    }
}
