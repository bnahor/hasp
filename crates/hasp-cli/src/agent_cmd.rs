//! `--agent NAME`: drive an external agent (Claude Code, Codex, Gemini,
//! OpenCode, any ACP agent) on the user's own plan, with the harness TUI,
//! approvals and reviews on top. `harness agents` lists what is available.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use hasp_agents::{Agent, Asker, Cancel, Decision, Event, Opts, Spec, TurnEnd};
use hasp_core::Usage;
use hasp_core::engine::{ToolEvent, TurnStats};
use serde_json::{Value, json};

use crate::backend::Backend;
use crate::gate::PromptRequest;
use crate::tui::{DoneKind, UiEvent};

pub fn list() {
    println!(
        "{:<10} {:<10} {:<9} plan · login",
        "agent", "installed", "protocol"
    );
    for s in hasp_agents::specs::all() {
        println!(
            "{:<10} {:<10} {:<9} {} · {}",
            s.name,
            if hasp_agents::specs::installed(&s) {
                "yes"
            } else {
                "-"
            },
            format!("{:?}", s.protocol).to_lowercase(),
            if s.plan.is_empty() { "-" } else { &s.plan },
            if s.login.is_empty() { "-" } else { &s.login },
        );
    }
    println!(
        "\nuse: hasp --agent <name> [--model <id>] [-p \"task\"] [--new] [--yolo]\n\
         add any ACP agent in ~/.hasp/agents.json\n\
         API keys and coding plans on hasp's own engine: hasp providers"
    );
}

pub fn spec(name: &str) -> Result<Spec> {
    let spec = hasp_agents::specs::find(name)
        .ok_or_else(|| anyhow!("unknown agent {name} — `hasp agents` lists them"))?;
    if !hasp_agents::specs::installed(&spec) {
        bail!(
            "{name} is not installed (`{}` is not on PATH){}",
            spec.command,
            if spec.login.is_empty() {
                String::new()
            } else {
                format!("; after installing: {}", spec.login)
            }
        );
    }
    Ok(spec)
}

// ---- which agent session to continue, per workspace ----

fn resume_file(root: &Path) -> PathBuf {
    hasp_workspace::state_path(root, "agent-sessions.json")
}

pub fn last_session(root: &Path, agent: &str) -> Option<String> {
    let text = std::fs::read_to_string(resume_file(root)).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    v[agent].as_str().map(String::from)
}

pub fn remember_session(root: &Path, agent: &str, id: &str) {
    let path = resume_file(root);
    let mut v: Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| json!({}));
    if v[agent].as_str() == Some(id) {
        return;
    }
    v[agent] = json!(id);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&path, serde_json::to_string_pretty(&v).unwrap_or_default());
}

// ---- one-shot ----

/// Ask on the terminal; without one, deny (the agent is told, and carries on).
pub fn terminal_asker() -> Asker {
    Arc::new(|question: String| {
        use std::io::IsTerminal;
        if !std::io::stdin().is_terminal() {
            return Decision::Deny;
        }
        eprint!("{question} ");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_err() {
            return Decision::Deny;
        }
        answer(line.trim().chars().next().unwrap_or('n'))
    })
}

fn answer(c: char) -> Decision {
    match c {
        'y' | 'Y' => Decision::Once,
        'a' | 'A' => Decision::Always,
        _ => Decision::Deny,
    }
}

