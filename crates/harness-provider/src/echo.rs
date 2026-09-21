//! A deterministic provider for keyless runs: tests, demos, PTY e2e.
//! `HARNESS_MOCK_DELAY_MS` adds an artificial per-sample delay (cancellation
//! tests). `HARNESS_MOCK_SCRIPT` replaces echoing with a fixed script of turns,
//! which is how daemon/PTY tests drive real tool calls — and therefore real
//! approval round-trips.

use async_trait::async_trait;
use harness_core::{
    AgentMessage, AssistantTurn, ContentBlock, ModelRequest, Provider, ProviderError, Sink,
    StopReason, Usage,
};
use std::collections::VecDeque;
use std::sync::Mutex;

pub struct EchoProvider {
    delay_ms: u64,
    script: Mutex<VecDeque<AssistantTurn>>,
}

impl EchoProvider {
    pub fn from_env() -> Self {
        let script = std::env::var("HARNESS_MOCK_SCRIPT")
            .ok()
            .and_then(|raw| parse_script(&raw))
            .unwrap_or_default();
        Self {
            delay_ms: std::env::var("HARNESS_MOCK_DELAY_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            script: Mutex::new(script),
        }
    }
}

/// Script format — a JSON array of turns, consumed in order:
///   `{"text": "…"}`                          one assistant text block
///   `{"tool": "bash", "input": {…}}`         one tool call
///   `{"tools": [{"name": …, "input": …}]}`   several tool calls in one turn
fn parse_script(raw: &str) -> Option<VecDeque<AssistantTurn>> {
    let items: Vec<serde_json::Value> = serde_json::from_str(raw).ok()?;
    let mut turns = VecDeque::new();
    let mut next_id = 0;
    let call = |name: &str, input: serde_json::Value, id: u64| ContentBlock::ToolCall {
        id: format!("call_{id}"),
        name: name.to_string(),
        arguments: input,
    };
    for item in items {
        if let Some(thought) = item["thinking"].as_str() {
            let mut blocks = vec![ContentBlock::Thinking {
                thinking: thought.into(),
            }];
            if let Some(text) = item["text"].as_str() {
                blocks.push(ContentBlock::Text { text: text.into() });
            }
            turns.push_back(AssistantTurn {
                blocks,
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
            });
        } else if let Some(text) = item["text"].as_str() {
            turns.push_back(AssistantTurn {
                blocks: vec![ContentBlock::Text { text: text.into() }],
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
            });
        } else if let Some(name) = item["tool"].as_str() {
            next_id += 1;
            turns.push_back(AssistantTurn {
                blocks: vec![call(name, item["input"].clone(), next_id)],
                usage: Usage::default(),
                stop_reason: StopReason::ToolUse,
            });
        } else if let Some(list) = item["tools"].as_array() {
            let blocks = list
                .iter()
                .filter_map(|t| {
                    let name = t["name"].as_str()?;
                    next_id += 1;
                    Some(call(name, t["input"].clone(), next_id))
                })
                .collect::<Vec<_>>();
            turns.push_back(AssistantTurn {
                blocks,
                usage: Usage::default(),
                stop_reason: StopReason::ToolUse,
            });
        }
    }
    Some(turns)
}

#[async_trait]
impl Provider for EchoProvider {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn context_window(&self) -> usize {
        200_000
    }

    async fn complete(
        &self,
        req: &ModelRequest,
        sink: &mut dyn Sink,
    ) -> Result<AssistantTurn, ProviderError> {
        if self.delay_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
        }
        let scripted = self.script.lock().expect("mock script lock").pop_front();
        if let Some(turn) = scripted {
            for block in &turn.blocks {
                let (text, thinking) = match block {
                    ContentBlock::Text { text } => (Some(text), false),
                    ContentBlock::Thinking { thinking } => (Some(thinking), true),
                    _ => (None, false),
                };
                if let Some(text) = text {
                    for chunk in text.chars().collect::<Vec<_>>().chunks(12) {
                        let piece = chunk.iter().collect::<String>();
                        if thinking {
                            sink.on_thinking(&piece);
                        } else {
                            sink.on_delta(&piece);
                        }
                    }
                }
            }
            return Ok(turn);
        }
        let prompt = req
            .messages
            .iter()
            .rev()
            .find_map(|m| match m {
                AgentMessage::User { content, .. } => Some(content.clone()),
                _ => None,
            })
            .unwrap_or_default();
        let text = format!("echo: {prompt}");
        let out_tokens = (text.len() as u64) / 4;
        // char-boundary chunks so delta rendering is exercised
        let chars: Vec<char> = text.chars().collect();
        for chunk in chars.chunks(12) {
            let s: String = chunk.iter().collect();
            sink.on_delta(&s);
        }
        Ok(AssistantTurn {
            blocks: vec![ContentBlock::Text { text }],
            usage: Usage {
                input: (req.system.len() as u64) / 4,
                output: out_tokens,
                ..Usage::default()
            },
            stop_reason: StopReason::Stop,
        })
    }
}
