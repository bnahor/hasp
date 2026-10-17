//! The JSONL journal is canonical: a failed append must never be silently
//! dropped while the engine carries on, and a crash mid-append must not make
//! the session unopenable (nor be "repaired" by rewriting valid entries).

use hasp_core::engine::{Engine, TurnSink};
use hasp_core::session::SessionManager;
use hasp_core::{AgentMessage, ContentBlock, Gate, GateDecision, Sink, StopReason, Usage};
use hasp_provider::mock::MockProvider;
use hasp_workspace::Workspace;
use serde_json::json;

struct Notes(Vec<String>);
impl Sink for Notes {
    fn on_delta(&mut self, _: &str) {}
}
impl TurnSink for Notes {
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

/// A real on-disk journal in `dir`, opened like a resumed session.
fn journal(ws: &Workspace, dir: &std::path::Path) -> (SessionManager, std::path::PathBuf) {
    let header = SessionManager::in_memory(ws).header;
    let path = dir.join("s.jsonl");
    std::fs::write(&path, format!("{}\n", serde_json::to_string(&header).unwrap())).unwrap();
    (SessionManager::open(&path).unwrap(), path)
}

fn turn(blocks: Vec<ContentBlock>, stop: StopReason) -> hasp_core::AssistantTurn {
    hasp_core::AssistantTurn { blocks, usage: Usage::default(), stop_reason: stop }
}

fn write_marker_call() -> hasp_core::AssistantTurn {
    turn(
        vec![ContentBlock::ToolCall {
            id: "c1".into(),
            name: "write".into(),
            arguments: json!({"path": "marker.txt", "content": "side effect"}),
        }],
        StopReason::ToolUse,
    )
}

#[test]
fn failed_append_faults_without_touching_history() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = Workspace::resolve(tmp.path()).unwrap();
    let (mut s, path) = journal(&ws, tmp.path());
    s.append_message(AgentMessage::user("kept"));
    s.fail_appends_after(0);
    s.append_message(AgentMessage::user("lost"));
    assert!(s.fault().is_some());
    assert!(s.ensure_durable().is_err());
    assert_eq!(s.entries().len(), 1, "the failed entry never joins history");
    // latched: later appends are refused too, even if the disk recovered
    s.append_message(AgentMessage::user("after"));
    assert_eq!(s.entries().len(), 1);
    // the half-written line was cut off: the file reopens cleanly with only "kept"
    let again = SessionManager::open(&path).unwrap();
    assert_eq!(again.entries().len(), 1);
    assert!(again.recovered_tail.is_none());
}

#[test]
fn torn_final_record_is_set_aside_and_valid_entries_kept() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = Workspace::resolve(tmp.path()).unwrap();
    let (mut s, path) = journal(&ws, tmp.path());
    s.append_message(AgentMessage::user("one"));
    s.append_message(AgentMessage::user("two"));
    drop(s);
    let valid = std::fs::read(&path).unwrap();
    // crash mid-append: half a record, no newline
    let mut torn = valid.clone();
    torn.extend_from_slice(br#"{"type":"message","id":"x","parent"#);
    std::fs::write(&path, &torn).unwrap();

    let mut s = SessionManager::open(&path).unwrap();
    let tail = s.recovered_tail.clone().expect("torn tail reported");
    assert_eq!(tail.offset as usize, valid.len());
    assert_eq!(std::fs::read(&tail.sidecar).unwrap(), &torn[valid.len()..]);
    assert_eq!(std::fs::read(&path).unwrap(), valid, "valid prefix byte-identical");
    assert_eq!(s.entries().len(), 2);
    // and the session keeps working after the restart
    s.append_message(AgentMessage::user("three"));
    drop(s);
    let reopened = SessionManager::open(&path).unwrap();
    assert_eq!(reopened.entries().len(), 3);
    assert!(reopened.recovered_tail.is_none());
}

#[test]
fn complete_last_entry_missing_its_newline_is_kept() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = Workspace::resolve(tmp.path()).unwrap();
    let (mut s, path) = journal(&ws, tmp.path());
    s.append_message(AgentMessage::user("one"));
    drop(s);
    let mut raw = std::fs::read_to_string(&path).unwrap();
    raw.pop(); // drop the trailing newline only
    std::fs::write(&path, &raw).unwrap();
    let mut s = SessionManager::open(&path).unwrap();
    assert_eq!(s.entries().len(), 1);
    assert!(s.recovered_tail.is_none());
    s.append_message(AgentMessage::user("two"));
    drop(s);
    assert_eq!(SessionManager::open(&path).unwrap().entries().len(), 2);
}

#[test]
fn mid_file_corruption_is_an_error_not_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = Workspace::resolve(tmp.path()).unwrap();
    let (mut s, path) = journal(&ws, tmp.path());
    s.append_message(AgentMessage::user("one"));
    s.append_message(AgentMessage::user("two"));
    drop(s);
    let raw = std::fs::read_to_string(&path).unwrap();
    let mut lines: Vec<&str> = raw.lines().collect();
    lines[1] = "{not json";
    let broken = lines.join("\n") + "\n";
    std::fs::write(&path, &broken).unwrap();
    let err = SessionManager::open(&path).err().expect("refused");
    assert!(err.to_string().contains("line 2"), "{err}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), broken, "nothing rewritten");
}

#[tokio::test]
async fn engine_makes_no_request_when_the_prompt_cannot_be_journaled() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = Workspace::resolve(tmp.path()).unwrap();
    let (mut s, _) = journal(&ws, tmp.path());
    s.fail_appends_after(0);
    let mut engine = Engine::new(ws, s, Box::new(MockProvider::new(vec![write_marker_call()])),
                                 hasp_tools::builtins(), Box::new(AllowGate), "mock".into(), 1024).unwrap();
    let mut notes = Notes(vec![]);
    let err = engine.run_turn("go", &mut notes).await.err().expect("turn stops");
    assert!(err.to_string().contains("not durable"), "{err}");
    assert!(!tmp.path().join("marker.txt").exists());
}

#[tokio::test]
async fn engine_runs_no_tool_whose_call_was_not_journaled() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = Workspace::resolve(tmp.path()).unwrap();
    let (mut s, path) = journal(&ws, tmp.path());
    s.fail_appends_after(1); // the prompt is written; the assistant's tool call is not
    let mut engine = Engine::new(ws, s, Box::new(MockProvider::new(vec![write_marker_call()])),
                                 hasp_tools::builtins(), Box::new(AllowGate), "mock".into(), 1024).unwrap();
    let mut notes = Notes(vec![]);
    assert!(engine.run_turn("write the marker", &mut notes).await.is_err());
    assert!(!tmp.path().join("marker.txt").exists(), "tool must not run");
    assert!(notes.0.iter().any(|n| n.contains("stopping")), "{:?}", notes.0);
    // what's on disk is exactly what's in memory: the prompt only
    assert_eq!(SessionManager::open(&path).unwrap().entries().len(), 1);
    assert_eq!(engine.session.entries().len(), 1);
}
