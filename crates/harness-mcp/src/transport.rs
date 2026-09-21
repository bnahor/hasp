//! stdio transport: spawn a server, handshake, route request/response by id.

use crate::{McpServerConfig, initialize_params, render_content};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, oneshot};

const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct McpToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

pub struct McpClient {
    pub name: String,
    /// Populated by the handshake; empty until then.
    pub tools: Vec<McpToolDef>,
    stdin: Mutex<tokio::process::ChildStdin>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    next_id: AtomicU64,
    /// Held for its Drop: `kill_on_drop` reaps the server with the client.
    _child: tokio::process::Child,
}

impl McpClient {
    pub async fn connect(name: &str, cfg: &McpServerConfig) -> Result<Self> {
        let mut command = tokio::process::Command::new(&cfg.command);
        if let Some(cwd) = &cfg.cwd {
            command.current_dir(cwd);
        }
        command
            .args(&cfg.args)
            .envs(&cfg.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .with_context(|| format!("spawning {}", cfg.command))?;
        let stdin = child.stdin.take().context("server stdin unavailable")?;
        let stdout = child.stdout.take().context("server stdout unavailable")?;

        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        {
            let pending = pending.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    // Only responses carry an id we own; notifications and
                    // server-initiated requests are dropped.
                    let Some(id) = msg["id"].as_u64() else {
                        continue;
                    };
                    if let Some(tx) = pending.lock().await.remove(&id) {
                        let _ = tx.send(msg);
                    }
                }
            });
        }

        let mut client = Self {
            name: name.to_string(),
            tools: Vec::new(),
            stdin: Mutex::new(stdin),
            pending,
            next_id: AtomicU64::new(1),
            _child: child,
        };
        client
            .request("initialize", initialize_params())
            .await
            .context("mcp initialize")?;
        client
            .notify("notifications/initialized", json!({}))
            .await?;
        let listed = client
            .request("tools/list", json!({}))
            .await
            .context("mcp tools/list")?;
        client.tools = listed["tools"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| {
                        Some(McpToolDef {
                            name: t["name"].as_str()?.to_string(),
                            description: t["description"].as_str().unwrap_or("").to_string(),
                            input_schema: t
                                .get("inputSchema")
                                .cloned()
                                .unwrap_or_else(|| json!({"type": "object"})),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(client)
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        self.send(&json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params,
        }))
        .await?;

        let msg = tokio::time::timeout(REQUEST_TIMEOUT, rx)
            .await
            .map_err(|_| anyhow!("{method} timed out after {}s", REQUEST_TIMEOUT.as_secs()))?
            .map_err(|_| anyhow!("{method}: server closed the connection"))?;
        if let Some(err) = msg.get("error") {
            let text = err["message"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| err.to_string());
            bail!("{method}: {text}");
        }
        Ok(msg.get("result").cloned().unwrap_or(Value::Null))
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await
    }

    async fn send(&self, msg: &Value) -> Result<()> {
        let mut line = serde_json::to_string(msg)?;
        line.push('\n');
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(line.as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }

    pub async fn call_tool(&self, name: &str, args: Value) -> Result<(String, bool)> {
        let result = self
            .request("tools/call", json!({"name": name, "arguments": args}))
            .await?;
        Ok(render_content(&result))
    }
}
