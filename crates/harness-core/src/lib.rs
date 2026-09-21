//! Core types, session model, context assembly, turn engine, trace.
//! Provider/Tool/Gate are traits defined here; implementations live in
//! satellite crates so the dependency graph stays acyclic.

pub mod compaction;
pub mod context;
pub mod cron;
pub mod engine;
pub mod session;
pub mod trace;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use harness_workspace::Workspace;
use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---- messages ----

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: Value,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    #[default]
    Stop,
    ToolUse,
    Length,
    Aborted,
    Error,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl Usage {
    pub fn total_in(&self) -> u64 {
        self.input + self.cache_read + self.cache_write
    }

    pub fn add(&mut self, other: &Usage) {
        self.input += other.input;
        self.output += other.output;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum AgentMessage {
    User {
        content: String,
        ts: i64,
    },
    Assistant {
        content: Vec<ContentBlock>,
        provider: String,
        model: String,
        usage: Usage,
        stop_reason: StopReason,
        ts: i64,
    },
    ToolResult {
        tool_call_id: String,
        tool_name: String,
        content: String,
        is_error: bool,
        ts: i64,
    },
}

/// Token estimate for budgeting and the context bar.
///
/// Calibrated rather than naive `chars/4`: words run about four characters per
/// token, but punctuation and digits tokenize far more densely — which is
/// exactly why code costs more than prose at the same character count — and
/// non-ASCII text is charged per character.
///
/// Deliberately not a real BPE. A bundled table would be exact for one model
/// family only, and the models in use here (Claude, DeepSeek) are not that
/// family, so it would be a differently-shaped guess with a megabyte attached.
/// Where accuracy matters — the compaction trigger — the provider's own
/// reported usage is used instead, and this is only the fallback.
pub fn estimate_tokens(messages: &[AgentMessage]) -> u64 {
    messages
        .iter()
        .map(|m| match m {
            AgentMessage::User { content, .. } | AgentMessage::ToolResult { content, .. } => {
                estimate_text_tokens(content)
            }
            AgentMessage::Assistant { content, .. } => content
                .iter()
                .map(|b| match b {
                    ContentBlock::Text { text } => estimate_text_tokens(text),
                    ContentBlock::Thinking { thinking } => estimate_text_tokens(thinking),
                    ContentBlock::ToolCall {
                        name, arguments, ..
                    } => estimate_text_tokens(name) + estimate_text_tokens(&arguments.to_string()),
                })
                .sum(),
        })
        .sum()
}

/// Tokens in one string (see [`estimate_tokens`]).
pub fn estimate_text_tokens(text: &str) -> u64 {
    let mut tokens = 0.0f64;
    let mut run: u32 = 0;
    let mut class = Class::Space;
    for ch in text.chars() {
        let next = Class::of(ch);
        if next == class {
            run += 1;
            continue;
        }
        tokens += class.cost(run);
        class = next;
        run = 1;
    }
    (tokens + class.cost(run)).ceil() as u64
}

#[derive(Clone, Copy, PartialEq)]
enum Class {
    Space,
    Word,
    Punct,
    Digit,
    /// Anything outside ASCII: charged per character.
    Wide,
}

impl Class {
    fn of(ch: char) -> Self {
        if ch.is_whitespace() {
            Class::Space
        } else if ch.is_ascii_digit() {
            Class::Digit
        } else if !ch.is_ascii() {
            Class::Wide
        } else if ch.is_alphanumeric() || ch == '_' || ch == '-' {
            Class::Word
        } else {
            Class::Punct
        }
    }

    /// Tokens consumed by a run of `len` characters of this class.
    ///
    /// Fractional on purpose: rounding each run up would charge a lone space a
    /// whole token, which roughly doubles the estimate for ordinary prose.
    /// Every non-whitespace run still costs at least one token — a lone `;` or
    /// `==` is a token of its own — while whitespace mostly merges into the
    /// neighbouring token and is charged in bulk.
    fn cost(self, len: u32) -> f64 {
        if len == 0 {
            return 0.0;
        }
        match self {
            Class::Space => len as f64 / 10.0,
            Class::Word => (len as f64 / 4.0).max(1.0),
            Class::Punct | Class::Digit => (len as f64 / 2.0).max(1.0),
            Class::Wide => len as f64,
        }
    }
}

// ---- provider ----

#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Clone)]
pub struct ModelRequest {
    pub system: String,
    pub tools: Vec<ToolSpec>,
    pub messages: Vec<AgentMessage>,
    pub model: String,
    pub max_tokens: u32,
}

#[derive(Debug, Clone)]
pub struct AssistantTurn {
    pub blocks: Vec<ContentBlock>,
    pub usage: Usage,
    pub stop_reason: StopReason,
}

/// Streaming callback for live token deltas.
pub trait Sink: Send {
    fn on_delta(&mut self, text: &str);

    /// Reasoning/thinking deltas, kept separate from user-visible text. Sinks
    /// that do not care (plain mode, children) ignore it by default.
    fn on_thinking(&mut self, _text: &str) {}
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("provider api error {status}: {body}")]
    Api { status: u16, body: String },
    #[error("transport error: {0}")]
    Transport(String),
    #[error("protocol error: {0}")]
    Protocol(String),
}

#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &'static str;
    fn context_window(&self) -> usize;

    async fn complete(
        &self,
        req: &ModelRequest,
        sink: &mut dyn Sink,
    ) -> Result<AssistantTurn, ProviderError>;
}

