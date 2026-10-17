//! Test double: a minimal MCP server over stdio, used by the integration tests.
//! Built as part of the crate so tests need no interpreter or network.

use serde_json::{Value, json};
use std::io::{BufRead, Write};

fn main() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let method = msg["method"].as_str().unwrap_or_default();
        let id = msg["id"].clone();
        let reply = match method {
            "initialize" => Some(json!({
                "jsonrpc": "2.0", "id": id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "fake", "version": "1"},
                }
            })),
            "notifications/initialized" => None,
            "tools/list" => Some(json!({
                "jsonrpc": "2.0", "id": id,
                "result": {"tools": [
                    {
                        "name": "echo",
                        "description": "Echo text back",
                        "inputSchema": {
                            "type": "object",
                            "properties": {"text": {"type": "string"}},
                            "required": ["text"]
                        }
                    },
                    {
                        "name": "boom",
                        "description": "Always fails",
                        "inputSchema": {"type": "object", "properties": {}}
                    }
                ]}
            })),
            "tools/call" => {
                let name = msg["params"]["name"].as_str().unwrap_or_default();
                let text = msg["params"]["arguments"]["text"]
                    .as_str()
                    .unwrap_or_default();
                match name {
                    "echo" => Some(json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": {"content": [{"type": "text", "text": format!("echoed: {text}")}]}
                    })),
                    "boom" => Some(json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": {"content": [{"type": "text", "text": "kaboom"}], "isError": true}
                    })),
                    _ => Some(json!({
                        "jsonrpc": "2.0", "id": id,
                        "error": {"code": -32601, "message": "no such tool"}
                    })),
                }
            }
            _ if id.is_null() => None,
            _ => Some(json!({
                "jsonrpc": "2.0", "id": id,
                "error": {"code": -32601, "message": "unknown method"}
            })),
        };
        if let Some(r) = reply {
            let _ = writeln!(stdout, "{r}");
            let _ = stdout.flush();
        }
    }
}
