//! MCP bridging against a real child process (the fake server binary).

use hasp_core::engine::{Engine, TurnSink};
use hasp_core::session::SessionManager;
use hasp_core::{AgentMessage, ContentBlock, Gate, GateDecision, Sink, StopReason, Usage};
use hasp_provider::mock::MockProvider;
use hasp_workspace::Workspace;
use serde_json::json;

struct NoteSink;

impl Sink for NoteSink {
    fn on_delta(&mut self, _t: &str) {}
}

impl TurnSink for NoteSink {
    fn on_note(&mut self, _n: &str) {}
}

struct AllowGate;

impl Gate for AllowGate {
    fn check_bash(&mut self, _: &str) -> GateDecision {
        GateDecision::Allow
    }
    fn check_write(&mut self, _: &str) -> GateDecision {
        GateDecision::Allow
    }
}

fn write_project_config(root: &std::path::Path, command: &str) {
    let dir = root.join(".harness");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("mcp.json"),
        json!({
            "servers": {"fake": {"command": command, "args": []}}
        })
        .to_string(),
    )
    .unwrap();
}

fn turn(blocks: Vec<ContentBlock>, stop: StopReason) -> hasp_core::AssistantTurn {
    hasp_core::AssistantTurn {
        blocks,
        usage: Usage::default(),
        stop_reason: stop,
    }
}

fn tool_results(engine: &Engine) -> Vec<(String, bool)> {
    engine
        .session
        .context_messages()
        .iter()
        .filter_map(|m| match m {
            AgentMessage::ToolResult {
                content, is_error, ..
            } => Some((content.clone(), *is_error)),
            _ => None,
        })
        .collect()
}

fn engine_with(
    ws: &Workspace,
    tools: Vec<Box<dyn hasp_core::Tool>>,
    script: Vec<hasp_core::AssistantTurn>,
) -> Engine {
    Engine::new(
        ws.clone(),
        SessionManager::in_memory(ws),
        Box::new(MockProvider::new(script)),
        tools,
        Box::new(AllowGate),
        "mock".into(),
        1024,
    )
    .unwrap()
}

#[tokio::test]
async fn tools_are_listed_and_a_call_reaches_the_transcript() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = Workspace::resolve(tmp.path()).unwrap();
    write_project_config(tmp.path(), env!("CARGO_BIN_EXE_mcp-fake-server"));

    // Only this project's server: the user's own ~/.hasp/mcp.json servers also
    // load here and must not make the test depend on the machine it runs on.
    let bridged: Vec<_> = hasp_mcp::tools_for(&ws)
        .await
        .into_iter()
        .filter(|t| t.spec().name.starts_with("mcp__fake__"))
        .collect();
    let names: Vec<String> = bridged.iter().map(|t| t.spec().name).collect();
    assert_eq!(
        names,
        vec!["mcp__fake__boom", "mcp__fake__echo"],
        "bridged tools are namespaced and sorted"
    );
    let echo_spec = bridged[1].spec();
    assert_eq!(echo_spec.description, "Echo text back");
    assert_eq!(echo_spec.input_schema["required"][0], "text");

    let mut tools = hasp_tools::builtins_base();
    tools.extend(bridged);
    let mut engine = engine_with(
        &ws,
        tools,
        vec![
            turn(
                vec![ContentBlock::ToolCall {
                    id: "c1".into(),
                    name: "mcp__fake__echo".into(),
                    arguments: json!({"text": "hi"}),
                }],
                StopReason::ToolUse,
            ),
            turn(
                vec![ContentBlock::Text {
                    text: "done".into(),
                }],
                StopReason::Stop,
            ),
        ],
    );
    let stats = engine
        .run_turn("call the tool", &mut NoteSink)
        .await
        .unwrap();
    assert_eq!(stats.tool_calls, 1);

    let results = tool_results(&engine);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].0, "echoed: hi");
    assert!(!results[0].1);
}

#[tokio::test]
async fn server_reported_errors_surface_as_tool_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = Workspace::resolve(tmp.path()).unwrap();
    write_project_config(tmp.path(), env!("CARGO_BIN_EXE_mcp-fake-server"));

    let mut tools = hasp_tools::builtins_base();
    tools.extend(hasp_mcp::tools_for(&ws).await);
    let mut engine = engine_with(
        &ws,
        tools,
        vec![
            turn(
                vec![ContentBlock::ToolCall {
                    id: "c1".into(),
                    name: "mcp__fake__boom".into(),
                    arguments: json!({}),
                }],
                StopReason::ToolUse,
            ),
            turn(
                vec![ContentBlock::Text {
                    text: "done".into(),
                }],
                StopReason::Stop,
            ),
        ],
    );
    engine
        .run_turn("call the failing tool", &mut NoteSink)
        .await
        .unwrap();

    let results = tool_results(&engine);
    assert_eq!(results[0].0, "kaboom");
    assert!(results[0].1, "isError from the server maps to a tool error");
}

#[tokio::test]
async fn a_broken_server_is_skipped_and_startup_still_succeeds() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = Workspace::resolve(tmp.path()).unwrap();
    write_project_config(tmp.path(), "harness-no-such-binary-xyz");

    let bridged = hasp_mcp::tools_for(&ws).await;
    assert!(
        !bridged.iter().any(|t| t.spec().name.starts_with("mcp__fake__")),
        "unstartable server contributes no tools"
    );
}
