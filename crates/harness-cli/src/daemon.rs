//! The daemon: owns engines behind a Unix socket (research/03-architecture.md
//! §8.1). v1: one engine per connection, per-workspace exclusivity,
//! permission round-trip, `session/update` fan-out to the owning client.

use anyhow::{Context, Result, anyhow};
use harness_core::engine::{Canceller, Engine, TurnSink, post_turn_check_shared, run_turn_shared};
use harness_core::session::SessionManager;
use harness_core::{AgentMessage, Bus, ContentBlock, Gate, GateDecision, Provider, Sink};
use harness_provider::catalog;
use harness_provider::echo::EchoProvider;
use harness_wire::{self, RpcError, Wire};
use harness_workspace::Workspace;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;
use tokio::sync::mpsc;

use crate::gate::GateCore;

pub fn socket_path() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("HARNESS_SOCKET") {
        return Ok(PathBuf::from(p));
    }
    Ok(harness_workspace::harness_home()
        .map_err(|e| anyhow!(e.to_string()))?
        .join("daemon.sock"))
}

pub async fn is_running(path: &std::path::Path) -> bool {
    UnixStream::connect(path).await.is_ok()
}

/// Connect or spawn: the daemon default-on decision in code. Stale socket
/// files are probed and removed before respawning.
pub async fn ensure_daemon(sock: &std::path::Path, extra_env: &[(String, String)]) -> Result<()> {
    if is_running(sock).await {
        return Ok(());
    }
    if sock.exists() {
        std::fs::remove_file(sock).context("removing stale daemon socket")?;
    }
    let exe = std::env::current_exe().context("locating the harness binary")?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--serve")
        .envs(extra_env.iter().cloned())
        .env("HARNESS_SOCKET", sock)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Own session so terminal/pty close (SIGHUP) can't take the daemon down.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn().context("spawning harness --serve")?;
    for _ in 0..100 {
        if is_running(sock).await {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    anyhow::bail!("daemon did not come up at {}", sock.display())
}

pub async fn serve(socket: PathBuf) -> Result<()> {
    if socket.exists() {
        if is_running(&socket).await {
            anyhow::bail!("daemon already running at {}", socket.display());
        }
        std::fs::remove_file(&socket).context("removing stale socket")?;
    }
    if let Some(dir) = socket.parent() {
        // Restrict only directories we created: the parent may be shared
        // (/tmp, $HOME) and chmod'ing it is EPERM at best and rude at worst.
        let existed = dir.exists();
        std::fs::create_dir_all(dir)?;
        if !existed {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
            }
        }
    }
    let listener =
        UnixListener::bind(&socket).with_context(|| format!("binding {}", socket.display()))?;
    eprintln!("harnessd listening on {}", socket.display());

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let registry: Registry = Arc::new(Mutex::new(HashMap::new()));
    let next_client_id = Arc::new(std::sync::atomic::AtomicU64::new(1));
    let mut tasks: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let registry = registry.clone();
                let client_id = next_client_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let shutdown_tx = shutdown_tx.clone();
                tasks.spawn(async move {
                    if let Err(e) = handle_connection(stream, registry, client_id, shutdown_tx).await {
                        eprintln!("connection ended: {e}");
                    }
                });
            }
            _ = shutdown_rx.changed() => break,
            // A daemon survives its clients on purpose, but not the loss of its
            // own socket: with the file unlinked nobody can ever connect again,
            // so lingering only strands the workspace lock.
            _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {
                if !socket.exists() {
                    eprintln!("harnessd: socket {} removed — exiting", socket.display());
                    break;
                }
            }
        }
    }
    // Residents outlive their clients by design, so they will not stop on their
    // own; tell them the daemon is going away before draining connections.
    stop_residents(&registry).await;
    // Drain in-flight connections so final replies are flushed before the
    // process exits; stragglers are aborted by the JoinSet drop after 5s.
    let deadline = tokio::time::sleep(std::time::Duration::from_secs(5));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            maybe = tasks.join_next() => {
                if maybe.is_none() {
                    break;
                }
            }
            _ = &mut deadline => break,
        }
    }
    std::fs::remove_file(&socket).ok();
    eprintln!("harnessd stopped");
    Ok(())
}

pub struct WirePrompt {
    pub question: String,
    pub respond: std::sync::mpsc::Sender<char>,
}

/// One attached client: where a resident's turn output and permission
/// questions go while it is connected.
struct Attached {
    out: mpsc::UnboundedSender<String>,
    prompt: mpsc::UnboundedSender<WirePrompt>,
    session_id: String,
}

/// Everyone watching a resident session right now.
///
/// A session outlives its clients, so this map is empty when nobody is
/// attached: turn updates are dropped (the journal stays canonical, so a later
/// `session/load` returns everything) and an approval question is denied rather
/// than blocking a worker on a client that no longer exists.
///
/// Several clients may watch at once — attaching is not a takeover, so a TUI
/// and a browser on the same session both see the turn stream — and a
/// permission question goes to all of them, first answer winning.
#[derive(Clone, Default)]
struct SharedClient {
    inner: Arc<std::sync::Mutex<HashMap<u64, Attached>>>,
}

impl SharedClient {
    fn attach(
        &self,
        id: u64,
        out: mpsc::UnboundedSender<String>,
        prompt: mpsc::UnboundedSender<WirePrompt>,
        session_id: String,
    ) {
        self.slot().insert(
            id,
            Attached {
                out,
                prompt,
                session_id,
            },
        );
    }

    fn detach(&self, id: u64) {
        self.slot().remove(&id);
    }

    /// Follows a `/worktree` switch: later updates carry the new session id.
    fn set_session_id(&self, session_id: &str) {
        for a in self.slot().values_mut() {
            a.session_id = session_id.to_string();
        }
    }

    fn update(&self, kind: &str, body: &str) {
        for a in self.slot().values() {
            let _ = a
                .out
                .send(harness_wire::encode(&harness_wire::session_update(
                    &a.session_id,
                    kind,
                    body,
                )));
        }
    }

    fn is_attached(&self) -> bool {
        !self.slot().is_empty()
    }

    /// Every attached client's question channel: ask all, first answer wins.
    fn prompt_senders(&self) -> Vec<mpsc::UnboundedSender<WirePrompt>> {
        self.slot().values().map(|a| a.prompt.clone()).collect()
    }

    fn slot(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Attached>> {
        self.inner.lock().expect("client slot")
    }
}

impl Sink for SharedClient {
    fn on_thinking(&mut self, text: &str) {
        self.update("thinking", text);
    }

