//! Codex through `codex app-server`: JSON-RPC over stdio, one thread per
//! session. Shapes follow `codex app-server generate-json-schema` (v2).

use std::collections::{HashMap, HashSet};

use anyhow::{Result, bail};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::proc::{Proc, same_id};
use crate::{
    Agent, Asker, Cancel, Decision, Event, OUTPUT_MAX, Opts, Spec, ToolCall, TurnEnd, Usage, clip,
};

pub struct Codex {
    proc: Proc,
    next_id: u64,
    thread: String,
    model: String,
    ask: Asker,
    /// Thread totals as last reported; a turn's usage is the difference.
    totals: Usage,
}

impl Codex {
    pub async fn start(spec: &Spec, opts: Opts) -> Result<Codex> {
        let proc = Proc::spawn("codex", &spec.command, &spec.args, &opts)?;
        let mut c = Codex {
            proc,
            next_id: 0,
            thread: String::new(),
            model: String::new(),
            ask: opts.ask.clone(),
            totals: Usage::default(),
        };
        c.request(
            "initialize",
            json!({"clientInfo": {"name": "hasp", "title": "hasp", "version": env!("CARGO_PKG_VERSION")}}),
        )
        .await?;
        c.proc.send(&json!({"method": "initialized"})).await?;
        let policy = if opts.yolo { "never" } else { "on-request" };
        let cwd = opts.cwd.display().to_string();
        let started = match &opts.resume {
            Some(id) => {
                c.request(
                    "thread/resume",
                    json!({"threadId": id, "cwd": cwd, "approvalPolicy": policy,
                           "sandbox": "workspace-write", "model": opts.model}),
                )
                .await
            }
            None => Err(anyhow::anyhow!("no thread to resume")),
        };
        let started = match started {
            Ok(r) => r,
            // A thread that cannot be resumed (deleted, other machine) is
            // not fatal: start fresh.
            Err(_) => {
                c.request(
                    "thread/start",
                    json!({"cwd": cwd, "approvalPolicy": policy,
                           "sandbox": "workspace-write", "model": opts.model}),
                )
                .await?
            }
        };
        c.thread = started["thread"]["id"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        c.model = started["model"].as_str().unwrap_or("default").to_string();
        if c.thread.is_empty() {
            bail!("codex started no thread: {started}");
        }
        Ok(c)
    }

    /// Send a request and wait for its response; anything else arriving
    /// meanwhile is setup noise (notifications) or refused (requests).
    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        self.proc
            .send(&json!({"id": id, "method": method, "params": params}))
            .await?;
        loop {
            let m = self.proc.next().await?;
            if m.get("method").is_none() && same_id(&m["id"], id) {
                if let Some(e) = m.get("error") {
                    bail!("codex {method}: {}", error_text(e));
                }
                return Ok(m["result"].clone());
            }
            if m.get("method").is_some() && m.get("id").is_some() {
                self.refuse(&m).await?;
            }
        }
    }

    async fn refuse(&mut self, m: &Value) -> Result<()> {
        self.proc
            .send(&json!({"id": m["id"], "error": {"code": -32601,
                "message": format!("hasp does not handle {}", m["method"])}}))
            .await
    }

    async fn answer_approval(
        &mut self,
        m: &Value,
        pending: &HashMap<String, String>,
    ) -> Result<()> {
        let method = m["method"].as_str().unwrap_or_default();
        let p = &m["params"];
        let item = p["itemId"].as_str().unwrap_or_default();
        let question = match method {
            "item/commandExecution/requestApproval" => {
                let cmd = p["command"]
                    .as_str()
                    .map(String::from)
                    .or_else(|| pending.get(item).cloned())
                    .unwrap_or_else(|| "a command".into());
                format!("codex wants to run `{cmd}`{}", reason(p))
            }
            "item/fileChange/requestApproval" => format!(
                "codex wants to change {}{}",
                pending.get(item).map(String::as_str).unwrap_or("files"),
                reason(p)
            ),
            "execCommandApproval" => format!(
                "codex wants to run `{}`{}",
                p["command"]
                    .as_array()
                    .map(|a| a
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" "))
                    .unwrap_or_default(),
                reason(p)
            ),
            "applyPatchApproval" => format!("codex wants to apply a patch{}", reason(p)),
            _ => return self.refuse(m).await,
        };
        let d = crate::ask(&self.ask, format!("{question} [y]es / [a]lways / [n]o")).await;
        let legacy = matches!(method, "execCommandApproval" | "applyPatchApproval");
        let decision = match (d, legacy) {
            (Decision::Once, false) => "accept",
            (Decision::Always, false) => "acceptForSession",
            (Decision::Deny, false) => "decline",
            (Decision::Once, true) => "approved",
            (Decision::Always, true) => "approved_for_session",
            (Decision::Deny, true) => "denied",
        };
        self.proc
            .send(&json!({"id": m["id"], "result": {"decision": decision}}))
            .await
    }
}

