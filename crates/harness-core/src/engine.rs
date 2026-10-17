//! Turn engine: sample → tools → sample (research/03-architecture.md §5).
//! One user prompt may produce many model samples; tools run between samples.
//!
//! The turn driver ([`run_turn_shared`]) takes the engine lock only to read or
//! change state — building a request, journaling a sample, journaling tool
//! results. The slow parts run with it released: the provider stream, tool
//! execution, hook commands, waiting for children, the autonomous gate and the
//! compaction summary. So a daemon can answer `/stats`, switch `/model` or set
//! a goal while a turn is mid-flight. Turns themselves are serialized by the
//! caller (the daemon's turn lock). [`Engine::run_turn`] wraps the same driver
//! for callers that simply own an engine.

use anyhow::Result;
use harness_workspace::Workspace;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use crate::compaction;
use crate::context;
use crate::hooks::{Hooks, Verdict};
use crate::session::{AutoState, SessionManager, now_ms};
use crate::trace::Trace;
use crate::{
    AgentMessage, ArtifactStore, Bus, ChildStatus, ContentBlock, Gate, Image, ModelRequest,
    Provider, Sink, Tool, ToolCtx, ToolOutput, ToolSpec, Usage,
};

/// Model calls one prompt may make before the turn stops. A backstop against
/// a runaway loop, not a budget: goal and autonomous modes have their own
/// token budgets, and a user can cancel at any time. 50 was too tight for
/// real work — on SWE-bench Verified 22% of tasks reached it (4 of 104 failed
/// there); reference agents allow 250 steps.
pub const MAX_SAMPLES_PER_TURN: usize = 250;
pub const COMPACT_WARN_AT: f64 = 0.85;
const NOTICE_DRAIN_CAP: usize = 8;
/// Hard stop on goal continuations even with no token budget — the model may
/// never call `goal_complete`.
pub const GOAL_CONTINUATION_CAP: u64 = 100;
/// Budget applied to goals that don't name one: a runaway continuation loop
/// re-sends the whole context every turn, so an unbounded goal can spend real
/// money before anyone notices. `--unlimited` opts out deliberately.
pub const GOAL_DEFAULT_BUDGET_TOKENS: u64 = 2_000_000;
const CHILD_POLL_MS: u64 = 250;
/// Retries of one sample after a transient provider failure (backoff 4, 8,
/// 16 s). Before anything streams, the provider clients retry on their own;
/// this covers what they cannot: a stream that breaks partway.
pub const SAMPLE_RETRIES: u32 = 3;
const GATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const GATE_OUTPUT_CAP: usize = 2000;
pub const GOAL_COMPLETE_TOOL: &str = "goal_complete";

/// Streaming callback plus human-facing notes (tool start/end, warnings).
pub trait TurnSink: Sink {
    fn on_note(&mut self, note: &str);

    /// Richer per-tool reporting. The default keeps the historical one-line
    /// note, so sinks that do not care (and the raw daemon wire) are unaffected.
    fn on_tool(&mut self, ev: &ToolEvent) {
        self.on_note(&ev.summary());
    }
}

/// What one tool call did. The UI shows more than a note can carry: the
/// command/path, the output, and whether it failed.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolEvent {
    pub name: String,
    /// Human-facing subject: the command, the path, or compact JSON.
    pub args_preview: String,
    pub content_preview: String,
    pub is_error: bool,
    pub ms: u64,
    /// Full output length, so the UI can say how much it is not showing.
    pub content_len: usize,
}

impl ToolEvent {
    pub fn summary(&self) -> String {
        format!(
            "{} {} ({} ms)",
            if self.is_error { "✗" } else { "✓" },
            self.name,
            self.ms
        )
    }
}

/// One line naming what the tool acted on.
pub fn args_preview(args: &Value) -> String {
    for key in ["command", "path", "file", "pattern", "query", "task"] {
        if let Some(v) = args.get(key).and_then(|v| v.as_str()) {
            return v.lines().next().unwrap_or_default().to_string();
        }
    }
    let compact = args.to_string();
    if compact == "{}" || compact == "null" {
        String::new()
    } else {
        compact
    }
}

/// Head of a tool's output: enough to be useful, capped so a huge dump cannot
/// swamp the transcript (the model still gets the full text).
pub const TOOL_PREVIEW_LINES: usize = 40;
pub const TOOL_PREVIEW_CHARS: usize = 4000;