    fn on_delta(&mut self, text: &str) {
        self.update("delta", text);
    }
}

impl TurnSink for SharedClient {
    fn on_note(&mut self, note: &str) {
        self.update("note", note);
    }

    /// Structured payload so a rich client can render the command and its
    /// output; `session/update` carries it as text like every other update.
    fn on_tool(&mut self, ev: &harness_core::engine::ToolEvent) {
        if let Ok(payload) = serde_json::to_string(ev) {
            self.update("tool", &payload);
        }
    }
}

pub struct WireGate {
    core: GateCore,
    client: SharedClient,
}

impl WireGate {
    fn new(core: GateCore, client: SharedClient) -> Self {
        Self { core, client }
    }

    fn ask(client: &SharedClient, question: &str) -> char {
        // Nobody is attached to answer, so deny: blocking here would hold a
        // worker until a client that may never come back replies.
        let senders = client.prompt_senders();
        if senders.is_empty() {
            return 'n';
        }
        // One response channel, cloned into every question: whoever answers
        // first decides, and the others' answers land on a dropped sender.
        let (respond, rx) = std::sync::mpsc::channel();
        let mut asked = 0;
        for prompt in senders {
            let req = WirePrompt {
                question: question.to_string(),
                respond: respond.clone(),
            };
            if prompt.send(req).is_ok() {
                asked += 1;
            }
        }
        drop(respond);
        if asked == 0 {
            return 'n';
        }
        // The Gate trait is synchronous, so this necessarily blocks a worker
        // until the client answers. `block_in_place` hands this worker's queued
        // tasks (including its non-stealable LIFO slot, which is where the
        // permission round-trip's own wake-up lands) to another worker —
        // without it the daemon deadlocks on the first approval.
        tokio::task::block_in_place(|| rx.recv().unwrap_or('n'))
    }
}

impl Gate for WireGate {
    fn check_bash(&mut self, command: &str) -> GateDecision {
        self.core
            .decide_bash(command, |q| Self::ask(&self.client, q))
    }

