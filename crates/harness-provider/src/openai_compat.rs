//! Any endpoint that speaks the OpenAI chat-completions wire format: Ollama
//! and vLLM/LM Studio on localhost, OpenRouter/Groq/Together and most new
//! vendors remotely. Model ids are whatever the endpoint serves — a model that
//! appeared after this file was written needs no code change, which is the
//! whole point.

use async_trait::async_trait;
use futures_util::StreamExt;
use harness_core::{
    AgentMessage, AssistantTurn, ContentBlock, ModelRequest, Provider, ProviderError, Sink,
    StopReason, Usage,
};
use serde_json::{Value, json};

/// A tool call assembled from streaming fragments: ids arrive once, names and
/// JSON arguments may be split across any number of deltas.
#[derive(Default)]
struct ToolAcc {
    id: Option<String>,
    name: String,
    args: String,
}

pub struct OpenAICompat {
    label: &'static str,
    base_url: String,
    api_key: String,
    model: String,
    window: usize,
    retry_base_ms: u64,
    stall: std::time::Duration,
    client: reqwest::Client,
}

impl OpenAICompat {
    pub fn new(
        label: &'static str,
        base_url: &str,
        api_key: String,
        model: String,
        window: usize,
    ) -> Self {
        Self {
            label,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            model,
            window,
            retry_base_ms: crate::retry::default_base_ms(),
            stall: crate::retry::stall_timeout(),
            client: reqwest::Client::builder()
                .build()
                .expect("reqwest client builds"),
        }
    }

    /// Retry cadence and mid-stream silence limit, injectable for tests.
    pub fn with_retry_base(mut self, ms: u64) -> Self {
        self.retry_base_ms = ms;
        self
    }

    pub fn with_stall_timeout(mut self, stall: std::time::Duration) -> Self {
        self.stall = stall;
        self
    }

    /// Ollama on localhost. Key is a placeholder — local servers ignore it —
    /// and models are whatever `ollama pull` has provided.
    pub fn ollama(model: String) -> Self {
        let base = std::env::var("HARNESS_OLLAMA_URL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "http://127.0.0.1:11434/v1".into());
        let window = window_from_env("HARNESS_OLLAMA_WINDOW").unwrap_or(128_000);
        Self::new("ollama", &base, "ollama".into(), model, window)
    }

    /// Any other OpenAI-compatible vendor: `HARNESS_COMPAT_URL` plus an
    /// optional key. `None` when the URL is unset.
    pub fn from_env(model: String) -> Option<Self> {
        let base = std::env::var("HARNESS_COMPAT_URL")
            .ok()
            .filter(|s| !s.is_empty())?;
        let key = std::env::var("HARNESS_COMPAT_KEY").unwrap_or_default();
        let window = window_from_env("HARNESS_COMPAT_WINDOW").unwrap_or(200_000);
        Some(Self::new("compat", &base, key, model, window))
    }

    fn url(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }
}

fn window_from_env(name: &str) -> Option<usize> {
    std::env::var(name).ok().and_then(|v| v.parse().ok())
}

/// Model ids the endpoint currently serves — the automatic half of model
/// extensibility: nothing is compiled in.
pub async fn list_models(base_url: &str, api_key: &str) -> Result<Vec<String>, String> {
    let client = reqwest::Client::new();
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let resp = client
        .get(&url)
        .header("authorization", format!("Bearer {api_key}"))
        .send()
        .await
        .map_err(|e| format!("{url}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("{url}: HTTP {}", resp.status()));
    }
    let v: Value = resp.json().await.map_err(|e| e.to_string())?;
    Ok(v["data"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|m| m["id"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default())
}

/// One request body for the endpoint. Mapping is public to the crate's tests:
/// the journal's message order (assistant tool_calls, then their results) is
/// exactly what this format requires.
fn request_body(model: &str, req: &ModelRequest, stream_options: bool) -> Value {
    let mut body = json!({
        "model": model,
        "messages": request_messages(req),
        "stream": true,
        "max_tokens": req.max_tokens,
    });
    if !req.tools.is_empty() {
        body["tools"] = Value::Array(
            req.tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.input_schema,
                        }
                    })
                })
                .collect(),
        );
    }
    if stream_options {
        // Ask for a final usage chunk; servers predating this field 400 on it
        // and get a retry without it below.
        body["stream_options"] = json!({"include_usage": true});
    }
    body
}

fn request_messages(req: &ModelRequest) -> Value {
    let mut out = vec![json!({"role": "system", "content": req.system})];
    for m in &req.messages {
        match m {
            AgentMessage::User { content, .. } => {
                out.push(json!({"role": "user", "content": content}));
            }
            AgentMessage::Assistant { content, .. } => {
                let text: String = content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let calls: Vec<Value> = content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::ToolCall {
                            id,
                            name,
                            arguments,
                            ..
                        } => Some(json!({
                            "id": id,
                            "type": "function",
                            "function": {"name": name, "arguments": arguments.to_string()},
                        })),
                        _ => None,
                    })
                    .collect();
                let mut msg = json!({"role": "assistant"});
                if !text.is_empty() {
                    msg["content"] = json!(text);
                }
                if !calls.is_empty() {
                    msg["tool_calls"] = Value::Array(calls);
                }
                out.push(msg);
            }
            AgentMessage::ToolResult {
                tool_call_id,
                content,
                ..
            } => out.push(json!({
                "role": "tool",
                "tool_call_id": tool_call_id,
                "content": content,
            })),
        }
    }
    Value::Array(out)
}

