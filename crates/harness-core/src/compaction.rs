//! Compaction: a standalone summarization request producing a 9-section
//! successor brief (Grok Build's template), with a token-budgeted retained
//! tail persisted as a checkpoint entry. Booked as a cache *reset*, never a
//! miss (research/01-source-notes.md §3.4, research/03-architecture.md §7).

use crate::session::{SessionManager, now_ms};
use crate::{AgentMessage, ContentBlock, ModelRequest, Provider, Sink, estimate_tokens};
use anyhow::{Result, bail};

pub const TAIL_TOKEN_BUDGET: u64 = 20_000;
const TOOL_RESULT_SNIPPET: usize = 1_500;

const SUMMARIZER_SYSTEM: &str = "You are a context summarization assistant. Produce a faithful, \
     structured handoff summary of the transcript you are given. Be concise and complete; never \
     invent facts. Write the summary in English, even when the transcript is in another language.";

const BRIEF_PROMPT: &str = r#"Produce a successor brief for the conversation below so a new assistant can continue seamlessly after the earlier turns are discarded. Output numbered sections 1-9, including every section heading even if the section is empty (write "None"):

1. Primary Request and Intent: all explicit user requests and underlying intent, with constraints and scope boundaries.
2. Key Technical Concepts: languages, frameworks, libraries, tools, patterns discussed or relied upon.
3. Files and Code Sections: every file examined, created, or modified — full path, why it matters, and recent edits in full.
4. Errors and Fixes: every error, failed command, or test failure, its root cause, and exactly how it was fixed.
5. Problem Solving: problems already solved and diagnosis in progress, including open hypotheses.
6. All User Messages: every user message verbatim, in order (not tool results).
7. Pending Tasks: tasks the user explicitly requested that are not yet complete. Do not invent tasks.
8. Current Work: precisely what was happening immediately before this summary — files, commands, and state.
9. Optional Next Step: the single next action that directly continues the most recent request, with a verbatim quote showing where work left off.

If an earlier summary appears in the transcript, carry its still-relevant content forward so nothing important is lost. Respond with only the brief."#;

#[derive(Debug, Clone)]
pub struct CompactStats {
    pub tokens_before: u64,
    pub summary_tokens: u64,
    pub retained_messages: usize,
}

struct Mute;

impl Sink for Mute {
    fn on_delta(&mut self, _text: &str) {}
}

/// `context_tokens` is the caller's measurement of what was actually sent
/// (system prompt and tool schemas included) — the engine owns that number, and
/// reporting a messages-only estimate here would disagree with the threshold
/// that triggered compaction.
pub async fn compact(
    provider: &dyn Provider,
    model: &str,
    session: &mut SessionManager,
    max_tokens: u32,
    context_tokens: u64,
) -> Result<CompactStats> {
    compact_with_budget(
        provider,
        model,
        session,
        max_tokens,
        TAIL_TOKEN_BUDGET,
        context_tokens,
    )
    .await
}

pub async fn compact_with_budget(
    provider: &dyn Provider,
    model: &str,
    session: &mut SessionManager,
    max_tokens: u32,
    tail_budget: u64,
    context_tokens: u64,
) -> Result<CompactStats> {
    let messages = session.context_messages();
    let mut used: u64 = 0;
    let mut cut = 0;
    for (i, m) in messages.iter().enumerate().rev() {
        used += estimate_tokens(std::slice::from_ref(m));
        if used > tail_budget {
            cut = i + 1;
            break;
        }
    }
    if cut == 0 {
        bail!("nothing to compact: history fits within the retained-tail budget");
    }
    let (older, tail) = messages.split_at(cut);
    let tokens_before = context_tokens;
    let transcript = serialize(older);
    let req = ModelRequest {
        system: SUMMARIZER_SYSTEM.into(),
        tools: Vec::new(),
        messages: vec![AgentMessage::User {
            content: format!("{BRIEF_PROMPT}\n\n<transcript>\n{transcript}\n</transcript>"),
            ts: now_ms(),
        }],
        model: model.to_string(),
        max_tokens,
    };
    let mut mute = Mute;
    let turn = provider.complete(&req, &mut mute).await?;
    let summary = turn
        .blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    if summary.trim().is_empty() {
        bail!("compaction produced an empty summary");
    }
    let stats = CompactStats {
        tokens_before,
        summary_tokens: turn.usage.output,
        retained_messages: tail.len(),
    };
    session.append_compaction(summary, tokens_before, tail.to_vec());
    Ok(stats)
}

/// Transcript serialization for the summarizer: full text for user/assistant,
/// snippets for tool results.
fn serialize(messages: &[AgentMessage]) -> String {
    let mut out = String::new();
    for m in messages {
        match m {
            AgentMessage::User { content, .. } => {
                out.push_str("user: ");
                out.push_str(content);
                out.push('\n');
            }
            AgentMessage::Assistant { content, .. } => {
                out.push_str("assistant: ");
                for b in content {
                    match b {
                        ContentBlock::Text { text } => out.push_str(text),
                        ContentBlock::ToolCall { name, .. } => {
                            out.push_str(&format!("[calls {name}]"));
                        }
                        ContentBlock::Thinking { .. } => {}
                    }
                }
                out.push('\n');
            }
            AgentMessage::ToolResult {
                tool_name,
                content,
                is_error,
                ..
            } => {
                let snippet: String = content.chars().take(TOOL_RESULT_SNIPPET).collect();
                let status = if *is_error { "error" } else { "ok" };
                out.push_str(&format!("tool {tool_name} ({status}): {snippet}\n"));
            }
        }
    }
    out
}