    fn check_write(&mut self, path: &str) -> GateDecision {
        self.core.decide_write(path, |q| Self::ask(&self.client, q))
    }
}

/// A session that lives in the daemon rather than in a client.
///
/// Clients attach and detach; the engine, its agent bus, its heartbeat runtime
/// and any in-flight turn stay put. That is what lets a detached daemon keep
/// running goals and heartbeats, and what lets a turn survive its client.
struct Resident {
    engine: Arc<Mutex<Engine>>,
    /// Held for a whole turn (and its continuations), so turns never
    /// interleave. The engine lock itself is only held for moments, which is
    /// what lets stats, model and goal requests through mid-turn.
    turn: Arc<Mutex<()>>,
    bus: Arc<harness_bus::LocalBus>,
    canceller: Canceller,
    session_id: std::sync::Mutex<String>,
    ws_key: std::sync::Mutex<String>,
    path: String,
    worktree_stack: std::sync::Mutex<Vec<(Workspace, String)>>,
    heartbeats: Arc<Mutex<std::collections::HashMap<String, HbRuntime>>>,
    hb_counter: std::sync::atomic::AtomicU64,
    notice_inbox: Arc<std::sync::Mutex<Vec<String>>>,
    client: SharedClient,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl Resident {
    fn session_id(&self) -> String {
        self.session_id.lock().expect("session id").clone()
    }

    fn ws_key(&self) -> String {
        self.ws_key.lock().expect("ws key").clone()
    }
}

/// Daemon-wide session registry, keyed by workspace. It replaces the old
/// per-connection ownership: `busy` meant "somebody else owns this workspace,
/// refuse them"; a registry slot means "this workspace already has a session —
/// attach to it".
type Registry = Arc<Mutex<HashMap<String, ResidentSlot>>>;

enum ResidentSlot {
    /// A client is constructing the resident; a second one must not race it.
    Opening,
    Ready(Arc<Resident>),
}

/// Tells every resident to stop scheduling new work. Turns already running are
/// left to the shutdown drain so their final replies still reach a client.
async fn stop_residents(registry: &Registry) {
    for slot in registry.lock().await.values() {
        if let ResidentSlot::Ready(r) = slot {
            r.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// Runtime timer for a journaled heartbeat; the journal only stores intent.
struct HbRuntime {
    instruction: String,
    mode: String,
    every_ms: Option<u64>,
    cron: Option<harness_core::cron::CronSpec>,
    paused: bool,
    next_due_ms: i64,
    /// Consecutive failed turns, which stretch the next wait.
    failures: u32,
}

/// Failure bookkeeping for one scheduled turn: a failure backs the next
/// attempt off (doubling, capped) so a broken schedule cannot spend all day at
/// full rate, and one success puts it back on its interval.
fn apply_hb_outcome(rt: &mut HbRuntime, ok: bool, now_ms: i64) {
    if ok {
        rt.failures = 0;
        return;
    }
    rt.failures = rt.failures.saturating_add(1);
    if let Some(every) = rt.every_ms {
        rt.next_due_ms = now_ms + harness_core::cron::backoff_ms(rt.failures, every) as i64;
    }
}

/// First due time for a heartbeat armed or resumed at `now`. An interval adds
/// a delay; cron jumps to its next occurrence, which may be far away — or
/// beyond the search window, which parks it.
fn initial_due(
    now: i64,
    cron: Option<&harness_core::cron::CronSpec>,
    delay_ms: Option<u64>,
) -> i64 {
    match cron {
        Some(spec) => spec
            .next_due(now.max(0) as u64)
            .map(|ms| ms as i64)
            .unwrap_or(i64::MAX),
        None => now + delay_ms.unwrap_or_default() as i64,
    }
}

const HB_TICK_MS: u64 = 250;

/// `--yolo` reaches the daemon through the environment (see main.rs):
/// the gate lives here, not in the client.
fn yolo_from_env() -> bool {
    std::env::var("HARNESS_YOLO")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Selected provider for this daemon (fixed for its lifetime).
fn provider_name() -> String {
    std::env::var("HARNESS_PROVIDER").unwrap_or_else(|_| "anthropic".into())
}

/// The model this daemon will use unless the client names one. Shares
/// `default_model` with the CLI so the two cannot drift apart.
fn default_model_for(provider: &str) -> String {
    std::env::var("ANTHROPIC_MODEL")
        .unwrap_or_else(|_| harness_provider::default_model(provider).to_string())
}

/// One provider per engine (children included); model selection comes from
/// env so a single factory serves the whole family.
fn provider_factory() -> harness_bus::ProviderFactory {
    let provider = provider_name();
    if provider == "mock" {
        return Arc::new(|| Ok(Box::new(EchoProvider::from_env()) as Box<dyn Provider>));
    }
    let default_model = harness_provider::default_model(&provider);
    let key = crate::creds::resolve_key(&provider);
    Arc::new(move || {
        let model = std::env::var("ANTHROPIC_MODEL").unwrap_or_else(|_| default_model.into());
        match harness_provider::build(&provider, key.clone().unwrap_or_default(), model) {
            Some(client) => Ok(client),
            None => Err(format!("unknown provider “{provider}”")),
        }
    })
}

/// Refusal for requests that restructure the session while a turn runs.
const TURN_BUSY: &str = "a turn is running — wait for it, or cancel it (esc), then retry";

/// `images: [{mediaType, data}]` on a prompt request (ACP editors send them).
fn wire_images(params: &serde_json::Value) -> Vec<harness_core::Image> {
    params["images"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|i| {
                    Some(harness_core::Image {
                        media_type: i["mediaType"].as_str()?.to_string(),
                        data: i["data"].as_str()?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn last_assistant_text(session: &SessionManager) -> String {
    session
        .context_messages()
        .iter()
        .rev()
        .find_map(|m| match m {
            AgentMessage::Assistant { content, .. } => Some(
                content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => None,
        })
        .unwrap_or_default()
}

async fn handle_connection(
    stream: UnixStream,
    registry: Registry,
    client_id: u64,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
) -> Result<()> {
    let (rd, wr) = stream.into_split();
    let mut lines = BufReader::new(rd).lines();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    let writer_task = tokio::spawn(async move {
        let mut writer = wr;
        while let Some(line) = out_rx.recv().await {
            // `encode` already ends with a newline; adding another would put a
            // blank line between messages, which every client would have to
            // know to skip.
            if writer.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            let _ = writer.flush().await;
        }
    });

    let (prompt_tx, mut prompt_rx) = mpsc::unbounded_channel::<WirePrompt>();
    let mut handler = Handler {
        out_tx: out_tx.clone(),
        prompt_tx: prompt_tx.clone(),
        registry,
        client_id,
        next_id: 1,
        state: None,
        initialized: false,
    };
    let mut pending: HashMap<u64, std::sync::mpsc::Sender<char>> = HashMap::new();
    let mut shutdown = false;

    let result: Result<()> = loop {
        tokio::select! {
            line = lines.next_line() => {
                match line {
                    Ok(Some(l)) => {
                        let Some(msg) = harness_wire::decode(&l) else { continue };
                        if let Err(e) = handler.handle(msg, &mut pending, &mut shutdown).await {
                            break Err(e);
                        }
                        if shutdown {
                            let _ = shutdown_tx.send(true);
                            break Ok(());
                        }
                    }
                    Ok(None) => break Ok(()),
                    Err(e) => break Err(e.into()),
                }
            }
            wp = prompt_rx.recv() => {
                if let Some(wp) = wp {
                    let id = handler.next_id;
                    handler.next_id += 1;
                    pending.insert(id, wp.respond);
                    let _ = out_tx.send(harness_wire::encode(&Wire::request(
                        id,
                        harness_wire::METHOD_PERMISSION_REQUEST,
                        serde_json::json!({"question": wp.question}),
                    )));
                }
            }
        }
    };
    // The resident stays in the registry: detaching is not ending. A turn still
    // running keeps running, and its updates resume when a client attaches
    // again (the journal holds everything written while nobody was attached).
    if let Some(st) = handler.state.take() {
        st.client.detach(client_id);
        eprintln!(
            "harnessd: client {client_id} detached from {} — session stays resident",
            st.session_id()
        );
    }
    drop(handler);
    drop(out_tx);
    // Drain: the writer task flushes any final replies, then exits when the
    // channel closes — aborting here would race the shutdown response.
    let _ = writer_task.await;
    result
}

struct Handler {
    out_tx: mpsc::UnboundedSender<String>,
    prompt_tx: mpsc::UnboundedSender<WirePrompt>,
    registry: Registry,
    client_id: u64,
    next_id: u64,
    state: Option<Arc<Resident>>,
    initialized: bool,
}

impl Handler {
    fn reply(&self, id: u64, r: Result<serde_json::Value, RpcError>) {
        let w = match r {
            Ok(v) => Wire::ok(id, v),
            Err(e) => Wire::err(id, e),
        };
        let _ = self.out_tx.send(harness_wire::encode(&w));
    }

    async fn handle(
        &mut self,
        msg: Wire,
        pending: &mut HashMap<u64, std::sync::mpsc::Sender<char>>,
        shutdown: &mut bool,
    ) -> Result<()> {
        match msg {
            Wire::Request { id, method, params } => {
                self.request(id, &method, params, shutdown).await
            }
            Wire::Response { id, result, error } => {
                if let Some(tx) = pending.remove(&id) {
                    let answer = error
                        .map(|_| 'n')
                        .or_else(|| {
                            result.as_ref().and_then(|r| {
                                r["answer"]
                                    .as_str()
                                    .map(|s| s.chars().next().unwrap_or('n'))
                            })
                        })
                        .unwrap_or('n');
                    let _ = tx.send(answer);
                }
                Ok(())
            }
            Wire::Notification { .. } => Ok(()),
        }
    }

    async fn request(
        &mut self,
        id: u64,
        method: &str,
        params: serde_json::Value,
        shutdown: &mut bool,
    ) -> Result<()> {
        match method {
            harness_wire::METHOD_INITIALIZE => {
                self.initialized = true;
                self.reply(
                    id,
                    Ok(serde_json::json!({
                        "protocol": harness_wire::PROTOCOL_VERSION,
                        "agent": "harness",
                    })),
                );
            }
            harness_wire::METHOD_SESSION_NEW
            | harness_wire::METHOD_SESSION_CONTINUE
            | harness_wire::METHOD_SESSION_LOAD => {
                if !self.initialized {
                    self.reply(id, Err(RpcError::internal("initialize first")));
                } else if self.state.is_some() {
                    self.reply(
                        id,
                        Err(RpcError::internal(
                            "session already open on this connection",
                        )),
                    );
                } else {
                    self.open_session(id, method, params).await;
                }
            }
            harness_wire::METHOD_SESSION_PROMPT => self.prompt(id, params),
            harness_wire::METHOD_SESSION_RESIDENTS => self.residents(id).await,
            harness_wire::METHOD_SESSION_COMPACT => self.compact(id),
            harness_wire::METHOD_HEARTBEAT_SET => self.heartbeat_set(id, params).await,
            harness_wire::METHOD_HEARTBEAT_LIST => self.heartbeat_list(id).await,
            harness_wire::METHOD_SESSION_CANCEL => {
                if let Some(st) = &self.state {
                    // Canceller lives outside the engine lock so a running turn
                    // can't serialize its own cancellation behind itself.
                    st.canceller.cancel();
                    self.reply(id, Ok(serde_json::json!({"cancelled": true})));
                } else {
                    self.reply(id, Err(RpcError::internal("no session")));
                }
            }
            harness_wire::METHOD_SESSION_NAME => {
                match self.state.as_ref().zip(params["name"].as_str()) {
                    Some((st, name)) if !name.is_empty() => {
                        st.engine.lock().await.session.append_session_info(name);
                        self.reply(id, Ok(serde_json::json!({"named": true})));
                    }
                    Some(_) => self.reply(id, Err(RpcError::internal("name required"))),
                    None => self.reply(id, Err(RpcError::internal("no session"))),
                }
            }
            harness_wire::METHOD_SESSION_STATS => {
                if let Some(st) = &self.state {
                    let eng = st.engine.lock().await;
                    let totals = eng.session.totals();
                    self.reply(
                        id,
                        Ok(serde_json::json!({
                            "sessionId": st.session_id(),
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
                            "ctx": crate::backend::ctx_json(&eng),
                        })),
                    );
                } else {
                    self.reply(id, Err(RpcError::internal("no session")));
                }
            }
            harness_wire::METHOD_MODEL_SET
            | harness_wire::METHOD_SESSION_HISTORY
            | harness_wire::METHOD_SESSION_LISTINGS
            | harness_wire::METHOD_SESSION_BRANCH
            | harness_wire::METHOD_SESSION_FORK
            | harness_wire::METHOD_GOAL_SET
            | harness_wire::METHOD_GOAL_STATUS
            | harness_wire::METHOD_AUTONOMOUS_SET
            | harness_wire::METHOD_WORKSPACE_FILES => self.sync_call(id, method, params).await,
            harness_wire::METHOD_SESSION_WORKTREE => self.worktree(id, params).await,
            harness_wire::METHOD_SESSION_WORKTREE_EXIT => self.worktree_exit(id, params).await,
            harness_wire::METHOD_AGENT_SPAWN => self.agent_spawn(id, params).await,
            harness_wire::METHOD_AGENT_MESSAGE => self.agent_message(id, params).await,
            harness_wire::METHOD_AGENT_LIST => self.agent_list(id).await,
            harness_wire::METHOD_SHUTDOWN => {
                *shutdown = true;
                self.reply(id, Ok(serde_json::json!({"bye": true})));
            }
            other => {
                self.reply(
                    id,
                    Err(RpcError::app(-32601, format!("unknown method {other}"))),
                );
            }
        }
        Ok(())
    }

    async fn open_session(&mut self, id: u64, method: &str, params: serde_json::Value) {
        let cwd = params["cwd"]
            .as_str()
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let ws = match Workspace::resolve(&cwd) {
            Ok(ws) => ws,
            Err(e) => {
                self.reply(id, Err(RpcError::internal(e.to_string())));
                return;
            }
        };
        let key = ws.session_key();
        // Attach to a live resident, or claim the slot and build one.
        let live = {
            let mut reg = self.registry.lock().await;
            match reg.get(&key) {
                Some(ResidentSlot::Ready(r)) => Some(Ok(r.clone())),
                Some(ResidentSlot::Opening) => Some(Err(())),
                None => {
                    reg.insert(key.clone(), ResidentSlot::Opening);
                    None
                }
            }
        };
        match live {
            Some(Err(())) => {
                self.reply(
                    id,
                    Err(RpcError::app(
                        harness_wire::ERR_WORKSPACE_BUSY,
                        format!("workspace {} is opening", ws.root().display()),
                    )),
                );
                return;
            }
            Some(Ok(resident)) => {
                // A resident owns this workspace. `new` would have to evict a
                // session that may be mid-goal with live heartbeats, so refuse
                // and say how to get a fresh one instead of doing it silently.
                if method == harness_wire::METHOD_SESSION_NEW {
                    self.reply(
                        id,
                        Err(RpcError::app(
                            harness_wire::ERR_WORKSPACE_BUSY,
                            format!(
                                "a live session already owns {} — continue it, or \
                                 `harness shutdown` to end it",
                                ws.root().display()
                            ),
                        )),
                    );
                    return;
                }
                self.attach(id, resident, true);
                return;
            }
            None => {}
        }

        let session = match method {
            harness_wire::METHOD_SESSION_NEW => match SessionManager::create(&ws) {
                Ok(s) => s,
                Err(e) => {
                    self.fail_open(id, &key, e.to_string()).await;
                    return;
                }
            },
            _ => match SessionManager::open_latest(&ws) {
                Ok(Some(s)) => s,
                Ok(None) => match SessionManager::create(&ws) {
                    Ok(s) => s,
                    Err(e) => {
                        self.fail_open(id, &key, e.to_string()).await;
                        return;
                    }
                },
                Err(e) => {
                    self.fail_open(id, &key, e.to_string()).await;
                    return;
                }
            },
        };
        let session_id = session.header.id.clone();
        let path = session
            .path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let repaired_tool_calls = session.repaired_tool_calls;

        let factory = provider_factory();
        let model = default_model_for(&provider_name());
        let mut tools = harness_tools::builtins_for(ws.root());
        tools.extend(harness_mcp::tools_for(&ws).await);
        // The resident's output slot: turn updates and approval questions reach
        // whichever client is attached, or nobody when the session is detached.
        let client = SharedClient::default();
        let mut engine = match (factory)().map_err(|e| e.to_string()).and_then(|provider| {
            Engine::new(
                ws.clone(),
                session,
                provider,
                tools,
                Box::new(WireGate::new(
                    GateCore::new(yolo_from_env(), true),
                    client.clone(),
                )),
                model.clone(),
                harness_provider::max_output_for(&model),
            )
            .map_err(|e| e.to_string())
        }) {
            Ok(e) => e,
            Err(e) => {
                self.fail_open(id, &key, e).await;
                return;
            }
        };
        // Agent bus: child approvals and child-exit notices reach whichever
        // client is attached, and simply go nowhere when none is.
        let child_client = client.clone();
        let gate_factory: harness_bus::GateFactory = Arc::new(move || {
            Box::new(WireGate::new(
                GateCore::new(yolo_from_env(), true),
                child_client.clone(),
            )) as Box<dyn Gate>
        });
        let fanout_client = client.clone();
        let fanout: harness_bus::NoticeFanout = Arc::new(move |note: &str| {
            fanout_client.update("note", note);
        });
        let bus = Arc::new(harness_bus::LocalBus::new(
            ws.clone(),
            model.clone(),
            harness_provider::max_output_for(&model),
            factory,
            gate_factory,
            engine.notice_inbox(),
            Some(fanout),
        ));
        engine.set_bus(bus.clone());
        if repaired_tool_calls > 0 {
            let _ = self.out_tx.send(harness_wire::encode(&harness_wire::session_update(
                &session_id,
                "note",
                &format!(
                    "▪ last run was interrupted mid-tool: closed {repaired_tool_calls} unfinished tool call(s)"
                ),
            )));
        }
        let notice_inbox = engine.notice_inbox();
        // Re-arm heartbeats journaled in this session (they outlive clients).
        let journaled = engine.session.heartbeats();
        let engine = Arc::new(Mutex::new(engine));
        let canceller = engine.lock().await.canceller();
        let mut hbs = std::collections::HashMap::new();
        let now = harness_core::session::now_ms();
        for hb in journaled
            .iter()
            .filter(|h| h.every_ms.is_some() || h.cron.is_some())
        {
            // A hand-edited journal could hold an unparsable expression; it
            // would be wrong to refuse every restart over it, so it just never
            // fires.
            let cron = hb
                .cron
                .as_deref()
                .and_then(|e| harness_core::cron::parse(e).ok());
            let next_due_ms = initial_due(now, cron.as_ref(), hb.every_ms);
            hbs.insert(
                hb.id.clone(),
                HbRuntime {
                    instruction: hb.instruction.clone(),
                    mode: hb.mode.clone(),
                    every_ms: hb.every_ms,
                    cron,
                    paused: hb.paused,
                    failures: 0,
                    next_due_ms,
                },
            );
        }
        let heartbeats = Arc::new(Mutex::new(hbs));
        let resident = Arc::new(Resident {
            engine,
            turn: Arc::new(Mutex::new(())),
            bus,
            canceller,
            session_id: std::sync::Mutex::new(session_id),
            ws_key: std::sync::Mutex::new(key.clone()),
            path,
            worktree_stack: std::sync::Mutex::new(Vec::new()),
            heartbeats,
            hb_counter: std::sync::atomic::AtomicU64::new(1),
            notice_inbox,
            client,
            stop: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        });
        self.registry
            .lock()
            .await
            .insert(key, ResidentSlot::Ready(resident.clone()));
        tokio::spawn(heartbeat_loop(resident.clone()));
        self.attach(id, resident, false);
    }

    /// Points this connection at a resident: from here on it receives that
    /// session's turn updates and approval questions. `adopted` distinguishes
    /// joining a session that was already running from creating one.
    fn attach(&mut self, id: u64, resident: Arc<Resident>, adopted: bool) {
        let session_id = resident.session_id();
        resident.client.attach(
            self.client_id,
            self.out_tx.clone(),
            self.prompt_tx.clone(),
            session_id.clone(),
        );
        self.reply(
            id,
            Ok(serde_json::json!({
                "sessionId": session_id,
                "path": resident.path,
                "adopted": adopted,
            })),
        );
        self.state = Some(resident);
    }

    /// Releases the `Opening` claim when construction fails, so a later client
    /// can try again instead of finding a permanently busy workspace.
    /// Which sessions a live daemon is holding. Answered from the registry, so
    /// it stays responsive while a turn runs — callers use it to know what must
    /// not be deleted out from under an appending journal.
    async fn residents(&mut self, id: u64) {
        let reg = self.registry.lock().await;
        let list: Vec<serde_json::Value> = reg
            .iter()
            .filter_map(|(key, slot)| match slot {
                ResidentSlot::Ready(r) => Some(serde_json::json!({
                    "key": key,
                    "sessionId": r.session_id(),
                    "attached": r.client.is_attached(),
                })),
                ResidentSlot::Opening => None,
            })
            .collect();
        self.reply(id, Ok(serde_json::Value::Array(list)));
    }

    async fn fail_open(&mut self, id: u64, key: &str, error: String) {
        self.registry.lock().await.remove(key);
        self.reply(id, Err(RpcError::internal(error)));
    }

    async fn agent_spawn(&mut self, id: u64, params: serde_json::Value) {
        let Some(st) = &self.state else {
            self.reply(id, Err(RpcError::internal("no session")));
            return;
        };
        let Some(task) = params["task"].as_str() else {
            self.reply(id, Err(RpcError::internal("task required")));
            return;
        };
        let req = harness_core::SpawnRequest {
            task: task.to_string(),
            name: params["name"].as_str().map(String::from),
            mutation: params["mutation"].as_bool().unwrap_or(false),
        };
        match st.bus.spawn_child(req).await {
            Ok(h) => self.reply(
                id,
                Ok(serde_json::json!({
                    "childId": h.child_id,
                    "sessionId": h.session_id,
                    "root": h.root,
                })),
            ),
            Err(e) => self.reply(id, Err(RpcError::internal(e))),
        }
    }

    async fn agent_message(&mut self, id: u64, params: serde_json::Value) {
        let Some(st) = &self.state else {
            self.reply(id, Err(RpcError::internal("no session")));
            return;
        };
        let (Some(child_id), Some(text)) = (params["childId"].as_str(), params["text"].as_str())
        else {
            self.reply(id, Err(RpcError::internal("childId and text required")));
            return;
        };
        let trigger = params["trigger"].as_bool().unwrap_or(true);
        match st.bus.message_child(child_id, text, trigger).await {
            Ok(r) => self.reply(id, Ok(serde_json::json!({"receipt": r.label()}))),
            Err(e) => self.reply(id, Err(RpcError::internal(e))),
        }
    }

    async fn agent_list(&mut self, id: u64) {
        let Some(st) = &self.state else {
            self.reply(id, Err(RpcError::internal("no session")));
            return;
        };
        let list = st.bus.list_children().await;
        self.reply(
            id,
            Ok(serde_json::Value::Array(
                list.iter()
                    .map(|c| {
                        serde_json::json!({
                            "childId": c.child_id,
                            "name": c.name,
                            "status": c.status.label(),
                            "sessionId": c.session_id,
                            "mutation": c.mutation,
                            "root": c.root,
                            "report": c.report,
                        })
                    })
                    .collect(),
            )),
        );
    }

    fn prompt(&mut self, id: u64, params: serde_json::Value) {
        let Some(st) = &self.state else {
            self.reply(id, Err(RpcError::internal("no session")));
            return;
        };
        let Some(prompt) = params["prompt"].as_str() else {
            self.reply(id, Err(RpcError::internal("prompt required")));
            return;
        };
        let prompt = prompt.to_string();
        let images = wire_images(&params);
        // Reset before the task starts so a cancel that arrives after this
        // request applies to this turn, and one from before does not.
        st.canceller.reset();
        let resident = st.clone();
        let out = self.out_tx.clone();
        tokio::spawn(run_turn_loop(
            resident,
            prompt,
            images,
            Some((out, id)),
            None,
        ));
    }

    /// Heartbeat/one-shot delivery: fire a prompt with no RPC reply pending.
    /// Busy engines are steered through the notice inbox instead (below).
    async fn heartbeat_set(&mut self, id: u64, params: serde_json::Value) {
        let Some(st) = &self.state else {
            self.reply(id, Err(RpcError::internal("no session")));
            return;
        };
        let action = params["action"].as_str().unwrap_or("add");
        let instruction = params["instruction"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let every_ms = params["everyMs"].as_u64();
        let in_ms = params["inMs"].as_u64();
        let cron_expr = params["cron"].as_str().map(str::to_string);
        // Validate before journaling: a dud expression in the journal would
        // fail again on every restart.
        let cron_spec = match cron_expr.as_deref() {
            Some(expr) => match harness_core::cron::parse(expr) {
                Ok(spec) => Some(spec),
                Err(e) => {
                    self.reply(id, Err(RpcError::app(-32602, format!("invalid cron: {e}"))));
                    return;
                }
            },
            None => None,
        };
        let mode = params["mode"].as_str().unwrap_or("steer").to_string();
        let hb_id = match params["id"].as_str() {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => {
                let n = st
                    .hb_counter
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                format!("hb-{n}")
            }
        };
        if action == "add"
            && (cron_expr.is_none() && every_ms.is_none() && in_ms.is_none()
                || instruction.is_empty())
        {
            self.reply(
                id,
                Err(RpcError::internal(
                    "heartbeat needs a schedule (everyMs/inMs/cron) and an instruction",
                )),
            );
            return;
        }
        let mut eng = st.engine.lock().await;
        eng.session.append_heartbeat(
            action,
            &hb_id,
            every_ms,
            in_ms,
            cron_expr.as_deref(),
            &instruction,
            &mode,
        );
        drop(eng);
        let now = harness_core::session::now_ms();
        let due = initial_due(now, cron_spec.as_ref(), every_ms.or(in_ms));
        {
            let mut hbs = st.heartbeats.lock().await;
            match action {
                "add" => {
                    hbs.insert(
                        hb_id.clone(),
                        HbRuntime {
                            instruction,
                            mode,
                            every_ms,
                            cron: cron_spec,
                            paused: false,
                            next_due_ms: due,
                            failures: 0,
                        },
                    );
                }
                "pause" => hbs
                    .iter_mut()
                    .filter(|(k, _)| **k == hb_id)
                    .for_each(|(_, h)| h.paused = true),
                "resume" => hbs
                    .iter_mut()
                    .filter(|(k, _)| **k == hb_id)
                    .for_each(|(_, h)| {
                        h.paused = false;
                        // Fires at the next tick as before; cron re-arms to its
                        // next occurrence instead of firing immediately.
                        h.next_due_ms = initial_due(now, h.cron.as_ref(), None);
                    }),
                "clear" => {
                    hbs.remove(&hb_id);
                }
                _ => {}
            }
        }
        self.reply(id, Ok(serde_json::json!({"id": hb_id, "action": action})));
    }

    async fn heartbeat_list(&mut self, id: u64) {
        let Some(st) = &self.state else {
            self.reply(id, Err(RpcError::internal("no session")));
            return;
        };
        let journal = st.engine.lock().await.session.heartbeats();
        let runtime = st.heartbeats.lock().await;
        let list: Vec<serde_json::Value> = journal
            .iter()
            .map(|h| {
                serde_json::json!({
                    "id": h.id,
                    "instruction": h.instruction,
                    "mode": h.mode,
                    "everyMs": h.every_ms,
                    "inMs": h.in_ms,
                    "cron": h.cron,
                    "paused": h.paused,
                    "nextDueMs": runtime.get(&h.id).map(|r| r.next_due_ms),
                })
            })
            .collect();
        self.reply(id, Ok(serde_json::Value::Array(list)));
    }

    fn compact(&mut self, id: u64) {
        let Some(st) = &self.state else {
            self.reply(id, Err(RpcError::internal("no session")));
            return;
        };
        let resident = st.clone();
        let out = self.out_tx.clone();
        tokio::spawn(async move {
            // After any running turn: compaction rewrites what the turn reads.
            let _turn = resident.turn.lock().await;
            let msg = match harness_core::engine::compact_shared(&*resident.engine).await {
                Ok(cs) => Wire::ok(
                    id,
                    serde_json::json!({
                        "tokensBefore": cs.tokens_before,
                        "summaryTokens": cs.summary_tokens,
                        "retained": cs.retained_messages,
                    }),
                ),
                Err(e) => Wire::err(id, RpcError::internal(e.to_string())),
            };
            let _ = out.send(harness_wire::encode(&msg));
        });
    }

    async fn sync_call(&mut self, id: u64, method: &str, params: serde_json::Value) {
        let r: Result<serde_json::Value, String> = match self.state.as_mut() {
            None => Err("no session".into()),
            Some(st) => {
                let mut eng = st.engine.lock().await;
                match method {
                    harness_wire::METHOD_MODEL_SET => {
                        let model = params["model"].as_str().unwrap_or_default();
                        if model.is_empty() {
                            Err("model required".into())
                        } else if !harness_provider::is_dynamic(eng.provider.name())
                            && catalog::find(&catalog::load(), model).is_none()
                        {
                            Err(format!("unknown model {model}"))
                        } else {
                            eng.model = model.to_string();
                            eng.max_tokens = harness_provider::max_output_for(model);
                            let provider = eng.provider.name();
                            eng.session.append_model_change(provider, model);
                            Ok(serde_json::json!({"model": model}))
                        }
                    }
                    harness_wire::METHOD_GOAL_SET => {
                        eng.set_goal(
                            params["action"].as_str().unwrap_or("set"),
                            params["text"].as_str().unwrap_or_default(),
                            params["budget"].as_u64(),
                        );
                        Ok(crate::backend::goal_status(&eng))
                    }
                    harness_wire::METHOD_GOAL_STATUS => Ok(crate::backend::goal_status(&eng)),
                    harness_wire::METHOD_AUTONOMOUS_SET => {
                        let on = params["on"].as_bool().unwrap_or(false);
                        eng.set_autonomous(
                            on,
                            params["maxTurns"].as_u64(),
                            params["maxTokens"].as_u64(),
                            params["gate"].as_str(),
                        );
                        Ok(serde_json::json!({"on": on}))
                    }
                    harness_wire::METHOD_WORKSPACE_FILES => {
                        let limit = params["limit"].as_u64().unwrap_or(4000) as usize;
                        Ok(serde_json::json!({"files": eng.workspace.list_files(limit)}))
                    }
                    harness_wire::METHOD_SESSION_HISTORY => {
                        Ok(crate::backend::history_json(&eng.session))
                    }
                    harness_wire::METHOD_SESSION_LISTINGS => {
                        SessionManager::list_for(&eng.workspace).map_err(|e| e.to_string()).map(|l| {
                            serde_json::Value::Array(
                                l.iter()
                                    .map(|s| {
                                        serde_json::json!({
                                            "createdMs": s.created_ms,
                                            "id": s.id,
                                            "name": s.name,
                                            "entries": s.message_entries,
                                            "path": s.path.display().to_string(),
                                        })
                                    })
                                    .collect(),
                            )
                        })
                    }
                    harness_wire::METHOD_SESSION_BRANCH => match params["target"].as_str() {
                        None => Ok(serde_json::Value::Array(
                            eng.session
                                .branch_points()
                                .into_iter()
                                .map(|(bid, label)| serde_json::json!({"id": bid, "label": label}))
                                .collect(),
                        )),
                        Some(target) => match st.turn.try_lock() {
                        // Moving the leaf under a running turn would graft its
                        // next sample onto another branch.
                        Err(_) => Err(TURN_BUSY.into()),
                        Ok(_turn) => Ok(serde_json::json!({"moved": eng.session.branch(target)})),
                    },
                    },
                    harness_wire::METHOD_SESSION_FORK => eng
                        .session
                        .clone_session(&eng.workspace)
                        .map(|f| {
                            serde_json::json!({
                                "sessionId": f.header.id,
                                "path": f.path.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
                            })
                        })
                        .map_err(|e| e.to_string()),
                    _ => Err(format!("unknown method {method}")),
                }
            }
        };
        self.reply(id, r.map_err(RpcError::internal));
    }

    async fn worktree(&mut self, id: u64, params: serde_json::Value) {
        let outcome: Result<serde_json::Value, String> = async {
            let Some(st) = self.state.as_mut() else {
                return Err("no session".into());
            };
            let name: String = params["name"]
                .as_str()
                .unwrap_or_default()
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
                .collect();
            let name = if name.is_empty() {
                format!("wt-{}", harness_core::session::now_ms() % 100_000)
            } else {
                name
            };
            let resident = st.clone();
            // Held throughout: no turn may start while the session is swapped.
            let Ok(_turn) = resident.turn.clone().try_lock_owned() else {
                return Err(TURN_BUSY.into());
            };
            let old_key = resident.ws_key();
            let mut eng = st.engine.lock().await;
            let ws2 = eng
                .workspace
                .create_worktree(&name)
                .map_err(|e| e.to_string())?;
            let new_key = ws2.session_key();
            {
                // The resident moves with the session, so re-key it before
                // anything looks the new workspace up.
                let mut reg = self.registry.lock().await;
                if reg.contains_key(&new_key) {
                    return Err(format!("worktree busy: {name}"));
                }
                reg.remove(&old_key);
                reg.insert(new_key.clone(), ResidentSlot::Ready(resident.clone()));
            }
            let session = match SessionManager::create(&ws2) {
                Ok(s) => s,
                Err(e) => {
                    let mut reg = self.registry.lock().await;
                    reg.remove(&new_key);
                    reg.insert(old_key, ResidentSlot::Ready(resident.clone()));
                    return Err(e.to_string());
                }
            };
            let reply = serde_json::json!({
                "name": name,
                "root": ws2.root().display().to_string(),
                "sessionId": session.header.id,
            });
            resident
                .worktree_stack
                .lock()
                .expect("worktree stack")
                .push((eng.workspace.clone(), name));
            eng.workspace = ws2;
            let sid = session.header.id.clone();
            eng.session = session;
            drop(eng);
            *resident.ws_key.lock().expect("ws key") = new_key;
            *resident.session_id.lock().expect("session id") = sid.clone();
            resident.client.set_session_id(&sid);
            Ok(reply)
        }
        .await;
        match outcome {
            Ok(v) => self.reply(id, Ok(v)),
            Err(e) => self.reply(id, Err(RpcError::internal(e))),
        }
    }

    async fn worktree_exit(&mut self, id: u64, params: serde_json::Value) {
        let remove = params["remove"].as_bool().unwrap_or(false);
        let outcome: Result<serde_json::Value, String> = async {
            let Some(st) = self.state.as_mut() else {
                return Err("no session".into());
            };
            let resident = st.clone();
            let Ok(_turn) = resident.turn.clone().try_lock_owned() else {
                return Err(TURN_BUSY.into());
            };
            let current_key = resident.ws_key();
            let Some((old_ws, name)) = resident
                .worktree_stack
                .lock()
                .expect("worktree stack")
                .last()
                .cloned()
            else {
                return Err("not in a worktree".into());
            };
            if remove {
                old_ws
                    .remove_worktree(&name)
                    .map_err(|e| format!("worktree remove failed: {e} (kept)"))?;
            }
            let old_key = old_ws.session_key();
            {
                let mut reg = self.registry.lock().await;
                if reg.contains_key(&old_key) && old_key != current_key {
                    return Err(format!(
                        "original workspace busy: {}",
                        old_ws.root().display()
                    ));
                }
                reg.remove(&current_key);
                reg.insert(old_key.clone(), ResidentSlot::Ready(resident.clone()));
            }
            let mut eng = st.engine.lock().await;
            let session = SessionManager::open_latest(&old_ws)
                .map_err(|e| e.to_string())?
                .unwrap_or_else(|| SessionManager::create(&old_ws).expect("session dir writable"));
            let reply = serde_json::json!({
                "root": old_ws.root().display().to_string(),
                "sessionId": session.header.id,
            });
            let sid = session.header.id.clone();
            eng.workspace = old_ws;
            eng.session = session;
            drop(eng);
            resident
                .worktree_stack
                .lock()
                .expect("worktree stack")
                .pop();
            *resident.ws_key.lock().expect("ws key") = old_key;
            *resident.session_id.lock().expect("session id") = sid.clone();
            resident.client.set_session_id(&sid);
            Ok(reply)
        }
        .await;
        match outcome {
            Ok(v) => self.reply(id, Ok(v)),
            Err(e) => self.reply(id, Err(RpcError::internal(e))),
        }
    }
}

/// One prompt's full lifecycle: the turn, then any goal/autonomous
/// continuations. Only client-initiated turns carry an RPC reply id.
async fn run_turn_loop(
    resident: Arc<Resident>,
    prompt: String,
    images: Vec<harness_core::Image>,
    reply: Option<(mpsc::UnboundedSender<String>, u64)>,
    hb: Option<String>,
) {
    let _turn = resident.turn.lock().await;
    let cell: &Mutex<Engine> = &resident.engine;
    // Forwards to whichever client is attached, so a turn survives its client:
    // output written while nobody is attached is dropped, not lost, because the
    // journal is canonical and a later `session/load` returns all of it.
    let mut sink = resident.client.clone();
    let mut res = run_turn_shared(cell, &prompt, images, &mut sink).await;
    while res.is_ok() {
        match post_turn_check_shared(cell, &mut sink).await {
            Some(cont) => res = run_turn_shared(cell, &cont, Vec::new(), &mut sink).await,
            None => break,
        }
    }
    let text = last_assistant_text(&cell.lock().await.session);
    // Captured before the reply is built, which moves `res`.
    let ok = res.is_ok();
    match reply {
        Some((out, id)) => {
            let msg = match res {
                Ok(stats) => Wire::ok(
                    id,
                    serde_json::json!({
                        "samples": stats.samples,
                        "toolCalls": stats.tool_calls,
                        "aborted": stats.aborted,
                        "ms": stats.ms,
                        "input": stats.usage.total_in(),
                        "cacheRead": stats.usage.cache_read,
                        "cacheWrite": stats.usage.cache_write,
                        "output": stats.usage.output,
                        "text": text,
                    }),
                ),
                Err(e) => Wire::err(id, RpcError::internal(e.to_string())),
            };
            let _ = out.send(harness_wire::encode(&msg));
        }
        None => {
            if let Err(e) = &res {
                sink.on_note(&format!("✗ background turn failed: {e}"));
            }
        }
    }
    // A scheduled turn's result feeds back into its own cadence.
    if let Some(hb_id) = hb {
        let mut hbs = resident.heartbeats.lock().await;
        if let Some(rt) = hbs.get_mut(&hb_id) {
            apply_hb_outcome(rt, ok, harness_core::session::now_ms());
        }
    }
}

/// Per-resident heartbeat scheduler. A due tick is claimed (its next due time
/// advanced) before firing, so missed ticks coalesce instead of replaying a
/// backlog. A busy engine is steered via the notice inbox; an idle one gets its
/// own turn — which, unlike when this lived in a connection, also happens with
/// no client attached. Stops only when the daemon does.
async fn heartbeat_loop(resident: Arc<Resident>) {
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(HB_TICK_MS)).await;
        if resident.stop.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let heartbeats = resident.heartbeats.clone();
        let notice_inbox = resident.notice_inbox.clone();
        let now = harness_core::session::now_ms();
        let mut due: Vec<(String, String, String)> = Vec::new();
        let mut fired_one_shots: Vec<String> = Vec::new();
        {
            let mut hbs = heartbeats.lock().await;
            for (id, hb) in hbs.iter_mut() {
                if hb.paused || hb.next_due_ms > now {
                    continue;
                }
                if hb.every_ms.is_some() || hb.cron.is_some() {
                    // Cron jumps to its next occurrence; intervals add a period.
                    hb.next_due_ms = initial_due(now, hb.cron.as_ref(), hb.every_ms);
                } else {
                    // One-shot: it has fired.
                    hb.next_due_ms = i64::MAX;
                    fired_one_shots.push(id.clone());
                }
                due.push((id.clone(), hb.instruction.clone(), hb.mode.clone()));
            }
            for id in &fired_one_shots {
                hbs.remove(id);
            }
        }
        for (id, instruction, mode) in due {
            // `follow_up` heartbeats read as queued work, not interruptions.
            let tag = if mode == "follow_up" {
                "scheduled task"
            } else {
                "heartbeat"
            };
            let head = format!("[{tag} {id}]");
            let prompt = format!("{head} {instruction}");
            if resident.turn.try_lock().is_ok() {
                // A due tick fires whether or not a client is attached: this is
                // the whole point of a resident session.
                let hb = id.clone();
                tokio::spawn(run_turn_loop(
                    resident.clone(),
                    prompt,
                    Vec::new(),
                    None,
                    Some(hb),
                ));
            } else {
                // Coalesce: a slow turn must not accumulate a tick backlog.
                let mut inbox = notice_inbox.lock().expect("notice inbox");
                if !inbox.iter().any(|m| m.starts_with(&head)) {
                    inbox.push(prompt);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hb(every_ms: u64) -> HbRuntime {
        HbRuntime {
            instruction: "tick".into(),
            mode: "steer".into(),
            every_ms: Some(every_ms),
            cron: None,
            paused: false,
            next_due_ms: 0,
            failures: 0,
        }
    }

    #[test]
    fn a_failing_schedule_backs_off_and_a_success_resets_it() {
        let mut rt = hb(1_000);
        let now = 10_000;
        apply_hb_outcome(&mut rt, false, now);
        assert_eq!(rt.failures, 1);
        assert_eq!(rt.next_due_ms, now + 2_000, "the retry waits longer");
        apply_hb_outcome(&mut rt, false, now);
        assert_eq!(rt.next_due_ms, now + 4_000, "and longer again");
        apply_hb_outcome(&mut rt, true, now);
        assert_eq!(rt.failures, 0, "one success clears the count");
    }

    #[test]
    fn cron_keeps_its_wall_clock_after_a_failure() {
        // A daily 9am job must still run at 9am tomorrow; doubling a cron
        // schedule would silently move it.
        let mut rt = hb(0);
        rt.every_ms = None;
        rt.cron = harness_core::cron::parse("0 9 * * *").ok();
        rt.next_due_ms = 123;
        apply_hb_outcome(&mut rt, false, 10_000);
        assert_eq!(rt.failures, 1);
        assert_eq!(rt.next_due_ms, 123, "the occurrence is not stretched");
    }
}