pub async fn one_shot(spec: &Spec, root: &Path, opts: Opts, prompt: &str) -> Result<()> {
    let mut agent = hasp_agents::start(spec, opts).await?;
    eprintln!("▪ {} in {}", agent.label(), root.display());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let printer = tokio::spawn(async move {
        let mut out = std::io::stdout();
        while let Some(e) = rx.recv().await {
            match e {
                Event::Text(t) => {
                    let _ = out.write_all(t.as_bytes());
                    let _ = out.flush();
                }
                Event::Thinking(_) => {}
                Event::Tool(t) => eprintln!(
                    "\n▸ {} {} {} ({} ms)",
                    t.name,
                    t.subject.lines().next().unwrap_or(""),
                    if t.is_error { "✗" } else { "✓" },
                    t.ms
                ),
                Event::Note(n) => eprintln!("\n▪ {n}"),
            }
        }
    });
    let started = std::time::Instant::now();
    let end = agent.prompt(prompt, &tx, &Cancel::default()).await;
    drop(tx);
    let _ = printer.await;
    if let Some(id) = agent.session_id() {
        remember_session(root, &spec.name, &id);
    }
    let end = end?;
    println!();
    eprintln!(
        "─ {} · {} tool call(s) · in {} (cache R {}) out {} · {} ms",
        agent.label(),
        end.tool_calls,
        end.usage.input,
        end.usage.cache_read,
        end.usage.output,
        started.elapsed().as_millis()
    );
    if let Some(e) = end.error {
        bail!("{}: {e}", spec.name);
    }
    Ok(())
}

// ---- TUI backend ----

enum Cmd {
    Prompt(String),
    Compact,
    New,
}

/// The TUI's view of an external agent. The agent keeps its own session;
/// the harness adds the interface, approvals (through the same prompt the
/// built-in engine uses) and everything git-based (reviews, worktrees).
pub struct AgentBackend {
    tx: tokio::sync::mpsc::UnboundedSender<Cmd>,
    cancel: Cancel,
    name: String,
    label: Arc<Mutex<String>>,
    session: Arc<Mutex<Option<String>>>,
    root: PathBuf,
    branch: Option<String>,
}

/// Approvals go to the TUI's prompt line; `y`/`a`/`n` as for built-in tools.
pub fn tui_asker(prompt_tx: std::sync::mpsc::Sender<PromptRequest>) -> Asker {
    let prompt_tx = Mutex::new(prompt_tx);
    Arc::new(move |question: String| {
        let (respond, rx) = std::sync::mpsc::channel();
        let sent = prompt_tx
            .lock()
            .map(|tx| tx.send(PromptRequest { question, respond }).is_ok())
            .unwrap_or(false);
        if !sent {
            return Decision::Deny;
        }
        answer(rx.recv().unwrap_or('n'))
    })
}

fn tool_event(t: hasp_agents::ToolCall) -> ToolEvent {
    ToolEvent {
        name: t.name,
        args_preview: t.subject,
        content_len: t.output.len(),
        content_preview: t.output,
        is_error: t.is_error,
        ms: t.ms,
    }
}

fn turn_done(end: &TurnEnd, ms: u128) -> Result<DoneKind, String> {
    if let Some(e) = &end.error {
        return Err(e.clone());
    }
    Ok(DoneKind::Turn(TurnStats {
        samples: 1,
        tool_calls: end.tool_calls,
        usage: Usage {
            input: end.usage.input,
            output: end.usage.output,
            cache_read: end.usage.cache_read,
            cache_write: end.usage.cache_write,
        },
        ms,
        aborted: end.cancelled,
    }))
}

impl AgentBackend {
    pub async fn start(
        spec: Spec,
        opts: Opts,
        root: PathBuf,
        branch: Option<String>,
        events: std::sync::mpsc::Sender<UiEvent>,
    ) -> Result<AgentBackend> {
        let agent = hasp_agents::start(&spec, opts.clone())
            .await
            .with_context(|| format!("starting {}", spec.name))?;
        let label = Arc::new(Mutex::new(agent.label()));
        let session = Arc::new(Mutex::new(agent.session_id()));
        let cancel = Cancel::default();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(actor(
            agent,
            spec.clone(),
            opts,
            root.clone(),
            rx,
            events,
            cancel.clone(),
            label.clone(),
            session.clone(),
        ));
        Ok(AgentBackend {
            tx,
            cancel,
            name: spec.name,
            label,
            session,
            root,
            branch,
        })
    }
}

