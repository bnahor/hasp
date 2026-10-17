//! WhatsApp through the local Baileys bridge (bridges/whatsapp-baileys): a
//! linked device on the owner's own account. The bridge persists every
//! `messages.upsert` (type `notify` only) with a sequence number; this
//! adapter reads after its cursor and sends through the bridge.
//!
//! Because it is the owner's personal account, only the "Message yourself"
//! chat is the assistant conversation; every other chat is personal inbox
//! (third-party content, never commands; sending there needs approval).
//! In the self chat the owner's messages are `fromMe`, and so are ours:
//! echoes are told apart by the ids of messages we sent.

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Adapter, Polled, Role};
use crate::model::{AttachmentRef, Caps, Inbound, Outbound, SendError, classify_reqwest};

pub struct WhatsApp {
    base: String,
    token: String,
    /// The owner's own chat (PN or LID JID, normalized by the bridge).
    self_jids: Vec<String>,
    http: reqwest::Client,
    wait_s: u64,
}

impl WhatsApp {
    pub fn new(base: &str, token: &str, self_jids: Vec<String>, wait_s: u64) -> Self {
        Self {
            base: base.trim_end_matches('/').into(),
            token: token.into(),
            self_jids,
            http: reqwest::Client::builder().timeout(std::time::Duration::from_secs(wait_s + 15)).build().expect("client"),
            wait_s,
        }
    }

    fn is_self_chat(&self, jid: &str) -> bool {
        self.self_jids.iter().any(|j| j == jid)
    }

    /// One bridge event (see bridges/whatsapp-baileys/src/normalize.ts).
    pub fn parse_event(&self, e: &Value) -> Option<Inbound> {
        let chat = e["chat"].as_str()?.to_string();
        let id = e["id"].as_str()?.to_string();
        let from_me = e["fromMe"].as_bool().unwrap_or(false);
        let self_chat = self.is_self_chat(&chat);
        Some(Inbound {
            event_key: format!("{chat}:{id}"),
            chat_id: chat.clone(),
            thread_id: None,
            // Group: the participant; DM: the chat; self chat: the owner's own JID.
            sender_id: e["sender"].as_str().unwrap_or(&chat).to_string(),
            sender_name: e["pushName"].as_str().map(String::from),
            // fromMe outside the self chat is the owner typing on their phone
            // to someone else: not addressed to the assistant.
            from_self: from_me && !self_chat,
            direct: !chat.ends_with("@g.us"),
            text: e["text"].as_str().unwrap_or("").into(),
            attachments: e["attachments"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|x| AttachmentRef {
                            id: x["id"].as_str().unwrap_or("").into(),
                            kind: x["kind"].as_str().unwrap_or("document").into(),
                            mime: x["mime"].as_str().map(String::from),
                            name: x["name"].as_str().map(String::from),
                            size: x["size"].as_u64(),
                        })
                        .collect()
                })
                .unwrap_or_default(),
            reply_to: e["quotedId"].as_str().map(String::from),
            message_id: Some(id),
            button: None,
            ts_ms: e["timestamp"].as_i64().unwrap_or(0) * 1000,
        })
    }
}

#[async_trait]
impl Adapter for WhatsApp {
    fn name(&self) -> &str {
        "whatsapp"
    }

    fn caps(&self) -> Caps {
        // No buttons for personal accounts: approvals use the /approve command.
        Caps { max_len: 4000, buttons: false, edits: false, per_chat_interval_ms: 2_000 }
    }

    fn role_of(&self, chat_id: &str, _direct: bool) -> Role {
        if self.is_self_chat(chat_id) { Role::Bot } else { Role::Inbox }
    }

    async fn poll(&self, cursor: Option<String>) -> Result<Polled> {
        let after = cursor.clone().unwrap_or_else(|| "0".into());
        let r = self
            .http
            .get(format!("{}/events?after={after}&wait={}", self.base, self.wait_s))
            .bearer_auth(&self.token)
            .send()
            .await?;
        if !r.status().is_success() {
            return Err(anyhow!("whatsapp bridge: {}", r.status()));
        }
        let v: Value = r.json().await?;
        let events = v["events"].as_array().cloned().unwrap_or_default();
        Ok(Polled {
            events: events.iter().filter_map(|e| self.parse_event(e)).collect(),
            cursor: v["next"].as_i64().map(|n| n.to_string()).or(cursor),
        })
    }

    async fn send(&self, msg: &Outbound) -> std::result::Result<String, SendError> {
        let mut body = json!({"jid": msg.chat_id, "text": msg.text});
        if let Some(q) = &msg.reply_to {
            body["quotedId"] = json!(q);
        }
        let r = self.http.post(format!("{}/send", self.base)).bearer_auth(&self.token).json(&body).send().await
            .map_err(|e| classify_reqwest(&e))?;
        let status = r.status().as_u16();
        let v: Value = r.json().await.map_err(|e| SendError::Uncertain(format!("unreadable reply: {e}")))?;
        match status {
            200 => Ok(v["id"].as_str().unwrap_or("").to_string()),
            // the bridge says the socket is down: nothing was sent
            503 => Err(SendError::Retry { after_ms: 10_000, why: v["error"].as_str().unwrap_or("not connected").into() }),
            // the bridge handed it to WhatsApp but didn't get an ack
            504 => Err(SendError::Uncertain(v["error"].as_str().unwrap_or("no ack").into())),
            _ => Err(SendError::Permanent(format!("{status}: {}", v["error"]))),
        }
    }
}
