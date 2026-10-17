//! Anthropic Messages API over rustls with SSE streaming, explicit
//! `cache_control` breakpoints (last tool, system, and the conversation tail),
//! signed-thinking replay, and cache read/write usage capture
//! (research/01-source-notes.md §1.1, §5.4).

use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use hasp_core::{
    AgentMessage, AssistantTurn, ContentBlock, ModelRequest, Provider, ProviderError, Sink,
    StopReason, Usage,
};
use serde_json::{Value, json};

const ANTHROPIC_VERSION: &str = "2023-06-01";
const CONTEXT_WINDOW: usize = 200_000;

pub struct Anthropic {
    api_key: String,
    pub model: String,
    base_url: String,
    /// Per-model context window (catalog-driven); the old hardcoded constant
    /// under-reported DeepSeek's 1M by 5x, which compacted far too eagerly.
    window: usize,
    /// Which service this client actually talks to — transcripts, traces and
    /// the model-change journal record it, so it must not claim "anthropic"
    /// for a DeepSeek or z.ai endpoint.
    label: &'static str,
    /// Send the key as `Authorization: Bearer` instead of `x-api-key`.
    bearer: bool,
    /// Image support, when the endpoint's table entry says so.
    vision: Option<bool>,
    /// Retry cadence and mid-stream silence limit; injectable so tests need no
    /// environment mutation (which is unsafe in this edition).
    retry_base_ms: u64,
    stall: Duration,
    /// First-party reasoning controls on (thinking + effort, per model).
    reasoning: bool,
    /// Requested effort; clamped per model at request time.
    effort_pref: Option<String>,
    client: reqwest::Client,
}

impl Anthropic {
    pub fn new(api_key: String, model: String) -> Self {
        Self {
            api_key,
            model,
            base_url: "https://api.anthropic.com".into(),
            window: CONTEXT_WINDOW,
            label: "anthropic",
            bearer: false,
            vision: None,
            retry_base_ms: crate::retry::default_base_ms(),
            stall: crate::retry::stall_timeout(),
            reasoning: false,
            effort_pref: None,
            // No total timeout: a long, healthy stream (a 64k-token answer at
            // 50 tok/s is 20 minutes) must not be cut off mid-flight. Silence
            // is what kills a turn — the per-read timeout before headers and
            // the stall guard while streaming.
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(30))
                .read_timeout(Duration::from_secs(300))
                .build()
                .expect("reqwest client builds"),
        }
    }

    /// Anthropic-compatible endpoints (z.ai GLM, gateways, …).
    pub fn with_base_url(mut self, base_url: &str) -> Self {
        self.base_url = base_url.to_string();
        self
    }

    /// Retry cadence for transient failures, and how long a silent stream may
    /// stay silent.
    pub fn with_retry_base(mut self, ms: u64) -> Self {
        self.retry_base_ms = ms;
        self
    }

    pub fn with_stall_timeout(mut self, stall: Duration) -> Self {
        self.stall = stall;
        self
    }

    /// Declare this model's context window (from the catalog).
    pub fn with_window(mut self, window: usize) -> Self {
        self.window = window;
        self
    }

    /// Reasoning configuration for first-party Claude models: adaptive thinking
    /// (summarized, so the UI has something to show) and an effort level where
    /// the model takes one. `HASP_THINKING=off` omits the thinking field
    /// (models that always think keep thinking — that is the API's choice).
    pub fn with_reasoning(mut self, effort: Option<&str>) -> Self {
        self.reasoning = true;
        self.effort_pref = effort.map(str::to_string);
        self
    }

    /// `(thinking, effort)` for the model a request names. Per request, not
    /// per client: `/model` switches mid-session, and adaptive thinking sent
    /// to a model that does not take it is a 400.
    fn reasoning_for(
        &self,
        model: &str,
        requested: Option<&str>,
    ) -> (Option<Value>, Option<String>) {
        let pref = requested.or(self.effort_pref.as_deref());
        if !self.reasoning {
            // Other endpoints: only an effort their table entry asked for (or
            // the user did, with HASP_EFFORT). Kimi K3 defaults to its
            // maximum when none is sent — about twice the output tokens of
            // `high` — so leaving it out is not neutral.
            return (None, pref.map(str::to_string));
        }
        let caps = ClaudeCaps::of(model);
        let thinking_off = hasp_workspace::env_var("HASP_THINKING").is_ok_and(|v| v == "off");
        let thinking = (caps.adaptive && !thinking_off)
            .then(|| json!({"type": "adaptive", "display": "summarized"}));
        let effort = match pref {
            Some(e) if caps.effort => Some(caps.clamp_effort(e).to_string()),
            // xhigh is the recommended level for coding and agentic work.
            None if caps.xhigh => Some("xhigh".into()),
            None if caps.effort => Some("high".into()),
            _ => None,
        };
        (thinking, effort)
    }

    /// Whether `model` on this endpoint takes images: every Claude model does;
    /// DeepSeek's models are text-only; on z.ai only the `…v` vision models.
    fn vision_for(&self, model: &str) -> bool {
        if let Some(v) = self.vision {
            return v;
        }
        match self.label {
            "deepseek" => false,
            "zai" => model.to_ascii_lowercase().ends_with('v'),
            _ => true,
        }
    }

    pub fn with_bearer(mut self, bearer: bool) -> Self {
        self.bearer = bearer;
        self
    }

    /// Effort sent as `output_config.effort` on endpoints without Claude's
    /// reasoning controls.
    pub fn with_effort(mut self, effort: Option<String>) -> Self {
        self.effort_pref = effort;
        self
    }

    pub fn with_vision(mut self, vision: bool) -> Self {
        self.vision = Some(vision);
        self
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Name the service behind an Anthropic-compatible endpoint.
    pub fn with_name(mut self, label: &'static str) -> Self {
        self.label = label;
        self
    }
}

