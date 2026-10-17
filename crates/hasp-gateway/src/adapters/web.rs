//! The local web view as a channel: what the owner types there is ingested
//! like any platform message (adapter "web", sender "owner-web"), and replies
//! are stored for the page to show. Same pipeline, same audit trail.

use anyhow::Result;
use async_trait::async_trait;
use rusqlite::params;

use super::{Adapter, Polled, Role};
use crate::model::{Caps, Outbound, SendError, now_ms};
use crate::store::Store;

pub const WEB_SENDER: &str = "owner-web";

pub struct Web {
    store: Store,
}

impl Web {
    pub fn new(store: Store) -> Self {
        let _ = store.conn().execute_batch(
            "CREATE TABLE IF NOT EXISTS web_messages (id INTEGER PRIMARY KEY, chat_id TEXT NOT NULL, dir TEXT NOT NULL,
               text TEXT NOT NULL, ts INTEGER NOT NULL);",
        );
        Self { store }
    }
}

#[async_trait]
impl Adapter for Web {
    fn name(&self) -> &str {
        "web"
    }
    fn caps(&self) -> Caps {
        Caps { max_len: 100_000, buttons: true, edits: false, per_chat_interval_ms: 0 }
    }
    fn role_of(&self, _chat_id: &str, _direct: bool) -> Role {
        Role::Bot
    }
    async fn poll(&self, cursor: Option<String>) -> Result<Polled> {
        Ok(Polled { events: vec![], cursor }) // the web API ingests directly
    }
    async fn send(&self, msg: &Outbound) -> std::result::Result<String, SendError> {
        let db = self.store.conn();
        db.execute("INSERT INTO web_messages(chat_id, dir, text, ts) VALUES (?1, 'out', ?2, ?3)", params![msg.chat_id, msg.text, now_ms()])
            .map_err(|e| SendError::Retry { after_ms: 1000, why: e.to_string() })?;
        Ok(format!("web-{}", db.last_insert_rowid()))
    }
}
