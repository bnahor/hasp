//! Claude Code through `claude -p` with stream-json in and out — the
//! transport the Claude Agent SDK uses. One long-lived process per session;
//! each prompt is a user message on stdin. With `--permission-prompt-tool
//! stdio`, Claude asks for tool permission over the same pipe
//! (`control_request` / `can_use_tool`), so approvals land in the harness.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use anyhow::Result;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::proc::Proc;
use crate::{
    Agent, Asker, Cancel, Decision, Event, OUTPUT_MAX, Opts, Spec, ToolCall, TurnEnd, Usage, clip,
};

pub struct Claude {
    proc: Proc,
    session: Option<String>,
    model: String,
    ask: Asker,
    /// Tools (and, for Bash, commands) the user said "always" to.
    always: HashSet<String>,
    requests: u64,
}

impl Claude {
    pub async fn start(spec: &Spec, opts: Opts) -> Result<Claude> {
        let mut args = spec.args.clone();
        args.extend(
            [
                "-p",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
                "--include-partial-messages",
                "--permission-prompt-tool",
                "stdio",
            ]
            .map(String::from),
        );
        if opts.yolo {
            args.extend(["--permission-mode", "bypassPermissions"].map(String::from));
        }
        if let Some(m) = &opts.model {
            args.extend(["--model".to_string(), m.clone()]);
        }
        if let Some(id) = &opts.resume {
            args.extend(["--resume".to_string(), id.clone()]);
        }
        let proc = Proc::spawn("claude", &spec.command, &args, &opts)?;
        let mut c = Claude {
            proc,
            session: opts.resume.clone(),
            model: opts.model.clone().unwrap_or_else(|| "default".into()),
            ask: opts.ask.clone(),
            always: HashSet::new(),
            requests: 0,
        };
        c.control(json!({"subtype": "initialize"})).await?;
        Ok(c)
    }

    async fn control(&mut self, request: Value) -> Result<()> {
        self.requests += 1;
        self.proc
            .send(&json!({"type": "control_request",
                "request_id": format!("harness-{}", self.requests), "request": request}))
            .await
    }

    async fn answer_permission(&mut self, m: &Value) -> Result<()> {
        let r = &m["request"];
        let tool = r["tool_name"].as_str().unwrap_or("a tool").to_string();
        let input = r["input"].clone();
        let subject = subject_of(&input);
        let key = match tool.as_str() {
            "Bash" => format!("Bash:{}", subject.split_whitespace().next().unwrap_or("")),
            _ => tool.clone(),
        };
        let decision = if self.always.contains(&key) {
            Decision::Once
        } else {
            crate::ask(
                &self.ask,
                format!("claude wants to use {tool}: {subject} [y]es / [a]lways / [n]o"),
            )
            .await
        };
        if decision == Decision::Always {
            self.always.insert(key);
        }
        let response = match decision {
            Decision::Deny => {
                json!({"behavior": "deny", "message": "the user declined this in harness"})
            }
            _ => json!({"behavior": "allow", "updatedInput": input}),
        };
        self.proc
            .send(&json!({"type": "control_response", "response": {
                "subtype": "success", "request_id": m["request_id"], "response": response}}))
            .await
    }
}

/// What a tool call is about, from its input.
fn subject_of(input: &Value) -> String {
    for k in [
        "command",
        "file_path",
        "path",
        "pattern",
        "url",
        "query",
        "description",
        "prompt",
    ] {
        if let Some(s) = input[k].as_str() {
            return clip(s, 200);
        }
    }
    clip(&input.to_string(), 200)
}

