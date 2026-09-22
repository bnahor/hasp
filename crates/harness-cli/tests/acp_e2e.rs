//! ACP over stdio against the real binary: an editor's side of the wire.

use serde_json::{Value, json};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

struct Acp {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Acp {
    async fn start(env: &[(&str, &str)]) -> Self {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_harness"));
        cmd.arg("--acp")
            .env("HARNESS_PROVIDER", "mock")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().expect("spawn harness --acp");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        Self {
            child,
            stdin,
            stdout,
            next_id: 0,
        }
    }

    async fn send(&mut self, method: &str, params: Value) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.stdin
            .write_all(format!("{line}\n").as_bytes())
            .await
            .unwrap();
        self.stdin.flush().await.unwrap();
        id
    }

    async fn respond(&mut self, id: u64, result: Value) {
        let line = json!({"jsonrpc": "2.0", "id": id, "result": result});
        self.stdin
            .write_all(format!("{line}\n").as_bytes())
            .await
            .unwrap();
        self.stdin.flush().await.unwrap();
    }

    async fn next(&mut self) -> Value {
        let mut line = String::new();
        let n = tokio::time::timeout(Duration::from_secs(60), self.stdout.read_line(&mut line))
            .await
            .expect("no ACP message within 60s")
            .expect("read");
        assert!(n > 0, "ACP stream closed");
        serde_json::from_str(&line).expect("ACP message is JSON")
    }

    /// Read until the response for `id`, collecting everything else.
    async fn until_response(&mut self, id: u64) -> (Value, Vec<Value>) {
        let mut seen = Vec::new();
        loop {
            let msg = self.next().await;
            if msg["id"].as_u64() == Some(id) && msg.get("method").is_none() {
                return (msg, seen);
            }
            seen.push(msg);
        }
    }
}

fn chunk_text(msg: &Value) -> Option<&str> {
    if msg["method"] != "session/update" {
        return None;
    }
    let update = &msg["params"]["update"];
    match update["sessionUpdate"].as_str()? {
        "agent_message_chunk" => update["content"]["text"].as_str(),
        _ => None,
    }
}

#[tokio::test]
async fn an_editor_can_run_a_turn_over_acp() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let mut acp = Acp::start(&[
        ("HOME", home.path().to_str().unwrap()),
        (
            "HARNESS_SOCKET",
            home.path().join("acp.sock").to_str().unwrap(),
        ),
    ])
    .await;

    let id = acp.send("initialize", json!({"protocolVersion": 1})).await;
    let (reply, _) = acp.until_response(id).await;
    assert_eq!(reply["result"]["agentInfo"]["name"], "harness");
    assert_eq!(reply["result"]["agentCapabilities"]["loadSession"], true);

    let id = acp
        .send(
            "session/new",
            json!({"cwd": work.path().to_str().unwrap(), "mcpServers": []}),
        )
        .await;
    let (reply, _) = acp.until_response(id).await;
    let session_id = reply["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();

    let id = acp
        .send(
            "session/prompt",
            json!({"sessionId": session_id, "prompt": [{"type": "text", "text": "hello acp"}]}),
        )
        .await;
    let (reply, seen) = acp.until_response(id).await;
    let text: String = seen.iter().filter_map(chunk_text).collect();
    assert!(
        text.contains("echo: hello acp"),
        "streamed assistant text: {text:?} (raw: {seen:?})"
    );
    assert_eq!(reply["result"]["stopReason"], "end_turn");

    // A second turn on the same session keeps working.
    let id = acp
        .send(
            "session/prompt",
            json!({"sessionId": session_id, "prompt": [{"type": "text", "text": "again"}]}),
        )
        .await;
    let (reply, seen) = acp.until_response(id).await;
    let text: String = seen.iter().filter_map(chunk_text).collect();
    assert!(text.contains("echo: again"), "{text:?}");
    assert_eq!(reply["result"]["stopReason"], "end_turn");

    let _ = acp.child.kill().await;
}

/// The gate reaches the editor as a permission request, and the editor's
/// refusal is what the tool sees — no silent approval, nothing written.
#[tokio::test]
async fn approvals_round_trip_through_the_editor() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let mut acp = Acp::start(&[
        ("HOME", home.path().to_str().unwrap()),
        (
            "HARNESS_SOCKET",
            home.path().join("acp.sock").to_str().unwrap(),
        ),
        (
            "HARNESS_MOCK_SCRIPT",
            r#"[{"tool":"bash","input":{"command":"touch probe.txt"}}]"#,
        ),
    ])
    .await;

    let id = acp.send("initialize", json!({"protocolVersion": 1})).await;
    acp.until_response(id).await;
    let id = acp
        .send(
            "session/new",
            json!({"cwd": work.path().to_str().unwrap(), "mcpServers": []}),
        )
        .await;
    let (reply, _) = acp.until_response(id).await;
    let session_id = reply["result"]["sessionId"].as_str().unwrap().to_string();

    let prompt_id = acp
        .send(
            "session/prompt",
            json!({"sessionId": session_id, "prompt": [{"type": "text", "text": "touch it"}]}),
        )
        .await;

    // Read until the permission request arrives, then refuse it.
    let mut refused = false;
    loop {
        let msg = acp.next().await;
        if msg["method"] == "session/request_permission" {
            let perm_id = msg["id"].as_u64().expect("permission request id");
            assert!(
                msg["params"]["toolCall"]["title"].as_str().is_some(),
                "question travels with it"
            );
            acp.respond(
                perm_id,
                json!({"outcome": {"outcome": "selected", "optionId": "reject"}}),
            )
            .await;
            refused = true;
        }
        if msg["id"].as_u64() == Some(prompt_id) && msg.get("method").is_none() {
            assert_eq!(msg["result"]["stopReason"], "end_turn");
            break;
        }
    }
    assert!(refused, "the editor must have been asked");
    assert!(
        !work.path().join("probe.txt").exists(),
        "a refused approval must not run the command"
    );

    let _ = acp.child.kill().await;
}