/// Apply one streaming delta to the accumulators, forwarding text/thinking to
/// the sink as it arrives.
fn apply_delta(
    delta: &Value,
    sink: &mut dyn Sink,
    text: &mut String,
    thinking: &mut String,
    tools: &mut Vec<ToolAcc>,
) {
    if let Some(t) = delta["reasoning_content"]
        .as_str()
        .or_else(|| delta["reasoning"].as_str())
    {
        thinking.push_str(t);
        sink.on_thinking(t);
    }
    if let Some(t) = delta["content"].as_str() {
        text.push_str(t);
        sink.on_delta(t);
    }
    for tc in delta["tool_calls"].as_array().into_iter().flatten() {
        let idx = tc["index"].as_u64().unwrap_or(0) as usize;
        while tools.len() <= idx {
            tools.push(ToolAcc::default());
        }
        let acc = &mut tools[idx];
        if let Some(id) = tc["id"].as_str().filter(|s| !s.is_empty()) {
            acc.id = Some(id.to_string());
        }
        if let Some(n) = tc["function"]["name"].as_str().filter(|s| !s.is_empty()) {
            acc.name.push_str(n);
        }
        if let Some(a) = tc["function"]["arguments"].as_str() {
            acc.args.push_str(a);
        }
    }
}

fn finish_reason_to_stop(reason: Option<&str>) -> StopReason {
    match reason {
        Some("tool_calls") => StopReason::ToolUse,
        Some("length") => StopReason::Length,
        _ => StopReason::Stop,
    }
}

fn parse_usage(v: &Value) -> Usage {
    Usage {
        input: v["prompt_tokens"].as_u64().unwrap_or(0),
        output: v["completion_tokens"].as_u64().unwrap_or(0),
        cache_read: v["prompt_tokens_details"]["cached_tokens"]
            .as_u64()
            .unwrap_or(0),
        cache_write: 0,
    }
}

