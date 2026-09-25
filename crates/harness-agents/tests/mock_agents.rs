//! The adapters against scripted agents (tests/mocks/*.py), so the protocol
//! handling is covered without any vendor CLI installed or logged in.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use harness_agents::{Cancel, Decision, Event, Opts, Protocol, Spec};

fn mock(name: &str, protocol: Protocol) -> Spec {
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/mocks")
        .join(name);
    Spec {
        name: "mock".into(),
        command: "python3".into(),
        args: vec![script.display().to_string()],
        protocol,
        login: String::new(),
        plan: String::new(),
    }
}

/// Opts whose asker answers `answer` and records the questions.
fn opts(answer: Decision) -> (Opts, Arc<Mutex<Vec<String>>>) {
    let asked = Arc::new(Mutex::new(Vec::new()));
    let log = asked.clone();
    let opts = Opts {
        cwd: std::env::temp_dir(),
        model: None,
        yolo: false,
        resume: None,
        ask: Arc::new(move |q| {
            log.lock().unwrap().push(q);
            answer
        }),
    };
    (opts, asked)
}

async fn run(
    spec: &Spec,
    answer: Decision,
    prompt: &str,
) -> (harness_agents::TurnEnd, Vec<Event>, Vec<String>) {
    let (opts, asked) = opts(answer);
    let mut agent = harness_agents::start(spec, opts).await.unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let end = agent.prompt(prompt, &tx, &Cancel::default()).await.unwrap();
    drop(tx);
    let mut events = Vec::new();
    while let Some(e) = rx.recv().await {
        events.push(e);
    }
    let asked = asked.lock().unwrap().clone();
    (end, events, asked)
}

fn text(events: &[Event]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect()
}

fn tools(events: &[Event]) -> Vec<&harness_agents::ToolCall> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Tool(t) => Some(t),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn acp_streams_asks_and_obeys_the_answer() {
    let spec = mock("acp_agent.py", Protocol::Acp);
    let (end, events, asked) = run(&spec, Decision::Once, "go").await;
    assert_eq!(asked.len(), 1);
    assert!(asked[0].contains("rm -rf build"), "{asked:?}");
    assert_eq!(text(&events), "done: completed");
    assert!(events.contains(&Event::Thinking("hmm".into())));
    let t = tools(&events);
    assert_eq!(
        (t[0].name.as_str(), t[0].is_error, t[0].output.as_str()),
        ("execute", false, "chose yes")
    );
    assert_eq!(
        (end.tool_calls, end.usage.input, end.cancelled),
        (1, 5, false)
    );

    let (_, events, _) = run(&spec, Decision::Deny, "go").await;
    assert_eq!(text(&events), "done: failed");
    assert!(tools(&events)[0].is_error);
}

#[tokio::test(flavor = "multi_thread")]
async fn claude_streams_asks_and_obeys_the_answer() {
    let spec = mock("claude_agent.py", Protocol::Claude);
    let (end, events, asked) = run(&spec, Decision::Once, "go").await;
    assert!(asked[0].contains("Bash: make test"), "{asked:?}");
    assert_eq!(text(&events), "working\n\nfinished");
    let t = tools(&events);
    assert_eq!(
        (
            t[0].name.as_str(),
            t[0].subject.as_str(),
            t[0].output.as_str()
        ),
        ("Bash", "make test", "ok")
    );
    assert_eq!(
        (
            end.usage.input,
            end.usage.cache_read,
            end.usage.cache_write,
            end.error
        ),
        (3, 10, 1, None)
    );

    let (_, events, _) = run(&spec, Decision::Deny, "go").await;
    assert!(tools(&events)[0].is_error);
}

#[tokio::test(flavor = "multi_thread")]
async fn codex_streams_asks_and_obeys_the_answer() {
    let spec = mock("codex_agent.py", Protocol::Codex);
    let (end, events, asked) = run(&spec, Decision::Once, "go").await;
    assert!(asked[0].contains("`cargo test`"), "{asked:?}");
    assert_eq!(text(&events), "on it\n\nall green");
    let t = tools(&events);
    assert_eq!(
        (
            t[0].name.as_str(),
            t[0].subject.as_str(),
            t[0].output.as_str(),
            t[0].ms
        ),
        ("bash", "cargo test", "passed", 7)
    );
    // 100 input, 40 of them cached.
    assert_eq!(
        (end.usage.input, end.usage.cache_read, end.usage.output),
        (60, 40, 9)
    );

    let (_, events, _) = run(&spec, Decision::Deny, "go").await;
    assert!(tools(&events)[0].is_error);
    assert!(text(&events).ends_with("skipped"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_codex_thread_that_cannot_resume_starts_fresh() {
    let (mut opts, _) = opts(Decision::Once);
    opts.resume = Some("gone".into());
    let agent = harness_agents::start(&mock("codex_agent.py", Protocol::Codex), opts)
        .await
        .unwrap();
    assert_eq!(agent.session_id().as_deref(), Some("th-1"));
    assert_eq!(agent.label(), "codex · mock-5");
}

#[tokio::test(flavor = "multi_thread")]
async fn always_is_remembered_for_the_session() {
    let spec = mock("claude_agent.py", Protocol::Claude);
    let (opts, asked) = opts(Decision::Always);
    let mut agent = harness_agents::start(&spec, opts).await.unwrap();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    agent.prompt("go", &tx, &Cancel::default()).await.unwrap();
    agent.prompt("go", &tx, &Cancel::default()).await.unwrap();
    assert_eq!(
        asked.lock().unwrap().len(),
        1,
        "asked once, then remembered"
    );
    assert_eq!(agent.session_id().as_deref(), Some("sess-9"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancel_ends_the_turn_through_the_protocol() {
    for (script, protocol) in [
        ("acp_agent.py", Protocol::Acp),
        ("claude_agent.py", Protocol::Claude),
        ("codex_agent.py", Protocol::Codex),
    ] {
        let (opts, _) = opts(Decision::Once);
        let mut agent = harness_agents::start(&mock(script, protocol), opts)
            .await
            .unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = Cancel::default();
        let c = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            c.cancel();
        });
        let end = tokio::time::timeout(Duration::from_secs(10), agent.prompt("hang", &tx, &cancel))
            .await
            .expect("the turn ended")
            .unwrap();
        assert!(end.cancelled, "{script}");
        assert_eq!(end.error, None, "{script}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_command_says_so() {
    let mut spec = mock("acp_agent.py", Protocol::Acp);
    spec.command = "definitely-not-an-agent".into();
    let (opts, _) = opts(Decision::Once);
    let err = harness_agents::start(&spec, opts)
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(err.contains("is it installed"), "{err}");
}
