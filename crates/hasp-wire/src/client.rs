//! Minimal async wire client: one-shot `-p --daemon`, `harness shutdown`,
//! and tests.

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::{METHOD_INITIALIZE, METHOD_PERMISSION_REQUEST, METHOD_SHUTDOWN, Wire, decode, encode};

pub struct WireClient {
    rd: BufReader<tokio::net::unix::OwnedReadHalf>,
    wr: tokio::net::unix::OwnedWriteHalf,
    next_id: u64,
}

impl WireClient {
    pub async fn connect(path: &std::path::Path) -> Result<Self> {
        let stream = UnixStream::connect(path)
            .await
            .with_context(|| format!("connecting to daemon at {}", path.display()))?;
        let (rd, wr) = stream.into_split();
        Ok(Self {
            rd: BufReader::new(rd),
            wr,
            next_id: 0,
        })
    }

    async fn send(&mut self, msg: &Wire) -> Result<()> {
        self.wr.write_all(encode(msg).as_bytes()).await?;
        self.wr.flush().await?;
        Ok(())
    }

    /// Sends a request and waits for its response. Permission requests from
    /// the daemon are answered "n" (non-interactive semantics);
    /// `session/update` notifications are ignored.
    pub async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&Wire::request(id, method, params)).await?;
        loop {
            let mut line = String::new();
            if self.rd.read_line(&mut line).await? == 0 {
                bail!("daemon closed the connection");
            }
            let Some(msg) = decode(&line) else { continue };
            match msg {
                Wire::Response {
                    id: rid,
                    result,
                    error,
                } if rid == id => {
                    if let Some(e) = error {
                        bail!("daemon error {}: {}", e.code, e.message);
                    }
                    return Ok(result.unwrap_or(Value::Null));
                }
                Wire::Request {
                    id: rid, method, ..
                } if method == METHOD_PERMISSION_REQUEST => {
                    self.send(&Wire::ok(rid, serde_json::json!({"answer": "n"})))
                        .await?;
                }
                _ => {}
            }
        }
    }

    /// Like `request`, but permission questions from the daemon go to
    /// `on_permission` (answer 'y', 'a' or 'n') instead of an automatic "n";
    /// `on_update` sees `session/update` notifications (progress).
    pub async fn request_with<P, Fut>(
        &mut self,
        method: &str,
        params: Value,
        mut on_permission: P,
        mut on_update: impl FnMut(&Value),
    ) -> Result<Value>
    where
        P: FnMut(String) -> Fut,
        Fut: std::future::Future<Output = char>,
    {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&Wire::request(id, method, params)).await?;
        loop {
            let mut line = String::new();
            if self.rd.read_line(&mut line).await? == 0 {
                bail!("daemon closed the connection");
            }
            let Some(msg) = decode(&line) else { continue };
            match msg {
                Wire::Response { id: rid, result, error } if rid == id => {
                    if let Some(e) = error {
                        bail!("daemon error {}: {}", e.code, e.message);
                    }
                    return Ok(result.unwrap_or(Value::Null));
                }
                Wire::Request { id: rid, method, params } if method == METHOD_PERMISSION_REQUEST => {
                    let q = params["question"].as_str().unwrap_or("").to_string();
                    let answer = on_permission(q).await;
                    self.send(&Wire::ok(rid, serde_json::json!({"answer": answer.to_string()}))).await?;
                }
                Wire::Notification { params, .. } => on_update(&params),
                _ => {}
            }
        }
    }

    pub async fn initialize(&mut self) -> Result<()> {
        self.request(
            METHOD_INITIALIZE,
            serde_json::json!({"client": "oneshot", "protocol": crate::PROTOCOL_VERSION}),
        )
        .await?;
        Ok(())
    }

    pub async fn shutdown(&mut self) -> Result<()> {
        self.request(METHOD_SHUTDOWN, serde_json::json!({})).await?;
        Ok(())
    }
}
