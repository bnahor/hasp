//! Platform-neutral message types shared by adapters and the gateway core.

use serde::{Deserialize, Serialize};

/// A reference to an attachment kept on the platform side (or on disk by the
/// bridge): fetched only when something needs it, never inlined into history.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AttachmentRef {
    pub id: String,
    pub kind: String, // photo | voice | audio | document | video | other
    #[serde(default)]
    pub mime: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Inbound {
    /// Unique per platform account: Telegram update_id, Signal
    /// `<sourceUuid>:<timestamp>`, WhatsApp `<remoteJid>:<key.id>`.
    pub event_key: String,
    pub chat_id: String,
    #[serde(default)]
    pub thread_id: Option<String>,
    /// Platform-authenticated sender id (never a display name).
    pub sender_id: String,
    /// Display name, for showing only. Never used for identity.
    #[serde(default)]
    pub sender_name: Option<String>,
    /// Sent by this account/bot itself (Telegram is_bot, Signal sync message,
    /// WhatsApp fromMe): never acted on, so the bot can't answer itself.
    #[serde(default)]
    pub from_self: bool,
    /// Direct chat with the bot/account (vs a group).
    #[serde(default)]
    pub direct: bool,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub attachments: Vec<AttachmentRef>,
    /// Platform id of the message this one replies to.
    #[serde(default)]
    pub reply_to: Option<String>,
    /// Platform id of this message (for replies and echo detection).
    #[serde(default)]
    pub message_id: Option<String>,
    /// A button press: (callback id to answer, opaque data we put on the button).
    #[serde(default)]
    pub button: Option<(String, String)>,
    pub ts_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Button {
    pub label: String,
    /// Opaque, short (Telegram allows 64 bytes).
    pub data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Outbound {
    pub chat_id: String,
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub reply_to: Option<String>,
    pub text: String,
    #[serde(default)]
    pub buttons: Vec<Button>,
}

/// What a platform can render. Long replies are split to `max_len`; buttons
/// fall back to numbered commands where unsupported.
#[derive(Debug, Clone, Copy)]
pub struct Caps {
    pub max_len: usize,
    pub buttons: bool,
    pub edits: bool,
    /// Minimum gap between sends to one chat (platform rate limits).
    pub per_chat_interval_ms: u64,
}

#[derive(Debug)]
pub enum SendError {
    /// Definitely not delivered; try again after (at least) this many ms.
    Retry { after_ms: u64, why: String },
    /// Definitely not delivered and won't be (bad chat, blocked bot...).
    Permanent(String),
    /// The request went out but no answer came back: it may have been
    /// delivered. Never silently retried as if it hadn't.
    Uncertain(String),
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SendError::Retry { after_ms, why } => write!(f, "retry in {after_ms} ms: {why}"),
            SendError::Permanent(w) => write!(f, "permanent: {w}"),
            SendError::Uncertain(w) => write!(f, "uncertain: {w}"),
        }
    }
}

/// Map a reqwest error from a *send*: a connect failure never reached the
/// platform; anything after the request went out is uncertain.
pub fn classify_reqwest(e: &reqwest::Error) -> SendError {
    if e.is_connect() || e.is_builder() {
        SendError::Retry { after_ms: 2_000, why: e.to_string() }
    } else {
        SendError::Uncertain(e.to_string())
    }
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

/// Split text for a platform limit, preferring paragraph, then line, then
/// word boundaries; never splits inside a UTF-8 character.
pub fn split_text(text: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text.trim_end();
    while rest.chars().count() > max {
        let cut_byte = rest.char_indices().nth(max).map(|(i, _)| i).unwrap_or(rest.len());
        let window = &rest[..cut_byte];
        let at = window
            .rfind("\n\n")
            .or_else(|| window.rfind('\n'))
            .or_else(|| window.rfind(' '))
            .filter(|&i| i > max / 3)
            .unwrap_or(cut_byte);
        out.push(rest[..at].trim_end().to_string());
        rest = rest[at..].trim_start();
    }
    if !rest.is_empty() || out.is_empty() {
        out.push(rest.to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_boundaries_and_never_inside_a_char() {
        let s = format!("{}\n\n{}", "a".repeat(30), "é".repeat(30));
        let parts = split_text(&s, 40);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0], "a".repeat(30));
        assert!(parts.iter().all(|p| p.chars().count() <= 40));
        assert_eq!(split_text("short", 10), vec!["short"]);
        let long = "x".repeat(95);
        assert!(split_text(&long, 40).iter().all(|p| p.len() <= 40));
    }
}