/// What a first-party Claude model id accepts. Parsed from the id so new
/// point releases work without a code change; unknown shapes get nothing
/// extra, which is always a valid request.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct ClaudeCaps {
    /// `thinking: {type: "adaptive"}` (4.6 and later, every Fable/Mythos).
    pub adaptive: bool,
    /// `output_config.effort`.
    pub effort: bool,
    /// The `xhigh` effort level (4.7 and later).
    pub xhigh: bool,
}

impl ClaudeCaps {
    pub fn of(model: &str) -> Self {
        let Some(rest) = model.strip_prefix("claude-") else {
            return Self::default();
        };
        let mut parts = rest.split('-');
        let family = parts.next().unwrap_or("");
        let nums: Vec<u32> = parts.map_while(|p| p.parse().ok()).collect();
        let version = (
            nums.first().copied().unwrap_or(0),
            nums.get(1).copied().unwrap_or(0),
        );
        match family {
            "fable" | "mythos" => Self {
                adaptive: true,
                effort: true,
                xhigh: true,
            },
            "opus" | "sonnet" if version >= (4, 6) => Self {
                adaptive: true,
                effort: true,
                xhigh: version >= (4, 7),
            },
            "opus" if version >= (4, 5) => Self {
                adaptive: false,
                effort: true,
                xhigh: false,
            },
            _ => Self::default(),
        }
    }

    fn clamp_effort(self, e: &str) -> &str {
        match e {
            "xhigh" if !self.xhigh => "high",
            "low" | "medium" | "high" | "xhigh" | "max" => e,
            _ => "high",
        }
    }
}

fn image_block(img: &hasp_core::Image) -> Value {
    json!({
        "type": "image",
        "source": {"type": "base64", "media_type": img.media_type, "data": img.data},
    })
}

/// Text followed by images (or, for a model without vision, a placeholder
/// saying they were left out). An empty text block is dropped — the API
/// rejects those.
fn content_blocks(text: &str, images: &[hasp_core::Image], vision: bool) -> Vec<Value> {
    let mut blocks = Vec::new();
    let text = if !images.is_empty() && !vision {
        format!("{text}\n{}", crate::omitted(images.len()))
    } else {
        text.to_string()
    };
    if !text.is_empty() || images.is_empty() {
        blocks.push(json!({"type": "text", "text": text}));
    }
    if vision {
        blocks.extend(images.iter().map(image_block));
    }
    blocks
}

fn tool_result_block(m: &AgentMessage, vision: bool) -> Value {
    let AgentMessage::ToolResult {
        tool_call_id,
        content,
        is_error,
        images,
        ..
    } = m
    else {
        unreachable!("tool_result_block is only called for ToolResult")
    };
    json!({
        "type": "tool_result",
        "tool_use_id": tool_call_id,
        "content": content_blocks(content, images, vision),
        "is_error": is_error,
    })
}

