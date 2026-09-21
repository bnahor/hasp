//! The harness as an MCP server, driven by the harness's own MCP client.
//!
//! What matters here is that a foreign agent gets the *same* tool layer — the
//! confinement and the gate policy included — rather than a way around them.

use harness_mcp::{McpClient, McpServerConfig};

fn config(cwd: &std::path::Path, env: &[(&str, &str)]) -> McpServerConfig {
    McpServerConfig {
        command: env!("CARGO_BIN_EXE_harness").into(),
        args: vec!["--mcp-serve".into()],
        cwd: Some(cwd.to_path_buf()),
        env: env
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect(),
        ..Default::default()
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
    assert_eq!(names, vec!["bash", "edit", "read", "write"], "sorted specs");

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

    // With the explicit opt-in the same tools mutate.
    let client = McpClient::connect("harness", &config(tmp.path(), &[("HARNESS_MCP_YOLO", "1")]))
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
