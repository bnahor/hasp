//! The harness as an MCP server, driven by the harness's own MCP client.
//!
//! What matters here is that a foreign agent gets the *same* tool layer — the
//! confinement and the gate policy included — rather than a way around them.

use hasp_mcp::{McpClient, McpServerConfig};

fn config(cwd: &std::path::Path, env: &[(&str, &str)]) -> McpServerConfig {
    McpServerConfig {
        command: env!("CARGO_BIN_EXE_hasp").into(),
        args: vec!["--mcp-serve".into()],
        cwd: Some(cwd.to_path_buf()),
        env: env
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect(),
    }
}

#[tokio::test]
async fn serves_the_tool_layer_with_the_same_confinement_and_policy() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("notes.txt"), "alpha\nbeta\n").unwrap();

    let client = McpClient::connect("harness", &config(tmp.path(), &[]))
        .await
        .expect("connect to the harness MCP server");
    let names: Vec<&str> = client.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "bash",
            "edit",
            "glob",
            "grep",
            "job",
            "read",
            "symbol_search",
            "web_fetch",
            "write"
        ],
        "sorted specs"
    );

    // Read-only work is allowed.
    let (text, is_error) = client
        .call_tool("read", serde_json::json!({"path": "notes.txt"}))
        .await
        .unwrap();
    assert!(!is_error, "{text}");
    assert!(text.contains("alpha"), "{text}");

    // Writes need an approver, and there isn't one: refused, and nothing lands.
    let (text, is_error) = client
        .call_tool(
            "write",
            serde_json::json!({"path": "new.txt", "content": "x"}),
        )
        .await
        .unwrap();
    assert!(is_error, "write refused: {text}");
    assert!(text.contains("not approved"), "{text}");
    assert!(!tmp.path().join("new.txt").exists());

    // Confinement is structural, not advisory: absolute paths are rejected.
    let (text, is_error) = client
        .call_tool("read", serde_json::json!({"path": "/etc/passwd"}))
        .await
        .unwrap();
    assert!(is_error, "absolute path rejected: {text}");

    // Shell redirection is not an allowlisted simple command.
    let (text, is_error) = client
        .call_tool(
            "bash",
            serde_json::json!({"command": "echo hi > escaped.txt"}),
        )
        .await
        .unwrap();
    assert!(is_error, "refused: {text}");
    assert!(!tmp.path().join("escaped.txt").exists());

    // The symbol scan is served over MCP too — an external agent driving this
    // harness gets the same code-intelligence surface our own agent has.
    std::fs::write(tmp.path().join("lib.rs"), "pub fn alpha_target() {}\n").unwrap();
    let (text, is_error) = client
        .call_tool(
            "symbol_search",
            serde_json::json!({"query": "alpha_target"}),
        )
        .await
        .unwrap();
    assert!(!is_error, "{text}");
    assert!(text.contains("lib.rs:1"), "{text}");
    assert!(text.contains("alpha_target"), "{text}");

    // The egress guard is structural: it holds here as it does in-process.
    let (text, is_error) = client
        .call_tool(
            "web_fetch",
            serde_json::json!({"url": "http://169.254.169.254/latest"}),
        )
        .await
        .unwrap();
    assert!(is_error, "metadata endpoint refused: {text}");
    assert!(text.contains("local or private"), "{text}");

    // With the explicit opt-in the same tools mutate.
    let client = McpClient::connect("harness", &config(tmp.path(), &[("HASP_MCP_YOLO", "1")]))
        .await
        .expect("connect with yolo");
    let (text, is_error) = client
        .call_tool(
            "write",
            serde_json::json!({"path": "allowed.txt", "content": "x"}),
        )
        .await
        .unwrap();
    assert!(!is_error, "{text}");
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("allowed.txt")).unwrap(),
        "x"
    );
}

/// Calls run concurrently: a slow command does not hold up a read.
#[tokio::test]
async fn calls_run_concurrently() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.txt"), "alpha").unwrap();
    let client = McpClient::connect("harness", &config(tmp.path(), &[("HASP_MCP_YOLO", "1")]))
        .await
        .unwrap();
    let t = std::time::Instant::now();
    let slow = client.call_tool("bash", serde_json::json!({"command": "sleep 2; echo slow"}));
    let fast = async {
        let r = client
            .call_tool("read", serde_json::json!({"path": "a.txt"}))
            .await
            .unwrap();
        (r, t.elapsed())
    };
    let (slow, (read, read_at)) = tokio::join!(slow, fast);
    assert!(slow.unwrap().0.contains("slow"));
    assert!(read.0.contains("alpha"));
    assert!(
        read_at < std::time::Duration::from_millis(1500),
        "the read waited for the command: {read_at:?}"
    );
}

/// An abandoned call (here: a client-side timeout) is cancelled on the server,
/// and cancelling stops the command itself.
#[tokio::test]
async fn an_abandoned_call_stops_its_command() {
    let tmp = tempfile::tempdir().unwrap();
    let client = McpClient::connect("harness", &config(tmp.path(), &[("HASP_MCP_YOLO", "1")]))
        .await
        .unwrap()
        .with_call_timeout(std::time::Duration::from_secs(1));
    let err = client
        .call_tool(
            "bash",
            serde_json::json!({"command": "sleep 3 && touch finished"}),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("timed out"), "{err}");
    tokio::time::sleep(std::time::Duration::from_secs(4)).await;
    assert!(
        !tmp.path().join("finished").exists(),
        "the cancelled command ran to completion"
    );
    // The server is still healthy after the cancellation.
    let ok = client
        .call_tool("bash", serde_json::json!({"command": "echo still here"}))
        .await
        .unwrap();
    assert!(ok.0.contains("still here"));
}