/// The API requires every `tool_result` answering an assistant turn to arrive
/// in the single message that follows it, so consecutive results are merged
/// into one user message. The transcript keeps them as separate entries.
///
/// Thinking is replayed exactly as it came back — signature and all — when it
/// was produced by the model being called. Another model cannot verify the
/// signature, so for it the block is left out (what every provider expects
/// after a model switch).
#[cfg(test)]
fn map_messages(messages: &[AgentMessage], model: &str) -> Vec<Value> {
    map_messages_with(messages, model, true)
}

fn map_messages_with(messages: &[AgentMessage], model: &str, vision: bool) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut open_results: Option<usize> = None;
    for m in messages {
        match m {
            AgentMessage::ToolResult { .. } => match open_results {
                Some(i) => out[i]["content"]
                    .as_array_mut()
                    .expect("tool_result message holds an array")
                    .push(tool_result_block(m, vision)),
                None => {
                    out.push(json!({"role": "user", "content": [tool_result_block(m, vision)]}));
                    open_results = Some(out.len() - 1);
                }
            },
            AgentMessage::Assistant {
                content,
                model: produced_by,
                ..
            } => {
                let replay_thinking = produced_by == model;
                let blocks: Vec<Value> = content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(json!({"type": "text", "text": text})),
                        ContentBlock::ToolCall {
                            id,
                            name,
                            arguments,
                        } => Some(
                            json!({"type": "tool_use", "id": id, "name": name, "input": arguments}),
                        ),
                        ContentBlock::Thinking {
                            redacted: Some(data),
                            ..
                        } if replay_thinking => {
                            Some(json!({"type": "redacted_thinking", "data": data}))
                        }
                        ContentBlock::Thinking {
                            thinking,
                            signature: Some(sig),
                            ..
                        } if replay_thinking => Some(
                            json!({"type": "thinking", "thinking": thinking, "signature": sig}),
                        ),
                        // Unsigned, or from another model: not replayable.
                        ContentBlock::Thinking { .. } => None,
                    })
                    .collect();
                open_results = None;
                // A turn that was nothing but thinking would serialize as an
                // empty content array, which the API rejects outright.
                if !blocks.is_empty() {
                    out.push(json!({"role": "assistant", "content": blocks}));
                }
            }
            AgentMessage::User {
                content, images, ..
            } => {
                open_results = None;
                out.push(
                    json!({"role": "user", "content": content_blocks(content, images, vision)}),
                );
            }
        }
    }
    out
}

/// Mark the last content block of `message` as a cache breakpoint.
fn mark_cache_point(message: &mut Value) {
    if let Some(last) = message["content"]
        .as_array_mut()
        .and_then(|blocks| blocks.last_mut())
        // The API rejects a breakpoint on an empty text block.
        && last["text"].as_str() != Some("")
    {
        last["cache_control"] = json!({"type": "ephemeral"});
    }
}

#[cfg(test)]
pub(crate) fn build_body(req: &ModelRequest) -> Value {
    build_body_with(req, None, None, true)
}

/// Four breakpoints, the API maximum: tools, system, and two on the
/// conversation. The last message caches the whole history for the next
/// request; the one before it (`len - 3`, where the previous request put its
/// own tail breakpoint in a tool loop) guarantees an exact read point even
/// when a turn appended more blocks than the server's 20-block lookback. Without
/// these the history — nearly all of a long agent session — was re-billed at
/// full input price on every sample.
pub(crate) fn build_body_with(
    req: &ModelRequest,
    thinking: Option<&Value>,
    effort: Option<&str>,
    vision: bool,
) -> Value {
    let mut tools: Vec<Value> = req
        .tools
        .iter()
        .map(|t| json!({"name": t.name, "description": t.description, "input_schema": t.input_schema}))
        .collect();
    if let Some(last) = tools.last_mut() {
        last["cache_control"] = json!({"type": "ephemeral"});
    }
    let mut messages = map_messages_with(&req.messages, &req.model, vision);
    let n = messages.len();
    if n >= 1 {
        mark_cache_point(&mut messages[n - 1]);
    }
    if n >= 3 {
        mark_cache_point(&mut messages[n - 3]);
    }
    let mut body = json!({
        "model": req.model,
        "max_tokens": req.max_tokens,
        "stream": true,
        "system": [{"type": "text", "text": req.system, "cache_control": {"type": "ephemeral"}}],
        "tools": tools,
        "messages": messages,
    });
    if let Some(t) = thinking {
        body["thinking"] = t.clone();
    }
    if let Some(e) = effort {
        body["output_config"] = json!({"effort": e});
    }
    body
}

