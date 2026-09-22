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

async fn write_message(out: &mut tokio::io::Stdout, msg: &Value) -> Result<()> {
    let mut line = serde_json::to_string(msg)?;
    line.push('\n');
    out.write_all(line.as_bytes()).await?;
    out.flush().await?;
    Ok(())
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
    let mut gate: Box<dyn Gate> = Box::new(DenyGate(GateCore::new(yolo(), false)));
    // `todo` is a planning aid for this harness's own loop; to another agent
    // it would be a tool that does nothing.
    let tools: Vec<Box<dyn Tool>> = harness_tools::builtins_base()
        .into_iter()
        .filter(|t| t.spec().name != "todo")
        .collect();
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

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut out = tokio::io::stdout();
    while let Some(line) = lines.next_line().await.context("reading stdin")? {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let method = msg["method"].as_str().unwrap_or_default().to_string();
        let id = msg["id"].clone();

        // Notifications carry no id and expect no reply.
        if id.is_null() {
            continue;
        }

        let reply = match method.as_str() {
            "initialize" => Ok(json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "harness", "version": env!("CARGO_PKG_VERSION")},
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": specs})),
            "tools/call" => {
                let name = msg["params"]["name"].as_str().unwrap_or_default();
                let args = msg["params"]["arguments"].clone();
                let result = match tools.iter().find(|t| t.spec().name == name) {
                    Some(tool) => {
                        let mut ctx = ToolCtx {
                            workspace: &ws,
                            gate: gate.as_mut(),
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
                Ok(json!({"content": content, "isError": result.is_error}))
            }
            other => Err(format!("unknown method {other}")),
        };

        let message = match reply {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(message) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": message},
            }),
        };
        write_message(&mut out, &message).await?;
    }
    Ok(())
}