pub fn content_preview(content: &str) -> String {
    let mut out = String::new();
    for (i, line) in content.lines().enumerate() {
        if i >= TOOL_PREVIEW_LINES || out.len() >= TOOL_PREVIEW_CHARS {
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    if out.len() > TOOL_PREVIEW_CHARS {
        // Find the boundary *before* cutting: `str::truncate` panics when the
        // cut lands inside a multi-byte character.
        let mut cut = TOOL_PREVIEW_CHARS;
        while cut > 0 && !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
        out.push('…');
    }
    out.trim_end().to_string()
}

#[derive(Clone)]
pub struct Canceller {
    flag: Arc<AtomicBool>,
    notify: Arc<tokio::sync::Notify>,
}

impl Canceller {
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::Relaxed);
        // notify_one leaves a permit even with no waiter, so a cancel that
        // races turn-start is still observed.
        self.notify.notify_one();
    }

    pub fn reset(&self) {
        self.flag.store(false, Ordering::Relaxed);
    }
}

#[derive(Clone)]
pub struct TurnStats {
    pub samples: usize,
    pub tool_calls: usize,
    pub usage: Usage,
    pub ms: u128,
    pub aborted: bool,
}

pub struct Engine {
    pub provider: Arc<dyn Provider>,
    /// Shared with in-flight tool calls; change it between turns with
    /// [`Engine::tools_mut`].
    pub tools: Arc<Vec<Box<dyn Tool>>>,
    pub workspace: Workspace,
    pub session: SessionManager,
    pub trace: Trace,
    pub artifacts: Arc<ArtifactStore>,
    /// Its own lock: held by the one gated tool call running, never by the
    /// engine lock's holders.
    pub gate: Arc<tokio::sync::Mutex<Box<dyn Gate>>>,
    pub model: String,
    pub max_tokens: u32,
    /// Reasoning effort for this session's requests; `None` is the
    /// provider's default for the model.
    pub effort: Option<String>,
    cancel_flag: Arc<AtomicBool>,
    cancel_notify: Arc<tokio::sync::Notify>,
    notice_inbox: Arc<Mutex<Vec<String>>>,
    bus: Option<Arc<dyn Bus>>,
    /// Goal continuation turns since the goal was last set.
    goal_continuations: u64,
    /// Autonomous-mode turn count and the token baseline it is measured from.
    auto_turns: u64,
    auto_baseline_tokens: u64,
    hooks: Arc<Hooks>,
    hook_warnings_shown: bool,
}

/// Something the turn driver can lock to reach an [`Engine`]: a shared
/// `tokio::sync::Mutex<Engine>` (the daemon), or [`Exclusive`] around a plain
/// `&mut Engine`.
pub trait EngineCell: Sync {
    type Guard<'a>: std::ops::DerefMut<Target = Engine> + Send
    where
        Self: 'a;
    fn lock(&self) -> impl std::future::Future<Output = Self::Guard<'_>> + Send;
}

impl EngineCell for tokio::sync::Mutex<Engine> {
    type Guard<'a> = tokio::sync::MutexGuard<'a, Engine>;
    fn lock(&self) -> impl std::future::Future<Output = Self::Guard<'_>> + Send {
        tokio::sync::Mutex::lock(self)
    }
}

/// An engine the caller owns outright, presented as a cell.
pub struct Exclusive<'e>(tokio::sync::Mutex<&'e mut Engine>);

impl<'e> Exclusive<'e> {
    pub fn new(engine: &'e mut Engine) -> Self {
        Self(tokio::sync::Mutex::new(engine))
    }
}

impl EngineCell for Exclusive<'_> {
    type Guard<'a>
        = tokio::sync::MappedMutexGuard<'a, Engine>
    where
        Self: 'a;
    async fn lock(&self) -> Self::Guard<'_> {
        tokio::sync::MutexGuard::map(self.0.lock().await, |e| &mut **e)
    }
}

/// What tool calls need, cloned out from under the engine lock.
struct ToolEnv {
    tools: Arc<Vec<Box<dyn Tool>>>,
    workspace: Workspace,
    artifacts: Arc<ArtifactStore>,
    bus: Option<Arc<dyn Bus>>,
    gate: Arc<tokio::sync::Mutex<Box<dyn Gate>>>,
    hooks: Arc<Hooks>,
}

impl ToolEnv {
    fn find(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|t| t.spec().name == name)
            .map(|t| t.as_ref())
    }

    fn is_concurrency_safe(&self, name: &str) -> bool {
        self.find(name).is_some_and(|t| t.concurrency_safe())
    }
}

/// Where the autonomous-mode check stands before (maybe) running the gate.
enum AutoStep {
    Done(Option<String>),
    Gate {
        command: String,
        root: std::path::PathBuf,
    },
}