#[derive(Clone)]
enum BlockAcc {
    Text {
        text: String,
    },
    Tool {
        id: String,
        name: String,
        json_in: String,
    },
    Thinking {
        text: String,
        signature: String,
    },
    Redacted {
        data: String,
    },
    Other,
}

#[derive(Default)]
pub(crate) struct SseParser {
    buf: String,
}

impl SseParser {
    /// Feed raw bytes; returns complete `data:` payloads.
    pub(crate) fn feed(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.push_str(&String::from_utf8_lossy(chunk));
        let mut out = Vec::new();
        while let Some(pos) = self.buf.find("\n\n") {
            let block: String = self.buf.drain(..pos + 2).collect();
            let mut data = String::new();
            for line in block.lines() {
                if let Some(rest) = line.strip_prefix("data:") {
                    if !data.is_empty() {
                        data.push('\n');
                    }
                    data.push_str(rest.trim_start());
                }
            }
            if !data.is_empty() {
                out.push(data);
            }
        }
        out
    }
}

#[async_trait]
impl Provider for Anthropic {
    fn name(&self) -> &'static str {
        self.label
    }

    fn context_window(&self) -> usize {
        self.window
    }

    fn effort_levels(&self, model: &str) -> Vec<&'static str> {
        if !self.reasoning {
            // Anthropic-compatible endpoints take the field as-is.
            return vec!["low", "medium", "high", "max"];
        }
        let caps = ClaudeCaps::of(model);
        match (caps.effort, caps.xhigh) {
            (false, _) => Vec::new(),
            (true, false) => vec!["low", "medium", "high", "max"],
            (true, true) => vec!["low", "medium", "high", "xhigh", "max"],
        }
    }

    fn default_effort(&self, model: &str) -> Option<String> {
        self.reasoning_for(model, None).1
    }

    async fn complete(
        &self,
        req: &ModelRequest,
        sink: &mut dyn Sink,
    ) -> Result<AssistantTurn, ProviderError> {
        let (thinking, effort) = self.reasoning_for(&req.model, req.effort.as_deref());
        let body = build_body_with(
            req,
            thinking.as_ref(),
            effort.as_deref(),
            self.vision_for(&req.model),
        );
        // Retried only here, before anything has reached the sink: a turn that
        // already streamed text cannot be un-streamed.
        let resp = crate::retry::send_retrying(self.retry_base_ms, || {
            let req = self.client.post(format!("{}/v1/messages", self.base_url));
            let req = if self.bearer {
                req.bearer_auth(&self.api_key)
            } else {
                req.header("x-api-key", &self.api_key)
            };
            req.header("anthropic-version", ANTHROPIC_VERSION)
                .json(&body)
        })
        .await?;

        let mut stream = resp.bytes_stream();
        let mut sse = SseParser::default();
        let mut usage = Usage::default();
        let mut stop_reason = StopReason::Stop;
        let mut blocks: Vec<BlockAcc> = Vec::new();
        let mut done = false;

        while !done {
            let chunk = match tokio::time::timeout(self.stall, stream.next()).await {
                Err(_) => {
                    return Err(ProviderError::Transport(format!(
                        "provider sent nothing for {}s — turn abandoned",
                        self.stall.as_secs()
                    )));
                }
                // A body that ends before `message_stop` is a cut connection,
                // not a finished answer: returning it would journal a
                // truncated turn as if the model had meant it.
                Ok(None) => {
                    return Err(ProviderError::Transport(
                        "stream ended before message_stop".into(),
                    ));
                }
                Ok(Some(c)) => c.map_err(|e| ProviderError::Transport(e.to_string()))?,
            };
            for data in sse.feed(&chunk) {
                let ev: Value = serde_json::from_str(&data)
                    .map_err(|e| ProviderError::Protocol(format!("bad event: {e}")))?;
                match ev["type"].as_str().unwrap_or("") {
                    "message_start" => {
                        let u = &ev["message"]["usage"];
                        usage.input = u["input_tokens"].as_u64().unwrap_or(0);
                        usage.cache_read = u["cache_read_input_tokens"].as_u64().unwrap_or(0);
                        usage.cache_write = u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
                    }
                    "content_block_start" => {
                        let idx = ev["index"].as_u64().unwrap_or(0) as usize;
                        let block = &ev["content_block"];
                        let acc = match block["type"].as_str().unwrap_or("") {
                            "text" => BlockAcc::Text {
                                text: String::new(),
                            },
                            "thinking" => BlockAcc::Thinking {
                                text: String::new(),
                                signature: block["signature"]
                                    .as_str()
                                    .unwrap_or_default()
                                    .to_string(),
                            },
                            "redacted_thinking" => BlockAcc::Redacted {
                                data: block["data"].as_str().unwrap_or_default().to_string(),
                            },
                            "tool_use" => BlockAcc::Tool {
                                id: block["id"].as_str().unwrap_or_default().to_string(),
                                name: block["name"].as_str().unwrap_or_default().to_string(),
                                json_in: String::new(),
                            },
                            _ => BlockAcc::Other,
                        };
                        if blocks.len() <= idx {
                            blocks.resize(idx + 1, BlockAcc::Other);
                        }
                        blocks[idx] = acc;
                    }
                    "content_block_delta" => {
                        let idx = ev["index"].as_u64().unwrap_or(0) as usize;
                        match blocks.get_mut(idx) {
                            Some(BlockAcc::Text { text }) => {
                                let d = ev["delta"]["text"].as_str().unwrap_or("");
                                text.push_str(d);
                                sink.on_delta(d);
                            }
                            Some(BlockAcc::Tool { json_in, .. }) => {
                                json_in
                                    .push_str(ev["delta"]["partial_json"].as_str().unwrap_or(""));
                            }
                            Some(BlockAcc::Thinking { text, signature }) => {
                                if let Some(sig) = ev["delta"]["signature"].as_str() {
                                    signature.push_str(sig);
                                }
                                let d = ev["delta"]["thinking"].as_str().unwrap_or("");
                                text.push_str(d);
                                sink.on_thinking(d);
                            }
                            _ => {}
                        }
                    }
                    "message_delta" => {
                        stop_reason = match ev["delta"]["stop_reason"].as_str() {
                            Some("tool_use") => StopReason::ToolUse,
                            Some("max_tokens") | Some("model_context_window_exceeded") => {
                                StopReason::Length
                            }
                            Some("refusal") => {
                                sink.on_delta(
                                    "\n[the provider's safety filter declined this request]",
                                );
                                StopReason::Error
                            }
                            _ => StopReason::Stop,
                        };
                        if let Some(o) = ev["usage"]["output_tokens"].as_u64() {
                            usage.output = o;
                        }
                    }
                    "message_stop" => done = true,
                    // Mid-stream failure (e.g. `overloaded_error`) after the
                    // HTTP 200: surface it so the engine can retry the sample.
                    "error" => {
                        let kind = ev["error"]["type"].as_str().unwrap_or("error");
                        let status = match kind {
                            "overloaded_error" => 529,
                            "rate_limit_error" => 429,
                            "api_error" => 500,
                            _ => 400,
                        };
                        return Err(ProviderError::Api {
                            status,
                            body: ev.to_string(),
                        });
                    }
                    _ => {}
                }
                if done {
                    break;
                }
            }
        }

        let blocks = blocks
            .into_iter()
            .filter_map(|b| match b {
                BlockAcc::Text { text } => Some(ContentBlock::Text { text }),
                BlockAcc::Tool { id, name, json_in } => Some(ContentBlock::ToolCall {
                    id,
                    name,
                    arguments: parse_tool_input(&json_in),
                }),
                // A signed block is kept even when its text is empty (models
                // that omit the visible summary still need it replayed).
                BlockAcc::Thinking { text, signature }
                    if !signature.is_empty() || !text.trim().is_empty() =>
                {
                    Some(ContentBlock::Thinking {
                        thinking: text,
                        signature: (!signature.is_empty()).then_some(signature),
                        redacted: None,
                    })
                }
                BlockAcc::Redacted { data } => Some(ContentBlock::Thinking {
                    thinking: String::new(),
                    signature: None,
                    redacted: Some(data),
                }),
                _ => None,
            })
            .collect();
        Ok(AssistantTurn {
            blocks,
            usage,
            stop_reason,
        })
    }
}