impl Codex {
    async fn interrupt(&mut self, turn: &str) -> Result<()> {
        self.next_id += 1;
        let id = self.next_id;
        self.proc
            .send(&json!({"id": id, "method": "turn/interrupt",
                "params": {"threadId": self.thread, "turnId": turn}}))
            .await
    }
}

fn reason(p: &Value) -> String {
    p["reason"]
        .as_str()
        .filter(|r| !r.is_empty())
        .map(|r| format!(" ({r})"))
        .unwrap_or_default()
}

/// Codex error messages are sometimes a JSON document in a string; dig out
/// the human part.
fn error_text(e: &Value) -> String {
    let msg = e["message"].as_str().unwrap_or("error").to_string();
    serde_json::from_str::<Value>(&msg)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(String::from))
        .unwrap_or(msg)
}

fn usage_of(v: &Value) -> Usage {
    // Codex's input count includes the cached part.
    let cached = v["cachedInputTokens"].as_u64().unwrap_or(0);
    Usage {
        input: v["inputTokens"]
            .as_u64()
            .unwrap_or(0)
            .saturating_sub(cached),
        output: v["outputTokens"].as_u64().unwrap_or(0),
        cache_read: cached,
        cache_write: v["cacheWriteInputTokens"].as_u64().unwrap_or(0),
    }
}