impl Engine {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        workspace: Workspace,
        session: SessionManager,
        provider: Box<dyn Provider>,
        tools: Vec<Box<dyn Tool>>,
        gate: Box<dyn Gate>,
        model: String,
        max_tokens: u32,
    ) -> std::io::Result<Self> {
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let cancel_notify = Arc::new(tokio::sync::Notify::new());
        let run_id = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
        let state = workspace
            .state_dir()
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let trace = Trace::create(&state.join("traces"), &run_id)?;
        let artifacts = ArtifactStore::new(
            workspace
                .root()
                .join(".harness")
                .join("artifacts")
                .join(&run_id),
            run_id,
        )?;
        let hooks = Arc::new(Hooks::load(workspace.root()));
        Ok(Self {
            provider: Arc::from(provider),
            tools: Arc::new(tools),
            workspace,
            session,
            trace,
            artifacts: Arc::new(artifacts),
            gate: Arc::new(tokio::sync::Mutex::new(gate)),
            model,
            max_tokens,
            effort: None,
            cancel_flag,
            cancel_notify,
            notice_inbox: Arc::new(Mutex::new(Vec::new())),
            bus: None,
            goal_continuations: 0,
            auto_turns: 0,
            auto_baseline_tokens: 0,
            hooks,
            hook_warnings_shown: false,
        })
    }

    /// The tool list, for changing between turns.
    pub fn tools_mut(&mut self) -> &mut Vec<Box<dyn Tool>> {
        Arc::get_mut(&mut self.tools).expect("tools are only shared while a turn runs")
    }

    /// Shared handle to this engine's notice inbox — the bus pushes child
    /// exits here without ever taking the engine lock.
    pub fn notice_inbox(&self) -> Arc<Mutex<Vec<String>>> {
        self.notice_inbox.clone()
    }

    /// Replace the hooks loaded from disk (tests, embedders).
    pub fn set_hooks(&mut self, hooks: Hooks) {
        self.hooks = Arc::new(hooks);
    }

    pub fn set_bus(&mut self, bus: Arc<dyn Bus>) {
        self.bus = Some(bus);
    }

    fn tool_env(&self) -> ToolEnv {
        ToolEnv {
            tools: self.tools.clone(),
            workspace: self.workspace.clone(),
            artifacts: self.artifacts.clone(),
            bus: self.bus.clone(),
            gate: self.gate.clone(),
            hooks: self.hooks.clone(),
        }
    }

    fn drain_notices(&mut self) -> Vec<String> {
        let mut inbox = self.notice_inbox.lock().expect("notice inbox");
        let take = inbox.len().min(NOTICE_DRAIN_CAP);
        inbox.drain(..take).collect()
    }

    fn deliver_notices(&mut self, sink: &mut dyn TurnSink) {
        for notice in self.drain_notices() {
            let note_line = format!("▪ {notice}");
            self.session.append_message(AgentMessage::user(notice));
            sink.on_note(&note_line);
        }
    }

    /// User-facing goal control. `action` is `set|pause|resume|clear`;
    /// `complete` is reserved for the engine (the `goal_complete` tool).
    pub fn set_goal(&mut self, action: &str, text: &str, budget_tokens: Option<u64>) {
        if action == "set" {
            self.goal_continuations = 0;
        }
        self.session.append_goal(action, text, budget_tokens);
    }

    pub fn set_autonomous(
        &mut self,
        on: bool,
        max_turns: Option<u64>,
        max_tokens: Option<u64>,
        gate: Option<&str>,
    ) {
        self.auto_turns = 0;
        self.auto_baseline_tokens = self.session_tokens();
        self.session
            .append_autonomous(on, max_turns, max_tokens, gate);
    }

    /// Total tokens attributed to this session so far (input incl. cache + output).
    fn session_tokens(&self) -> u64 {
        let t = self.session.totals();
        t.total_in() + t.output
    }

    /// Context size for the context bar and the compaction trigger: the
    /// provider's own count from the last sample when it still describes the
    /// context (exact), otherwise the calibrated estimate. The estimate is a
    /// floor, so a stale count can never make the context look smaller than it is.
    /// The `bool` says whether the number was measured.
    pub fn context_tokens(&self) -> (u64, bool) {
        match self.session.last_input_tokens() {
            Some(measured) => (measured.max(self.context_est_tokens()), true),
            None => (self.context_est_tokens(), false),
        }
    }

    /// Context size as the compaction threshold measures it — system prompt and
    /// tool schemas included, unlike the raw message estimate.
    pub fn context_est_tokens(&self) -> u64 {
        context::assemble(&self.session, &self.tool_specs()).est_tokens
    }

    /// The next sample's request, from the journal as it stands.
    fn build_request(&self) -> ModelRequest {
        let specs = self.tool_specs();
        let idle = self
            .session
            .context_messages()
            .iter()
            .rev()
            .find_map(|m| match m {
                AgentMessage::Assistant { ts, .. } => Some(*ts),
                _ => None,
            })
            .is_some_and(|ts| now_ms() - ts > context::IDLE_PRUNE_AFTER_MS);
        let ctx = context::assemble_with(&self.session, &specs, idle);
        ModelRequest {
            system: ctx.system,
            tools: specs,
            messages: ctx.messages,
            model: self.model.clone(),
            max_tokens: self.max_tokens,
            effort: self.effort.clone(),
        }
    }

    /// After a turn: pick up notices, wait for spawned children to settle, then
    /// decide whether a goal or autonomous mode wants another turn. Returns the
    /// continuation prompt; the caller runs it and calls back here.
    pub async fn post_turn_check(&mut self, sink: &mut dyn TurnSink) -> Option<String> {
        post_turn_check_shared(&Exclusive::new(self), sink).await
    }

    pub async fn run_turn(&mut self, prompt: &str, sink: &mut dyn TurnSink) -> Result<TurnStats> {
        run_turn_shared(&Exclusive::new(self), prompt, Vec::new(), sink).await
    }

    /// Summarize older history now (`/compact`).
    pub async fn compact(&mut self) -> Result<compaction::CompactStats> {
        compact_shared(&Exclusive::new(self)).await
    }

    fn goal_continuation(&mut self, sink: &mut dyn TurnSink) -> Option<String> {
        let g = self.session.goal()?;
        if g.completed || g.paused {
            return None;
        }
        if let Some(budget) = g.budget_tokens {
            let used = self.session_tokens();
            if used >= budget {
                self.session.append_goal("pause", &g.text, g.budget_tokens);
                sink.on_note(&format!(
                    "▪ goal paused: token budget {budget} reached ({used} used)"
                ));
                return None;
            }
        }
        if self.goal_continuations >= GOAL_CONTINUATION_CAP {
            self.session.append_goal("pause", &g.text, g.budget_tokens);
            sink.on_note(&format!(
                "▪ goal paused: continuation cap ({GOAL_CONTINUATION_CAP}) reached"
            ));
            return None;
        }
        self.goal_continuations += 1;
        sink.on_note(&format!(
            "▪ continuing toward goal (turn {})",
            self.goal_continuations
        ));
        Some(format!(
            "Continue working toward this goal:\n\n{}\n\nCall the {GOAL_COMPLETE_TOOL} tool once \
             the goal is fully achieved. If you are blocked and need input, say so and stop.",
            g.text
        ))
    }

    /// Everything about autonomous mode that does not need the gate command's
    /// result.
    fn autonomous_step(&mut self, sink: &mut dyn TurnSink) -> AutoStep {
        let Some(a) = self.session.autonomous() else {
            return AutoStep::Done(None);
        };
        if !a.on {
            return AutoStep::Done(None);
        }
        if a.max_turns.is_some_and(|m| self.auto_turns >= m) {
            self.stop_autonomous(&a, sink, "turn limit reached");
            return AutoStep::Done(None);
        }
        let used = self
            .session_tokens()
            .saturating_sub(self.auto_baseline_tokens);
        if a.max_tokens.is_some_and(|m| used >= m) {
            self.stop_autonomous(&a, sink, "token limit reached");
            return AutoStep::Done(None);
        }
        match a.gate.clone() {
            Some(command) => AutoStep::Gate {
                command,
                root: self.workspace.root().to_path_buf(),
            },
            None => {
                self.auto_turns += 1;
                sink.on_note(&format!(
                    "▪ autonomous mode: continuing (turn {})",
                    self.auto_turns
                ));
                AutoStep::Done(Some(
                    "Autonomous mode: continue the task. Stop when the work is complete.".into(),
                ))
            }
        }
    }

    fn autonomous_after_gate(
        &mut self,
        sink: &mut dyn TurnSink,
        gate: &str,
        passed: bool,
        output: String,
    ) -> Option<String> {
        self.trace.record(json!({
            "event": "gate",
            "command": gate,
            "passed": passed,
        }));
        // Switched off (or changed) while the gate ran: the user's word wins.
        let a = self.session.autonomous().filter(|a| a.on)?;
        if passed {
            sink.on_note("▪ gate passed — autonomous mode stopping");
            self.stop_autonomous(&a, sink, "gate passed");
            return None;
        }
        self.auto_turns += 1;
        sink.on_note(&format!(
            "▪ gate failed — continuing (turn {})",
            self.auto_turns
        ));
        Some(format!(
            "Autonomous mode: the quality gate still fails.\n\n`{gate}` output:\n```\n{output}\n\
             ```\n\nFix the failure, then stop once the gate passes."
        ))
    }

    fn stop_autonomous(&mut self, a: &AutoState, sink: &mut dyn TurnSink, why: &str) {
        self.session
            .append_autonomous(false, a.max_turns, a.max_tokens, a.gate.as_deref());
        sink.on_note(&format!("▪ autonomous mode off: {why}"));
    }

    /// A handle that cancels the in-flight turn. `reset()` clears a previous
    /// cancel so the next turn starts fresh; callers that queue turns
    /// (the daemon) reset before starting one.
    pub fn canceller(&self) -> Canceller {
        Canceller {
            flag: self.cancel_flag.clone(),
            notify: self.cancel_notify.clone(),
        }
    }

    /// Name-sorted specs: byte-stable tool order is part of the cache contract.
    pub fn tool_specs(&self) -> Vec<ToolSpec> {
        let mut specs: Vec<ToolSpec> = self.tools.iter().map(|t| t.spec()).collect();
        specs.sort_by(|a, b| a.name.cmp(&b.name));
        specs
    }
}

