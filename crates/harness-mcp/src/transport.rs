//! stdio transport: spawn a server, handshake, route request/response by id.

use crate::{McpServerConfig, initialize_params};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, oneshot};

const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

type Stdin = Arc<Mutex<tokio::process::ChildStdin>>;

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
    stdin: Stdin,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    next_id: AtomicU64,
    /// How long one `tools/call` may take. Third-party servers keep the
    /// 30 s default; a guest agent runs commands that legitimately take
    /// minutes (see [`McpClient::with_call_timeout`]).
    call_timeout: std::time::Duration,
    /// Held for its Drop: `kill_on_drop` reaps the server with the client.
    _child: tokio::process::Child,
}

impl McpClient {
    pub async fn connect(name: &str, cfg: &McpServerConfig) -> Result<Self> {
        Self::connect_within(name, cfg, REQUEST_TIMEOUT).await
    }

    /// `connect` with a longer handshake allowance, for servers that start
    /// slowly — a guest agent in a freshly booted microVM, say.
    pub async fn connect_within(
        name: &str,
        cfg: &McpServerConfig,
        handshake: std::time::Duration,
    ) -> Result<Self> {
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
            stdin: Arc::new(Mutex::new(stdin)),
            pending,
            next_id: AtomicU64::new(1),
            call_timeout: REQUEST_TIMEOUT,
            _child: child,
        };
        client
            .request_within("initialize", initialize_params(), handshake)
            .await
            .context("mcp initialize")?;
        client
            .notify("notifications/initialized", json!({}))
            .await?;
        let listed = client
            .request_within("tools/list", json!({}), handshake)
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

    /// Allow `tools/call` to run this long before giving up on it.
    pub fn with_call_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.call_timeout = timeout;
        self
    }

    async fn request_within(
        &self,
        method: &str,
        params: Value,
        timeout: std::time::Duration,
    ) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        // If this future is dropped (the turn was cancelled) or times out
        // before the reply, tell the server to stop the work: MCP's
        // `notifications/cancelled`. A command left running inside an
        // environment after the user pressed esc would be a leak.
        let mut guard = CancelOnDrop {
            id,
            stdin: self.stdin.clone(),
            pending: self.pending.clone(),
            armed: true,
        };
        self.send(&json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params,
        }))
        .await?;

        let msg = tokio::time::timeout(timeout, rx)
            .await
            .map_err(|_| anyhow!("{method} timed out after {}s", timeout.as_secs()))?
            .map_err(|_| anyhow!("{method}: server closed the connection"))?;
        guard.armed = false;
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
        write_line(&self.stdin, msg).await
    }

    pub async fn call_tool(&self, name: &str, args: Value) -> Result<(String, bool)> {
        let (text, is_error, _) = self.call_tool_full(name, args).await?;
        Ok((text, is_error))
    }

    /// `call_tool` plus any images the result carries.
    pub async fn call_tool_full(
        &self,
        name: &str,
        args: Value,
    ) -> Result<(String, bool, Vec<harness_core::Image>)> {
        let result = self
            .request_within(
                "tools/call",
                json!({"name": name, "arguments": args}),
                self.call_timeout,
            )
            .await?;
        Ok(crate::render_content_full(&result))
    }
}

async fn write_line(stdin: &Stdin, msg: &Value) -> Result<()> {
    let mut line = serde_json::to_string(msg)?;
    line.push('\n');
    let mut stdin = stdin.lock().await;
    stdin.write_all(line.as_bytes()).await?;
    stdin.flush().await?;
    Ok(())
}

/// Sends `notifications/cancelled` for a request that is abandoned before
/// its reply arrives (dropped future or timeout).
struct CancelOnDrop {
    id: u64,
    stdin: Stdin,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    armed: bool,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let (id, stdin, pending) = (self.id, self.stdin.clone(), self.pending.clone());
        // Drop cannot await; a runtime is always present where requests run.
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                pending.lock().await.remove(&id);
                let _ = write_line(
                    &stdin,
                    &json!({
                        "jsonrpc": "2.0",
                        "method": "notifications/cancelled",
                        "params": {"requestId": id, "reason": "cancelled by the client"},
                    }),
                )
                .await;
            });
        }
    }
}
