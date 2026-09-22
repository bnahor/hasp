//! Turn engine: sample → tools → sample (research/03-architecture.md §5).
//! One user prompt may produce many model samples; tools run between samples.

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
    AgentMessage, ArtifactStore, Bus, ChildStatus, ContentBlock, Gate, ModelRequest, Provider,
    Sink, Tool, ToolCtx, ToolOutput, ToolSpec, Usage,
};

pub const MAX_SAMPLES_PER_TURN: usize = 50;
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
    pub provider: Box<dyn Provider>,
    pub tools: Vec<Box<dyn Tool>>,
    pub workspace: Workspace,
    pub session: SessionManager,
    pub trace: Trace,
    pub artifacts: ArtifactStore,
    pub gate: Box<dyn Gate>,
    pub model: String,
    pub max_tokens: u32,
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
            provider,
            tools,
            workspace,
            session,
            trace,
            artifacts,
            gate,
            model,
            max_tokens,
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

    fn drain_notices(&mut self) -> Vec<String> {
        let mut inbox = self.notice_inbox.lock().expect("notice inbox");
        let take = inbox.len().min(NOTICE_DRAIN_CAP);
        inbox.drain(..take).collect()
    }

    fn deliver_notices(&mut self, sink: &mut dyn TurnSink) {
        for notice in self.drain_notices() {
            let note_line = format!("▪ {notice}");
            self.session.append_message(AgentMessage::User {
                content: notice,
                ts: now_ms(),
            });
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

    /// After a turn: pick up notices, wait for spawned children to settle, then
    /// decide whether a goal or autonomous mode wants another turn. Returns the
    /// continuation prompt; the caller runs it and calls back here.
    pub async fn post_turn_check(&mut self, sink: &mut dyn TurnSink) -> Option<String> {
        self.deliver_notices(sink);
        if let Some(bus) = self.bus.clone() {
            let running = |cs: Vec<crate::ChildInfo>| {
                cs.iter().any(|c| matches!(c.status, ChildStatus::Running))
            };
            if running(bus.list_children().await) {
                sink.on_note("▪ holding continuation — children still running");
                while running(bus.list_children().await) {
                    tokio::time::sleep(std::time::Duration::from_millis(CHILD_POLL_MS)).await;
                }
                self.deliver_notices(sink);
            }
        }
        if let Some(cont) = self.goal_continuation(sink) {
            return Some(cont);
        }
        self.autonomous_continuation(sink).await
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

    async fn autonomous_continuation(&mut self, sink: &mut dyn TurnSink) -> Option<String> {
        let a = self.session.autonomous()?;
        if !a.on {
            return None;
        }
        if a.max_turns.is_some_and(|m| self.auto_turns >= m) {
            self.stop_autonomous(&a, sink, "turn limit reached");
            return None;
        }
        let used = self
            .session_tokens()
            .saturating_sub(self.auto_baseline_tokens);
        if a.max_tokens.is_some_and(|m| used >= m) {
            self.stop_autonomous(&a, sink, "token limit reached");
            return None;
        }
        let gate = match a.gate.clone() {
            Some(g) => g,
            None => {
                self.auto_turns += 1;
                sink.on_note(&format!(
                    "▪ autonomous mode: continuing (turn {})",
                    self.auto_turns
                ));
                return Some(
                    "Autonomous mode: continue the task. Stop when the work is complete.".into(),
                );
            }
        };
        let (passed, output) = self.run_gate(&gate).await;
        self.trace.record(json!({
            "event": "gate",
            "command": gate,
            "passed": passed,
        }));
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

    /// Run the autonomous-mode quality gate in the workspace root. Output is
    /// tail-capped; a timeout kills the whole process group.
    async fn run_gate(&self, cmd: &str) -> (bool, String) {
        let mut command = tokio::process::Command::new("bash");
        command
            .arg("-lc")
            .arg(cmd)
            .current_dir(self.workspace.root())
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
                #[cfg(unix)]
                if let Some(pid) = pid {
                    let _ = std::process::Command::new("kill")
                        .arg("-TERM")
                        .arg(format!("-{pid}"))
                        .status();
                }
                (
                    false,
                    format!("gate timed out after {}s", GATE_TIMEOUT.as_secs()),
                )
            }
        }
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

    pub async fn run_turn(&mut self, prompt: &str, sink: &mut dyn TurnSink) -> Result<TurnStats> {
        let start = Instant::now();
        let mut stats = TurnStats {
            samples: 0,
            tool_calls: 0,
            usage: Usage::default(),
            ms: 0,
            aborted: false,
        };
        // `/name args` naming a prompt template runs the template. Expanded
        // here, where every front end (TUI, REPL, one-shot, ACP, web) meets.
        let expanded = crate::extensions::expand_command(self.workspace.root(), prompt);
        if expanded.is_some() {
            sink.on_note(&format!(
                "▪ running command {}",
                prompt.split_whitespace().next().unwrap_or_default()
            ));
        }
        self.session.append_message(AgentMessage::User {
            content: expanded.unwrap_or_else(|| prompt.to_string()),
            ts: now_ms(),
        });
        self.deliver_notices(sink);
        if !self.hook_warnings_shown {
            self.hook_warnings_shown = true;
            for w in &self.hooks.warnings {
                sink.on_note(&format!("▪ hooks: {w}"));
            }
        }
        let mut stop_rounds = 0;

        for _ in 0..MAX_SAMPLES_PER_TURN {
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
            let req = ModelRequest {
                system: ctx.system,
                tools: specs,
                messages: ctx.messages,
                model: self.model.clone(),
                max_tokens: self.max_tokens,
            };
            let t0 = Instant::now();
            let sink_base: &mut dyn Sink = sink;
            let mut fut = Box::pin(self.provider.complete(&req, sink_base));
            let turn = loop {
                let notified = self.cancel_notify.notified();
                tokio::pin!(notified);
                tokio::select! {
                    t = &mut fut => match t {
                        Ok(t) => break Some(t),
                        Err(e) => {
                            self.trace.record(json!({
                                "event": "api_error", "provider": self.provider.name(),
                                "error": e.to_string(), "ms": t0.elapsed().as_millis() as u64,
                            }));
                            return Err(e.into());
                        }
                    },
                    _ = &mut notified => {
                        // Stale permits (e.g. a cancel that raced turn-start and
                        // was reset) just re-check the flag and keep polling
                        // the same sample.
                        if self.cancel_flag.load(Ordering::Relaxed) {
                            break None;
                        }
                    }
                }
            };
            drop(fut); // release the sink borrow before notes below
            let turn = match turn {
                Some(t) => t,
                None => {
                    stats.aborted = true;
                    sink.on_note("▪ turn cancelled");
                    break;
                }
            };
            let api_ms = t0.elapsed().as_millis() as u64;
            stats.samples += 1;
            stats.usage.add(&turn.usage);
            self.trace
                .record_api(self.provider.name(), &self.model, api_ms, &turn.usage);
            self.session.append_message(AgentMessage::Assistant {
                content: turn.blocks.clone(),
                provider: self.provider.name().to_string(),
                model: self.model.clone(),
                usage: turn.usage,
                stop_reason: turn.stop_reason,
                ts: now_ms(),
            });

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
                if self.hooks.has_stop()
                    && stop_rounds < crate::hooks::STOP_HOOK_ROUNDS
                    && !self.cancel_flag.load(Ordering::Relaxed)
                    && let Verdict::Feedback(msg) = self.hooks.stop().await
                {
                    stop_rounds += 1;
                    sink.on_note("▪ stop hook sent the model back");
                    self.session.append_message(AgentMessage::User {
                        content: format!(
                            "A stop hook reported a problem with the work so far. Address it, \
                             then finish:\n\n{msg}"
                        ),
                        ts: now_ms(),
                    });
                    continue;
                }
                break;
            }
            let cancelled = self.run_tool_calls(calls, &mut stats, sink).await;
            // Child exits and heartbeats land here between tool calls — the
            // whole point of the notice inbox. Delivering only at turn start
            // left a parent blind to its own children's reports for the rest of
            // a long turn, so it would go hunting the filesystem for them.
            self.deliver_notices(sink);
            if cancelled || self.cancel_flag.load(Ordering::Relaxed) {
                stats.aborted = true;
                sink.on_note("▪ turn cancelled");
                break;
            }
            // A `max_tokens` stop that still produced tool calls keeps going:
            // any call it cut off got an error result, and the model deserves
            // a sample to re-issue it rather than a silently dead turn.
        }

        if stats.samples >= MAX_SAMPLES_PER_TURN {
            sink.on_note("turn stopped: sample limit reached");
        }
        let window = self.provider.context_window() as u64;
        let (ctx_tokens, measured) = self.context_tokens();
        let est = ctx_tokens;
        if est as f64 / window as f64 > COMPACT_WARN_AT {
            sink.on_note(&format!(
                "▪ context {approx}{est}k/{window}k tokens — compacting…",
                approx = if measured { "" } else { "~" },
                est = est / 1000,
                window = window / 1000,
            ));
            match compaction::compact(
                self.provider.as_ref(),
                &self.model,
                &mut self.session,
                self.max_tokens,
                est,
            )
            .await
            {
                Ok(cs) => {
                    self.trace.record(json!({
                        "event": "compaction",
                        "tokens_before": cs.tokens_before,
                        "summary_tokens": cs.summary_tokens,
                        "retained": cs.retained_messages,
                    }));
                    sink.on_note(&format!(
                        "▪ compacted ~{}k tokens → brief + {} retained message(s)",
                        cs.tokens_before / 1000,
                        cs.retained_messages
                    ));
                }
                Err(e) => sink.on_note(&format!(
                    "compaction failed: {e} (start a new session if quality degrades)"
                )),
            }
        }
        stats.ms = start.elapsed().as_millis();
        Ok(stats)
    }

    /// Run one model turn's tool calls and journal every result, in call
    /// order. Consecutive concurrency-safe calls run together; everything else
    /// runs one at a time because it may prompt. A cancel interrupts the call
    /// in flight (its future is dropped, which kills a running command) and
    /// every call without a result gets one saying so — the transcript must
    /// answer each `tool_use` or the next request is rejected. Returns whether
    /// the turn was cancelled.
    async fn run_tool_calls(
        &mut self,
        calls: Vec<(String, String, Value)>,
        stats: &mut TurnStats,
        sink: &mut dyn TurnSink,
    ) -> bool {
        let mut cancelled = false;
        let mut i = 0;
        while i < calls.len() {
            let mut end = i + 1;
            if self.is_concurrency_safe(&calls[i].1) {
                while end < calls.len() && self.is_concurrency_safe(&calls[end].1) {
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
                let notify = self.cancel_notify.clone();
                let flag = self.cancel_flag.clone();
                let run = self.execute_batch(batch);
                tokio::pin!(run);
                loop {
                    let notified = notify.notified();
                    tokio::pin!(notified);
                    tokio::select! {
                        outs = &mut run => break outs,
                        _ = &mut notified => {
                            if flag.load(Ordering::Relaxed) {
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
            for ((id, name, args), (out, tool_ms)) in batch.iter().zip(outputs) {
                stats.tool_calls += 1;
                self.trace.record_tool(name, tool_ms, !out.is_error);
                sink.on_tool(&ToolEvent {
                    name: name.clone(),
                    args_preview: args_preview(args),
                    content_preview: content_preview(&out.content),
                    is_error: out.is_error,
                    ms: tool_ms,
                    content_len: out.content.len(),
                });
                self.session.append_message(AgentMessage::ToolResult {
                    tool_call_id: id.clone(),
                    tool_name: name.clone(),
                    content: out.content,
                    is_error: out.is_error,
                    ts: now_ms(),
                });
            }
            i = end;
        }
        cancelled
    }

    fn is_concurrency_safe(&self, name: &str) -> bool {
        self.tools
            .iter()
            .find(|t| t.spec().name == name)
            .is_some_and(|t| t.concurrency_safe())
    }

    /// One call on its own (with the real gate), or a run of read-only calls
    /// concurrently (each under [`crate::RefuseGate`]).
    async fn execute_batch(&mut self, batch: &[(String, String, Value)]) -> Vec<(ToolOutput, u64)> {
        let hooks = self.hooks.clone();
        if let [(_, name, args)] = batch {
            let t = Instant::now();
            let out = match hooks.pre_tool(name, args).await {
                Verdict::Feedback(msg) => ToolOutput::error(format!("blocked by a hook: {msg}")),
                Verdict::Continue => self.execute_tool(name, args).await,
            };
            let out = with_post_hook(&hooks, name, args, out).await;
            return vec![(out, t.elapsed().as_millis() as u64)];
        }
        let (workspace, artifacts, bus) = (&self.workspace, &self.artifacts, self.bus.as_deref());
        let tools = &self.tools;
        let hooks = &hooks;
        let futs = batch.iter().map(|(_, name, args)| async move {
            let t = Instant::now();
            if let Verdict::Feedback(msg) = hooks.pre_tool(name, args).await {
                let out = ToolOutput::error(format!("blocked by a hook: {msg}"));
                return (out, t.elapsed().as_millis() as u64);
            }
            let out = match tools.iter().find(|tool| tool.spec().name == *name) {
                Some(_) if args.get(crate::INVALID_INPUT_KEY).is_some() => invalid_input_error(),
                Some(tool) => {
                    let mut gate = crate::RefuseGate;
                    let mut ctx = ToolCtx {
                        workspace,
                        gate: &mut gate,
                        artifacts,
                        bus,
                    };
                    tool.execute(args.clone(), &mut ctx).await
                }
                None => ToolOutput::error(format!("unknown tool: {name}")),
            };
            let out = with_post_hook(hooks, name, args, out).await;
            (out, t.elapsed().as_millis() as u64)
        });
        futures_util::future::join_all(futs).await
    }

    async fn execute_tool(&mut self, name: &str, args: &Value) -> ToolOutput {
        if args.get(crate::INVALID_INPUT_KEY).is_some() {
            return invalid_input_error();
        }
        // Meta tool: the engine owns goal state, so this never reaches a Tool.
        if name == GOAL_COMPLETE_TOOL {
            return match self.session.goal() {
                Some(g) if !g.completed => {
                    self.session
                        .append_goal("complete", &g.text, g.budget_tokens);
                    ToolOutput::text(format!("goal marked complete: {}", g.text))
                }
                Some(_) => ToolOutput::text("goal was already complete"),
                None => ToolOutput::error("no active goal to complete"),
            };
        }
        let Some(tool) = self.tools.iter().find(|t| t.spec().name == name) else {
            return ToolOutput::error(format!("unknown tool: {name}"));
        };
        let mut ctx = ToolCtx {
            workspace: &self.workspace,
            gate: self.gate.as_mut(),
            artifacts: &self.artifacts,
            bus: self.bus.as_deref(),
        };
        tool.execute(args.clone(), &mut ctx).await
    }

    /// Name-sorted specs: byte-stable tool order is part of the cache contract.
    pub fn tool_specs(&self) -> Vec<ToolSpec> {
        let mut specs: Vec<ToolSpec> = self.tools.iter().map(|t| t.spec()).collect();
        specs.sort_by(|a, b| a.name.cmp(&b.name));
        specs
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
