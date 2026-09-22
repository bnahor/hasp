//! End-to-end turn tests against the mock provider. Lives in `tests/` so the
//! dev-dependency cycle (core ⇄ provider/tools) sees a single harness-core.

use harness_core::engine::{Engine, TurnSink};
use harness_core::session::SessionManager;
use harness_core::{
    AgentMessage, Bus, ChildInfo, ChildReceipt, ChildStatus, ContentBlock, Gate, GateDecision,
    Sink, SpawnHandle, SpawnRequest, StopReason, Tool, ToolCtx, ToolOutput, ToolSpec, Usage,
};
use harness_provider::mock::MockProvider;
use harness_workspace::Workspace;
use serde_json::json;
use std::sync::Arc;

struct NoteSink(Vec<String>);

impl Sink for NoteSink {
    fn on_delta(&mut self, t: &str) {
        self.0.push(t.into());
    }
}

impl TurnSink for NoteSink {
    fn on_note(&mut self, n: &str) {
        self.0.push(n.into());
    }
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

struct DenyBashGate;

impl Gate for DenyBashGate {
    fn check_bash(&mut self, _: &str) -> GateDecision {
        GateDecision::Deny("not in this test".into())
    }
    fn check_write(&mut self, _: &str) -> GateDecision {
        GateDecision::Allow
    }
}

fn turn(blocks: Vec<ContentBlock>, stop: StopReason) -> harness_core::AssistantTurn {
    harness_core::AssistantTurn {
        blocks,
        usage: Usage {
            input: 10,
            output: 5,
            cache_read: 100,
            cache_write: 0,
        },
        stop_reason: stop,
    }
}

fn engine_in(
    tmp: &std::path::Path,
    script: Vec<harness_core::AssistantTurn>,
    gate: Box<dyn Gate>,
) -> Engine {
    let ws = Workspace::resolve(tmp).unwrap();
    let session = SessionManager::in_memory(&ws);
    Engine::new(
        ws,
        session,
        Box::new(MockProvider::new(script)),
        harness_tools::builtins(),
        gate,
        "mock".into(),
        1024,
    )
    .unwrap()
}

#[tokio::test]
async fn two_sample_turn_with_tool_call() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("notes.txt"), "hello world").unwrap();
    let mut engine = engine_in(
        tmp.path(),
        vec![
            turn(
                vec![ContentBlock::ToolCall {
                    id: "c1".into(),
                    name: "read".into(),
                    arguments: json!({"path": "notes.txt"}),
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
        Box::new(AllowGate),
    );
    let mut sink = NoteSink(vec![]);
    let stats = engine.run_turn("read the notes", &mut sink).await.unwrap();

    assert_eq!(stats.samples, 2);
    assert_eq!(stats.tool_calls, 1);
    assert_eq!(stats.usage.cache_read, 200);

    let msgs = engine.session.context_messages();
    assert_eq!(msgs.len(), 4);
    match &msgs[2] {
        AgentMessage::ToolResult {
            content, is_error, ..
        } => {
            assert!(!is_error);
            assert!(content.contains("hello world"), "content: {content}");
        }
        other => panic!("expected tool result, got {other:?}"),
    }

    let trace_path = tmp
        .path()
        .join(".harness")
        .join("traces")
        .join(format!("{}.jsonl", engine.trace.run_id));
    let trace_text = std::fs::read_to_string(trace_path).unwrap();
    assert!(trace_text.contains("\"event\":\"api\""));
    assert!(trace_text.contains("\"event\":\"tool\""));
}

#[tokio::test]
async fn gate_denial_becomes_tool_result() {
    let tmp = tempfile::tempdir().unwrap();
    let mut engine = engine_in(
        tmp.path(),
        vec![
            turn(
                vec![ContentBlock::ToolCall {
                    id: "c1".into(),
                    name: "bash".into(),
                    arguments: json!({"command": "cargo build"}),
                }],
                StopReason::ToolUse,
            ),
            turn(
                vec![ContentBlock::Text { text: "ok".into() }],
                StopReason::Stop,
            ),
        ],
        Box::new(DenyBashGate),
    );
    let mut sink = NoteSink(vec![]);
    engine.run_turn("build it", &mut sink).await.unwrap();
    match &engine.session.context_messages()[2] {
        AgentMessage::ToolResult {
            content, is_error, ..
        } => {
            assert!(is_error);
            assert!(content.contains("not in this test"));
        }
        other => panic!("expected tool result, got {other:?}"),
    }
}

#[tokio::test]
async fn compact_produces_brief_and_tail() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = Workspace::resolve(tmp.path()).unwrap();
    let mut session = SessionManager::in_memory(&ws);
    session.append_message(AgentMessage::User {
        content: "old ".repeat(200),
        ts: 1,
        images: Vec::new(),
    });
    session.append_message(AgentMessage::User {
        content: "recent".into(),
        ts: 2,
        images: Vec::new(),
    });
    let provider = MockProvider::new(vec![turn(
        vec![ContentBlock::Text {
            text: "1. Primary Request and Intent: testing\n8. Current Work: done".into(),
        }],
        StopReason::Stop,
    )]);
    let stats = {
        let est = session.context_est_tokens();
        harness_core::compaction::compact_with_budget(
            &provider,
            "mock",
            &mut session,
            1024,
            10,
            est,
        )
        .await
        .unwrap()
    };
    assert!(stats.tokens_before > 100);
    assert_eq!(stats.retained_messages, 1);
    let msgs = session.context_messages();
    assert_eq!(msgs.len(), 2);
    assert!(matches!(
        &msgs[0],
        AgentMessage::User { content, .. } if content.contains("[Continuation summary")
    ));
    assert!(matches!(&msgs[1], AgentMessage::User { content, .. } if content == "recent"));
}

#[tokio::test]
async fn auto_compact_triggers_over_threshold() {
    let tmp = tempfile::tempdir().unwrap();
    let big = "x".repeat(120_000);
    let provider = MockProvider::new(vec![
        turn(
            vec![ContentBlock::Text { text: "ok".into() }],
            StopReason::Stop,
        ),
        turn(
            vec![ContentBlock::Text {
                text: "1. Primary Request and Intent: big input\n8. Current Work: ok".into(),
            }],
            StopReason::Stop,
        ),
    ])
    .with_window(32_000);
    let ws = Workspace::resolve(tmp.path()).unwrap();
    let session = SessionManager::in_memory(&ws);
    let mut engine = Engine::new(
        ws,
        session,
        Box::new(provider),
        harness_tools::builtins(),
        Box::new(AllowGate),
        "mock".into(),
        1024,
    )
    .unwrap();
    let mut sink = NoteSink(vec![]);
    engine.run_turn(&big, &mut sink).await.unwrap();
    let msgs = engine.session.context_messages();
    assert!(matches!(
        &msgs[0],
        AgentMessage::User { content, .. } if content.contains("[Continuation summary")
    ));
    assert!(
        msgs.len() <= 2,
        "expected brief + small tail, got {}",
        msgs.len()
    );
}

struct MockBus {
    spawns: std::sync::Mutex<Vec<SpawnRequest>>,
    /// What `list_children` reports, so a synchronous `subagent` call has
    /// something to observe.
    child: Option<ChildInfo>,
}

impl MockBus {
    fn new() -> Self {
        Self {
            spawns: Default::default(),
            child: None,
        }
    }

    fn with_child(child: ChildInfo) -> Self {
        Self {
            child: Some(child),
            ..Self::new()
        }
    }
}

fn child(status: ChildStatus, report: &str) -> ChildInfo {
    ChildInfo {
        child_id: "sa-001-test".into(),
        name: "kid".into(),
        status,
        session_id: "child-session".into(),
        mutation: false,
        root: "/w".into(),
        report: Some(report.to_string()),
    }
}

#[async_trait::async_trait]
impl Bus for MockBus {
    async fn spawn_child(&self, req: SpawnRequest) -> Result<SpawnHandle, String> {
        self.spawns.lock().unwrap().push(req);
        Ok(SpawnHandle {
            child_id: "sa-001-test".into(),
            session_id: "child-session".into(),
            root: "/w".into(),
        })
    }

    async fn message_child(&self, _: &str, _: &str, _: bool) -> Result<ChildReceipt, String> {
        Ok(ChildReceipt::Delivered)
    }

    async fn list_children(&self) -> Vec<ChildInfo> {
        self.child.clone().into_iter().collect()
    }
}

#[tokio::test]
async fn notices_deliver_at_boundaries_and_spawn_reaches_bus() {
    let tmp = tempfile::tempdir().unwrap();
    let mut engine = engine_in(
        tmp.path(),
        vec![
            turn(
                vec![ContentBlock::ToolCall {
                    id: "c1".into(),
                    name: "spawn".into(),
                    arguments: json!({"task": "do thing", "mutation": true}),
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
        Box::new(AllowGate),
    );
    let bus = Arc::new(MockBus::new());
    engine.set_bus(bus.clone());
    engine
        .notice_inbox()
        .lock()
        .unwrap()
        .push("[child sa-009 (other) finished]\nresult text".into());

    let mut sink = NoteSink(vec![]);
    engine.run_turn("delegate please", &mut sink).await.unwrap();

    let msgs = engine.session.context_messages();
    assert!(matches!(
        &msgs[1],
        AgentMessage::User { content, .. } if content.contains("[child sa-009")
    ));
    match &msgs[3] {
        AgentMessage::ToolResult { content, .. } => {
            assert!(content.contains("sa-001-test"), "spawn handle: {content}");
        }
        other => panic!("expected tool result, got {other:?}"),
    }
    let spawns = bus.spawns.lock().unwrap();
    assert_eq!(spawns.len(), 1);
    assert_eq!(spawns[0].task, "do thing");
    assert!(spawns[0].mutation);
}

#[tokio::test]
async fn goal_continuation_runs_until_goal_complete_is_called() {
    let tmp = tempfile::tempdir().unwrap();
    let mut engine = engine_in(
        tmp.path(),
        vec![
            turn(
                vec![ContentBlock::Text {
                    text: "step one".into(),
                }],
                StopReason::Stop,
            ),
            turn(
                vec![ContentBlock::ToolCall {
                    id: "g1".into(),
                    name: "goal_complete".into(),
                    arguments: json!({}),
                }],
                StopReason::ToolUse,
            ),
            turn(
                vec![ContentBlock::Text {
                    text: "wrapped up".into(),
                }],
                StopReason::Stop,
            ),
        ],
        Box::new(AllowGate),
    );
    engine.set_goal("set", "make the suite green", None);
    let mut sink = NoteSink(vec![]);

    engine.run_turn("start", &mut sink).await.unwrap();
    let cont = engine
        .post_turn_check(&mut sink)
        .await
        .expect("an active goal wants another turn");
    assert!(cont.contains("make the suite green"), "{cont}");

    engine.run_turn(&cont, &mut sink).await.unwrap();
    assert!(engine.session.goal().unwrap().completed);
    assert!(engine.post_turn_check(&mut sink).await.is_none());
    assert!(
        sink.0.iter().any(|n| n.contains("continuing toward goal")),
        "notes: {:?}",
        sink.0
    );
}

#[tokio::test]
async fn goal_budget_pauses_the_loop() {
    let tmp = tempfile::tempdir().unwrap();
    let mut engine = engine_in(
        tmp.path(),
        vec![turn(
            vec![ContentBlock::Text { text: "one".into() }],
            StopReason::Stop,
        )],
        Box::new(AllowGate),
    );
    engine.set_goal("set", "endless", Some(0));
    let mut sink = NoteSink(vec![]);
    engine.run_turn("start", &mut sink).await.unwrap();

    assert!(engine.post_turn_check(&mut sink).await.is_none());
    let g = engine.session.goal().unwrap();
    assert!(g.paused && !g.completed);
    assert!(
        sink.0.iter().any(|n| n.contains("token budget")),
        "notes: {:?}",
        sink.0
    );
}

#[tokio::test]
async fn autonomous_gate_continues_on_failure_and_stops_on_pass() {
    let tmp = tempfile::tempdir().unwrap();
    let mut engine = engine_in(
        tmp.path(),
        vec![
            turn(
                vec![ContentBlock::Text {
                    text: "attempt".into(),
                }],
                StopReason::Stop,
            ),
            turn(
                vec![ContentBlock::Text {
                    text: "attempt 2".into(),
                }],
                StopReason::Stop,
            ),
        ],
        Box::new(AllowGate),
    );
    let mut sink = NoteSink(vec![]);

    // failing gate → the failure output becomes the continuation
    engine.set_autonomous(true, None, None, Some("echo gate-detail && exit 1"));
    engine.run_turn("go", &mut sink).await.unwrap();
    let cont = engine
        .post_turn_check(&mut sink)
        .await
        .expect("a failing gate wants another turn");
    assert!(cont.contains("gate-detail"), "{cont}");
    assert!(engine.session.autonomous().unwrap().on);

    // passing gate → autonomous mode turns itself off
    engine.set_autonomous(true, None, None, Some("exit 0"));
    engine.run_turn("go", &mut sink).await.unwrap();
    assert!(engine.post_turn_check(&mut sink).await.is_none());
    assert!(!engine.session.autonomous().unwrap().on);
    assert!(
        sink.0.iter().any(|n| n.contains("gate passed")),
        "notes: {:?}",
        sink.0
    );
}

/// A tool that pushes a child-exit notice while it runs, standing in for a
/// child finishing mid-turn.
struct NotifyTool {
    inbox: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl Tool for NotifyTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "notify".into(),
            description: "push a notice".into(),
            input_schema: json!({"type": "object", "properties": {}}),
        }
    }

    async fn execute(&self, _args: serde_json::Value, _ctx: &mut ToolCtx<'_>) -> ToolOutput {
        self.inbox
            .lock()
            .unwrap()
            .push("[child sa-001-kid (kid) finished]\n42".into());
        ToolOutput::text("noted")
    }
}

#[tokio::test]
async fn a_notice_arriving_mid_turn_reaches_the_very_next_sample() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = Workspace::resolve(tmp.path()).unwrap();
    let mut engine = Engine::new(
        ws,
        SessionManager::in_memory(&Workspace::resolve(tmp.path()).unwrap()),
        Box::new(MockProvider::new(vec![
            turn(
                vec![ContentBlock::ToolCall {
                    id: "c1".into(),
                    name: "notify".into(),
                    arguments: json!({}),
                }],
                StopReason::ToolUse,
            ),
            turn(
                vec![ContentBlock::Text {
                    text: "the child said 42".into(),
                }],
                StopReason::Stop,
            ),
        ])),
        Vec::new(),
        Box::new(AllowGate),
        "mock".into(),
        1024,
    )
    .unwrap();
    // Bind the tool to the engine's own inbox, which only exists post-construction.
    let inbox = engine.notice_inbox();
    engine.tools_mut().push(Box::new(NotifyTool { inbox }));

    let mut sink = NoteSink(vec![]);
    engine.run_turn("spawn something", &mut sink).await.unwrap();

    // Ordering proves delivery happened between the two samples rather than at
    // the end of the turn: the notice precedes the text that used it.
    let msgs = engine.session.context_messages();
    let notice_at = msgs
        .iter()
        .position(
            |m| matches!(m, AgentMessage::User { content, .. } if content.contains("sa-001-kid")),
        )
        .expect("notice delivered during the turn");
    let final_at = msgs
        .iter()
        .position(|m| matches!(m, AgentMessage::Assistant { content, .. }
            if content.iter().any(|b| matches!(b, ContentBlock::Text { text } if text.contains("the child said")))))
        .expect("final assistant text present");
    assert!(
        notice_at < final_at,
        "notice must precede the sample that used it"
    );
}

fn tool_results(engine: &Engine) -> Vec<String> {
    engine
        .session
        .context_messages()
        .iter()
        .filter_map(|m| match m {
            AgentMessage::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
        .collect()
}

fn subagent_script() -> Vec<harness_core::AssistantTurn> {
    vec![
        turn(
            vec![ContentBlock::ToolCall {
                id: "c1".into(),
                name: "subagent".into(),
                arguments: json!({"task": "count the lines in notes.txt"}),
            }],
            StopReason::ToolUse,
        ),
        turn(
            vec![ContentBlock::Text { text: "ok".into() }],
            StopReason::Stop,
        ),
    ]
}

#[tokio::test]
async fn sync_subagent_returns_the_child_report_inline() {
    let tmp = tempfile::tempdir().unwrap();
    let mut engine = engine_in(tmp.path(), subagent_script(), Box::new(AllowGate));
    engine.set_bus(Arc::new(MockBus::with_child(child(
        ChildStatus::Completed,
        "notes.txt has 5 lines",
    ))));

    let mut sink = NoteSink(vec![]);
    engine.run_turn("delegate this", &mut sink).await.unwrap();

    let results = tool_results(&engine);
    assert_eq!(results.len(), 1);
    assert!(results[0].contains("has 5 lines"), "{}", results[0]);
    assert!(
        !results[0].contains("still running"),
        "finished children are not reported as pending: {}",
        results[0]
    );
}

#[tokio::test]
async fn sync_subagent_gives_up_and_points_at_the_notice() {
    let tmp = tempfile::tempdir().unwrap();
    let mut engine = engine_in(tmp.path(), subagent_script(), Box::new(AllowGate));
    // A child that never finishes, with a wait short enough to test.
    engine.tools_mut().retain(|t| {
        t.spec().name != harness_core::engine::GOAL_COMPLETE_TOOL && t.spec().name != "subagent"
    });
    engine
        .tools_mut()
        .push(Box::new(harness_tools::SubagentTool {
            wait: std::time::Duration::from_millis(80),
        }));
    engine.set_bus(Arc::new(MockBus::with_child(child(
        ChildStatus::Running,
        "",
    ))));

    let mut sink = NoteSink(vec![]);
    engine.run_turn("delegate this", &mut sink).await.unwrap();

    let results = tool_results(&engine);
    assert!(results[0].contains("still running"), "{}", results[0]);
    assert!(results[0].contains("notice"), "{}", results[0]);
}

/// Read-only tool that takes a while, to observe concurrency.
struct SlowRead;

#[async_trait::async_trait]
impl Tool for SlowRead {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "slow_read".into(),
            description: "d".into(),
            input_schema: json!({"type": "object"}),
        }
    }
    fn concurrency_safe(&self) -> bool {
        true
    }
    async fn execute(&self, args: serde_json::Value, _ctx: &mut ToolCtx<'_>) -> ToolOutput {
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        ToolOutput::text(format!("read {}", args["n"]))
    }
}

fn call(id: &str, name: &str, arguments: serde_json::Value) -> ContentBlock {
    ContentBlock::ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
    }
}

fn results_of(engine: &Engine) -> Vec<(String, String, bool)> {
    engine
        .session
        .context_messages()
        .into_iter()
        .filter_map(|m| match m {
            AgentMessage::ToolResult {
                tool_call_id,
                content,
                is_error,
                ..
            } => Some((tool_call_id, content, is_error)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn read_only_calls_in_one_turn_run_concurrently_and_answer_in_order() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = Workspace::resolve(tmp.path()).unwrap();
    let session = SessionManager::in_memory(&ws);
    let script = vec![
        turn(
            vec![
                call("a", "slow_read", json!({"n": 1})),
                call("b", "slow_read", json!({"n": 2})),
                call("c", "slow_read", json!({"n": 3})),
            ],
            StopReason::ToolUse,
        ),
        turn(
            vec![ContentBlock::Text {
                text: "done".into(),
            }],
            StopReason::Stop,
        ),
    ];
    let mut engine = Engine::new(
        ws,
        session,
        Box::new(MockProvider::new(script)),
        vec![Box::new(SlowRead)],
        Box::new(AllowGate),
        "mock".into(),
        1024,
    )
    .unwrap();
    let t = std::time::Instant::now();
    let stats = engine.run_turn("go", &mut NoteSink(vec![])).await.unwrap();
    assert!(
        t.elapsed() < std::time::Duration::from_millis(1000),
        "three 400 ms reads ran concurrently, took {:?}",
        t.elapsed()
    );
    assert_eq!(stats.tool_calls, 3);
    let results = results_of(&engine);
    let ids: Vec<&str> = results.iter().map(|r| r.0.as_str()).collect();
    assert_eq!(ids, vec!["a", "b", "c"], "results keep call order");
    assert_eq!(results[2].1, "read 3");
}

#[tokio::test]
async fn cancel_interrupts_a_running_command_and_answers_every_call() {
    let tmp = tempfile::tempdir().unwrap();
    let mut engine = engine_in(
        tmp.path(),
        vec![turn(
            vec![
                call("a", "bash", json!({"command": "sleep 30"})),
                call("b", "bash", json!({"command": "echo never"})),
            ],
            StopReason::ToolUse,
        )],
        Box::new(AllowGate),
    );
    let canceller = engine.canceller();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        canceller.cancel();
    });
    let t = std::time::Instant::now();
    let stats = engine.run_turn("go", &mut NoteSink(vec![])).await.unwrap();
    assert!(stats.aborted);
    assert!(
        t.elapsed() < std::time::Duration::from_secs(5),
        "{:?}",
        t.elapsed()
    );
    let results = results_of(&engine);
    assert_eq!(results.len(), 2, "every tool_use is answered: {results:?}");
    assert!(results[0].1.contains("interrupted") && results[0].2);
    assert!(results[1].1.contains("not run") && results[1].2);
}

#[tokio::test]
async fn truncated_tool_input_is_refused_and_the_turn_continues() {
    let tmp = tempfile::tempdir().unwrap();
    let mut engine = engine_in(
        tmp.path(),
        vec![
            turn(
                vec![call(
                    "a",
                    "write",
                    json!({ harness_core::INVALID_INPUT_KEY: "{\"path\":\"x\",\"content\":\"hal" }),
                )],
                StopReason::Length,
            ),
            turn(
                vec![ContentBlock::Text {
                    text: "retrying".into(),
                }],
                StopReason::Stop,
            ),
        ],
        Box::new(AllowGate),
    );
    let stats = engine.run_turn("go", &mut NoteSink(vec![])).await.unwrap();
    assert_eq!(
        stats.samples, 2,
        "a max_tokens stop with tool calls gets another sample"
    );
    let results = results_of(&engine);
    assert!(results[0].2 && results[0].1.contains("not valid JSON"));
    assert!(!tmp.path().join("x").exists());
}

#[tokio::test]
async fn hooks_block_calls_add_feedback_and_send_the_model_back() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.txt"), "x").unwrap();
    let mut engine = engine_in(
        tmp.path(),
        vec![
            turn(
                vec![
                    call("a", "bash", json!({"command": "rm -rf important"})),
                    call("b", "read", json!({"path": "a.txt"})),
                ],
                StopReason::ToolUse,
            ),
            turn(
                vec![ContentBlock::Text {
                    text: "done".into(),
                }],
                StopReason::Stop,
            ),
            turn(
                vec![ContentBlock::Text {
                    text: "fixed".into(),
                }],
                StopReason::Stop,
            ),
        ],
        Box::new(AllowGate),
    );
    let file: harness_core::hooks::HookFile = serde_json::from_value(json!({
        "pre_tool": [{"match": "bash", "command": "grep -q rm && { echo 'rm is not allowed' >&2; exit 2; }; exit 0"}],
        "post_tool": [{"match": "read", "command": "echo 'remember the style guide' >&2; exit 2"}],
        // Objects once: the marker file makes the second stop pass.
        "stop": [{"command": "test -f stopped && exit 0; touch stopped; echo 'tests fail' >&2; exit 2"}]
    }))
    .unwrap();
    engine.set_hooks(harness_core::hooks::Hooks::from_file(tmp.path(), file));
    let stats = engine.run_turn("go", &mut NoteSink(vec![])).await.unwrap();
    assert_eq!(stats.samples, 3, "the stop hook bought one more sample");
    let results = results_of(&engine);
    assert!(results[0].2 && results[0].1.contains("rm is not allowed"));
    assert!(
        results[1]
            .1
            .contains("[hook feedback]\nremember the style guide")
    );
    let users: Vec<String> = engine
        .session
        .context_messages()
        .into_iter()
        .filter_map(|m| match m {
            AgentMessage::User { content, .. } => Some(content),
            _ => None,
        })
        .collect();
    assert!(users.iter().any(|u| u.contains("tests fail")), "{users:?}");
}

#[tokio::test]
async fn a_prompt_template_expands_before_it_reaches_the_model() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".harness/commands")).unwrap();
    std::fs::write(
        tmp.path().join(".harness/commands/greet.md"),
        "---\ndescription: say hi\n---\nSay hello to $ARGUMENTS.",
    )
    .unwrap();
    let mut engine = engine_in(
        tmp.path(),
        vec![turn(
            vec![ContentBlock::Text { text: "hi".into() }],
            StopReason::Stop,
        )],
        Box::new(AllowGate),
    );
    engine
        .run_turn("/greet the team", &mut NoteSink(vec![]))
        .await
        .unwrap();
    let first = engine
        .session
        .context_messages()
        .into_iter()
        .next()
        .unwrap();
    assert!(
        matches!(&first, AgentMessage::User { content, .. } if content == "Say hello to the team."),
        "{first:?}"
    );
}
