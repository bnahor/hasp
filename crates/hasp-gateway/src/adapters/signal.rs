//! Signal through a local signal-cli daemon in HTTP mode
//! (`signal-cli -a +NUM daemon --http=127.0.0.1:8080`, v0.14.x).
//!
//! - Events: SSE `GET /api/v1/events`, resumed with `Last-Event-ID` (our
//!   cursor). The daemon keeps only its last 1000 events and replays all of
//!   them for an unknown id, so dedup by `<sourceUuid>:<timestamp>` (Signal's
//!   own message identity) is what makes replays harmless.
//! - Sends: JSON-RPC `send` on `POST /api/v1/rpc`; the result `timestamp` is the
//!   sent message's id (used for quotes and echo detection).
//! - `sourceUuid` (ACI) is the identity; numbers can be hidden or change.
//! - Messages this account sent from another device arrive as
//!   `syncMessage.sentMessage`: echoes, except "Note to Self" in inbox mode,
//!   which is how the owner talks to the assistant on their own number.

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};

use super::{Adapter, Polled, Role};
use crate::model::{AttachmentRef, Caps, Inbound, Outbound, SendError, classify_reqwest};

pub struct Signal {
    base: String,
    /// This account's ACI (uuid): identifies Note to Self.
    account_uuid: String,
    role: Role,
    http: reqwest::Client,
    per_chat_interval_ms: u64,
    window_ms: u64,
    rpc_id: AtomicU64,
}

impl Signal {
    pub fn new(base: &str, account_uuid: &str, role: Role, window_ms: u64) -> Self {
        Self { base: base.trim_end_matches('/').into(), account_uuid: account_uuid.into(), role,
               http: reqwest::Client::new(), per_chat_interval_ms: 1_000, window_ms, rpc_id: AtomicU64::new(1) }
    }

    pub fn with_per_chat_interval_ms(mut self, ms: u64) -> Self {
        self.per_chat_interval_ms = ms;
        self
    }

