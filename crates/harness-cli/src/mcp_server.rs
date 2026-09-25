//! Serve the harness's own tools over MCP on stdio (`--mcp-serve`).
//!
//! The point is capability sharing, not a bypass: this dispatches to the same
//! `Tool` implementations the agent uses, so workspace confinement (relative
//! paths only, no `..`, symlink checks) and the bash metacharacter rule still
//! apply. Writes and non-allowlisted commands are refused unless
//! `HARNESS_MCP_YOLO=1`, because there is no human here to approve them.

use anyhow::{Context, Result};
use harness_core::{ArtifactStore, Gate, GateDecision, Tool, ToolCtx, ToolOutput};
use harness_workspace::Workspace;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::gate::GateCore;

const PROTOCOL_VERSION: &str = "2024-11-05";

/// Non-interactive gate: there is nobody to ask, so the core's policy decides
/// alone — allowlisted read-only bash runs, everything else is refused.
struct DenyGate(GateCore);

impl Gate for DenyGate {
    fn check_bash(&mut self, command: &str) -> GateDecision {
        self.0.decide_bash(command, |_| 'n')
    }

    fn check_write(&mut self, path: &str) -> GateDecision {
        self.0.decide_write(path, |_| 'n')
    }
}

fn yolo() -> bool {
    matches!(
        std::env::var("HARNESS_MCP_YOLO").as_deref(),
        Ok("1") | Ok("true")
    )
}

pub async fn serve() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let ws = Workspace::resolve(&cwd)?;
    let run_id = format!("mcp-{}", harness_core::session::now_ms());
    let artifacts = ArtifactStore::new(
        ws.root().join(".harness").join("artifacts").join(&run_id),
        run_id,
    )?;
    // Not interactive: the gate's existing policy already means "allowlisted
    // read-only commands only, no writes" — exactly the right default here.
    let ws = std::sync::Arc::new(ws);
    let artifacts = std::sync::Arc::new(artifacts);
    // `todo` is a planning aid for this harness's own loop; to another agent
    // it would be a tool that does nothing.
    let tools: std::sync::Arc<Vec<Box<dyn Tool>>> = std::sync::Arc::new(
        harness_tools::builtins_base()
            .into_iter()
            .filter(|t| t.spec().name != "todo")
            .collect(),
    );
    let specs: Vec<Value> = tools
        .iter()
        .map(|t| {
            let spec = t.spec();
            json!({
                "name": spec.name,
                "description": spec.description,
                "inputSchema": spec.input_schema,
            })
        })
        .collect();

    eprintln!(
        "harness mcp server: {} tool(s) at {} ({})",
        specs.len(),
        ws.root().display(),
        if yolo() {
            "yolo"
        } else {
            "read-only + allowlisted bash"
        }
    );

    // Calls run concurrently — a client batches independent reads, and a
    // long command must not hold up the rest — so replies go through one
    // writer, and each in-flight call can be aborted by
    // `notifications/cancelled` (dropping a tool's future stops its command,
    // process group and all).
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
    let writer = tokio::spawn(async move {
        let mut out = tokio::io::stdout();
        while let Some(msg) = out_rx.recv().await {
            let mut line = serde_json::to_string(&msg).unwrap_or_default();
            line.push('\n');
            if out.write_all(line.as_bytes()).await.is_err() || out.flush().await.is_err() {
                break;
            }
        }
    });
    let in_flight: std::sync::Arc<
        std::sync::Mutex<std::collections::HashMap<String, tokio::task::AbortHandle>>,
    > = Default::default();

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await.context("reading stdin")? {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let method = msg["method"].as_str().unwrap_or_default().to_string();
        let id = msg["id"].clone();

        // Notifications carry no id and expect no reply; cancellation is the
        // one that does something.
        if id.is_null() {
            if method == "notifications/cancelled"
                && let Some(handle) = in_flight
                    .lock()
                    .expect("in-flight calls")
                    .remove(&msg["params"]["requestId"].to_string())
            {
                handle.abort();
            }
            continue;
        }

        let reply = |result: Result<Value, String>| match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(message) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": message},
            }),
        };
        match method.as_str() {
            "initialize" => {
                let _ = out_tx.send(reply(Ok(json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "harness", "version": env!("CARGO_PKG_VERSION")},
                }))));
            }
            "ping" => {
                let _ = out_tx.send(reply(Ok(json!({}))));
            }
            "tools/list" => {
                let _ = out_tx.send(reply(Ok(json!({"tools": specs}))));
            }
            "tools/call" => {
                let name = msg["params"]["name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let args = msg["params"]["arguments"].clone();
                let (tools, ws, artifacts) = (tools.clone(), ws.clone(), artifacts.clone());
                let (out_tx, in_flight_done) = (out_tx.clone(), in_flight.clone());
                let key = id.to_string();
                let id2 = id.clone();
                let task = tokio::spawn(async move {
                    // Each call gets its own gate: with nobody to ask, the
                    // policy alone decides, so there is no state to share.
                    let mut gate = DenyGate(GateCore::new(yolo(), false));
                    let result = match tools.iter().find(|t| t.spec().name == name) {
                        Some(tool) => {
                            let mut ctx = ToolCtx {
                                workspace: &ws,
                                gate: &mut gate,
                                artifacts: &artifacts,
                                bus: None,
                            };
                            tool.execute(args, &mut ctx).await
                        }
                        None => ToolOutput::error(format!("unknown tool: {name}")),
                    };
                    let mut content = vec![json!({"type": "text", "text": result.content})];
                    content.extend(result.images.iter().map(
                        |img| json!({"type": "image", "data": img.data, "mimeType": img.media_type}),
                    ));
                    let _ = out_tx.send(json!({
                        "jsonrpc": "2.0",
                        "id": id2,
                        "result": {"content": content, "isError": result.is_error},
                    }));
                    in_flight_done.lock().expect("in-flight calls").remove(&key);
                });
                in_flight
                    .lock()
                    .expect("in-flight calls")
                    .insert(id.to_string(), task.abort_handle());
            }
            other => {
                let _ = out_tx.send(reply(Err(format!("unknown method {other}"))));
            }
        }
    }
    // The client is gone: nobody will read these results, and a command left
    // running would outlive the session that asked for it.
    for (_, handle) in in_flight.lock().expect("in-flight calls").drain() {
        handle.abort();
    }
    drop(out_tx);
    let _ = writer.await;
    Ok(())
}