#[allow(clippy::too_many_arguments)]
async fn actor(
    mut agent: Box<dyn Agent>,
    spec: Spec,
    opts: Opts,
    root: PathBuf,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<Cmd>,
    events: std::sync::mpsc::Sender<UiEvent>,
    cancel: Cancel,
    label: Arc<Mutex<String>>,
    session: Arc<Mutex<Option<String>>>,
) {
    while let Some(cmd) = rx.recv().await {
        match cmd {
            Cmd::Prompt(text) => {
                cancel.reset();
                let (tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
                let ui = events.clone();
                let forward = tokio::spawn(async move {
                    while let Some(e) = ev_rx.recv().await {
                        let _ = ui.send(match e {
                            Event::Text(t) => UiEvent::Delta(t),
                            Event::Thinking(t) => UiEvent::Thinking(t),
                            Event::Tool(t) => UiEvent::Tool(tool_event(t)),
                            Event::Note(n) => UiEvent::Note(n),
                        });
                    }
                });
                let started = std::time::Instant::now();
                let result = agent.prompt(&text, &tx, &cancel).await;
                drop(tx);
                let _ = forward.await;
                if let Some(id) = agent.session_id() {
                    remember_session(&root, &spec.name, &id);
                    *session.lock().expect("session") = Some(id);
                }
                *label.lock().expect("label") = agent.label();
                let done = match result {
                    Ok(end) => turn_done(&end, started.elapsed().as_millis()),
                    Err(e) => Err(format!("{e:#}")),
                };
                let _ = events.send(UiEvent::Done(done));
            }
            Cmd::Compact => {
                let _ = events.send(UiEvent::Done(Err(format!(
                    "{} manages its own context; /compact is the built-in engine's",
                    spec.name
                ))));
            }
            Cmd::New => {
                let fresh = Opts {
                    resume: None,
                    ..opts.clone()
                };
                match hasp_agents::start(&spec, fresh).await {
                    Ok(a) => {
                        agent = a;
                        *label.lock().expect("label") = agent.label();
                        *session.lock().expect("session") = agent.session_id();
                        let _ = events.send(UiEvent::Note(format!("▪ new {} session", spec.name)));
                    }
                    Err(e) => {
                        let _ = events.send(UiEvent::Note(format!("✗ {e:#}")));
                    }
                }
            }
        }
    }
}

impl Backend for AgentBackend {
    fn submit_prompt(&mut self, prompt: String) {
        let _ = self.tx.send(Cmd::Prompt(prompt));
    }

    fn submit_compact(&mut self) {
        let _ = self.tx.send(Cmd::Compact);
    }

    fn cancel(&mut self) {
        self.cancel.cancel();
    }

    fn call(&mut self, method: &str, _params: Value) -> Result<Value> {
        match method {
            "session/stats" => Ok(json!({
                "sessionId": self.session.lock().expect("session").clone(),
                "model": self.label.lock().expect("label").clone(),
                "root": self.root.display().to_string(),
                "branch": self.branch,
                "window": 0,
                "entries": 0,
                "input": 0, "cacheRead": 0, "cacheWrite": 0, "output": 0,
                "estTokens": 0,
                "ctx": {"tokens": 0, "measured": false},
            })),
            "session/history" => Ok(json!({"total": 0, "messages": []})),
            "session/new" => {
                let _ = self.tx.send(Cmd::New);
                Ok(json!({"sessionId": null}))
            }
            "workspace/files" => {
                let out = std::process::Command::new("git")
                    .args(["ls-files", "--cached", "--others", "--exclude-standard"])
                    .current_dir(&self.root)
                    .output()?;
                let files: Vec<String> = String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .take(4000)
                    .map(String::from)
                    .collect();
                Ok(json!({"files": files}))
            }
            other => bail!(
                "{other} is not available while driving {} — it runs its own session \
                 (its own slash commands work: type them as a prompt)",
                self.name
            ),
        }
    }
}
