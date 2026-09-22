//! A session killed mid-tool must stay usable: the dangling `tool_use` turn is
//! closed with an explicit interrupted result when the session is reopened,
//! otherwise every later request fails with "tool_use ids were found without
//! tool_result blocks".

use harness_core::session::SessionManager;
use harness_core::{AgentMessage, ContentBlock, StopReason, Usage};
use harness_workspace::Workspace;

fn assistant_turn(calls: &[(&str, &str)]) -> AgentMessage {
    AgentMessage::Assistant {
        content: calls
            .iter()
            .map(|(id, name)| ContentBlock::ToolCall {
                id: (*id).into(),
                name: (*name).into(),
                arguments: serde_json::json!({}),
            })
            .collect(),
        provider: "mock".into(),
        model: "mock".into(),
        usage: Usage::default(),
        stop_reason: StopReason::ToolUse,
        ts: 1,
    }
}

fn tool_result(id: &str) -> AgentMessage {
    AgentMessage::ToolResult {
        tool_call_id: id.into(),
        tool_name: "bash".into(),
        content: "done".into(),
        is_error: false,
        ts: 2,
        images: Vec::new(),
    }
}

fn results(session: &SessionManager) -> Vec<(String, bool)> {
    session
        .context_messages()
        .iter()
        .filter_map(|m| match m {
            AgentMessage::ToolResult {
                tool_call_id,
                is_error,
                ..
            } => Some((tool_call_id.clone(), *is_error)),
            _ => None,
        })
        .collect()
}

#[test]
fn an_interrupted_tool_turn_is_closed_on_reopen() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = Workspace::resolve(tmp.path()).unwrap();
    let mut session = SessionManager::create(&ws).unwrap();
    session.append_message(AgentMessage::User {
        content: "go".into(),
        ts: 0,
        images: Vec::new(),
    });
    session.append_message(assistant_turn(&[("a", "bash"), ("b", "read")]));
    session.append_message(tool_result("a")); // "b" never reported: killed here
    let path = session.path.clone().unwrap();
    drop(session);

    let reopened = SessionManager::open(&path).unwrap();
    assert_eq!(reopened.repaired_tool_calls, 1);
    let got = results(&reopened);
    assert_eq!(got.len(), 2, "both calls now have results: {got:?}");
    assert_eq!(got[0], ("a".to_string(), false));
    assert_eq!(got[1], ("b".to_string(), true), "repaired as an error");
    drop(reopened);

    // Repair is journaled, so reopening again has nothing left to do.
    let again = SessionManager::open(&path).unwrap();
    assert_eq!(again.repaired_tool_calls, 0);
    assert_eq!(results(&again).len(), 2);
}

#[test]
fn a_complete_session_is_left_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = Workspace::resolve(tmp.path()).unwrap();
    let mut session = SessionManager::create(&ws).unwrap();
    session.append_message(assistant_turn(&[("a", "bash")]));
    session.append_message(tool_result("a"));
    session.append_message(AgentMessage::Assistant {
        content: vec![ContentBlock::Text { text: "ok".into() }],
        provider: "mock".into(),
        model: "mock".into(),
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        ts: 3,
    });
    let path = session.path.clone().unwrap();
    drop(session);

    let reopened = SessionManager::open(&path).unwrap();
    assert_eq!(reopened.repaired_tool_calls, 0);
    assert_eq!(results(&reopened).len(), 1);
}