/// After a turn (see [`Engine::post_turn_check`]), without holding the lock
/// while children finish or the autonomous gate runs.
pub async fn post_turn_check_shared<C: EngineCell>(
    cell: &C,
    sink: &mut dyn TurnSink,
) -> Option<String> {
    let bus = {
        let mut eng = cell.lock().await;
        eng.deliver_notices(sink);
        eng.bus.clone()
    };
    if let Some(bus) = bus {
        let running =
            |cs: Vec<crate::ChildInfo>| cs.iter().any(|c| matches!(c.status, ChildStatus::Running));
        if running(bus.list_children().await) {
            sink.on_note("▪ holding continuation — children still running");
            while running(bus.list_children().await) {
                tokio::time::sleep(std::time::Duration::from_millis(CHILD_POLL_MS)).await;
            }
            cell.lock().await.deliver_notices(sink);
        }
    }
    let step = {
        let mut eng = cell.lock().await;
        if let Some(cont) = eng.goal_continuation(sink) {
            return Some(cont);
        }
        eng.autonomous_step(sink)
    };
    match step {
        AutoStep::Done(next) => next,
        AutoStep::Gate { command, root } => {
            let (passed, output) = run_gate(&root, &command).await;
            cell.lock()
                .await
                .autonomous_after_gate(sink, &command, passed, output)
        }
    }
}

