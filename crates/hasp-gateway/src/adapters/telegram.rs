//! Telegram Bot API (10.3) via long polling.
//!
//! - getUpdates with `offset = last update_id + 1` acknowledges everything
//!   before it, so the gateway stores events (and this cursor) first and only
//!   then polls again. `allowed_updates` is sent every time (it is sticky).
//! - deleteWebhook at start (webhooks and getUpdates are exclusive). A 409 means
//!   another poller holds the token: fatal for this instance, never retried in a loop.
//! - sendMessage: 4096 chars, `reply_parameters`, inline keyboard
//!   (`callback_data` ≤ 64 bytes); 429 honours `parameters.retry_after`.
//! - Every button press is answered (answerCallbackQuery) so it stops spinning.

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Adapter, Polled, Role};
use crate::model::{AttachmentRef, Caps, Inbound, Outbound, SendError, classify_reqwest};

pub struct Telegram {
    base: String, // https://api.telegram.org (or a fake server in tests)
    token: String,
    http: reqwest::Client,
    poll_timeout_s: u64,
    per_chat_interval_ms: u64,
}

impl Telegram {
    pub fn new(base: &str, token: &str, poll_timeout_s: u64) -> Self {
        Self {
            base: base.trim_end_matches('/').into(),
            token: token.into(),
            http: reqwest::Client::builder().timeout(std::time::Duration::from_secs(poll_timeout_s + 15)).build().expect("client"),
            poll_timeout_s,
            per_chat_interval_ms: 1_000, // ~1 msg/s per chat (Bot API FAQ)
        }
    }

    pub fn with_per_chat_interval_ms(mut self, ms: u64) -> Self {
        self.per_chat_interval_ms = ms;
        self
    }

    fn url(&self, method: &str) -> String {
        format!("{}/bot{}/{method}", self.base, self.token)
    }

    async fn call(&self, method: &str, body: Value) -> Result<Value> {
        let r = self.http.post(self.url(method)).json(&body).send().await?;
        let status = r.status().as_u16();
        let v: Value = r.json().await?;
        if v["ok"].as_bool() == Some(true) {
            return Ok(v["result"].clone());
        }
        if status == 409 {
            bail!("telegram 409 conflict: another poller or a webhook holds this token ({})", v["description"]);
        }
        Err(anyhow!("telegram {status}: {}", v["description"]))
    }

    pub async fn start(&self) -> Result<()> {
        self.call("deleteWebhook", json!({"drop_pending_updates": false})).await?;
        Ok(())
    }

    fn file_ref(kind: &str, f: &Value) -> AttachmentRef {
        AttachmentRef {
            id: f["file_id"].as_str().unwrap_or("").into(),
            kind: kind.into(),
            mime: f["mime_type"].as_str().map(String::from),
            name: f["file_name"].as_str().map(String::from),
            size: f["file_size"].as_u64(),
        }
    }

    /// One Update -> Inbound (None for update types we don't handle).
    pub fn parse_update(u: &Value) -> Option<Inbound> {
        let update_id = u["update_id"].as_i64()?;
        if let Some(cb) = u.get("callback_query") {
            let msg = &cb["message"];
            return Some(Inbound {
                event_key: update_id.to_string(),
                chat_id: msg["chat"]["id"].as_i64().map(|i| i.to_string()).unwrap_or_default(),
                thread_id: None,
                sender_id: cb["from"]["id"].as_i64()?.to_string(),
                sender_name: cb["from"]["first_name"].as_str().map(String::from),
                from_self: cb["from"]["is_bot"].as_bool().unwrap_or(false),
                direct: msg["chat"]["type"] == "private",
                text: String::new(),
                attachments: vec![],
                reply_to: msg["message_id"].as_i64().map(|i| i.to_string()),
                message_id: None,
                button: Some((cb["id"].as_str()?.into(), cb["data"].as_str().unwrap_or("").into())),
                ts_ms: crate::model::now_ms(),
            });
        }
        let m = u.get("message").or_else(|| u.get("edited_message"))?;
        let mut attachments = Vec::new();
        for kind in ["voice", "audio", "document", "video"] {
            if let Some(f) = m.get(kind) {
                attachments.push(Self::file_ref(kind, f));
            }
        }
        if let Some(sizes) = m["photo"].as_array()
            && let Some(largest) = sizes.last()
        {
            attachments.push(Self::file_ref("photo", largest));
        }
        Some(Inbound {
            event_key: update_id.to_string(),
            chat_id: m["chat"]["id"].as_i64()?.to_string(),
            thread_id: m["message_thread_id"].as_i64().map(|i| i.to_string()),
            sender_id: m["from"]["id"].as_i64().map(|i| i.to_string()).unwrap_or_default(),
            sender_name: m["from"]["first_name"].as_str().map(String::from),
            from_self: m["from"]["is_bot"].as_bool().unwrap_or(false),
            direct: m["chat"]["type"] == "private",
            text: m["text"].as_str().or_else(|| m["caption"].as_str()).unwrap_or("").into(),
            attachments,
            reply_to: m["reply_to_message"]["message_id"].as_i64().map(|i| i.to_string()),
            message_id: m["message_id"].as_i64().map(|i| i.to_string()),
            button: None,
            ts_ms: m["date"].as_i64().unwrap_or(0) * 1000,
        })
    }