    /// One SSE `data:` payload (a receive notification or its params) -> Inbound.
    pub fn parse_event(&self, data: &Value) -> Option<Inbound> {
        let params = if data.get("params").is_some() { &data["params"] } else { data };
        let params = if params.get("result").is_some() { &params["result"] } else { params }; // manual mode
        let env = &params["envelope"];
        let source = env["sourceUuid"].as_str()?.to_string();
        let ts = env["timestamp"].as_i64()?;
        let (msg, from_self, chat, direct) = if let Some(dm) = env.get("dataMessage").filter(|v| !v.is_null()) {
            let group = dm["groupInfo"]["groupId"].as_str();
            (dm.clone(), false, group.map(|g| format!("group:{g}")).unwrap_or_else(|| source.clone()), group.is_none())
        } else if let Some(sent) = env["syncMessage"].get("sentMessage").filter(|v| !v.is_null()) {
            let to_self = sent["destinationUuid"].as_str() == Some(self.account_uuid.as_str());
            // Note to Self is the owner's own assistant chat; anything else we sent is an echo.
            (sent.clone(), !to_self, sent["destinationUuid"].as_str().unwrap_or("").to_string(), true)
        } else {
            return None; // receipts, typing, calls
        };
        let attachments = msg["attachments"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|x| AttachmentRef {
                        id: x["id"].as_str().unwrap_or("").into(),
                        kind: if x["isVoiceNote"].as_bool() == Some(true) { "voice".into() } else { "document".into() },
                        mime: x["contentType"].as_str().map(String::from),
                        name: x["filename"].as_str().map(String::from),
                        size: x["size"].as_u64(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Some(Inbound {
            event_key: format!("{source}:{ts}"),
            chat_id: chat,
            thread_id: None,
            sender_id: source,
            sender_name: env["sourceName"].as_str().map(String::from),
            from_self,
            direct,
            text: msg["message"].as_str().unwrap_or("").into(),
            attachments,
            reply_to: msg["quote"]["id"].as_i64().map(|q| format!("{}:{q}", msg["quote"]["authorUuid"].as_str().unwrap_or(""))),
            message_id: Some(format!("{}:{ts}", env["sourceUuid"].as_str().unwrap_or(""))),
            button: None,
            ts_ms: ts,
        })
    }

    pub fn send_params(&self, msg: &Outbound) -> Value {
        let mut p = match msg.chat_id.strip_prefix("group:") {
            Some(g) => json!({"groupId": g, "message": msg.text}),
            None => json!({"recipient": [msg.chat_id], "message": msg.text}),
        };
        if let Some((author, ts)) = msg.reply_to.as_deref().and_then(|r| r.rsplit_once(':'))
            && let Ok(ts) = ts.parse::<i64>()
        {
            p["quoteTimestamp"] = json!(ts);
            p["quoteAuthor"] = json!(author);
        }
        p
    }

    async fn rpc(&self, method: &str, params: Value) -> std::result::Result<Value, SendError> {
        let id = self.rpc_id.fetch_add(1, Ordering::Relaxed);
        let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let r = self.http.post(format!("{}/api/v1/rpc", self.base)).json(&body).send().await.map_err(|e| classify_reqwest(&e))?;
        let v: Value = r.json().await.map_err(|e| SendError::Uncertain(format!("unreadable reply: {e}")))?;
        if let Some(e) = v.get("error").filter(|e| !e.is_null()) {
            let msg = e["message"].as_str().unwrap_or("").to_string();
            // Untrusted identity (safety number changed) needs the owner, not a retry.
            return Err(if msg.to_lowercase().contains("untrusted") || msg.to_lowercase().contains("identity") {
                SendError::Permanent(format!("safety number changed: verify it before sending ({msg})"))
            } else {
                SendError::Permanent(msg)
            });
        }
        Ok(v["result"].clone())
    }
}

#[async_trait]
impl Adapter for Signal {
    fn name(&self) -> &str {
        "signal"
    }

    fn caps(&self) -> Caps {
        Caps { max_len: 2000, buttons: false, edits: false, per_chat_interval_ms: self.per_chat_interval_ms }
    }

    fn role_of(&self, chat_id: &str, _direct: bool) -> Role {
        match self.role {
            Role::Bot => Role::Bot,
            Role::Inbox if chat_id == self.account_uuid => Role::Bot, // Note to Self
            Role::Inbox => Role::Inbox,
        }
    }

    async fn poll(&self, cursor: Option<String>) -> Result<Polled> {
        let mut req = self.http.get(format!("{}/api/v1/events", self.base)).header("accept", "text/event-stream");
        if let Some(c) = &cursor {
            req = req.header("last-event-id", c);
        }
        let resp = req.send().await?;
        if !resp.status().is_success() {
            return Err(anyhow!("signal-cli events: {}", resp.status()));
        }
        let mut stream = resp.bytes_stream();
        let mut buf = String::new();
        let (mut events, mut last_id) = (Vec::new(), cursor.clone());
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(self.window_ms);
        loop {
            let chunk = tokio::select! {
                c = stream.next() => c,
                _ = tokio::time::sleep_until(deadline) => None,
            };
            let Some(chunk) = chunk else { break };
            buf.push_str(&String::from_utf8_lossy(&chunk?));
            while let Some(end) = buf.find("\n\n") {
                let block: String = buf.drain(..end + 2).collect();
                let (mut id, mut data) = (None, String::new());
                for line in block.lines() {
                    if let Some(v) = line.strip_prefix("id:") {
                        id = Some(v.trim().to_string());
                    } else if let Some(v) = line.strip_prefix("data:") {
                        data.push_str(v.trim_start());
                    }
                }
                if let Ok(v) = serde_json::from_str::<Value>(&data)
                    && let Some(e) = self.parse_event(&v)
                {
                    events.push(e);
                }
                if id.is_some() {
                    last_id = id;
                }
            }
        }
        Ok(Polled { events, cursor: last_id })
    }

    async fn send(&self, msg: &Outbound) -> std::result::Result<String, SendError> {
        let res = self.rpc("send", self.send_params(msg)).await?;
        Ok(format!("{}:{}", self.account_uuid, res["timestamp"].as_i64().unwrap_or_default()))
    }

    async fn typing(&self, chat_id: &str) -> Result<()> {
        let p = match chat_id.strip_prefix("group:") {
            Some(g) => json!({"groupId": g}),
            None => json!({"recipient": [chat_id]}),
        };
        self.rpc("sendTyping", p).await.map_err(|e| anyhow!("{e}"))?;
        Ok(())
    }
}