/// Run a user prompt to completion. `images` are attached to the prompt in
/// addition to any the prompt text names (see [`crate::image::attachments`]).
pub async fn run_turn_shared<C: EngineCell>(
    cell: &C,
    prompt: &str,
    mut images: Vec<Image>,
    sink: &mut dyn TurnSink,
) -> Result<TurnStats> {
    let start = Instant::now();
    let mut stats = TurnStats {
        samples: 0,
        tool_calls: 0,
        usage: Usage::default(),
        ms: 0,
        aborted: false,
    };
    let root = cell.lock().await.workspace.root().to_path_buf();
    // `/name args` naming a prompt template runs the template. Expanded here,
    // where every front end (TUI, REPL, one-shot, ACP, web) meets.
    let expanded = crate::extensions::expand_command(&root, prompt);
    if expanded.is_some() {
        sink.on_note(&format!(
            "▪ running command {}",
            prompt.split_whitespace().next().unwrap_or_default()
        ));
    }
    let text = expanded.unwrap_or_else(|| prompt.to_string());
    let (text, found, notes) = crate::image::attachments(&text, &root);
    for n in notes {
        sink.on_note(&format!("▪ {n}"));
    }
    images.extend(found);
    if !images.is_empty() {
        sink.on_note(&format!("▪ {} image(s) attached", images.len()));
    }
    let (cancel_flag, cancel_notify) = {
        let mut eng = cell.lock().await;
        eng.session.append_message(AgentMessage::User {
            content: text,
            ts: now_ms(),
            images,
        });
        eng.deliver_notices(sink);
        if !eng.hook_warnings_shown {
            eng.hook_warnings_shown = true;
            for w in &eng.hooks.warnings {
                sink.on_note(&format!("▪ hooks: {w}"));
            }
        }
        (eng.cancel_flag.clone(), eng.cancel_notify.clone())
    };
    let mut stop_rounds = 0;

    for _ in 0..MAX_SAMPLES_PER_TURN {
        let (req, provider) = {
            let eng = cell.lock().await;
            (eng.build_request(), eng.provider.clone())
        };
        let t0 = Instant::now();
        let mut attempt = 0;
        let turn = loop {
            let outcome = {
                let sink_base: &mut dyn Sink = sink;
                let fut = provider.complete(&req, sink_base);
                tokio::pin!(fut);
                loop {
                    let notified = cancel_notify.notified();
                    tokio::pin!(notified);
                    tokio::select! {
                        t = &mut fut => break Some(t),
                        _ = &mut notified => {
                            // Stale permits (e.g. a cancel that raced turn-start
                            // and was reset) just re-check the flag and keep
                            // polling the same sample.
                            if cancel_flag.load(Ordering::Relaxed) {
                                break None;
                            }
                        }
                    }
                }
            };
            match outcome {
                None => break None,
                Some(Ok(t)) => break Some(t),
                Some(Err(e)) => {
                    cell.lock().await.trace.record(json!({
                        "event": "api_error", "provider": provider.name(),
                        "error": e.to_string(), "ms": t0.elapsed().as_millis() as u64,
                        "attempt": attempt + 1,
                    }));
                    // Nothing of a failed sample is journaled, so it can be
                    // re-issued as-is. A dropped stream an hour into a task
                    // must not throw the task away.
                    if !e.is_transient() || attempt >= SAMPLE_RETRIES {
                        return Err(e.into());
                    }
                    attempt += 1;
                    let wait = std::time::Duration::from_secs(2u64 << attempt);
                    sink.on_note(&format!(
                        "▪ provider error ({e}) — retrying the sample in {}s ({attempt}/{SAMPLE_RETRIES})",
                        wait.as_secs()
                    ));
                    let notified = cancel_notify.notified();
                    tokio::select! {
                        _ = tokio::time::sleep(wait) => {}
                        _ = notified => {}
                    }
                    if cancel_flag.load(Ordering::Relaxed) {
                        break None;
                    }
                }
            }
        };
        let Some(turn) = turn else {
            stats.aborted = true;
            sink.on_note("▪ turn cancelled");
            break;
        };
        let api_ms = t0.elapsed().as_millis() as u64;
        stats.samples += 1;
        stats.usage.add(&turn.usage);
        {
            let mut eng = cell.lock().await;
            eng.trace
                .record_api(provider.name(), &req.model, api_ms, &turn.usage);
            // The model that produced the sample, not whatever `/model` says
            // now: thinking replays only to its producer.
            eng.session.append_message(AgentMessage::Assistant {
                content: turn.blocks.clone(),
                provider: provider.name().to_string(),
                model: req.model.clone(),
                usage: turn.usage,
                stop_reason: turn.stop_reason,
                ts: now_ms(),
            });
        }

        let calls: Vec<(String, String, Value)> = turn
            .blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolCall {
                    id,
                    name,
                    arguments,
                } => Some((id.clone(), name.clone(), arguments.clone())),
                _ => None,
            })
            .collect();
        if calls.is_empty() {
            // The model is done — unless a stop hook disagrees.
            let hooks = cell.lock().await.hooks.clone();
            if hooks.has_stop()
                && stop_rounds < crate::hooks::STOP_HOOK_ROUNDS
                && !cancel_flag.load(Ordering::Relaxed)
                && let Verdict::Feedback(msg) = hooks.stop().await
            {
                stop_rounds += 1;
                sink.on_note("▪ stop hook sent the model back");
                cell.lock()
                    .await
                    .session
                    .append_message(AgentMessage::user(format!(
                        "A stop hook reported a problem with the work so far. Address it, \
                     then finish:\n\n{msg}"
                    )));
                continue;
            }
            break;
        }
        let cancelled =
            run_tool_calls(cell, calls, &mut stats, sink, &cancel_flag, &cancel_notify).await;
        // Child exits and heartbeats land here between tool calls — the whole
        // point of the notice inbox. Delivering only at turn start left a
        // parent blind to its own children's reports for the rest of a long
        // turn, so it would go hunting the filesystem for them.
        cell.lock().await.deliver_notices(sink);
        if cancelled || cancel_flag.load(Ordering::Relaxed) {
            stats.aborted = true;
            sink.on_note("▪ turn cancelled");
            break;
        }
        // A `max_tokens` stop that still produced tool calls keeps going: any
        // call it cut off got an error result, and the model deserves a sample
        // to re-issue it rather than a silently dead turn.
    }

    if stats.samples >= MAX_SAMPLES_PER_TURN {
        sink.on_note("turn stopped: sample limit reached");
    }
    maybe_compact(cell, sink).await;
    stats.ms = start.elapsed().as_millis();
    Ok(stats)
}

