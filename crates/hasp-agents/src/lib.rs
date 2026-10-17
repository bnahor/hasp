//! External coding agents behind the harness: bring your own plan.
//!
//! A Claude or ChatGPT subscription can only be spent by its vendor's own
//! client, so the harness drives that client — the `claude` or `codex` CLI
//! the user already installed and logged into — the way an editor drives a
//! language server, and puts its own layer on top: the TUI, approvals,
//! reviews, worktrees. Three protocols cover the field:
//!
//! * [`codex`]: `codex app-server`, JSON-RPC over stdio (ChatGPT plans).
//! * [`claude`]: `claude -p` with stream-json in and out, permissions asked
//!   over the same pipe (Claude plans).
//! * [`acp`]: the Agent Client Protocol, which Gemini CLI, OpenCode and a
//!   growing list of agents speak natively; any ACP agent can be added in
//!   `~/.harness/agents.json`.
//!
//! Every adapter turns its agent's native stream into the same few
//! [`Event`]s, and every approval its agent asks for goes through one
//! [`Asker`] — so the rest of the harness does not care which agent runs.

pub mod acp;
pub mod claude;
pub mod codex;
mod proc;
pub mod specs;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use tokio::sync::{Notify, mpsc};

pub use specs::{Protocol, Spec};

/// Something the agent did, as it happens.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Answer text, streamed.
    Text(String),
    /// Reasoning, streamed, when the agent shares it.
    Thinking(String),
    /// A finished tool call.
    Tool(ToolCall),
    /// Anything worth telling the user that is neither (warnings, retries).
    Note(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub name: String,
    /// The command, the path, the query: what the call was about.
    pub subject: String,
    pub output: String,
    pub is_error: bool,
    pub ms: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Usage {
    /// Input tokens that were neither read from nor written to a cache.
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

/// How a turn ended.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TurnEnd {
    pub usage: Usage,
    pub tool_calls: usize,
    pub cancelled: bool,
    /// The agent reported the turn failed (not logged in, model refused, …).
    pub error: Option<String>,
}

/// The user's answer to an approval request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Once,
    /// Allow this and anything like it for the rest of the session.
    Always,
    Deny,
}

/// Asks the user a yes/no question; blocking (it waits for a human), so
/// adapters call it off the async runtime.
pub type Asker = Arc<dyn Fn(String) -> Decision + Send + Sync>;

/// Everything an adapter needs to start its agent.
#[derive(Clone)]
pub struct Opts {
    pub cwd: PathBuf,
    /// Model override; `None` keeps the agent's own default.
    pub model: Option<String>,
    /// Lift the agent's own approval prompts (its own sandbox still applies).
    pub yolo: bool,
    /// The agent's session to continue, if any.
    pub resume: Option<String>,
    pub ask: Asker,
}

/// Cancels the running turn. Cheap to clone; reset at the start of a turn.
#[derive(Clone, Default)]
pub struct Cancel {
    inner: Arc<(AtomicBool, Notify)>,
}

impl Cancel {
    pub fn cancel(&self) {
        self.inner.0.store(true, Ordering::SeqCst);
        self.inner.1.notify_waiters();
    }

    pub fn reset(&self) {
        self.inner.0.store(false, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.0.load(Ordering::SeqCst)
    }

    /// Resolves once [`Cancel::cancel`] has been called.
    pub async fn cancelled(&self) {
        loop {
            let notified = self.inner.1.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

/// A running agent.
#[async_trait::async_trait]
pub trait Agent: Send {
    /// "codex · gpt-5.6-sol": what the footer shows.
    fn label(&self) -> String;
    /// The agent's own session id, for resuming it next time.
    fn session_id(&self) -> Option<String>;
    /// Run one turn to the end, streaming what happens into `events`.
    async fn prompt(
        &mut self,
        text: &str,
        events: &mpsc::UnboundedSender<Event>,
        cancel: &Cancel,
    ) -> Result<TurnEnd>;
}

/// Start the agent a spec describes.
pub async fn start(spec: &Spec, opts: Opts) -> Result<Box<dyn Agent>> {
    Ok(match spec.protocol {
        Protocol::Codex => Box::new(codex::Codex::start(spec, opts).await?),
        Protocol::Claude => Box::new(claude::Claude::start(spec, opts).await?),
        Protocol::Acp => Box::new(acp::Acp::start(spec, opts).await?),
    })
}

/// Ask on a blocking thread; a failed ask is a denial.
pub(crate) async fn ask(asker: &Asker, question: String) -> Decision {
    let asker = asker.clone();
    tokio::task::spawn_blocking(move || asker(question))
        .await
        .unwrap_or(Decision::Deny)
}

/// Keep tool output readable in a transcript.
pub(crate) fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… ({} more bytes)", &s[..end], s.len() - end)
}

/// Tool output shown in the transcript is capped at this many bytes.
pub(crate) const OUTPUT_MAX: usize = 8_000;