/// Tool result content: a string, or text blocks.
fn content_text(c: &Value) -> String {
    match c {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

#[async_trait::async_trait]
impl Agent for Claude {
    fn label(&self) -> String {
        format!("claude · {}", self.model)
    }

    fn session_id(&self) -> Option<String> {
        self.session.clone()
    }

    async fn prompt(
        &mut self,
        text: &str,
        events: &mpsc::UnboundedSender<Event>,
        cancel: &Cancel,
    ) -> Result<TurnEnd> {
        self.proc
            .send(&json!({"type": "user", "message": {"role": "user", "content": text}}))
            .await?;
        let mut end = TurnEnd::default();
        let mut interrupted = false;
        let mut said_anything = false;
        // tool_use id → (name, subject, started)
        let mut calls: HashMap<String, (String, String, Instant)> = HashMap::new();
        loop {
            let next = tokio::select! {
                m = self.proc.next() => Some(m?),
                _ = cancel.cancelled(), if !interrupted => None,
            };
            let Some(m) = next else {
                interrupted = true;
                self.control(json!({"subtype": "interrupt"})).await?;
                continue;
            };
            match m["type"].as_str().unwrap_or_default() {
                "system" if m["subtype"] == "init" => {
                    if let Some(id) = m["session_id"].as_str() {
                        self.session = Some(id.to_string());
                    }
                    if let Some(model) = m["model"].as_str() {
                        self.model = model.to_string();
                    }
                }
                "stream_event" => {
                    let e = &m["event"];
                    match e["type"].as_str().unwrap_or_default() {
                        "message_start" if said_anything => {
                            let _ = events.send(Event::Text("\n\n".into()));
                            said_anything = false;
                        }
                        "content_block_delta" => {
                            let d = &e["delta"];
                            match d["type"].as_str().unwrap_or_default() {
                                "text_delta" => {
                                    if let Some(t) = d["text"].as_str() {
                                        said_anything = true;
                                        let _ = events.send(Event::Text(t.to_string()));
                                    }
                                }
                                "thinking_delta" => {
                                    if let Some(t) = d["thinking"].as_str() {
                                        let _ = events.send(Event::Thinking(t.to_string()));
                                    }
                                }
                                _ => {}
                            }
                        }
                        _ => {}
                    }
                }
                "assistant" => {
                    // Text arrived as deltas; tool calls are taken from here,
                    // where their input is complete.
                    for block in m["message"]["content"].as_array().into_iter().flatten() {
                        if block["type"] == "tool_use" {
                            calls.insert(
                                block["id"].as_str().unwrap_or_default().to_string(),
                                (
                                    block["name"].as_str().unwrap_or("tool").to_string(),
                                    subject_of(&block["input"]),
                                    Instant::now(),
                                ),
                            );
                        }
                    }
                }
                "user" => {
                    for block in m["message"]["content"].as_array().into_iter().flatten() {
                        if block["type"] != "tool_result" {
                            continue;
                        }
                        let id = block["tool_use_id"].as_str().unwrap_or_default();
                        let (name, subject, started) = calls
                            .remove(id)
                            .unwrap_or_else(|| ("tool".into(), String::new(), Instant::now()));
                        end.tool_calls += 1;
                        let _ = events.send(Event::Tool(ToolCall {
                            name,
                            subject,
                            output: clip(&content_text(&block["content"]), OUTPUT_MAX),
                            is_error: block["is_error"].as_bool().unwrap_or(false),
                            ms: started.elapsed().as_millis() as u64,
                        }));
                    }
                }
                "control_request" if m["request"]["subtype"] == "can_use_tool" => {
                    self.answer_permission(&m).await?;
                }
                "control_request" => {
                    // Hooks and other callbacks this client never registered.
                    self.proc
                        .send(&json!({"type": "control_response", "response": {
                            "subtype": "error", "request_id": m["request_id"],
                            "error": "not supported by harness"}}))
                        .await?;
                }
                "result" => {
                    if let Some(id) = m["session_id"].as_str() {
                        self.session = Some(id.to_string());
                    }
                    let u = &m["usage"];
                    end.usage = Usage {
                        input: u["input_tokens"].as_u64().unwrap_or(0),
                        output: u["output_tokens"].as_u64().unwrap_or(0),
                        cache_read: u["cache_read_input_tokens"].as_u64().unwrap_or(0),
                        cache_write: u["cache_creation_input_tokens"].as_u64().unwrap_or(0),
                    };
                    end.cancelled = interrupted;
                    if m["is_error"].as_bool().unwrap_or(false) && !interrupted {
                        end.error =
                            Some(m["result"].as_str().map(String::from).unwrap_or_else(|| {
                                m["subtype"].as_str().unwrap_or("error").to_string()
                            }));
                    }
                    return Ok(end);
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
    fn subjects_come_from_the_obvious_field() {
        assert_eq!(
            subject_of(&json!({"command": "ls", "description": "x"})),
            "ls"
        );
        assert_eq!(subject_of(&json!({"file_path": "a.rs"})), "a.rs");
        assert_eq!(subject_of(&json!({"n": 1})), "{\"n\":1}");
        assert_eq!(
            content_text(&json!([{"type": "text", "text": "a"}, {"type": "text", "text": "b"}])),
            "a\nb"
        );
    }
}