/// A finished item as a tool call, if it is one.
fn tool_of(item: &Value) -> Option<ToolCall> {
    let ms = item["durationMs"].as_u64().unwrap_or(0);
    let failed = |status: &str| matches!(status, "failed" | "declined");
    let status = item["status"].as_str().unwrap_or("");
    Some(match item["type"].as_str()? {
        "commandExecution" => ToolCall {
            name: "bash".into(),
            subject: item["command"].as_str().unwrap_or_default().to_string(),
            output: clip(
                item["aggregatedOutput"].as_str().unwrap_or_default(),
                OUTPUT_MAX,
            ),
            is_error: failed(status) || item["exitCode"].as_i64().is_some_and(|c| c != 0),
            ms,
        },
        "fileChange" => {
            let changes = item["changes"].as_array().cloned().unwrap_or_default();
            ToolCall {
                name: "edit".into(),
                subject: paths(&changes),
                output: clip(
                    &changes
                        .iter()
                        .filter_map(|c| c["diff"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n"),
                    OUTPUT_MAX,
                ),
                is_error: failed(status),
                ms,
            }
        }
        "mcpToolCall" => ToolCall {
            name: format!(
                "{}.{}",
                item["server"].as_str().unwrap_or("mcp"),
                item["tool"].as_str().unwrap_or("tool")
            ),
            subject: clip(&item["arguments"].to_string(), 200),
            output: clip(
                &item["result"]["content"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|c| c["text"].as_str())
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .or_else(|| item["error"]["message"].as_str().map(String::from))
                    .unwrap_or_default(),
                OUTPUT_MAX,
            ),
            is_error: failed(status) || !item["error"].is_null(),
            ms,
        },
        "webSearch" => ToolCall {
            name: "web_search".into(),
            subject: item["query"].as_str().unwrap_or_default().to_string(),
            output: String::new(),
            is_error: false,
            ms,
        },
        _ => return None,
    })
}

fn paths(changes: &[Value]) -> String {
    changes
        .iter()
        .filter_map(|c| c["path"].as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

#[async_trait::async_trait]
impl Agent for Codex {
    fn label(&self) -> String {
        format!("codex · {}", self.model)
    }

    fn session_id(&self) -> Option<String> {
        Some(self.thread.clone())
    }

    async fn prompt(
        &mut self,
        text: &str,
        events: &mpsc::UnboundedSender<Event>,
        cancel: &Cancel,
    ) -> Result<TurnEnd> {
        self.next_id += 1;
        let start_id = self.next_id;
        self.proc
            .send(&json!({"id": start_id, "method": "turn/start", "params": {
                "threadId": self.thread,
                "input": [{"type": "text", "text": text, "text_elements": []}],
            }}))
            .await?;
        let before = self.totals;
        let mut end = TurnEnd::default();
        let mut turn_id = String::new();
        let mut interrupted = false;
        let mut interrupt_sent = false;
        let mut said_anything = false;
        let mut streamed: HashSet<String> = HashSet::new();
        // What an in-flight item is about, for approval questions.
        let mut pending: HashMap<String, String> = HashMap::new();
        loop {
            let next = tokio::select! {
                m = self.proc.next() => Some(m?),
                _ = cancel.cancelled(), if !interrupted => None,
            };
            let Some(m) = next else {
                interrupted = true;
                if !turn_id.is_empty() {
                    self.interrupt(&turn_id).await?;
                    interrupt_sent = true;
                }
                continue;
            };
            let method = m["method"].as_str().unwrap_or_default();
            let p = &m["params"];
            if method.is_empty() {
                if same_id(&m["id"], start_id) {
                    if let Some(e) = m.get("error") {
                        end.error = Some(error_text(e));
                        return Ok(end);
                    }
                    if let Some(id) = m["result"]["turn"]["id"].as_str() {
                        turn_id = id.to_string();
                    }
                }
                // A cancel that arrived before the turn had an id.
                if interrupted && !interrupt_sent && !turn_id.is_empty() {
                    self.interrupt(&turn_id).await?;
                    interrupt_sent = true;
                }
                continue;
            }
            if m.get("id").is_some() {
                self.answer_approval(&m, &pending).await?;
                continue;
            }
            match method {
                "turn/started" => {
                    if let Some(id) = p["turn"]["id"].as_str() {
                        turn_id = id.to_string();
                    }
                    if interrupted && !interrupt_sent && !turn_id.is_empty() {
                        self.interrupt(&turn_id).await?;
                        interrupt_sent = true;
                    }
                }
                "item/agentMessage/delta" => {
                    if let Some(d) = p["delta"].as_str() {
                        streamed.insert(p["itemId"].as_str().unwrap_or_default().to_string());
                        said_anything = true;
                        let _ = events.send(Event::Text(d.to_string()));
                    }
                }
                "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" => {
                    if let Some(d) = p["delta"].as_str() {
                        let _ = events.send(Event::Thinking(d.to_string()));
                    }
                }
                "item/started" => {
                    let item = &p["item"];
                    let id = item["id"].as_str().unwrap_or_default().to_string();
                    match item["type"].as_str() {
                        Some("agentMessage") if said_anything => {
                            let _ = events.send(Event::Text("\n\n".into()));
                        }
                        Some("commandExecution") => {
                            pending.insert(id, item["command"].as_str().unwrap_or_default().into());
                        }
                        Some("fileChange") => {
                            pending.insert(
                                id,
                                paths(item["changes"].as_array().map(Vec::as_slice).unwrap_or(&[])),
                            );
                        }
                        _ => {}
                    }
                }
                "item/completed" => {
                    let item = &p["item"];
                    let id = item["id"].as_str().unwrap_or_default();
                    pending.remove(id);
                    if item["type"] == "agentMessage" && !streamed.contains(id) {
                        if let Some(t) = item["text"].as_str().filter(|t| !t.is_empty()) {
                            said_anything = true;
                            let _ = events.send(Event::Text(t.to_string()));
                        }
                    } else if let Some(tool) = tool_of(item) {
                        end.tool_calls += 1;
                        let _ = events.send(Event::Tool(tool));
                    }
                }
                "thread/tokenUsage/updated" => {
                    self.totals = usage_of(&p["tokenUsage"]["total"]);
                }
                "error" => {
                    let _ = events.send(Event::Note(format!("codex: {}", error_text(&p["error"]))));
                }
                "warning" => {
                    if let Some(w) = p["message"].as_str() {
                        let _ = events.send(Event::Note(format!("codex: {w}")));
                    }
                }
                "turn/completed" => {
                    let turn = &p["turn"];
                    match turn["status"].as_str() {
                        Some("interrupted") => end.cancelled = true,
                        Some("failed") => {
                            end.error = Some(error_text(&turn["error"]));
                        }
                        _ => {}
                    }
                    end.usage = Usage {
                        input: self.totals.input.saturating_sub(before.input),
                        output: self.totals.output.saturating_sub(before.output),
                        cache_read: self.totals.cache_read.saturating_sub(before.cache_read),
                        cache_write: self.totals.cache_write.saturating_sub(before.cache_write),
                    };
                    end.cancelled |= cancel.is_cancelled();
                    return Ok(end);
                }
                _ => {}
            }
        }
    }
}
