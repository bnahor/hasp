//! MCP servers as harness tools (research/03-architecture.md §9).
//!
//! Transport is the stdio flavour of MCP: newline-delimited JSON-RPC 2.0 to a
//! child process — the same framing `harness-wire` uses. Servers are configured
//! in `mcp.json`; every remote tool is bridged as `mcp__<server>__<tool>` with
//! its schema passed through unchanged, so the tool list stays cache-stable.

mod config;
mod transport;

pub use config::{McpServerConfig, load_config};
pub use transport::{McpClient, McpToolDef};

use async_trait::async_trait;
use harness_core::{Tool, ToolCtx, ToolOutput, ToolSpec};
use harness_workspace::Workspace;
use serde_json::{Value, json};
use std::sync::Arc;

/// Connect every configured server and return their tools, name-sorted.
/// A server that fails to start is reported and skipped: a broken MCP entry
/// must not stop the agent from running.
pub async fn tools_for(ws: &Workspace) -> Vec<Box<dyn Tool>> {
    let servers = load_config(ws);
    let mut tools: Vec<Box<dyn Tool>> = Vec::new();
    for (name, cfg) in servers {
        match McpClient::connect(&name, &cfg).await {
            Ok(client) => {
                let client = Arc::new(client);
                let defs = client.tools.clone();
                for def in defs {
                    tools.push(Box::new(McpTool {
                        client: client.clone(),
                        def,
                    }));
                }
            }
            Err(e) => eprintln!("▪ mcp server “{name}” unavailable: {e}"),
        }
    }
    tools.sort_by_key(|t| t.spec().name);
    tools
}

struct McpTool {
    client: Arc<McpClient>,
    def: McpToolDef,
}

#[async_trait]
impl Tool for McpTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: tool_name(&self.client.name, &self.def.name),
            description: self.def.description.clone(),
            input_schema: self.def.input_schema.clone(),
        }
    }

    async fn execute(&self, args: Value, _ctx: &mut ToolCtx<'_>) -> ToolOutput {
        match self.client.call_tool_full(&self.def.name, args).await {
            Ok((text, is_error, images)) => ToolOutput {
                content: text,
                is_error,
                images,
            },
            Err(e) => ToolOutput::error(format!("mcp server “{}” failed: {e}", self.client.name)),
        }
    }
}

fn tool_name(server: &str, tool: &str) -> String {
    format!("mcp__{server}__{tool}")
}

/// Flatten an MCP `tools/call` result into text. Content blocks are the
/// protocol's shape; anything unrecognised is passed through as JSON so
/// information is never silently dropped.
/// Text, error flag and images of a `tools/call` result. MCP `image` items
/// become real images for the model — as text they would be kilobytes of
/// base64 it cannot see.
pub(crate) fn render_content_full(result: &Value) -> (String, bool, Vec<harness_core::Image>) {
    let is_error = result["isError"].as_bool().unwrap_or(false);
    let Some(items) = result["content"].as_array() else {
        return (result.to_string(), is_error, Vec::new());
    };
    let mut parts = Vec::new();
    let mut images = Vec::new();
    for item in items {
        match item["type"].as_str() {
            Some("text") => parts.push(item["text"].as_str().unwrap_or_default().to_string()),
            Some("image") => match (item["data"].as_str(), item["mimeType"].as_str()) {
                (Some(data), Some(mime)) => {
                    images.push(harness_core::Image {
                        media_type: mime.to_string(),
                        data: data.to_string(),
                    });
                    parts.push(format!("[image {}]", images.len()));
                }
                _ => parts.push("[malformed image content]".into()),
            },
            _ => parts.push(item.to_string()),
        }
    }
    (parts.join("\n"), is_error, images)
}

/// `initialize` params — the client half of the MCP handshake.
pub(crate) fn initialize_params() -> Value {
    json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {},
        "clientInfo": {"name": "harness", "version": env!("CARGO_PKG_VERSION")},
    })
}