/// Compact when the context is over the threshold.
async fn maybe_compact<C: EngineCell>(cell: &C, sink: &mut dyn TurnSink) {
    let plan = {
        let eng = cell.lock().await;
        let window = eng.provider.context_window() as u64;
        let (est, measured) = eng.context_tokens();
        if est as f64 / window as f64 <= COMPACT_WARN_AT {
            return;
        }
        sink.on_note(&format!(
            "▪ context {approx}{k}k/{w}k tokens — compacting…",
            approx = if measured { "" } else { "~" },
            k = est / 1000,
            w = window / 1000,
        ));
        compaction::prepare(
            &eng.session,
            &eng.model,
            eng.max_tokens,
            compaction::TAIL_TOKEN_BUDGET,
            est,
        )
    };
    match compact_planned(cell, plan).await {
        Ok(cs) => sink.on_note(&format!(
            "▪ compacted ~{}k tokens → brief + {} retained message(s)",
            cs.tokens_before / 1000,
            cs.retained_messages
        )),
        Err(e) => sink.on_note(&format!(
            "compaction failed: {e} (start a new session if quality degrades)"
        )),
    }
}

/// `/compact`: summarize older history, holding the lock only to plan and to
/// apply — not while the model writes the summary.
pub async fn compact_shared<C: EngineCell>(cell: &C) -> Result<compaction::CompactStats> {
    let plan = {
        let eng = cell.lock().await;
        compaction::prepare(
            &eng.session,
            &eng.model,
            eng.max_tokens,
            compaction::TAIL_TOKEN_BUDGET,
            eng.context_est_tokens(),
        )
    };
    compact_planned(cell, plan).await
}

