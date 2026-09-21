//! Anthropic Messages API over rustls with SSE streaming, explicit
//! `cache_control` breakpoints (system + last tool), and cache read/write
//! usage capture (research/01-source-notes.md §1.1, §5.4).

use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use harness_core::{
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
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(600))
                .build()
                .expect("reqwest client builds"),
        }
    }

    /// Anthropic-compatible endpoints (z.ai GLM, gateways, …).
    pub fn with_base_url(mut self, base_url: &str) -> Self {
        self.base_url = base_url.to_string();
        self
    }

    /// Declare this model's context window (from the catalog).
    pub fn with_window(mut self, window: usize) -> Self {
        self.window = window;
        self
    }

    /// Name the service behind an Anthropic-compatible endpoint.
    pub fn with_name(mut self, label: &'static str) -> Self {
        self.label = label;
        self
    }
}

fn tool_result_block(m: &AgentMessage) -> Value {
    let AgentMessage::ToolResult {
        tool_call_id,
        content,
        is_error,
        ..
    } = m
    else {
        unreachable!("tool_result_block is only called for ToolResult")
    };
    json!({
        "type": "tool_result",
        "tool_use_id": tool_call_id,
        "content": [{"type": "text", "text": content}],
        "is_error": is_error,
    })
}

/// The API requires every `tool_result` answering an assistant turn to arrive
/// in the single message that follows it, so consecutive results are merged
/// into one user message. The transcript keeps them as separate entries.
fn map_messages(messages: &[AgentMessage]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut open_results: Option<usize> = None;
    for m in messages {
        match m {
            AgentMessage::ToolResult { .. } => match open_results {
                Some(i) => out[i]["content"]
                    .as_array_mut()
                    .expect("tool_result message holds an array")
                    .push(tool_result_block(m)),
                None => {
                    out.push(json!({"role": "user", "content": [tool_result_block(m)]}));
                    open_results = Some(out.len() - 1);
                }
            },
            AgentMessage::Assistant { content, .. } => {
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
                        // Thinking blocks are tied to the producing model's
                        // signature and are not replayed.
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
            AgentMessage::User { content, .. } => {
                open_results = None;
                out.push(json!({"role": "user", "content": [{"type": "text", "text": content}]}));
            }
        }
    }
    out
}

pub(crate) fn build_body(req: &ModelRequest) -> Value {
    let mut tools: Vec<Value> = req
        .tools
        .iter()
        .map(|t| json!({"name": t.name, "description": t.description, "input_schema": t.input_schema}))
        .collect();
    if let Some(last) = tools.last_mut() {
        last["cache_control"] = json!({"type": "ephemeral"});
    }
    json!({
        "model": req.model,
        "max_tokens": req.max_tokens,
        "stream": true,
        "system": [{"type": "text", "text": req.system, "cache_control": {"type": "ephemeral"}}],
        "tools": tools,
        "messages": map_messages(&req.messages),
    })
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

    async fn complete(
        &self,
        req: &ModelRequest,
        sink: &mut dyn Sink,
    ) -> Result<AssistantTurn, ProviderError> {
        let resp = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(&build_body(req))
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;
        let status = resp.status().as_u16();
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Api { status, body });
        }

        let mut stream = resp.bytes_stream();
        let mut sse = SseParser::default();
        let mut usage = Usage::default();
        let mut stop_reason = StopReason::Stop;
        let mut blocks: Vec<BlockAcc> = Vec::new();
        let mut done = false;

        while !done {
            let chunk = match stream.next().await {
                Some(c) => c.map_err(|e| ProviderError::Transport(e.to_string()))?,
                None => break,
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
                            Some(BlockAcc::Thinking { text }) => {
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
                            Some("max_tokens") => StopReason::Length,
                            _ => StopReason::Stop,
                        };
                        if let Some(o) = ev["usage"]["output_tokens"].as_u64() {
                            usage.output = o;
                        }
                    }
                    "message_stop" => done = true,
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
                    arguments: serde_json::from_str(&json_in).unwrap_or_default(),
                }),
                BlockAcc::Thinking { text } if !text.trim().is_empty() => {
                    Some(ContentBlock::Thinking { thinking: text })
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::ToolSpec;

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
            },
        ];

        let wire = map_messages(&messages);
        assert_eq!(wire.len(), 4, "results merged into one message: {wire:?}");
        let results = wire[2]["content"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["tool_use_id"], "a");
        assert_eq!(results[1]["tool_use_id"], "b");
        assert_eq!(wire[3]["role"], "user");
    }

    #[test]
    fn a_thinking_only_turn_is_not_sent_as_empty_content() {
        // Thinking blocks are not replayed, so a thinking-only turn would
        // otherwise serialize as `content: []`.
        let wire = map_messages(&[assistant(vec![ContentBlock::Thinking {
            thinking: "pondering".into(),
        }])]);
        assert!(wire.is_empty(), "{wire:?}");
    }

    #[test]
    fn single_result_messages_are_unchanged() {
        let wire = map_messages(&[tool_result("solo")]);
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
                },
                AgentMessage::Assistant {
                    content: vec![
                        ContentBlock::Thinking {
                            thinking: "hidden chain of thought".into(),
                        },
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
                },
            ],
            model: "m".into(),
            max_tokens: 16,
        };
        let body = build_body(&req);
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools.last().unwrap()["cache_control"]["type"], "ephemeral");

        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        let assistant_content = messages[1]["content"].as_array().unwrap();
        assert_eq!(assistant_content.len(), 2, "thinking is not replayed");
        assert_eq!(assistant_content[0]["type"], "text");
        assert_eq!(assistant_content[0]["text"], "visible");
        assert_eq!(assistant_content[1]["type"], "tool_use");
        assert_eq!(messages[2]["content"][0]["type"], "tool_result");
    }
}
