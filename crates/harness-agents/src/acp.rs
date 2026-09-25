//! Any agent that speaks the Agent Client Protocol (Gemini CLI, OpenCode,
//! and a growing list): JSON-RPC 2.0 over stdio. The harness is the client —
//! the role an editor plays — and declares no filesystem or terminal
//! capabilities, so the agent uses its own tools and asks for permission
//! with `session/request_permission`.

use std::collections::HashMap;
use std::time::Instant;

use anyhow::{Result, bail};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::proc::{Proc, same_id};
use crate::{
    Agent, Asker, Cancel, Decision, Event, OUTPUT_MAX, Opts, Spec, ToolCall, TurnEnd, Usage, clip,
};

pub struct Acp {
    name: String,
    proc: Proc,
    next_id: u64,
    session: String,
    model: Option<String>,
    ask: Asker,
    yolo: bool,
}

impl Acp {
    pub async fn start(spec: &Spec, opts: Opts) -> Result<Acp> {
        let proc = Proc::spawn(&spec.name, &spec.command, &spec.args, &opts)?;
        let mut a = Acp {
            name: spec.name.clone(),
            proc,
            next_id: 0,
            session: String::new(),
            model: opts.model.clone(),
            ask: opts.ask.clone(),
            yolo: opts.yolo,
        };
        let init = a
            .request(
                "initialize",
                json!({"protocolVersion": 1, "clientCapabilities": {
                    "fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false}}),
            )
            .await?;
        let cwd = opts.cwd.display().to_string();
        let can_load = init["agentCapabilities"]["loadSession"]
            .as_bool()
            .unwrap_or(false);
        let loaded = match (&opts.resume, can_load) {
            (Some(id), true) => a
                .request(
                    "session/load",
                    json!({"sessionId": id, "cwd": cwd, "mcpServers": []}),
                )
                .await
                .ok()
                .map(|_| id.clone()),
            _ => None,
        };
        a.session = match loaded {
            Some(id) => id,
            None => {
                let r = a
                    .request("session/new", json!({"cwd": cwd, "mcpServers": []}))
                    .await
                    .map_err(|e| {
                        anyhow::anyhow!(
                            "{e}\n(log in with the agent's own CLI first: {})",
                            spec.login
                        )
                    })?;
                r["sessionId"].as_str().unwrap_or_default().to_string()
            }
        };
        if a.session.is_empty() {
            bail!("{} started no session", a.name);
        }
        if let Some(m) = &opts.model {
            // Optional in the protocol; agents that lack it keep their default.
            let _ = a
                .request(
                    "session/set_model",
                    json!({"sessionId": a.session, "modelId": m}),
                )
                .await;
        }
        Ok(a)
    }

    /// A request and its response. Updates arriving meanwhile (history
    /// replayed by `session/load`, command lists) are not part of a turn.
    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        self.proc
            .send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;
        loop {
            let m = self.proc.next().await?;
            if m.get("method").is_none() && same_id(&m["id"], id) {
                if let Some(e) = m.get("error") {
                    bail!(
                        "{} {method}: {}",
                        self.name,
                        e["message"].as_str().unwrap_or("error")
                    );
                }
                return Ok(m["result"].clone());
            }
            if m.get("method").is_some() && m.get("id").is_some() {
                self.answer(&m).await?;
            }
        }
    }

    /// Requests from the agent: permissions are asked; the rest (fs,
    /// terminal) were not offered, so they are refused.
    async fn answer(&mut self, m: &Value) -> Result<()> {
        if m["method"] != "session/request_permission" {
            return self
                .proc
                .send(&json!({"jsonrpc": "2.0", "id": m["id"], "error": {
                    "code": -32601, "message": "not supported by harness"}}))
                .await;
        }
        let p = &m["params"];
        let options = p["options"].as_array().cloned().unwrap_or_default();
        let pick = |kinds: &[&str]| {
            kinds.iter().find_map(|k| {
                options
                    .iter()
                    .find(|o| o["kind"] == *k)
                    .and_then(|o| o["optionId"].as_str().map(String::from))
            })
        };
        let decision = if self.yolo {
            Decision::Once
        } else {
            let title = p["toolCall"]["title"].as_str().unwrap_or("a tool call");
            crate::ask(
                &self.ask,
                format!(
                    "{} wants to run: {title} [y]es / [a]lways / [n]o",
                    self.name
                ),
            )
            .await
        };
        let chosen = match decision {
            Decision::Once => pick(&["allow_once", "allow_always"]),
            Decision::Always => pick(&["allow_always", "allow_once"]),
            Decision::Deny => pick(&["reject_once", "reject_always"]),
        };
        let outcome = match chosen {
            Some(id) => json!({"outcome": "selected", "optionId": id}),
            None => json!({"outcome": "cancelled"}),
        };
        self.proc
            .send(&json!({"jsonrpc": "2.0", "id": m["id"], "result": {"outcome": outcome}}))
            .await
    }
}

/// Text of a tool call's content: text blocks, and diffs rendered unified.
fn tool_output(content: &Value) -> String {
    let mut out = Vec::new();
    for c in content.as_array().into_iter().flatten() {
        match c["type"].as_str() {
            Some("content") => {
                if let Some(t) = c["content"]["text"].as_str() {
                    out.push(t.to_string());
                }
            }
            Some("diff") => out.push(harness_diff::unified(
                c["oldText"].as_str().unwrap_or_default(),
                c["newText"].as_str().unwrap_or_default(),
                c["path"].as_str().unwrap_or("file"),
                80,
            )),
            _ => {}
        }
    }
    out.join("\n")
}

struct Pending {
    kind: String,
    title: String,
    output: String,
    started: Instant,
}

#[async_trait::async_trait]
impl Agent for Acp {
    fn label(&self) -> String {
        match &self.model {
            Some(m) => format!("{} · {m}", self.name),
            None => self.name.clone(),
        }
    }

    fn session_id(&self) -> Option<String> {
        Some(self.session.clone())
    }

    async fn prompt(
        &mut self,
        text: &str,
        events: &mpsc::UnboundedSender<Event>,
        cancel: &Cancel,
    ) -> Result<TurnEnd> {
        self.next_id += 1;
        let id = self.next_id;
        self.proc
            .send(
                &json!({"jsonrpc": "2.0", "id": id, "method": "session/prompt", "params": {
                "sessionId": self.session, "prompt": [{"type": "text", "text": text}]}}),
            )
            .await?;
        let mut end = TurnEnd::default();
        let mut interrupted = false;
        let mut calls: HashMap<String, Pending> = HashMap::new();
        loop {
            let next = tokio::select! {
                m = self.proc.next() => Some(m?),
                _ = cancel.cancelled(), if !interrupted => None,
            };
            let Some(m) = next else {
                interrupted = true;
                self.proc
                    .send(&json!({"jsonrpc": "2.0", "method": "session/cancel",
                        "params": {"sessionId": self.session}}))
                    .await?;
                continue;
            };
            if m.get("method").is_none() {
                if same_id(&m["id"], id) {
                    if let Some(e) = m.get("error") {
                        end.error = Some(e["message"].as_str().unwrap_or("error").to_string());
                    }
                    let r = &m["result"];
                    end.cancelled = interrupted || r["stopReason"] == "cancelled";
                    let u = &r["usage"];
                    end.usage = Usage {
                        input: u["inputTokens"].as_u64().unwrap_or(0),
                        output: u["outputTokens"].as_u64().unwrap_or(0),
                        cache_read: u["cachedReadTokens"].as_u64().unwrap_or(0),
                        cache_write: u["cachedWriteTokens"].as_u64().unwrap_or(0),
                    };
                    return Ok(end);
                }
                continue;
            }
            if m.get("id").is_some() {
                self.answer(&m).await?;
                continue;
            }
            if m["method"] != "session/update" {
                continue;
            }
            let u = &m["params"]["update"];
            match u["sessionUpdate"].as_str().unwrap_or_default() {
                "agent_message_chunk" => {
                    if let Some(t) = u["content"]["text"].as_str() {
                        let _ = events.send(Event::Text(t.to_string()));
                    }
                }
                "agent_thought_chunk" => {
                    if let Some(t) = u["content"]["text"].as_str() {
                        let _ = events.send(Event::Thinking(t.to_string()));
                    }
                }
                "tool_call" | "tool_call_update" => {
                    let tid = u["toolCallId"].as_str().unwrap_or_default().to_string();
                    let entry = calls.entry(tid.clone()).or_insert_with(|| Pending {
                        kind: String::new(),
                        title: String::new(),
                        output: String::new(),
                        started: Instant::now(),
                    });
                    if let Some(k) = u["kind"].as_str() {
                        entry.kind = k.to_string();
                    }
                    if let Some(t) = u["title"].as_str() {
                        entry.title = t.to_string();
                    }
                    if u.get("content").is_some_and(|c| !c.is_null()) {
                        entry.output = tool_output(&u["content"]);
                    }
                    if let Some(status @ ("completed" | "failed")) = u["status"].as_str()
                        && let Some(done) = calls.remove(&tid)
                    {
                        end.tool_calls += 1;
                        let _ = events.send(Event::Tool(ToolCall {
                            name: if done.kind.is_empty() {
                                "tool".into()
                            } else {
                                done.kind
                            },
                            subject: done.title,
                            output: clip(&done.output, OUTPUT_MAX),
                            is_error: status == "failed",
                            ms: done.started.elapsed().as_millis() as u64,
                        }));
                    }
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_output_renders_text_and_diffs() {
        let out = tool_output(&json!([
            {"type": "content", "content": {"type": "text", "text": "ran"}},
            {"type": "diff", "path": "a.txt", "oldText": "x\n", "newText": "y\n"},
        ]));
        assert!(out.starts_with("ran\n--- a.txt"), "{out}");
        assert!(out.contains("-x\n+y"), "{out}");
    }
}