#[async_trait]
impl Provider for OpenAICompat {
    fn name(&self) -> &'static str {
        self.label
    }

    fn context_window(&self) -> usize {
        self.window
    }

    async fn complete(
        &self,
        req: &ModelRequest,
        sink: &mut dyn Sink,
    ) -> Result<AssistantTurn, ProviderError> {
        let post = |body: Value| {
            self.client
                .post(self.url())
                .header("authorization", format!("Bearer {}", self.api_key))
                .json(&body)
        };
        let with_usage = request_body(&self.model, req, true);
        let resp = match crate::retry::send_retrying(self.retry_base_ms, || {
            post(with_usage.clone())
        })
        .await
        {
            Ok(r) => r,
            // Servers predating `stream_options` reject it with 400; the
            // request is otherwise identical, we just lose the usage chunk.
            Err(ProviderError::Api { status: 400, .. }) => {
                let plain = request_body(&self.model, req, false);
                crate::retry::send_retrying(self.retry_base_ms, || post(plain.clone())).await?
            }
            Err(e) => return Err(e),
        };

        let mut stream = resp.bytes_stream();
        let mut text = String::new();
        let mut thinking = String::new();
        let mut tools: Vec<ToolAcc> = Vec::new();
        let mut finish: Option<String> = None;
        let mut usage = Usage::default();
        let mut buf = String::new();
        loop {
            let chunk = match tokio::time::timeout(self.stall, stream.next()).await {
                Err(_) => {
                    return Err(ProviderError::Transport(format!(
                        "endpoint sent nothing for {}s — turn abandoned",
                        self.stall.as_secs()
                    )));
                }
                Ok(None) => break,
                Ok(Some(c)) => c.map_err(|e| ProviderError::Transport(e.to_string()))?,
            };
            buf.push_str(&String::from_utf8_lossy(&chunk));
            // SSE events are newline-delimited and a chunk boundary can split
            // one, so only whole lines are consumed here.
            while let Some(pos) = buf.find('\n') {
                let line: String = buf.drain(..=pos).collect();
                let line = line.trim();
                let Some(data) = line.strip_prefix("data:") else {
                    continue;
                };
                let data = data.trim();
                if data == "[DONE]" {
                    continue;
                }
                let Ok(ev) = serde_json::from_str::<Value>(data) else {
                    continue;
                };
                if let Some(u) = ev.get("usage").filter(|u| !u.is_null()) {
                    usage = parse_usage(u);
                }
                if let Some(choice) = ev["choices"].get(0) {
                    if let Some(f) = choice["finish_reason"].as_str() {
                        finish = Some(f.to_string());
                    }
                    apply_delta(&choice["delta"], sink, &mut text, &mut thinking, &mut tools);
                }
            }
        }

        let mut blocks = Vec::new();
        if !thinking.is_empty() {
            blocks.push(ContentBlock::Thinking { thinking });
        }
        if !text.is_empty() {
            blocks.push(ContentBlock::Text { text });
        }
        for (i, acc) in tools.into_iter().enumerate() {
            if acc.name.is_empty() {
                continue;
            }
            let arguments = serde_json::from_str(&acc.args).unwrap_or(Value::Null);
            blocks.push(ContentBlock::ToolCall {
                id: acc.id.unwrap_or_else(|| format!("call_{i}")),
                name: acc.name,
                arguments,
            });
        }
        // A turn with nothing at all is undisplayable in the journal; keep an
        // empty text block so downstream never sees a contentless assistant.
        if blocks.is_empty() {
            blocks.push(ContentBlock::Text {
                text: String::new(),
            });
        }
        Ok(AssistantTurn {
            blocks,
            usage,
            stop_reason: finish_reason_to_stop(finish.as_deref()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::{ContentBlock, ToolSpec};

    fn req(messages: Vec<AgentMessage>, tools: Vec<ToolSpec>) -> ModelRequest {
        ModelRequest {
            system: "sys".into(),
            tools,
            messages,
            model: "m".into(),
            max_tokens: 64,
        }
    }

    #[test]
    fn maps_the_journal_shape_to_chat_messages() {
        let messages = vec![
            AgentMessage::User {
                content: "hi".into(),
                ts: 0,
            },
            AgentMessage::Assistant {
                content: vec![
                    ContentBlock::Text {
                        text: "running".into(),
                    },
                    ContentBlock::ToolCall {
                        id: "c1".into(),
                        name: "bash".into(),
                        arguments: json!({"command": "ls"}),
                    },
                ],
                provider: "p".into(),
                model: "m".into(),
                usage: Usage::default(),
                stop_reason: StopReason::ToolUse,
                ts: 0,
            },
            AgentMessage::ToolResult {
                tool_call_id: "c1".into(),
                tool_name: "bash".into(),
                content: "out".into(),
                is_error: false,
                ts: 0,
            },
        ];
        let body = request_body("m", &req(messages, vec![]), false);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[1]["content"], "hi");
        assert_eq!(msgs[2]["tool_calls"][0]["id"], "c1");
        assert_eq!(
            msgs[2]["tool_calls"][0]["function"]["arguments"],
            r#"{"command":"ls"}"#
        );
        assert_eq!(msgs[3]["role"], "tool");
        assert_eq!(msgs[3]["tool_call_id"], "c1");
        assert!(body.get("tools").is_none(), "no tools, no tools field");
    }

    #[test]
    fn tool_schemas_carry_the_json_schema_verbatim() {
        let tools = vec![ToolSpec {
            name: "bash".into(),
            description: "run".into(),
            input_schema: json!({"type": "object", "properties": {"command": {"type": "string"}}}),
        }];
        let body = request_body("m", &req(vec![], tools), true);
        let f = &body["tools"][0]["function"];
        assert_eq!(f["name"], "bash");
        assert_eq!(f["parameters"]["type"], "object");
        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    #[test]
    fn fragmented_tool_deltas_assemble_into_one_call() {
        struct Rec(String);
        impl Sink for Rec {
            fn on_delta(&mut self, t: &str) {
                self.0.push_str(t);
            }
        }
        let mut sink = Rec(String::new());
        let mut text = String::new();
        let mut thinking = String::new();
        let mut tools = Vec::new();
        apply_delta(
            &json!({"reasoning_content": "hm", "content": "He"}),
            &mut sink,
            &mut text,
            &mut thinking,
            &mut tools,
        );
        apply_delta(
            &json!({"tool_calls": [{"index": 0, "id": "x1", "function": {"name": "ba"}}]}),
            &mut sink,
            &mut text,
            &mut thinking,
            &mut tools,
        );
        apply_delta(
            &json!({"tool_calls": [{"index": 0, "function": {"name": "sh", "arguments": "{\"a\":"}}]}),
            &mut sink,
            &mut text,
            &mut thinking,
            &mut tools,
        );
        apply_delta(
            &json!({"tool_calls": [{"index": 0, "function": {"arguments": "1}"}}]}),
            &mut sink,
            &mut text,
            &mut thinking,
            &mut tools,
        );
        assert_eq!(sink.0, "He");
        assert_eq!(thinking, "hm");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].id.as_deref(), Some("x1"));
        assert_eq!(tools[0].name, "bash");
        assert_eq!(tools[0].args, "{\"a\":1}");
    }

    #[test]
    fn finish_reasons_map_to_stop_kinds() {
        assert_eq!(
            finish_reason_to_stop(Some("tool_calls")),
            StopReason::ToolUse
        );
        assert_eq!(finish_reason_to_stop(Some("length")), StopReason::Length);
        assert_eq!(finish_reason_to_stop(None), StopReason::Stop);
        assert_eq!(finish_reason_to_stop(Some("stop")), StopReason::Stop);
    }
}