// ---- tools ----

#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

impl ToolOutput {
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

/// Out-of-line store for fat tool outputs (the handle design, §4.2):
/// `store()` returns a workspace-relative path the model pages with `read`.
pub struct ArtifactStore {
    dir: PathBuf,
    run_id: String,
    seq: AtomicU32,
}

impl ArtifactStore {
    pub fn new(dir: PathBuf, run_id: String) -> std::io::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            run_id,
            seq: AtomicU32::new(0),
        })
    }

    pub fn store(&self, kind: &str, bytes: &[u8]) -> std::io::Result<String> {
        let n = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let name = format!("{kind}-{n:04}.txt");
        std::fs::write(self.dir.join(&name), bytes)?;
        Ok(format!(".harness/artifacts/{}/{}", self.run_id, name))
    }
}

pub struct ToolCtx<'a> {
    pub workspace: &'a Workspace,
    pub gate: &'a mut dyn Gate,
    pub artifacts: &'a ArtifactStore,
    pub bus: Option<&'a dyn Bus>,
}

#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;

    async fn execute(&self, args: Value, ctx: &mut ToolCtx<'_>) -> ToolOutput;
}

// ---- gate ----

/// The gate resolves approvals; prompting (when interactive) happens inside
/// the implementation. A `Deny` becomes the tool result so the model adapts.
pub enum GateDecision {
    Allow,
    Deny(String),
}

pub trait Gate: Send + Sync {
    fn check_bash(&mut self, command: &str) -> GateDecision;
    fn check_write(&mut self, path: &str) -> GateDecision;
}

// ---- agent bus (research/03-architecture.md §8.2) ----

#[derive(Debug, Clone)]
pub struct SpawnRequest {
    pub task: String,
    pub name: Option<String>,
    pub mutation: bool,
}

#[derive(Debug, Clone)]
pub struct SpawnHandle {
    pub child_id: String,
    pub session_id: String,
    pub root: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ChildStatus {
    Running,
    Completed,
    Failed(String),
}

impl ChildStatus {
    pub fn label(&self) -> &str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed(_) => "failed",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ChildInfo {
    pub child_id: String,
    pub name: String,
    pub status: ChildStatus,
    pub session_id: String,
    pub mutation: bool,
    pub root: String,
    /// The child's final message, once it has one. Polling `agents` should be
    /// able to answer "what did it say?" without waiting for a notice.
    pub report: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ChildReceipt {
    /// Reached an idle target's context (a turn was triggered).
    Delivered,
    /// Accepted for delivery at the target's next boundary.
    Queued,
}

impl ChildReceipt {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Delivered => "delivered",
            Self::Queued => "queued",
        }
    }
}

/// The agent bus: spawn returns an admission handle, never the answer;
/// results arrive as notices and messages. One level deep — children of
/// children are the parent's job (depth cap by construction: child engines
/// get base tools only).
#[async_trait::async_trait]
pub trait Bus: Send + Sync {
    async fn spawn_child(&self, req: SpawnRequest) -> Result<SpawnHandle, String>;

    /// Queued-and-never-awaited is the hard invariant: this never blocks on
    /// the recipient running a turn to completion.
    async fn message_child(
        &self,
        child_id: &str,
        text: &str,
        trigger: bool,
    ) -> Result<ChildReceipt, String>;

    async fn list_children(&self) -> Vec<ChildInfo>;
}
