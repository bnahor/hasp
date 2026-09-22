//! LocalBus integration: real engines, echo provider, git worktrees.

use harness_bus::{GateFactory, LocalBus, ProviderFactory};
use harness_core::{Bus, ChildStatus, Gate, GateDecision, Provider, SpawnRequest};
use harness_provider::echo::EchoProvider;
use harness_workspace::Workspace;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct AllowGate;

impl Gate for AllowGate {
    fn check_bash(&mut self, _: &str) -> GateDecision {
        GateDecision::Allow
    }
    fn check_write(&mut self, _: &str) -> GateDecision {
        GateDecision::Allow
    }
}

fn git_repo(dir: &std::path::Path) {
    for args in [
        vec!["init", "--quiet"],
        vec!["config", "user.email", "t@t"],
        vec!["config", "user.name", "t"],
    ] {
        std::process::Command::new("git")
            .args(&args)
            .current_dir(dir)
            .status()
            .expect("git init commands");
    }
    std::fs::write(dir.join("a.txt"), "x").unwrap();
    std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(dir)
        .status()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "--quiet", "-m", "init"])
        .current_dir(dir)
        .status()
        .unwrap();
}

async fn wait_completed(bus: &LocalBus, child_id: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let list = bus.list_children().await;
        if let Some(c) = list.iter().find(|c| c.child_id == child_id)
            && c.status != ChildStatus::Running
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "child {child_id} never completed"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn notices(inbox: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    inbox.lock().unwrap().clone()
}

#[tokio::test]
async fn spawn_completes_message_triggers_worktree_isolates() {
    let tmp = tempfile::tempdir().unwrap();
    git_repo(tmp.path());
    let ws = Workspace::resolve(tmp.path()).unwrap();

    let factory: ProviderFactory =
        Arc::new(|| Ok(Box::new(EchoProvider::from_env()) as Box<dyn Provider>));
    let gate_factory: GateFactory = Arc::new(|| Box::new(AllowGate) as Box<dyn Gate>);
    let parent_inbox: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let bus = LocalBus::new(
        ws,
        "mock".into(),
        1024,
        factory,
        gate_factory,
        parent_inbox.clone(),
        None,
    );

    // spawn → completes → parent notice carries the child's final text
    // (status stamps before the notice lands, so poll the notice itself)
    let h = bus
        .spawn_child(SpawnRequest {
            task: "say hi".into(),
            name: Some("greeter".into()),
            mutation: false,
        })
        .await
        .unwrap();
    assert!(h.child_id.starts_with("sa-001-"));
    let finished_marker = format!("[child {} (greeter) finished]", h.child_id);
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let n = notices(&parent_inbox);
        if n.iter()
            .any(|s| s.contains(&finished_marker) && s.contains("echo: say hi"))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "finished notice never arrived: {n:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    wait_completed(&bus, &h.child_id).await;

    // message with trigger on the idle child → new turn → replied notice
    let receipt = bus
        .message_child(&h.child_id, "status?", true)
        .await
        .unwrap();
    assert_eq!(receipt.label(), "delivered");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if notices(&parent_inbox)
            .iter()
            .any(|s| s.contains("replied") && s.contains("echo: status?"))
        {
            break;
        }
        assert!(Instant::now() < deadline, "reply notice never arrived");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // mutating child lands in a managed worktree
    let h2 = bus
        .spawn_child(SpawnRequest {
            task: "edit things".into(),
            name: None,
            mutation: true,
        })
        .await
        .unwrap();
    assert!(
        h2.root.contains(".harness/worktrees/sa-002-"),
        "mutation root: {}",
        h2.root
    );
    wait_completed(&bus, &h2.child_id).await;

    // roster reflects both children (HashMap iteration order is arbitrary —
    // look up by id)
    let list = bus.list_children().await;
    assert_eq!(list.len(), 2);
    let greeter = list.iter().find(|c| c.child_id == h.child_id).unwrap();
    assert_eq!(greeter.name, "greeter");
    // Polling the roster answers "what did it say?" — otherwise a parent has to
    // wait for the notice, and models fill that gap by hunting the filesystem.
    // It holds the child's most recent report, so a follow-up turn supersedes
    // the first one.
    let report = greeter
        .report
        .as_deref()
        .expect("completed child keeps a report");
    assert!(report.contains("echo: status?"), "report: {report}");
}