async fn compact_planned<C: EngineCell>(
    cell: &C,
    plan: Result<compaction::CompactionPlan>,
) -> Result<compaction::CompactStats> {
    let plan = plan?;
    let provider = cell.lock().await.provider.clone();
    let turn = provider
        .complete(&plan.request, &mut compaction::Mute)
        .await?;
    let mut eng = cell.lock().await;
    let cs = compaction::finish(plan, turn, &mut eng.session)?;
    eng.trace.record(json!({
        "event": "compaction",
        "tokens_before": cs.tokens_before,
        "summary_tokens": cs.summary_tokens,
        "retained": cs.retained_messages,
    }));
    Ok(cs)
}

/// Run one model turn's tool calls and journal every result, in call order.
/// Consecutive concurrency-safe calls run together; everything else runs one
/// at a time because it may prompt. A cancel interrupts the call in flight
/// (its future is dropped, which kills a running command) and every call
/// without a result gets one saying so — the transcript must answer each
/// `tool_use` or the next request is rejected. Returns whether the turn was
/// cancelled.
async fn run_tool_calls<C: EngineCell>(
    cell: &C,
    calls: Vec<(String, String, Value)>,
    stats: &mut TurnStats,
    sink: &mut dyn TurnSink,
    cancel_flag: &AtomicBool,
    cancel_notify: &tokio::sync::Notify,
) -> bool {
    let env = cell.lock().await.tool_env();
    let mut cancelled = false;
    let mut i = 0;
    while i < calls.len() {
        let mut end = i + 1;
        if env.is_concurrency_safe(&calls[i].1) {
            while end < calls.len() && env.is_concurrency_safe(&calls[end].1) {
                end += 1;
            }
        }
        let batch = &calls[i..end];
        let outputs: Vec<(ToolOutput, u64)> = if cancelled {
            batch
                .iter()
                .map(|_| (ToolOutput::error("not run: the turn was cancelled"), 0))
                .collect()
        } else {
            for (_, name, _) in batch {
                sink.on_note(&format!("▶ {name} …"));
            }
            let run = execute_batch(cell, &env, batch);
            tokio::pin!(run);
            loop {
                let notified = cancel_notify.notified();
                tokio::pin!(notified);
                tokio::select! {
                    outs = &mut run => break outs,
                    _ = &mut notified => {
                        if cancel_flag.load(Ordering::Relaxed) {
                            cancelled = true;
                            break batch
                                .iter()
                                .map(|_| (ToolOutput::error("interrupted: the user cancelled the turn"), 0))
                                .collect();
                        }
                    }
                }
            }
        };
        let mut eng = cell.lock().await;
        for ((id, name, args), (out, tool_ms)) in batch.iter().zip(outputs) {
            stats.tool_calls += 1;
            eng.trace.record_tool(name, tool_ms, !out.is_error);
            sink.on_tool(&ToolEvent {
                name: name.clone(),
                args_preview: args_preview(args),
                content_preview: content_preview(&out.content),
                is_error: out.is_error,
                ms: tool_ms,
                content_len: out.content.len(),
            });
            eng.session.append_message(AgentMessage::ToolResult {
                tool_call_id: id.clone(),
                tool_name: name.clone(),
                content: out.content,
                is_error: out.is_error,
                ts: now_ms(),
                images: out.images,
            });
        }
        drop(eng);
        i = end;
    }
    cancelled
}

