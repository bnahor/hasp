//! Compaction: a standalone summarization request producing a 9-section
//! successor brief (Grok Build's template), with a token-budgeted retained
//! tail persisted as a checkpoint entry. Booked as a cache *reset*, never a
//! miss (research/01-source-notes.md §3.4, research/03-architecture.md §7).

use crate::session::SessionManager;
use crate::{
    AgentMessage, AssistantTurn, ContentBlock, ModelRequest, Provider, Sink, estimate_tokens,
};
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

pub struct Mute;

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
    let plan = prepare(session, model, max_tokens, tail_budget, context_tokens)?;
    let turn = provider.complete(&plan.request, &mut Mute).await?;
    finish(plan, turn, session)
}

/// A compaction in two halves around the summary request, so a caller that
/// shares the session (the daemon) need not hold it while the model writes.
pub struct CompactionPlan {
    pub request: ModelRequest,
    tail: Vec<AgentMessage>,
    tokens_before: u64,
    /// Context length when planned; `finish` refuses if it moved.
    messages_at_plan: usize,
}

pub fn prepare(
    session: &SessionManager,
    model: &str,
    max_tokens: u32,
    tail_budget: u64,
    context_tokens: u64,
) -> Result<CompactionPlan> {
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
    let transcript = serialize(older);
    let request = ModelRequest {
        system: SUMMARIZER_SYSTEM.into(),
        tools: Vec::new(),
        messages: vec![AgentMessage::user(format!(
            "{BRIEF_PROMPT}\n\n<transcript>\n{transcript}\n</transcript>"
        ))],
        model: model.to_string(),
        max_tokens,
    };
    Ok(CompactionPlan {
        request,
        tail: tail.to_vec(),
        tokens_before: context_tokens,
        messages_at_plan: messages.len(),
    })
}

/// Apply a summary to the session the plan was made from.
pub fn finish(
    plan: CompactionPlan,
    turn: AssistantTurn,
    session: &mut SessionManager,
) -> Result<CompactStats> {
    if session.context_messages().len() != plan.messages_at_plan {
        bail!("the session changed while the summary was written; nothing was compacted");
    }
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
        tokens_before: plan.tokens_before,
        summary_tokens: turn.usage.output,
        retained_messages: plan.tail.len(),
    };
    session.append_compaction(summary, plan.tokens_before, plan.tail);
    Ok(stats)
}

/// Transcript serialization for the summarizer: full text for user/assistant,
/// snippets for tool results.
fn serialize(messages: &[AgentMessage]) -> String {
    let mut out = String::new();
    for m in messages {
        match m {
            AgentMessage::User {
                content, images, ..
            } => {
                out.push_str("user: ");
                out.push_str(content);
                if !images.is_empty() {
                    out.push_str(&format!(" [{} image(s) attached]", images.len()));
                }
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