/// Tool input as streamed. A tool with no parameters streams nothing; input
/// that does not parse (usually cut off by `max_tokens`) is kept under
/// [`hasp_core::INVALID_INPUT_KEY`] so the engine can hand the model a
/// precise error instead of running the tool on `{}` — and the replayed
/// `input` is still an object, which the API requires.
pub(crate) fn parse_tool_input(raw: &str) -> Value {
    if raw.trim().is_empty() {
        return json!({});
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(v) if v.is_object() => v,
        _ => json!({ hasp_core::INVALID_INPUT_KEY: raw }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hasp_core::ToolSpec;

    fn assistant(blocks: Vec<ContentBlock>) -> AgentMessage {
        AgentMessage::Assistant {
            content: blocks,
            provider: "deepseek".into(),
            model: "deepseek-v4-pro".into(),
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            ts: 1,
        }
    }

    fn tool_result(id: &str) -> AgentMessage {
        AgentMessage::ToolResult {
            tool_call_id: id.into(),
            tool_name: "bash".into(),
            content: "ok".into(),
            is_error: false,
            ts: 2,
            images: Vec::new(),
        }
    }

    #[test]
    fn parallel_tool_results_share_one_user_message() {
        // Real models emit several tool_use blocks per turn; the API rejects a
        // second message between them and their results.
        let messages = vec![
            AgentMessage::User {
                content: "go".into(),
                ts: 0,
                images: Vec::new(),
            },
            assistant(vec![
                ContentBlock::ToolCall {
                    id: "a".into(),
                    name: "bash".into(),
                    arguments: json!({"command": "ls"}),
                },
                ContentBlock::ToolCall {
                    id: "b".into(),
                    name: "read".into(),
                    arguments: json!({"path": "x"}),
                },
            ]),
            tool_result("a"),
            tool_result("b"),
            AgentMessage::User {
                content: "next".into(),
                ts: 3,
                images: Vec::new(),
            },
        ];

        let wire = map_messages(&messages, "deepseek-v4-pro");
        assert_eq!(wire.len(), 4, "results merged into one message: {wire:?}");
        let results = wire[2]["content"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["tool_use_id"], "a");
        assert_eq!(results[1]["tool_use_id"], "b");
        assert_eq!(wire[3]["role"], "user");
    }

    #[test]
    fn a_thinking_only_turn_is_not_sent_as_empty_content() {
        // Unsigned thinking is not replayed, so a thinking-only turn would
        // otherwise serialize as `content: []`.
        let wire = map_messages(
            &[assistant(vec![ContentBlock::thinking("pondering")])],
            "deepseek-v4-pro",
        );
        assert!(wire.is_empty(), "{wire:?}");
    }

    #[test]
    fn single_result_messages_are_unchanged() {
        let wire = map_messages(&[tool_result("solo")], "m");
        assert_eq!(wire.len(), 1);
        assert_eq!(wire[0]["content"][0]["tool_use_id"], "solo");
    }

    fn spec(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: "d".into(),
            input_schema: json!({"type": "object"}),
        }
    }

    #[test]
    fn sse_parser_handles_split_and_multi_events() {
        let mut p = SseParser::default();
        let a = p.feed(b"event: message_start\ndata: {\"type\":\"message_start\"}\n\nevent: c");
        assert_eq!(a.len(), 1);
        assert!(a[0].contains("message_start"));
        let b = p.feed(b"ontent_block_start\ndata: {\"type\":\"content_block_start\"}\n\n");
        assert_eq!(b.len(), 1);
        assert!(b[0].contains("content_block_start"));
    }

    #[test]
    fn body_places_cache_breakpoints_and_maps_messages() {
        let req = ModelRequest {
            system: "s".into(),
            tools: vec![spec("b"), spec("a")],
            messages: vec![
                AgentMessage::User {
                    content: "hi".into(),
                    ts: 0,
                    images: Vec::new(),
                },
                AgentMessage::Assistant {
                    content: vec![
                        ContentBlock::thinking("hidden chain of thought"),
                        ContentBlock::Text {
                            text: "visible".into(),
                        },
                        ContentBlock::ToolCall {
                            id: "t1".into(),
                            name: "bash".into(),
                            arguments: json!({"command": "ls"}),
                        },
                    ],
                    provider: "anthropic".into(),
                    model: "m".into(),
                    usage: Usage::default(),
                    stop_reason: StopReason::ToolUse,
                    ts: 1,
                },
                AgentMessage::ToolResult {
                    tool_call_id: "t1".into(),
                    tool_name: "bash".into(),
                    content: "out".into(),
                    is_error: false,
                    ts: 2,
                    images: Vec::new(),
                },
            ],
            model: "m".into(),
            max_tokens: 16,
            effort: None,
        };
        let body = build_body(&req);
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools.last().unwrap()["cache_control"]["type"], "ephemeral");

        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        let assistant_content = messages[1]["content"].as_array().unwrap();
        assert_eq!(
            assistant_content.len(),
            2,
            "unsigned thinking is not replayed"
        );
        assert_eq!(assistant_content[0]["type"], "text");
        assert_eq!(assistant_content[0]["text"], "visible");
        assert_eq!(assistant_content[1]["type"], "tool_use");
        assert_eq!(messages[2]["content"][0]["type"], "tool_result");
    }

    fn signed(text: &str, sig: &str) -> ContentBlock {
        ContentBlock::Thinking {
            thinking: text.into(),
            signature: Some(sig.into()),
            redacted: None,
        }
    }

    #[test]
    fn signed_thinking_is_replayed_first_for_the_same_model_only() {
        let turn = assistant(vec![
            signed("", "sig-1"),
            ContentBlock::Thinking {
                thinking: String::new(),
                signature: None,
                redacted: Some("opaque".into()),
            },
            ContentBlock::ToolCall {
                id: "a".into(),
                name: "bash".into(),
                arguments: json!({}),
            },
        ]);
        let same = map_messages(std::slice::from_ref(&turn), "deepseek-v4-pro");
        let blocks = same[0]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "thinking");
        assert_eq!(blocks[0]["signature"], "sig-1");
        assert_eq!(blocks[1]["type"], "redacted_thinking");
        assert_eq!(blocks[1]["data"], "opaque");
        assert_eq!(blocks[2]["type"], "tool_use");

        // After a model switch the signature cannot be verified: drop it.
        let other = map_messages(&[turn], "claude-opus-5-5");
        let blocks = other[0]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "tool_use");
    }

    #[test]
    fn conversation_tail_gets_cache_breakpoints() {
        let mut messages = vec![AgentMessage::User {
            content: "go".into(),
            ts: 0,
            images: Vec::new(),
        }];
        for id in ["a", "b"] {
            messages.push(assistant(vec![ContentBlock::ToolCall {
                id: id.into(),
                name: "bash".into(),
                arguments: json!({}),
            }]));
            messages.push(tool_result(id));
        }
        let req = ModelRequest {
            system: "s".into(),
            tools: vec![spec("a")],
            messages,
            model: "deepseek-v4-pro".into(),
            max_tokens: 16,
            effort: None,
        };
        let body = build_body(&req);
        let wire = body["messages"].as_array().unwrap();
        let marked: Vec<usize> = wire
            .iter()
            .enumerate()
            .filter(|(_, m)| {
                m["content"]
                    .as_array()
                    .and_then(|b| b.last())
                    .is_some_and(|b| b.get("cache_control").is_some())
            })
            .map(|(i, _)| i)
            .collect();
        assert_eq!(marked, vec![wire.len() - 3, wire.len() - 1]);
        // tools + system + two message breakpoints = the API's limit of four
        let total = body.to_string().matches("cache_control").count();
        assert_eq!(total, 4);
    }

    #[test]
    fn a_session_effort_overrides_the_default_and_is_clamped() {
        let c = Anthropic::new("k".into(), "claude-opus-5".into()).with_reasoning(None);
        assert_eq!(
            c.reasoning_for("claude-opus-5", Some("low")).1.as_deref(),
            Some("low")
        );
        assert_eq!(c.default_effort("claude-opus-5").as_deref(), Some("xhigh"));
        // A session set to xhigh keeps working after /model to a 4.6 model.
        assert_eq!(
            c.reasoning_for("claude-sonnet-4-6", Some("xhigh"))
                .1
                .as_deref(),
            Some("high")
        );
        assert!(c.effort_levels("claude-opus-5").contains(&"xhigh"));
        assert!(!c.effort_levels("claude-sonnet-4-6").contains(&"xhigh"));
        assert!(c.effort_levels("claude-haiku-4-5").is_empty());
    }

    #[test]
    fn reasoning_fields_follow_the_model() {
        let caps = |m: &str| ClaudeCaps::of(m);
        assert!(caps("claude-opus-5-5").adaptive && caps("claude-opus-5-5").xhigh);
        assert!(caps("claude-sonnet-5").xhigh);
        assert!(caps("claude-fable-5-1").adaptive);
        assert!(caps("claude-sonnet-4-6").adaptive && !caps("claude-sonnet-4-6").xhigh);
        assert!(!caps("claude-haiku-4-5").adaptive && !caps("claude-haiku-4-5").effort);
        assert!(!caps("claude-sonnet-4-5").adaptive);
        assert!(caps("claude-opus-4-5").effort && !caps("claude-opus-4-5").adaptive);
        assert_eq!(caps("deepseek-v4-pro"), ClaudeCaps::default());

        let req = |model: &str| ModelRequest {
            system: "s".into(),
            tools: vec![],
            messages: vec![],
            model: model.into(),
            max_tokens: 16,
            effort: None,
        };
        let body_for = |c: &Anthropic, model: &str| {
            let (t, e) = c.reasoning_for(model, None);
            build_body_with(&req(model), t.as_ref(), e.as_deref(), true)
        };
        // One client, switched between models mid-session.
        let c = Anthropic::new("k".into(), "claude-opus-5".into()).with_reasoning(Some("xhigh"));
        let body = body_for(&c, "claude-sonnet-4-6");
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(
            body["output_config"]["effort"], "high",
            "xhigh clamps on 4.6"
        );
        let body = body_for(&c, "claude-haiku-4-5");
        assert!(body.get("thinking").is_none() && body.get("output_config").is_none());

        let c = Anthropic::new("k".into(), "claude-opus-5".into()).with_reasoning(None);
        assert_eq!(
            body_for(&c, "claude-opus-5")["output_config"]["effort"],
            "xhigh"
        );
        // Not first-party (DeepSeek/z.ai endpoints): no reasoning fields at all.
        let c = Anthropic::new("k".into(), "claude-opus-5".into());
        assert!(body_for(&c, "claude-opus-5").get("thinking").is_none());
    }

    #[test]
    fn unparseable_tool_input_is_flagged_not_emptied() {
        assert_eq!(parse_tool_input(""), json!({}));
        assert_eq!(parse_tool_input(r#"{"path":"a"}"#), json!({"path": "a"}));
        let cut = parse_tool_input(r#"{"path":"a","content":"hal"#);
        assert!(cut[hasp_core::INVALID_INPUT_KEY].is_string());
    }

    fn png() -> hasp_core::Image {
        hasp_core::Image {
            media_type: "image/png".into(),
            data: "iVBORw0KGgo=".into(),
        }
    }

    #[test]
    fn images_ride_in_user_messages_and_tool_results_or_become_placeholders() {
        let messages = vec![
            AgentMessage::User {
                content: "what is this?".into(),
                ts: 0,
                images: vec![png()],
            },
            assistant(vec![ContentBlock::ToolCall {
                id: "a".into(),
                name: "read".into(),
                arguments: json!({"path": "x.png"}),
            }]),
            AgentMessage::ToolResult {
                tool_call_id: "a".into(),
                tool_name: "read".into(),
                content: "x.png".into(),
                is_error: false,
                ts: 2,
                images: vec![png()],
            },
        ];
        let wire = map_messages_with(&messages, "m", true);
        let user = wire[0]["content"].as_array().unwrap();
        assert_eq!(user[0]["type"], "text");
        assert_eq!(user[1]["type"], "image");
        assert_eq!(user[1]["source"]["media_type"], "image/png");
        let result = &wire[2]["content"][0]["content"];
        assert_eq!(result[1]["type"], "image");

        let blind = map_messages_with(&messages, "m", false);
        let text = Value::Array(blind).to_string();
        assert!(
            !text.contains("iVBOR"),
            "no image data to a text-only model"
        );
        assert!(text.contains("image(s) omitted"));

        let c = Anthropic::new("k".into(), "deepseek-v4-pro".into()).with_name("deepseek");
        assert!(!c.vision_for("deepseek-v4-pro"));
        let c = Anthropic::new("k".into(), "glm-4.6".into()).with_name("zai");
        assert!(!c.vision_for("glm-4.6") && c.vision_for("glm-4.5v"));
        assert!(Anthropic::new("k".into(), "claude-opus-5".into()).vision_for("claude-opus-5"));
    }
}
