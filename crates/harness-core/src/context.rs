//! Context assembly with a byte-stable prefix (research/03-architecture.md §4.2).
//! Order: system prompt → project instructions → tool schemas → history.
//! Nothing dynamic (timestamps, run ids) may enter the system prompt.

use std::path::Path;

use crate::session::SessionManager;
use crate::{AgentMessage, ToolSpec, estimate_tokens};

pub const BASE_SYSTEM_PROMPT: &str = r#"You are a terminal-based coding agent. You help with software engineering tasks in the user's project.

# Environment
- Project root: {ROOT}. All file paths must be relative to it; absolute paths and `..` are rejected.
- A trace of every API call and tool execution is written under `.harness/traces/` in the project root (newest file = current run). Read it when you need to debug your own behavior.

# Working style
- Read before editing; prefer the smallest change that solves the problem.
- Verify work by running the project's own build/test commands when practical, and report results honestly. Do not claim something works unless it has been run or tested.
- Keep responses concise; match the surrounding code style.
- Write in the language the user writes in, defaulting to English. Never switch languages on your own, and keep code, commands, paths and identifiers exactly as they are written.

# Actions
- Local, reversible work (editing files, running builds and tests) is fine to do freely.
- Actions that are hard to reverse, reach shared state (pushes, PRs, messages), or delete work require explicit user confirmation. One approval never extends to later, different actions.
- If you find unexpected state (unfamiliar files, branches, configuration), investigate before overwriting; it may be work in progress."#;

pub struct AssembledContext {
    pub system: String,
    pub messages: Vec<AgentMessage>,
    pub est_tokens: u64,
}

pub fn assemble(sess: &SessionManager, tools: &[ToolSpec]) -> AssembledContext {
    assemble_with(sess, tools, false)
}

/// `idle_prune` applies Claude Code's time-based microcompact as a request-level
/// transform (the session log stays append-only): when the idle gap has
/// certainly killed the provider cache, shrink the inevitable rewrite by
/// clearing stale tool-result content. Never forces a miss that wouldn't have
/// happened.
pub fn assemble_with(
    sess: &SessionManager,
    tools: &[ToolSpec],
    idle_prune: bool,
) -> AssembledContext {
    let mut system = BASE_SYSTEM_PROMPT.replace("{ROOT}", &sess.identity().root.to_string_lossy());
    if let Some(instructions) = load_instructions(&sess.identity().root) {
        system.push_str("\n\n# Project instructions\n\n");
        system.push_str(&instructions);
    }
    let mut messages = sess.context_messages();
    if idle_prune {
        prune_stale(&mut messages);
    }
    let tool_chars: usize = tools
        .iter()
        .map(|t| t.name.len() + t.description.len() + t.input_schema.to_string().len())
        .sum();
    let est = (system.len() + tool_chars) as u64 / 4 + estimate_tokens(&messages) + 64;
    AssembledContext {
        system,
        messages,
        est_tokens: est,
    }
}

/// 60 minutes exceeds every provider's default prompt-cache TTL.
pub const IDLE_PRUNE_AFTER_MS: i64 = 60 * 60 * 1000;
pub const PRUNE_KEEP_RECENT: usize = 5;
const COMPACTABLE_TOOLS: &[&str] = &["read", "write", "edit", "bash"];
pub const CLEARED_MARKER: &str = "[Old tool result content cleared]";

pub fn prune_stale(messages: &mut [AgentMessage]) {
    let tool_idx: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter_map(|(i, m)| match m {
            AgentMessage::ToolResult { tool_name, .. }
                if COMPACTABLE_TOOLS.contains(&tool_name.as_str()) =>
            {
                Some(i)
            }
            _ => None,
        })
        .collect();
    if tool_idx.len() <= PRUNE_KEEP_RECENT {
        return;
    }
    let keep: std::collections::HashSet<usize> = tool_idx
        .iter()
        .rev()
        .take(PRUNE_KEEP_RECENT)
        .copied()
        .collect();
    for i in tool_idx {
        if !keep.contains(&i)
            && let AgentMessage::ToolResult { content, .. } = &mut messages[i]
        {
            *content = CLEARED_MARKER.to_string();
        }
    }
}

/// Fixed discovery chain: parent directory then root, each contributing the
/// first of AGENTS.md / HARNESS.md / CLAUDE.md. Ancestors first (general →
/// specific), byte-stable within a session.
fn load_instructions(root: &Path) -> Option<String> {
    let mut dirs: Vec<&Path> = vec![root];
    if let Some(parent) = root.parent() {
        dirs.insert(0, parent);
    }
    let mut parts = Vec::new();
    for dir in dirs {
        for name in ["AGENTS.md", "HARNESS.md", "CLAUDE.md"] {
            if let Ok(text) = std::fs::read_to_string(dir.join(name))
                && !text.trim().is_empty()
            {
                parts.push(format!(
                    "## from {name} in {}\n\n{}",
                    dir.display(),
                    text.trim()
                ));
                break;
            }
        }
    }
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionManager;
    use harness_workspace::Workspace;

    #[test]
    fn system_prompt_is_stable_across_calls_and_contains_root() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = Workspace::resolve(tmp.path()).unwrap();
        let mut sess = SessionManager::in_memory(&ws);
        sess.append_message(crate::AgentMessage::User {
            content: "hi".into(),
            ts: 0,
        });
        let a = assemble(&sess, &[]);
        let b = assemble(&sess, &[]);
        assert_eq!(a.system, b.system);
        assert!(
            a.system.contains(
                tmp.path()
                    .canonicalize()
                    .unwrap()
                    .to_string_lossy()
                    .as_ref()
            )
        );
    }

    #[test]
    fn agents_md_chain_is_ancestors_first() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(tmp.path().join("a").join("AGENTS.md"), "ancestor rules").unwrap();
        std::fs::write(nested.join("AGENTS.md"), "root rules").unwrap();
        let ws = Workspace::resolve(&nested).unwrap();
        let sess = SessionManager::in_memory(&ws);
        let ctx = assemble(&sess, &[]);
        let ancestor_at = ctx
            .system
            .find("ancestor rules")
            .expect("ancestor instructions included");
        let root_at = ctx
            .system
            .find("root rules")
            .expect("root instructions included");
        assert!(
            ancestor_at < root_at,
            "ancestors come first (general → specific)"
        );
    }

    #[test]
    fn prune_keeps_recent_and_skips_non_compactable() {
        let mut messages = Vec::new();
        for i in 0..8 {
            messages.push(crate::AgentMessage::ToolResult {
                tool_call_id: format!("c{i}"),
                tool_name: "read".into(),
                content: format!("payload {i}"),
                is_error: false,
                ts: i,
            });
        }
        messages.push(crate::AgentMessage::ToolResult {
            tool_call_id: "mcp1".into(),
            tool_name: "mcp__far__away".into(),
            content: "precious".into(),
            is_error: false,
            ts: 99,
        });
        prune_stale(&mut messages);
        let read: Vec<&str> = messages
            .iter()
            .filter_map(|m| match m {
                crate::AgentMessage::ToolResult {
                    tool_name, content, ..
                } if tool_name == "read" => Some(content.as_str()),
                _ => None,
            })
            .collect();
        // oldest 3 cleared, most recent 5 intact
        assert_eq!(read[0], CLEARED_MARKER);
        assert_eq!(read[2], CLEARED_MARKER);
        assert_eq!(read[3], "payload 3");
        assert_eq!(read[7], "payload 7");
        // non-compactable tool untouched
        assert!(
            matches!(&messages[8], crate::AgentMessage::ToolResult { content, .. } if content == "precious")
        );
    }
}