/// One call on its own (under the real gate), or a run of read-only calls
/// concurrently (each under [`crate::RefuseGate`]).
async fn execute_batch<C: EngineCell>(
    cell: &C,
    env: &ToolEnv,
    batch: &[(String, String, Value)],
) -> Vec<(ToolOutput, u64)> {
    if let [(_, name, args)] = batch {
        let t = Instant::now();
        let out = match env.hooks.pre_tool(name, args).await {
            Verdict::Feedback(msg) => ToolOutput::error(format!("blocked by a hook: {msg}")),
            Verdict::Continue => execute_one(cell, env, name, args).await,
        };
        let out = with_post_hook(&env.hooks, name, args, out).await;
        return vec![(out, t.elapsed().as_millis() as u64)];
    }
    let futs = batch.iter().map(|(_, name, args)| async move {
        let t = Instant::now();
        if let Verdict::Feedback(msg) = env.hooks.pre_tool(name, args).await {
            let out = ToolOutput::error(format!("blocked by a hook: {msg}"));
            return (out, t.elapsed().as_millis() as u64);
        }
        let out = match env.find(name) {
            Some(_) if args.get(crate::INVALID_INPUT_KEY).is_some() => invalid_input_error(),
            Some(tool) => {
                let mut gate = crate::RefuseGate;
                let mut ctx = ToolCtx {
                    workspace: &env.workspace,
                    gate: &mut gate,
                    artifacts: &env.artifacts,
                    bus: env.bus.as_deref(),
                };
                tool.execute(args.clone(), &mut ctx).await
            }
            None => ToolOutput::error(format!("unknown tool: {name}")),
        };
        let out = with_post_hook(&env.hooks, name, args, out).await;
        (out, t.elapsed().as_millis() as u64)
    });
    futures_util::future::join_all(futs).await
}

async fn execute_one<C: EngineCell>(
    cell: &C,
    env: &ToolEnv,
    name: &str,
    args: &Value,
) -> ToolOutput {
    if args.get(crate::INVALID_INPUT_KEY).is_some() {
        return invalid_input_error();
    }
    // Meta tool: the engine owns goal state, so this never reaches a Tool.
    if name == GOAL_COMPLETE_TOOL {
        let mut eng = cell.lock().await;
        return match eng.session.goal() {
            Some(g) if !g.completed => {
                eng.session
                    .append_goal("complete", &g.text, g.budget_tokens);
                ToolOutput::text(format!("goal marked complete: {}", g.text))
            }
            Some(_) => ToolOutput::text("goal was already complete"),
            None => ToolOutput::error("no active goal to complete"),
        };
    }
    let Some(tool) = env.find(name) else {
        return ToolOutput::error(format!("unknown tool: {name}"));
    };
    let mut gate = env.gate.lock().await;
    let mut ctx = ToolCtx {
        workspace: &env.workspace,
        gate: gate.as_mut(),
        artifacts: &env.artifacts,
        bus: env.bus.as_deref(),
    };
    tool.execute(args.clone(), &mut ctx).await
}

/// Run the autonomous-mode quality gate in the workspace root. Output is
/// tail-capped; a timeout kills the whole process group.
async fn run_gate(root: &std::path::Path, cmd: &str) -> (bool, String) {
    let mut command = tokio::process::Command::new("bash");
    command
        .arg("-lc")
        .arg(cmd)
        .current_dir(root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let Ok(child) = command.spawn() else {
        return (false, "gate failed to spawn".into());
    };
    let pid = child.id();
    match tokio::time::timeout(GATE_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(out)) => {
            let mut text = String::from_utf8_lossy(&out.stdout).to_string();
            let err = String::from_utf8_lossy(&out.stderr);
            if !err.trim().is_empty() {
                text.push_str("\n--- stderr ---\n");
                text.push_str(&err);
            }
            (out.status.success(), tail(&text, GATE_OUTPUT_CAP))
        }
        _ => {
            if let Some(pid) = pid {
                crate::process::signal_group(pid, crate::process::SIGTERM);
            }
            (
                false,
                format!("gate timed out after {}s", GATE_TIMEOUT.as_secs()),
            )
        }
    }
}

/// Append `post_tool` hook feedback to a tool's result.
async fn with_post_hook(
    hooks: &Hooks,
    name: &str,
    args: &Value,
    mut out: ToolOutput,
) -> ToolOutput {
    if let Verdict::Feedback(msg) = hooks
        .post_tool(name, args, &out.content, out.is_error)
        .await
    {
        out.content.push_str(&format!("\n\n[hook feedback]\n{msg}"));
    }
    out
}

fn invalid_input_error() -> ToolOutput {
    ToolOutput::error(
        "tool input was not valid JSON (most likely cut off by the output token limit); \
         nothing was run. Re-issue the call — for large content, split it into smaller edits.",
    )
}

/// Last `cap` chars, on a char boundary — the end of gate output is the
/// interesting part.
fn tail(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    let mut start = s.len() - cap;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    format!("[… output truncated …]\n{}", &s[start..])
}