    pub fn send_body(msg: &Outbound) -> Value {
        let mut body = json!({"chat_id": msg.chat_id, "text": msg.text});
        if let Some(t) = &msg.thread_id {
            body["message_thread_id"] = json!(t.parse::<i64>().unwrap_or_default());
        }
        if let Some(r) = &msg.reply_to {
            body["reply_parameters"] = json!({"message_id": r.parse::<i64>().unwrap_or_default(), "allow_sending_without_reply": true});
        }
        if !msg.buttons.is_empty() {
            let row: Vec<Value> = msg
                .buttons
                .iter()
                .filter(|b| b.data.len() <= 64)
                .map(|b| json!({"text": b.label, "callback_data": b.data}))
                .collect();
            body["reply_markup"] = json!({"inline_keyboard": [row]});
        }
        body
    }
}

#[async_trait]
impl Adapter for Telegram {
    fn name(&self) -> &str {
        "telegram"
    }

    fn caps(&self) -> Caps {
        Caps { max_len: 4096, buttons: true, edits: true, per_chat_interval_ms: self.per_chat_interval_ms }
    }

    fn role_of(&self, _chat_id: &str, _direct: bool) -> Role {
        Role::Bot // a bot only ever sees its own conversations
    }

    async fn poll(&self, cursor: Option<String>) -> Result<Polled> {
        let mut body = json!({"timeout": self.poll_timeout_s, "allowed_updates": ["message", "edited_message", "callback_query"]});
        if let Some(c) = &cursor {
            body["offset"] = json!(c.parse::<i64>().unwrap_or(0));
        }
        let updates = self.call("getUpdates", body).await?;
        let list = updates.as_array().cloned().unwrap_or_default();
        let next = list.iter().filter_map(|u| u["update_id"].as_i64()).max().map(|m| (m + 1).to_string()).or(cursor);
        Ok(Polled { events: list.iter().filter_map(Self::parse_update).collect(), cursor: next })
    }

    async fn send(&self, msg: &Outbound) -> std::result::Result<String, SendError> {
        let r = match self.http.post(self.url("sendMessage")).json(&Self::send_body(msg)).send().await {
            Ok(r) => r,
            Err(e) => return Err(classify_reqwest(&e)),
        };
        let status = r.status().as_u16();
        let v: Value = match r.json().await {
            Ok(v) => v,
            Err(e) => return Err(SendError::Uncertain(format!("unreadable reply: {e}"))),
        };
        if v["ok"].as_bool() == Some(true) {
            return Ok(v["result"]["message_id"].as_i64().map(|i| i.to_string()).unwrap_or_default());
        }
        let why = v["description"].as_str().unwrap_or("").to_string();
        match status {
            429 => Err(SendError::Retry { after_ms: v["parameters"]["retry_after"].as_u64().unwrap_or(5) * 1000, why }),
            500..=599 => Err(SendError::Retry { after_ms: 5_000, why }),
            _ => Err(SendError::Permanent(format!("{status}: {why}"))),
        }
    }

    async fn answer_button(&self, callback_id: &str, text: &str) -> Result<()> {
        self.call("answerCallbackQuery", json!({"callback_query_id": callback_id, "text": text})).await?;
        Ok(())
    }

    async fn typing(&self, chat_id: &str) -> Result<()> {
        self.call("sendChatAction", json!({"chat_id": chat_id, "action": "typing"})).await?;
        Ok(())
    }

    async fn fetch_attachment(&self, id: &str, dest: &std::path::Path) -> Result<()> {
        let f = self.call("getFile", json!({"file_id": id})).await?;
        let path = f["file_path"].as_str().ok_or_else(|| anyhow!("no file_path"))?;
        let bytes = self.http.get(format!("{}/file/bot{}/{path}", self.base, self.token)).send().await?.bytes().await?;
        std::fs::write(dest, bytes)?;
        Ok(())
    }
}
