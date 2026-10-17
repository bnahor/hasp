//! One interface for every messaging platform. An adapter only moves bytes:
//! it never decides identity, permissions or routing (the gateway does).

pub mod signal;
pub mod telegram;
pub mod web;
pub mod whatsapp;

use anyhow::Result;
use async_trait::async_trait;

use crate::model::{Caps, Inbound, Outbound, SendError};

/// What an account is for. `Bot`: a conversation with the assistant (Telegram
/// bot, a dedicated Signal number, the WhatsApp self-chat). `Inbox`: the
/// owner's personal account, where other people's messages are third-party
/// content for the event inbox, never commands, and sending needs approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Bot,
    Inbox,
}

pub struct Polled {
    pub events: Vec<Inbound>,
    /// Persisted together with the events; acknowledges them to the platform
    /// only on the *next* poll (or not at all if we crash first).
    pub cursor: Option<String>,
}

#[async_trait]
pub trait Adapter: Send + Sync {
    /// Stable name, also the key of stored rows: "telegram", "signal", "whatsapp", "web".
    fn name(&self) -> &str;
    fn caps(&self) -> Caps;
    /// Is this chat the assistant conversation (vs personal-inbox content)?
    fn role_of(&self, chat_id: &str, direct: bool) -> Role;
    async fn poll(&self, cursor: Option<String>) -> Result<Polled>;
    async fn send(&self, msg: &Outbound) -> std::result::Result<String, SendError>;
    /// Stop a button's spinner (Telegram); no-op elsewhere.
    async fn answer_button(&self, _callback_id: &str, _text: &str) -> Result<()> {
        Ok(())
    }
    /// Typing / "working…" indicator; best effort.
    async fn typing(&self, _chat_id: &str) -> Result<()> {
        Ok(())
    }
    /// Fetch an attachment to a local file (for transcription etc.).
    async fn fetch_attachment(&self, _id: &str, _dest: &std::path::Path) -> Result<()> {
        anyhow::bail!("attachments not supported on this adapter")
    }
}
