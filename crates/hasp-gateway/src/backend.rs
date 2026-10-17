//! The agent side: hasp's daemon over its existing wire protocol. Each
//! gateway session is a hasp resident session in its own workspace directory
//! (`<root>/<hex of session key>`), so the personal conversation, named
//! sessions and tasks each have their own transcript and context.

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Answers a permission question from inside a turn ('y' / 'n').
#[async_trait]
pub trait Asker: Send + Sync {
    async fn ask(&self, question: String) -> char;
}

#[async_trait]
pub trait Backend: Send + Sync {
    async fn prompt(&self, session_key: &str, text: &str, ask: Arc<dyn Asker>) -> Result<String>;
    async fn cancel(&self, session_key: &str) -> Result<()>;
    /// Into the running turn at its next boundary (or the next turn).
    async fn steer(&self, session_key: &str, text: &str) -> Result<()>;
}

pub fn session_dir(root: &Path, session_key: &str) -> PathBuf {
    let hex: String = session_key.bytes().map(|b| format!("{b:02x}")).collect();
    root.join(hex)
}

/// The session a workspace directory belongs to (the inverse of `session_dir`).
/// Used for agent-side API calls: the MCP process reports its working
/// directory, which the model can't change.
pub fn session_of_dir(root: &Path, dir: &Path) -> Option<String> {
    let name = dir.strip_prefix(root).ok()?.components().next()?.as_os_str().to_str()?;
    if name.len() % 2 != 0 {
        return None;
    }
    let bytes: Option<Vec<u8>> = (0..name.len()).step_by(2).map(|i| u8::from_str_radix(&name[i..i + 2], 16).ok()).collect();
    String::from_utf8(bytes?).ok()
}

pub struct HaspDaemon {
    pub socket: PathBuf,
    pub root: PathBuf,
}

impl HaspDaemon {
    async fn open(&self, session_key: &str) -> Result<hasp_wire::WireClient> {
        let dir = session_dir(&self.root, session_key);
        std::fs::create_dir_all(&dir)?;
        let mut c = hasp_wire::WireClient::connect(&self.socket).await.context("hasp daemon")?;
        c.initialize().await?;
        c.request(hasp_wire::METHOD_SESSION_CONTINUE, serde_json::json!({"cwd": dir})).await?;
        Ok(c)
    }
}

#[async_trait]
impl Backend for HaspDaemon {
    async fn prompt(&self, session_key: &str, text: &str, ask: Arc<dyn Asker>) -> Result<String> {
        let mut c = self.open(session_key).await?;
        let res = c
            .request_with(
                hasp_wire::METHOD_SESSION_PROMPT,
                serde_json::json!({"prompt": text}),
                |q| {
                    let ask = ask.clone();
                    async move { ask.ask(q).await }
                },
                |_| {},
            )
            .await?;
        if res["aborted"].as_bool() == Some(true) {
            bail!("stopped");
        }
        Ok(res["text"].as_str().unwrap_or("").to_string())
    }

    async fn cancel(&self, session_key: &str) -> Result<()> {
        let mut c = self.open(session_key).await?;
        c.request(hasp_wire::METHOD_SESSION_CANCEL, serde_json::json!({})).await?;
        Ok(())
    }

    async fn steer(&self, session_key: &str, text: &str) -> Result<()> {
        let mut c = self.open(session_key).await?;
        c.request(hasp_wire::METHOD_SESSION_STEER, serde_json::json!({"text": text})).await?;
        Ok(())
    }
}

/// Speech-to-text for voice notes. None configured = the note is passed on
/// as an attachment reference with a "no transcriber" remark.
#[async_trait]
pub trait Transcriber: Send + Sync {
    async fn transcribe(&self, audio: &Path) -> Result<String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_dirs_round_trip() {
        let root = Path::new("/r");
        for k in ["personal:owner", "task:t1", "named:owner:trip planning"] {
            assert_eq!(session_of_dir(root, &session_dir(root, k)).as_deref(), Some(k));
        }
        assert_eq!(session_of_dir(root, Path::new("/elsewhere/x")), None);
    }
}
