//! Daemon e2e against the real binary over a real Unix socket.

use hasp_wire::{
    METHOD_HEARTBEAT_LIST, METHOD_HEARTBEAT_SET, METHOD_INITIALIZE, METHOD_SESSION_CONTINUE,
    METHOD_SESSION_NEW, METHOD_SESSION_PROMPT, METHOD_SESSION_STATS, METHOD_SHUTDOWN, Wire,
    WireClient, decode, encode,
};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

async fn spawn_daemon(dir: &std::path::Path, extra_env: &[(&str, &str)]) -> tokio::process::Child {
    let sock = dir.join("d.sock");
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_hasp"));
    cmd.arg("--serve")
        .arg("--provider")
        .arg("mock")
        .env("HASP_SOCKET", &sock)
        .env("HOME", dir)
        .env("HASP_PROVIDER", "mock")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let child = cmd.spawn().expect("spawn harness --serve");
    for _ in 0..400 {
        if sock.exists() {
            return child;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("daemon socket never appeared at {sock:?}");
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

#[tokio::test]
async fn daemon_lifecycle_over_real_socket() {
    let tmp = tempfile::tempdir().unwrap();
    let mut child = spawn_daemon(tmp.path(), &[]).await;
    let sock = tmp.path().join("d.sock");

    let mut client = WireClient::connect(&sock).await.unwrap();
    client.initialize().await.unwrap();
    let session = client
        .request(
            METHOD_SESSION_CONTINUE,
            serde_json::json!({"cwd": tmp.path().to_string_lossy()}),
        )
        .await
        .unwrap();
    assert!(session["sessionId"].as_str().is_some());
    assert!(session["path"].as_str().unwrap().ends_with(".jsonl"));

    let turn = client
        .request(
            METHOD_SESSION_PROMPT,
            serde_json::json!({"prompt": "hello"}),
        )
        .await
        .unwrap();
    assert_eq!(turn["text"].as_str().unwrap(), "echo: hello");
    assert_eq!(turn["samples"].as_u64().unwrap(), 1);
    assert!(!turn["aborted"].as_bool().unwrap());

    let stats = client
        .request(METHOD_SESSION_STATS, serde_json::json!({}))
        .await
        .unwrap();
    assert!(stats["entries"].as_u64().unwrap() >= 2);
    assert!(stats["estTokens"].as_u64().unwrap() > 0);

    client.shutdown().await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .expect("daemon exits after shutdown")
        .unwrap();
}

#[tokio::test]
async fn command_surface_over_the_wire() {
    let tmp = tempfile::tempdir().unwrap();
    git_repo(tmp.path());
    let mut child = spawn_daemon(tmp.path(), &[]).await;
    let sock = tmp.path().join("d.sock");

    let mut c = WireClient::connect(&sock).await.unwrap();
    c.initialize().await.unwrap();
    let s = c
        .request(
            METHOD_SESSION_CONTINUE,
            serde_json::json!({"cwd": tmp.path().to_string_lossy()}),
        )
        .await
        .unwrap();
    let sid = s["sessionId"].as_str().unwrap().to_string();

    // prompt + branch points + move
    let t = c
        .request(METHOD_SESSION_PROMPT, serde_json::json!({"prompt": "one"}))
        .await
        .unwrap();
    assert_eq!(t["text"].as_str().unwrap(), "echo: one");
    let points = c
        .request(hasp_wire::METHOD_SESSION_BRANCH, serde_json::json!({}))
        .await
        .unwrap();
    let point_id = points[0]["id"].as_str().unwrap().to_string();
    let moved = c
        .request(
            hasp_wire::METHOD_SESSION_BRANCH,
            serde_json::json!({"target": point_id}),
        )
        .await
        .unwrap();
    assert!(moved["moved"].as_bool().unwrap());

    // fork
    let fork = c
        .request(hasp_wire::METHOD_SESSION_FORK, serde_json::json!({}))
        .await
        .unwrap();
    assert_ne!(fork["sessionId"].as_str().unwrap(), sid);

    // listings (fork + original)
    let listings = c
        .request(hasp_wire::METHOD_SESSION_LISTINGS, serde_json::json!({}))
        .await
        .unwrap();
    assert!(
        listings.as_array().unwrap().len() >= 2,
        "fork listed: {listings}"
    );

    // model/set validation + success
    let bad = c
        .request(
            hasp_wire::METHOD_MODEL_SET,
            serde_json::json!({"model": "nope"}),
        )
        .await;
    assert!(bad.is_err(), "unknown model rejected");
    let good = c
        .request(
            hasp_wire::METHOD_MODEL_SET,
            serde_json::json!({"model": "claude-haiku-4-5"}),
        )
        .await
        .unwrap();
    assert_eq!(good["model"].as_str().unwrap(), "claude-haiku-4-5");
    let stats = c
        .request(METHOD_SESSION_STATS, serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(stats["model"].as_str().unwrap(), "claude-haiku-4-5");
    assert!(stats["window"].as_u64().unwrap() > 0);

    // compaction: feed enough history to exceed the tail budget, then verify
    // the method contract. (The echo provider's "summary" is the echoed
    // transcript itself, so actual token shrinkage is asserted in the engine
    // unit tests with a tiny budget — not here.)
    let big = "x".repeat(120_000);
    let t2 = c
        .request(METHOD_SESSION_PROMPT, serde_json::json!({"prompt": big}))
        .await
        .unwrap();
    assert_eq!(t2["samples"].as_u64().unwrap(), 1);
    let stats = c
        .request(METHOD_SESSION_STATS, serde_json::json!({}))
        .await
        .unwrap();
    let before = stats["estTokens"].as_u64().unwrap();
    let entries_before = stats["entries"].as_u64().unwrap();
    assert!(before > 20_000, "history big enough to compact: {before}");
    let compact = c
        .request(hasp_wire::METHOD_SESSION_COMPACT, serde_json::json!({}))
        .await
        .unwrap();
    assert!(compact["tokensBefore"].as_u64().unwrap() >= before);
    assert!(compact["summaryTokens"].as_u64().unwrap() > 0);
    let after = c
        .request(METHOD_SESSION_STATS, serde_json::json!({}))
        .await
        .unwrap();
    assert!(
        after["entries"].as_u64().unwrap() > entries_before,
        "compaction entry journaled"
    );
    assert!(after["estTokens"].as_u64().unwrap() < before * 2);

    // worktree round-trip (git repo cwd)
    let wt = c
        .request(
            hasp_wire::METHOD_SESSION_WORKTREE,
            serde_json::json!({"name": "w1"}),
        )
        .await
        .unwrap();
    assert!(wt["root"].as_str().unwrap().contains(".hasp/worktrees/w1"));
    assert_ne!(wt["sessionId"].as_str().unwrap(), sid);
    let exit = c
        .request(
            hasp_wire::METHOD_SESSION_WORKTREE_EXIT,
            serde_json::json!({"remove": true}),
        )
        .await
        .unwrap();
    assert!(
        exit["root"]
            .as_str()
            .unwrap()
            .ends_with(tmp.path().file_name().unwrap().to_string_lossy().as_ref())
    );
    assert!(
        !tmp.path()
            .join(".hasp")
            .join("worktrees")
            .join("w1")
            .exists()
    );

    c.shutdown().await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .expect("daemon exits after shutdown")
        .unwrap();
}

/// Prompt then cancel while the sample is in flight (delayed echo), using a
/// raw stream so the two requests interleave.
#[tokio::test]
async fn session_cancel_aborts_in_flight_prompt() {
    let tmp = tempfile::tempdir().unwrap();
    let mut child = spawn_daemon(tmp.path(), &[("HASP_MOCK_DELAY_MS", "10000")]).await;
    let sock = tmp.path().join("d.sock");

    let stream = UnixStream::connect(&sock).await.unwrap();
    let (rd, mut wr) = stream.into_split();
    let mut rd = BufReader::new(rd);

    async fn send(
        wr: &mut tokio::net::unix::OwnedWriteHalf,
        id: u64,
        method: &str,
        params: serde_json::Value,
    ) {
        let line = encode(&Wire::request(id, method, params));
        wr.write_all(line.as_bytes()).await.unwrap();
        wr.flush().await.unwrap();
    }

    async fn wait_for(
        rd: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
        id: u64,
    ) -> serde_json::Value {
        loop {
            let mut line = String::new();
            if rd.read_line(&mut line).await.unwrap() == 0 {
                panic!("daemon closed while waiting for response {id}");
            }
            if let Some(Wire::Response {
                id: rid,
                result,
                error,
            }) = decode(&line)
                && rid == id
            {
                if let Some(e) = error {
                    panic!("response {id} error: {} {}", e.code, e.message);
                }
                return result.unwrap_or(serde_json::Value::Null);
            }
        }
    }

    send(
        &mut wr,
        1,
        METHOD_INITIALIZE,
        serde_json::json!({"client": "t"}),
    )
    .await;
    let _ = wait_for(&mut rd, 1).await;
    send(
        &mut wr,
        2,
        METHOD_SESSION_CONTINUE,
        serde_json::json!({"cwd": tmp.path().to_string_lossy()}),
    )
    .await;
    let _ = wait_for(&mut rd, 2).await;
    send(
        &mut wr,
        3,
        METHOD_SESSION_PROMPT,
        serde_json::json!({"prompt": "slow"}),
    )
    .await;
    send(
        &mut wr,
        4,
        hasp_wire::METHOD_SESSION_CANCEL,
        serde_json::json!({}),
    )
    .await;
    let _ = wait_for(&mut rd, 4).await; // cancel acks immediately
    let turn = wait_for(&mut rd, 3).await; // aborted turn comes back right away
    assert!(
        turn["aborted"].as_bool().unwrap(),
        "turn should be aborted: {turn}"
    );
    assert_eq!(turn["samples"].as_u64().unwrap(), 0);

    send(&mut wr, 5, METHOD_SHUTDOWN, serde_json::json!({})).await;
    let _ = wait_for(&mut rd, 5).await;
    tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .expect("daemon exits after shutdown")
        .unwrap();
}

/// Agent bus over the wire: spawn → child-exit notice on the connection →
/// roster → trigger-message → replied notice. Sequential reads only, so no
/// interleaving races.
#[tokio::test]
async fn agent_flow_over_the_wire() {
    let tmp = tempfile::tempdir().unwrap();
    git_repo(tmp.path());
    let mut child = spawn_daemon(tmp.path(), &[]).await;
    let sock = tmp.path().join("d.sock");

    let stream = UnixStream::connect(&sock).await.unwrap();
    let (rd, mut wr) = stream.into_split();
    let mut rd = BufReader::new(rd);

    async fn send(
        wr: &mut tokio::net::unix::OwnedWriteHalf,
        id: u64,
        method: &str,
        params: serde_json::Value,
    ) {
        let line = encode(&Wire::request(id, method, params));
        wr.write_all(line.as_bytes()).await.unwrap();
        wr.flush().await.unwrap();
    }
    async fn response(
        rd: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
        id: u64,
    ) -> serde_json::Value {
        loop {
            let mut line = String::new();
            tokio::time::timeout(Duration::from_secs(20), rd.read_line(&mut line))
                .await
                .expect("read within timeout")
                .unwrap();
            if let Some(Wire::Response {
                id: rid,
                result,
                error,
            }) = decode(&line)
                && rid == id
            {
                if let Some(e) = error {
                    panic!("response {id} error: {} {}", e.code, e.message);
                }
                return result.unwrap_or(serde_json::Value::Null);
            }
        }
    }
    async fn notice(rd: &mut BufReader<tokio::net::unix::OwnedReadHalf>, needle: &str) -> String {
        loop {
            let mut line = String::new();
            tokio::time::timeout(Duration::from_secs(20), rd.read_line(&mut line))
                .await
                .expect("notice within timeout")
                .unwrap();
            if let Some(Wire::Notification { method, params }) = decode(&line)
                && method == hasp_wire::METHOD_SESSION_UPDATE
                && params["text"].as_str().unwrap_or("").contains(needle)
            {
                return params["text"].as_str().unwrap_or_default().to_string();
            }
        }
    }

    send(
        &mut wr,
        1,
        METHOD_INITIALIZE,
        serde_json::json!({"client": "t"}),
    )
    .await;
    let _ = response(&mut rd, 1).await;
    send(
        &mut wr,
        2,
        METHOD_SESSION_CONTINUE,
        serde_json::json!({"cwd": tmp.path().to_string_lossy()}),
    )
    .await;
    let _ = response(&mut rd, 2).await;

    send(
        &mut wr,
        3,
        hasp_wire::METHOD_AGENT_SPAWN,
        serde_json::json!({"task": "agent task", "name": "wirechild"}),
    )
    .await;
    let spawned = response(&mut rd, 3).await;
    let child_id = spawned["childId"].as_str().unwrap().to_string();
    assert!(child_id.starts_with("sa-001-"));

    // echo children complete ~instantly; the exit note fans out to us
    let finished = notice(&mut rd, "[child").await;
    assert!(finished.contains("finished"), "{finished}");
    assert!(finished.contains("echo: agent task"), "{finished}");

    send(
        &mut wr,
        4,
        hasp_wire::METHOD_AGENT_LIST,
        serde_json::json!({}),
    )
    .await;
    let list = response(&mut rd, 4).await;
    assert_eq!(list[0]["status"].as_str().unwrap(), "completed");
    assert_eq!(list[0]["name"].as_str().unwrap(), "wirechild");
    assert!(
        list[0]["report"]
            .as_str()
            .unwrap_or_default()
            .contains("echo: agent task"),
        "roster carries the child's report: {list}"
    );

    send(
        &mut wr,
        5,
        hasp_wire::METHOD_AGENT_MESSAGE,
        serde_json::json!({"childId": child_id, "text": "follow up", "trigger": true}),
    )
    .await;
    let receipt = response(&mut rd, 5).await;
    assert_eq!(receipt["receipt"].as_str().unwrap(), "delivered");

    let replied = notice(&mut rd, "replied").await;
    assert!(replied.contains("echo: follow up"), "{replied}");

    send(&mut wr, 6, METHOD_SHUTDOWN, serde_json::json!({})).await;
    let _ = response(&mut rd, 6).await;
    tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .expect("daemon exits after shutdown")
        .unwrap();
}

/// Goals and heartbeats over the wire: goal state is journaled and reported,
/// and a heartbeat fires a turn with nobody prompting.
#[tokio::test]
async fn goal_and_heartbeat_over_the_wire() {
    let tmp = tempfile::tempdir().unwrap();
    let mut child = spawn_daemon(tmp.path(), &[]).await;
    let sock = tmp.path().join("d.sock");

    let stream = UnixStream::connect(&sock).await.unwrap();
    let (rd, mut wr) = stream.into_split();
    let mut rd = BufReader::new(rd);

    async fn send(
        wr: &mut tokio::net::unix::OwnedWriteHalf,
        id: u64,
        method: &str,
        params: serde_json::Value,
    ) {
        let line = encode(&Wire::request(id, method, params));
        wr.write_all(line.as_bytes()).await.unwrap();
        wr.flush().await.unwrap();
    }

    async fn response(
        rd: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
        id: u64,
    ) -> serde_json::Value {
        loop {
            let mut line = String::new();
            tokio::time::timeout(Duration::from_secs(20), rd.read_line(&mut line))
                .await
                .expect("response within timeout")
                .unwrap();
            if let Some(Wire::Response {
                id: rid,
                result,
                error,
            }) = decode(&line)
                && rid == id
            {
                if let Some(e) = error {
                    panic!("response {id} error: {} {}", e.code, e.message);
                }
                return result.unwrap_or(serde_json::Value::Null);
            }
        }
    }

    /// Streamed text arrives in token-sized chunks, so match on the
    /// accumulated transcript rather than a single frame.
    async fn notice(rd: &mut BufReader<tokio::net::unix::OwnedReadHalf>, needle: &str) -> String {
        let mut seen = String::new();
        loop {
            let mut line = String::new();
            tokio::time::timeout(Duration::from_secs(20), rd.read_line(&mut line))
                .await
                .expect("update within timeout")
                .unwrap();
            if let Some(Wire::Notification { method, params }) = decode(&line)
                && method == hasp_wire::METHOD_SESSION_UPDATE
            {
                let text = params["text"].as_str().unwrap_or("");
                seen.push_str(text);
                if text.contains(needle) || seen.contains(needle) {
                    return seen;
                }
            }
        }
    }

    send(
        &mut wr,
        1,
        METHOD_INITIALIZE,
        serde_json::json!({"client": "t"}),
    )
    .await;
    let _ = response(&mut rd, 1).await;
    send(
        &mut wr,
        2,
        METHOD_SESSION_CONTINUE,
        serde_json::json!({"cwd": tmp.path().to_string_lossy()}),
    )
    .await;
    let _ = response(&mut rd, 2).await;

    // A goal with an exhausted budget is journaled and reported, but does not
    // hold the prompt open with continuations (loop semantics live in the
    // engine tests, where the provider script is controllable).
    send(
        &mut wr,
        3,
        hasp_wire::METHOD_GOAL_SET,
        serde_json::json!({"action": "set", "text": "ship p4", "budget": 0}),
    )
    .await;
    let set = response(&mut rd, 3).await;
    assert_eq!(set["goal"]["text"].as_str().unwrap(), "ship p4");

    send(
        &mut wr,
        4,
        METHOD_SESSION_PROMPT,
        serde_json::json!({"prompt": "hi"}),
    )
    .await;
    let turn = response(&mut rd, 4).await;
    assert_eq!(turn["text"].as_str().unwrap(), "echo: hi");

    send(
        &mut wr,
        5,
        hasp_wire::METHOD_GOAL_STATUS,
        serde_json::json!({}),
    )
    .await;
    let status = response(&mut rd, 5).await;
    assert!(status["goal"]["paused"].as_bool().unwrap(), "{status}");

    send(
        &mut wr,
        6,
        hasp_wire::METHOD_AUTONOMOUS_SET,
        serde_json::json!({"on": false}),
    )
    .await;
    let auto = response(&mut rd, 6).await;
    assert!(!auto["on"].as_bool().unwrap());

    // heartbeat: fires its own turn with no client prompt
    send(
        &mut wr,
        7,
        hasp_wire::METHOD_HEARTBEAT_SET,
        serde_json::json!({"action": "add", "everyMs": 100, "instruction": "tick"}),
    )
    .await;
    let hb = response(&mut rd, 7).await;
    let hb_id = hb["id"].as_str().unwrap().to_string();

    let fired = notice(&mut rd, "tick").await;
    assert!(fired.contains("tick"), "{fired}");

    send(
        &mut wr,
        8,
        hasp_wire::METHOD_HEARTBEAT_LIST,
        serde_json::json!({}),
    )
    .await;
    let list = response(&mut rd, 8).await;
    assert_eq!(list[0]["id"].as_str().unwrap(), hb_id);

    send(
        &mut wr,
        9,
        hasp_wire::METHOD_HEARTBEAT_SET,
        serde_json::json!({"action": "pause", "id": hb_id}),
    )
    .await;
    let _ = response(&mut rd, 9).await;
    send(
        &mut wr,
        10,
        hasp_wire::METHOD_HEARTBEAT_LIST,
        serde_json::json!({}),
    )
    .await;
    let list = response(&mut rd, 10).await;
    assert!(list[0]["paused"].as_bool().unwrap(), "{list}");

    send(&mut wr, 11, METHOD_SHUTDOWN, serde_json::json!({})).await;
    let _ = response(&mut rd, 11).await;
    tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .expect("daemon exits after shutdown")
        .unwrap();
}

/// Approvals must round-trip: the daemon asks, the client answers, and the turn
/// resumes. This used to deadlock outright — the gate's blocking wait stranded
/// the connection loop on the worker's LIFO slot — so both answers are pinned.
#[tokio::test]
async fn approvals_round_trip_over_the_wire() {
    let tmp = tempfile::tempdir().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    let script = r#"[
        {"tool":"write","input":{"path":"yes.txt","content":"hi"}},
        {"text":"wrote it"},
        {"tool":"write","input":{"path":"no.txt","content":"hi"}},
        {"text":"declined"}
    ]"#;
    let mut child = spawn_daemon(tmp.path(), &[("HASP_MOCK_SCRIPT", script)]).await;
    let sock = tmp.path().join("d.sock");

    let stream = UnixStream::connect(&sock).await.unwrap();
    let (rd, mut wr) = stream.into_split();
    let mut rd = BufReader::new(rd);

    async fn send(
        wr: &mut tokio::net::unix::OwnedWriteHalf,
        id: u64,
        method: &str,
        params: serde_json::Value,
    ) {
        let line = encode(&Wire::request(id, method, params));
        wr.write_all(line.as_bytes()).await.unwrap();
        wr.flush().await.unwrap();
    }

    async fn reply(wr: &mut tokio::net::unix::OwnedWriteHalf, id: u64, answer: &str) {
        let line = encode(&Wire::ok(id, serde_json::json!({"answer": answer})));
        wr.write_all(line.as_bytes()).await.unwrap();
        wr.flush().await.unwrap();
    }

    /// Read until the daemon asks for permission; return its request id.
    async fn next_permission(rd: &mut BufReader<tokio::net::unix::OwnedReadHalf>) -> (u64, String) {
        loop {
            let mut line = String::new();
            tokio::time::timeout(Duration::from_secs(20), rd.read_line(&mut line))
                .await
                .expect("permission request within timeout")
                .unwrap();
            if let Some(Wire::Request { id, method, params }) = decode(&line)
                && method == hasp_wire::METHOD_PERMISSION_REQUEST
            {
                return (id, params["question"].as_str().unwrap_or("").to_string());
            }
        }
    }

    async fn response(
        rd: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
        want: u64,
    ) -> serde_json::Value {
        loop {
            let mut line = String::new();
            tokio::time::timeout(Duration::from_secs(20), rd.read_line(&mut line))
                .await
                .expect("response within timeout")
                .unwrap();
            if let Some(Wire::Response { id, result, error }) = decode(&line)
                && id == want
            {
                if let Some(e) = error {
                    panic!("response {want} error: {}", e.message);
                }
                return result.unwrap_or(serde_json::Value::Null);
            }
        }
    }

    send(
        &mut wr,
        1,
        METHOD_INITIALIZE,
        serde_json::json!({"client": "t"}),
    )
    .await;
    let _ = response(&mut rd, 1).await;
    send(
        &mut wr,
        2,
        hasp_wire::METHOD_SESSION_NEW,
        serde_json::json!({"cwd": proj.to_string_lossy()}),
    )
    .await;
    let _ = response(&mut rd, 2).await;

    // Approved: the turn is held while it asks, then finishes and writes.
    send(
        &mut wr,
        3,
        METHOD_SESSION_PROMPT,
        serde_json::json!({"prompt": "write yes.txt"}),
    )
    .await;
    let (perm_id, question) = next_permission(&mut rd).await;
    assert!(question.contains("yes.txt"), "{question}");
    reply(&mut wr, perm_id, "y").await;
    let turn = response(&mut rd, 3).await;
    assert_eq!(turn["text"].as_str().unwrap(), "wrote it");
    assert_eq!(std::fs::read_to_string(proj.join("yes.txt")).unwrap(), "hi");

    // Declined: the tool reports the refusal and nothing is written.
    send(
        &mut wr,
        4,
        METHOD_SESSION_PROMPT,
        serde_json::json!({"prompt": "write no.txt"}),
    )
    .await;
    let (perm_id, question) = next_permission(&mut rd).await;
    assert!(question.contains("no.txt"), "{question}");
    reply(&mut wr, perm_id, "n").await;
    let turn = response(&mut rd, 4).await;
    assert_eq!(turn["text"].as_str().unwrap(), "declined");
    assert!(
        !proj.join("no.txt").exists(),
        "a declined write must not land"
    );

    send(&mut wr, 11, METHOD_SHUTDOWN, serde_json::json!({})).await;
    let _ = response(&mut rd, 11).await;
    tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .expect("daemon exits")
        .unwrap();
}

/// Reasoning rides its own update kind, so a client can render it dim/italic
/// instead of mixing it into the answer (thinking models do this every turn).
#[tokio::test]
async fn thinking_streams_to_the_client() {
    let tmp = tempfile::tempdir().unwrap();
    let script = r#"[{"thinking":"weighing the options carefully","text":"here is the answer"}]"#;
    let mut child = spawn_daemon(tmp.path(), &[("HASP_MOCK_SCRIPT", script)]).await;
    let sock = tmp.path().join("d.sock");

    let stream = UnixStream::connect(&sock).await.unwrap();
    let (rd, mut wr) = stream.into_split();
    let mut rd = BufReader::new(rd);

    async fn send(
        wr: &mut tokio::net::unix::OwnedWriteHalf,
        id: u64,
        method: &str,
        params: serde_json::Value,
    ) {
        let line = encode(&Wire::request(id, method, params));
        wr.write_all(line.as_bytes()).await.unwrap();
        wr.flush().await.unwrap();
    }

    /// Collect session/update frames of one kind until the prompt answers.
    async fn collect(
        rd: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
        until: u64,
    ) -> (String, String) {
        let (mut thinking, mut deltas) = (String::new(), String::new());
        loop {
            let mut line = String::new();
            tokio::time::timeout(Duration::from_secs(20), rd.read_line(&mut line))
                .await
                .expect("update within timeout")
                .unwrap();
            match decode(&line) {
                Some(Wire::Notification { method, params })
                    if method == hasp_wire::METHOD_SESSION_UPDATE =>
                {
                    let text = params["text"].as_str().unwrap_or_default();
                    match params["kind"].as_str().unwrap_or("") {
                        "thinking" => thinking.push_str(text),
                        "delta" => deltas.push_str(text),
                        _ => {}
                    }
                }
                Some(Wire::Response { id, .. }) if id == until => return (thinking, deltas),
                _ => {}
            }
        }
    }

    send(
        &mut wr,
        1,
        METHOD_INITIALIZE,
        serde_json::json!({"client": "t"}),
    )
    .await;
    let mut line = String::new();
    rd.read_line(&mut line).await.unwrap();
    send(
        &mut wr,
        2,
        hasp_wire::METHOD_SESSION_NEW,
        serde_json::json!({"cwd": tmp.path().to_string_lossy()}),
    )
    .await;
    line.clear();
    rd.read_line(&mut line).await.unwrap();

    send(
        &mut wr,
        3,
        METHOD_SESSION_PROMPT,
        serde_json::json!({"prompt": "answer me"}),
    )
    .await;
    let (thinking, deltas) = collect(&mut rd, 3).await;
    assert!(thinking.contains("weighing the options"), "{thinking}");
    assert!(deltas.contains("here is the answer"), "{deltas}");
    assert!(
        !deltas.contains("weighing"),
        "reasoning is not the answer: {deltas}"
    );

    send(&mut wr, 9, METHOD_SHUTDOWN, serde_json::json!({})).await;
    let mut line = String::new();
    let _ = rd.read_line(&mut line).await;
    tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .expect("daemon exits")
        .unwrap();
}

/// A daemon survives its clients on purpose — but not the loss of its own
/// socket, because then nobody can ever connect again and it only strands the
/// workspace lock. (Found while cleaning up dozens of probes left behind.)
#[tokio::test]
async fn daemon_exits_when_its_socket_disappears() {
    let tmp = tempfile::tempdir().unwrap();
    let mut child = spawn_daemon(tmp.path(), &[]).await;
    let sock = tmp.path().join("d.sock");
    std::fs::remove_file(&sock).expect("unlink the socket");

    tokio::time::timeout(Duration::from_secs(20), child.wait())
        .await
        .expect("daemon exits once its socket is unlinked")
        .unwrap();
}

/// The point of a resident session: the daemon keeps running a detached
/// session's schedule. Before residents existed, the heartbeat loop returned as
/// soon as the client's channel closed, so a disconnected client meant a
/// scheduled task that silently never ran again.
#[tokio::test]
async fn detached_heartbeat_keeps_firing_with_no_client() {
    let tmp = tempfile::tempdir().unwrap();
    let mut child = spawn_daemon(tmp.path(), &[]).await;
    let sock = tmp.path().join("d.sock");

    let mut client = WireClient::connect(&sock).await.unwrap();
    client.initialize().await.unwrap();
    let session = client
        .request(
            METHOD_SESSION_CONTINUE,
            serde_json::json!({"cwd": tmp.path().to_string_lossy()}),
        )
        .await
        .unwrap();
    let path = std::path::PathBuf::from(session["path"].as_str().unwrap());
    client
        .request(
            METHOD_HEARTBEAT_SET,
            serde_json::json!({"action": "add", "instruction": "tick", "everyMs": 150}),
        )
        .await
        .unwrap();

    // Go away. The session — and its schedule — must not.
    drop(client);
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let body = std::fs::read_to_string(&path).unwrap();
    let ticks = body.matches("echo: [heartbeat hb-1]").count();
    assert!(
        ticks >= 3,
        "expected the schedule to keep firing while detached, journal had {ticks} completed tick(s): {body}"
    );

    // Reattaching joins the running session instead of starting a fresh one.
    let mut again = WireClient::connect(&sock).await.unwrap();
    again.initialize().await.unwrap();
    let rejoined = again
        .request(
            METHOD_SESSION_CONTINUE,
            serde_json::json!({"cwd": tmp.path().to_string_lossy()}),
        )
        .await
        .unwrap();
    assert_eq!(
        rejoined["adopted"].as_bool(),
        Some(true),
        "a client joining a live session must be told it adopted one"
    );
    assert_eq!(rejoined["sessionId"], session["sessionId"]);

    let _ = again.request(METHOD_SHUTDOWN, serde_json::json!({})).await;
    let _ = child.wait().await;
}

/// Two clients, one session: the second attaches rather than being refused.
#[tokio::test]
async fn second_client_adopts_the_live_resident() {
    let tmp = tempfile::tempdir().unwrap();
    let mut child = spawn_daemon(tmp.path(), &[]).await;
    let sock = tmp.path().join("d.sock");
    let cwd = serde_json::json!({"cwd": tmp.path().to_string_lossy()});

    let mut first = WireClient::connect(&sock).await.unwrap();
    first.initialize().await.unwrap();
    let a = first
        .request(METHOD_SESSION_CONTINUE, cwd.clone())
        .await
        .unwrap();

    let mut second = WireClient::connect(&sock).await.unwrap();
    second.initialize().await.unwrap();
    let b = second
        .request(METHOD_SESSION_CONTINUE, cwd.clone())
        .await
        .unwrap();
    assert_eq!(
        b["sessionId"], a["sessionId"],
        "the same session, not a copy"
    );
    assert_eq!(b["adopted"].as_bool(), Some(true));

    // `new` would have to evict a session that may be mid-goal, so it is refused
    // with an explanation instead. A fresh connection is needed because one
    // connection holds at most one session.
    let mut third = WireClient::connect(&sock).await.unwrap();
    third.initialize().await.unwrap();
    let err = third
        .request(METHOD_SESSION_NEW, cwd)
        .await
        .expect_err("session/new must not silently replace a live session");
    assert!(
        format!("{err:?}").contains("live session"),
        "unhelpful refusal: {err:?}"
    );

    let _ = first.request(METHOD_SHUTDOWN, serde_json::json!({})).await;
    let _ = child.wait().await;
}

/// A detached session that needs approval must be denied immediately rather
/// than parking a worker until a client that no longer exists answers.
#[tokio::test]
async fn detached_approval_denies_instead_of_hanging() {
    let tmp = tempfile::tempdir().unwrap();
    let mut child = spawn_daemon(
        tmp.path(),
        &[(
            "HASP_MOCK_SCRIPT",
            r#"[{"tool":"bash","input":{"command":"touch probe.txt"}}]"#,
        )],
    )
    .await;
    let sock = tmp.path().join("d.sock");

    let mut client = WireClient::connect(&sock).await.unwrap();
    client.initialize().await.unwrap();
    let session = client
        .request(
            METHOD_SESSION_CONTINUE,
            serde_json::json!({"cwd": tmp.path().to_string_lossy()}),
        )
        .await
        .unwrap();
    let path = std::path::PathBuf::from(session["path"].as_str().unwrap());
    client
        .request(
            METHOD_HEARTBEAT_SET,
            serde_json::json!({"action": "add", "instruction": "touch something", "everyMs": 150}),
        )
        .await
        .unwrap();

    // Detached, and deliberately not --yolo: nobody can approve anything now.
    drop(client);
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let body = std::fs::read_to_string(&path).unwrap();
    assert!(
        body.contains("command not approved: user declined"),
        "a detached approval must be denied in the journal: {body}"
    );
    assert!(
        !tmp.path().join("probe.txt").exists(),
        "denied means denied — the command must not have run"
    );
    assert!(
        body.matches("[heartbeat hb-1]").count() >= 2,
        "a denied approval must not wedge the scheduler: {body}"
    );

    // No client is left to ask for a shutdown.
    let _ = child.kill().await;
}

/// Documents (rather than fixes) a guarantee: a turn already in flight runs to
/// completion when its client vanishes, because the session, not the client,
/// owns the turn's lifecycle. The reply is recovered from the journal.
#[tokio::test]
async fn turn_survives_the_client_that_started_it() {
    let tmp = tempfile::tempdir().unwrap();
    let mut child = spawn_daemon(tmp.path(), &[("HASP_MOCK_DELAY_MS", "800")]).await;
    let sock = tmp.path().join("d.sock");

    let mut client = WireClient::connect(&sock).await.unwrap();
    client.initialize().await.unwrap();
    let session = client
        .request(
            METHOD_SESSION_CONTINUE,
            serde_json::json!({"cwd": tmp.path().to_string_lossy()}),
        )
        .await
        .unwrap();
    let path = std::path::PathBuf::from(session["path"].as_str().unwrap());

    let in_flight = tokio::spawn(async move {
        let _ = client
            .request(
                METHOD_SESSION_PROMPT,
                serde_json::json!({"prompt": "slow one"}),
            )
            .await;
    });
    tokio::time::sleep(Duration::from_millis(250)).await;
    // Killing the task drops the client, closing the socket mid-turn.
    in_flight.abort();
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let body = std::fs::read_to_string(&path).unwrap();
    assert!(
        body.contains("echo: slow one"),
        "the turn must finish without the client that started it: {body}"
    );

    // No client is left to ask for a shutdown.
    let _ = child.kill().await;
}

/// Cron schedules: malformed expressions are refused before anything reaches
/// the journal, and the list shows the expression with its next occurrence.
#[tokio::test]
async fn cron_heartbeats_validate_and_list() {
    let tmp = tempfile::tempdir().unwrap();
    let mut child = spawn_daemon(tmp.path(), &[]).await;
    let sock = tmp.path().join("d.sock");

    let mut client = WireClient::connect(&sock).await.unwrap();
    client.initialize().await.unwrap();
    client
        .request(
            METHOD_SESSION_CONTINUE,
            serde_json::json!({"cwd": tmp.path().to_string_lossy()}),
        )
        .await
        .unwrap();

    let bad = client
        .request(
            METHOD_HEARTBEAT_SET,
            serde_json::json!({"action": "add", "cron": "0 9 * *", "instruction": "x"}),
        )
        .await
        .expect_err("a four-field expression must be rejected");
    assert!(
        format!("{bad:?}").contains("cron"),
        "the error should name the problem: {bad:?}"
    );

    client
        .request(
            METHOD_HEARTBEAT_SET,
            serde_json::json!({
                "action": "add",
                "cron": "0 9 * * 1-5",
                "instruction": "weekday standup"
            }),
        )
        .await
        .unwrap();

    let list = client
        .request(METHOD_HEARTBEAT_LIST, serde_json::json!({}))
        .await
        .unwrap();
    let hb = &list.as_array().expect("one heartbeat")[0];
    assert_eq!(hb["cron"].as_str(), Some("0 9 * * 1-5"));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let next = hb["nextDueMs"].as_i64().expect("next due is reported");
    assert!(next > now, "next occurrence lies in the future");
    // A weekday-09:00 schedule is never more than ~3.5 days away.
    assert!(
        next - now < 4 * 24 * 3600 * 1000,
        "implausibly far away: {next}"
    );

    let _ = client.request(METHOD_SHUTDOWN, serde_json::json!({})).await;
    let _ = child.wait().await;
}

/// Two clients on one resident session: attaching is not a takeover, so both
/// see the turn stream. Before fan-out the second client stole the stream and
/// the first went silent.
#[tokio::test]
async fn both_attached_clients_see_the_turn_stream() {
    use tokio::io::Lines;
    use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

    type Raw = (Lines<BufReader<OwnedReadHalf>>, OwnedWriteHalf);

    async fn send(wr: &mut OwnedWriteHalf, msg: Wire) {
        wr.write_all(encode(&msg).as_bytes()).await.unwrap();
        wr.flush().await.unwrap();
    }

    /// Connect and open the workspace session over the raw wire, returning the
    /// stream and the session id.
    async fn open(sock: &std::path::Path, cwd: &str) -> (Raw, String) {
        let stream = UnixStream::connect(sock).await.unwrap();
        let (rd, mut wr) = stream.into_split();
        let mut lines = BufReader::new(rd).lines();
        send(
            &mut wr,
            Wire::request(1, METHOD_INITIALIZE, serde_json::json!({})),
        )
        .await;
        send(
            &mut wr,
            Wire::request(2, METHOD_SESSION_CONTINUE, serde_json::json!({"cwd": cwd})),
        )
        .await;
        loop {
            let line = lines.next_line().await.unwrap().expect("a line");
            if let Some(Wire::Response {
                id: 2,
                result: Some(r),
                ..
            }) = decode(&line)
            {
                let sid = r["sessionId"].as_str().expect("sessionId").to_string();
                return ((lines, wr), sid);
            }
        }
    }

    let tmp = tempfile::tempdir().unwrap();
    let mut child = spawn_daemon(tmp.path(), &[]).await;
    let sock = tmp.path().join("d.sock");
    let cwd = tmp.path().to_string_lossy().to_string();

    let ((mut lines_a, mut wr_a), sid_a) = open(&sock, &cwd).await;
    let ((mut lines_b, _wr_b), sid_b) = open(&sock, &cwd).await;
    assert_eq!(sid_a, sid_b, "both clients are on the same session");

    send(
        &mut wr_a,
        Wire::request(
            3,
            METHOD_SESSION_PROMPT,
            serde_json::json!({"prompt": "fan out"}),
        ),
    )
    .await;

    let mut a_text = String::new();
    loop {
        let line = lines_a.next_line().await.unwrap().expect("a line");
        match decode(&line) {
            Some(Wire::Notification { method, params }) if method == "session/update" => {
                if params["kind"] == "delta" {
                    a_text.push_str(params["text"].as_str().unwrap_or_default());
                }
            }
            Some(Wire::Response { id: 3, .. }) => break,
            _ => {}
        }
    }
    assert!(a_text.contains("echo: fan out"), "prompter saw: {a_text:?}");

    // The second client asked for nothing, yet must receive the same stream.
    let mut b_text = String::new();
    let saw = tokio::time::timeout(Duration::from_secs(30), async {
        while let Ok(Some(line)) = lines_b.next_line().await {
            if let Some(Wire::Notification { method, params }) = decode(&line)
                && method == "session/update"
                && params["kind"] == "delta"
            {
                b_text.push_str(params["text"].as_str().unwrap_or_default());
                if b_text.contains("echo: fan out") {
                    return;
                }
            }
        }
    })
    .await;
    assert!(saw.is_ok(), "the watcher was starved: {b_text:?}");
    assert!(b_text.contains("echo: fan out"), "watcher saw: {b_text:?}");

    let _ = child.kill().await;
}

/// The engine lock is not held across provider I/O: stats and rename requests
/// answer while a slow turn is in flight, and a request that would restructure
/// the session under the turn is refused instead of queueing behind it.
#[tokio::test]
async fn requests_answer_while_a_slow_turn_is_in_flight() {
    let tmp = tempfile::tempdir().unwrap();
    let mut child = spawn_daemon(tmp.path(), &[("HASP_MOCK_DELAY_MS", "3000")]).await;
    let sock = tmp.path().join("d.sock");
    let stream = UnixStream::connect(&sock).await.unwrap();
    let (rd, mut wr) = stream.into_split();
    let mut rd = BufReader::new(rd);

    async fn send(
        wr: &mut tokio::net::unix::OwnedWriteHalf,
        id: u64,
        method: &str,
        params: serde_json::Value,
    ) {
        let line = encode(&Wire::request(id, method, params));
        wr.write_all(line.as_bytes()).await.unwrap();
        wr.flush().await.unwrap();
    }

    /// Next response of any id: `(id, Ok(result) | Err(message))`.
    async fn next_response(
        rd: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
    ) -> (u64, Result<serde_json::Value, String>) {
        loop {
            let mut line = String::new();
            assert!(rd.read_line(&mut line).await.unwrap() > 0, "daemon closed");
            if let Some(Wire::Response { id, result, error }) = decode(&line) {
                return (
                    id,
                    match error {
                        Some(e) => Err(e.message),
                        None => Ok(result.unwrap_or_default()),
                    },
                );
            }
        }
    }

    send(
        &mut wr,
        1,
        METHOD_INITIALIZE,
        serde_json::json!({"client": "t"}),
    )
    .await;
    assert_eq!(next_response(&mut rd).await.0, 1);
    send(
        &mut wr,
        2,
        METHOD_SESSION_CONTINUE,
        serde_json::json!({"cwd": tmp.path().to_string_lossy()}),
    )
    .await;
    assert_eq!(next_response(&mut rd).await.0, 2);

    send(
        &mut wr,
        3,
        METHOD_SESSION_PROMPT,
        serde_json::json!({"prompt": "slow"}),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await; // the turn is now mid-sample
    let t = std::time::Instant::now();
    send(&mut wr, 4, METHOD_SESSION_STATS, serde_json::json!({})).await;
    send(
        &mut wr,
        5,
        hasp_wire::METHOD_SESSION_NAME,
        serde_json::json!({"name": "renamed mid-turn"}),
    )
    .await;
    send(
        &mut wr,
        6,
        hasp_wire::METHOD_SESSION_BRANCH,
        serde_json::json!({"target": "nope"}),
    )
    .await;

    let mut seen = Vec::new();
    for _ in 0..3 {
        seen.push(next_response(&mut rd).await);
    }
    assert!(
        t.elapsed() < Duration::from_millis(1500),
        "answered mid-turn, not after it: {:?}",
        t.elapsed()
    );
    let ids: Vec<u64> = seen.iter().map(|(id, _)| *id).collect();
    assert_eq!(ids, vec![4, 5, 6], "all three before the turn's own reply");
    assert!(seen[0].1.as_ref().unwrap()["entries"].as_u64().unwrap() >= 1);
    assert!(seen[1].1.is_ok());
    assert!(
        seen[2].1.as_ref().unwrap_err().contains("turn is running"),
        "{:?}",
        seen[2].1
    );

    let (id, turn) = next_response(&mut rd).await;
    assert_eq!(id, 3);
    assert_eq!(turn.unwrap()["text"], "echo: slow");

    send(&mut wr, 7, METHOD_SHUTDOWN, serde_json::json!({})).await;
    let _ = next_response(&mut rd).await;
    tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .expect("daemon exits after shutdown")
        .unwrap();
}

/// A daemon restart brings back an explicitly enabled resident and its
/// schedules with no client connecting: intervals resume, a one-shot that
/// already fired never fires again, one whose time passed during the downtime
/// is expired (not run late), and one still in the future fires on time.
#[tokio::test]
async fn enabled_resident_and_schedules_survive_a_daemon_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let sock = tmp.path().join("d.sock");
    let mut child = spawn_daemon(tmp.path(), &[]).await;

    let mut c = WireClient::connect(&sock).await.unwrap();
    c.initialize().await.unwrap();
    let s = c
        .request(METHOD_SESSION_CONTINUE, serde_json::json!({"cwd": ws.to_string_lossy()}))
        .await
        .unwrap();
    let path = std::path::PathBuf::from(s["path"].as_str().unwrap());
    let set = |v: serde_json::Value| v;
    for params in [
        set(serde_json::json!({"action": "add", "instruction": "tick", "everyMs": 200})), // hb-1
        set(serde_json::json!({"action": "add", "instruction": "near", "inMs": 50})),     // hb-2: fires now
        set(serde_json::json!({"action": "add", "instruction": "later", "inMs": 3500})),  // hb-3: after restart
    ] {
        c.request(METHOD_HEARTBEAT_SET, params).await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(600)).await;
    // hb-4 comes due while the daemon is down
    c.request(METHOD_HEARTBEAT_SET, serde_json::json!({"action": "add", "instruction": "missed", "inMs": 400}))
        .await
        .unwrap();

    // `hasp resident enable` from the workspace, against the same HOME.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_hasp"))
        .args(["resident", "enable"])
        .current_dir(&ws)
        .env("HOME", tmp.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    c.shutdown().await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), child.wait()).await.unwrap().unwrap();
    let before = std::fs::read_to_string(&path).unwrap();
    let ticks_before = before.matches("echo: [heartbeat hb-1] tick").count();
    assert_eq!(before.matches("echo: [heartbeat hb-2] near").count(), 1);
    tokio::time::sleep(Duration::from_millis(900)).await; // hb-4's time passes

    // Restart. Nobody connects.
    let mut child = spawn_daemon(tmp.path(), &[]).await;
    tokio::time::sleep(Duration::from_millis(3600)).await;
    let after = std::fs::read_to_string(&path).unwrap();
    assert!(
        after.matches("echo: [heartbeat hb-1] tick").count() >= ticks_before + 3,
        "interval schedule resumed after restart with no client"
    );
    assert_eq!(after.matches("echo: [heartbeat hb-2] near").count(), 1, "fired one-shot not replayed");
    assert_eq!(after.matches("[heartbeat hb-4] missed").count(), 0, "missed one-shot not run late");
    assert!(after.contains(r#""action":"expired","hb_id":"hb-4""#), "{after}");
    assert_eq!(after.matches("echo: [heartbeat hb-3] later").count(), 1, "future one-shot fired once");

    // New schedules don't reuse journaled ids.
    let mut c = WireClient::connect(&sock).await.unwrap();
    c.initialize().await.unwrap();
    let again = c
        .request(METHOD_SESSION_CONTINUE, serde_json::json!({"cwd": ws.to_string_lossy()}))
        .await
        .unwrap();
    assert_eq!(again["adopted"].as_bool(), Some(true), "joins the restored resident");
    let added = c
        .request(METHOD_HEARTBEAT_SET, serde_json::json!({"action": "add", "instruction": "fresh", "everyMs": 60000}))
        .await
        .unwrap();
    assert_eq!(added["id"].as_str(), Some("hb-5"), "{added}");
    c.shutdown().await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), child.wait()).await.unwrap().unwrap();
}
