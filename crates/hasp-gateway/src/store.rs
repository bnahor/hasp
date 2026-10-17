//! The gateway's durable state: one SQLite file, written only by the active
//! gateway (see `leader.rs`). Everything that must survive a crash goes here
//! *before* it is acknowledged or acted on.

use anyhow::{Result, anyhow};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::model::{Inbound, now_ms};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS cursors (adapter TEXT PRIMARY KEY, value TEXT NOT NULL, updated INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS inbound (
    id INTEGER PRIMARY KEY, adapter TEXT NOT NULL, event_key TEXT NOT NULL,
    chat_id TEXT NOT NULL, sender_id TEXT NOT NULL, payload TEXT NOT NULL,
    received INTEGER NOT NULL, state TEXT NOT NULL DEFAULT 'new', note TEXT,
    UNIQUE (adapter, event_key));
CREATE TABLE IF NOT EXISTS persons (id TEXT PRIMARY KEY, label TEXT NOT NULL, role TEXT NOT NULL, created INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS identities (
    adapter TEXT NOT NULL, sender_id TEXT NOT NULL, person_id TEXT NOT NULL,
    linked_by TEXT NOT NULL, linked INTEGER NOT NULL, PRIMARY KEY (adapter, sender_id));
CREATE TABLE IF NOT EXISTS link_codes (
    code_hash TEXT PRIMARY KEY, person_id TEXT NOT NULL, expires INTEGER NOT NULL, used INTEGER);
CREATE TABLE IF NOT EXISTS conversations (
    adapter TEXT NOT NULL, chat_id TEXT NOT NULL, thread_id TEXT NOT NULL DEFAULT '',
    person_id TEXT NOT NULL, session_key TEXT NOT NULL, updated INTEGER NOT NULL,
    PRIMARY KEY (adapter, chat_id, thread_id));
CREATE TABLE IF NOT EXISTS turns (
    id INTEGER PRIMARY KEY, session_key TEXT NOT NULL, inbound_id INTEGER UNIQUE,
    origin TEXT NOT NULL, prompt TEXT NOT NULL, kind TEXT NOT NULL,
    state TEXT NOT NULL, response TEXT, error TEXT,
    created INTEGER NOT NULL, started INTEGER, finished INTEGER);
CREATE INDEX IF NOT EXISTS turns_by_session ON turns(session_key, state);
CREATE TABLE IF NOT EXISTS outbox (
    id INTEGER PRIMARY KEY, dedupe_key TEXT NOT NULL UNIQUE, adapter TEXT NOT NULL,
    message TEXT NOT NULL, state TEXT NOT NULL, attempts INTEGER NOT NULL DEFAULT 0,
    next_attempt INTEGER NOT NULL, platform_id TEXT, last_error TEXT,
    created INTEGER NOT NULL, sent INTEGER, turn_id INTEGER, resend_of INTEGER, fencing INTEGER);
CREATE TABLE IF NOT EXISTS sent_ids (adapter TEXT NOT NULL, chat_id TEXT NOT NULL, platform_id TEXT NOT NULL,
    PRIMARY KEY (adapter, chat_id, platform_id));
CREATE TABLE IF NOT EXISTS approvals (
    id TEXT PRIMARY KEY, nonce_hash TEXT NOT NULL, kind TEXT NOT NULL, action TEXT NOT NULL,
    action_hash TEXT NOT NULL, session_key TEXT, task_id TEXT, scope TEXT NOT NULL,
    requested_by TEXT NOT NULL, owner_person TEXT NOT NULL, created INTEGER NOT NULL,
    expires INTEGER NOT NULL, state TEXT NOT NULL, decided_by TEXT, decided_via TEXT,
    decided INTEGER, result TEXT);
CREATE TABLE IF NOT EXISTS drafts (
    id TEXT PRIMARY KEY, session_key TEXT NOT NULL, adapter TEXT NOT NULL, recipient TEXT NOT NULL,
    text TEXT NOT NULL, context TEXT, created INTEGER NOT NULL, state TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS waiting (
    id TEXT PRIMARY KEY, kind TEXT NOT NULL, session_key TEXT NOT NULL, task_id TEXT,
    question TEXT NOT NULL, reply_dest TEXT NOT NULL, correlate TEXT NOT NULL,
    due INTEGER, on_due TEXT NOT NULL, state TEXT NOT NULL, created INTEGER NOT NULL,
    resolved INTEGER, resolution TEXT, asked_platform_id TEXT);
CREATE TABLE IF NOT EXISTS events (
    id INTEGER PRIMARY KEY, source TEXT NOT NULL, dedupe_key TEXT NOT NULL, kind TEXT NOT NULL,
    occurred INTEGER NOT NULL, title TEXT NOT NULL, body TEXT NOT NULL, data TEXT NOT NULL,
    trust TEXT NOT NULL, state TEXT NOT NULL, rule TEXT, decided INTEGER,
    UNIQUE (source, dedupe_key));
CREATE TABLE IF NOT EXISTS rules (
    id TEXT PRIMARY KEY, source TEXT NOT NULL, kind TEXT NOT NULL, contains TEXT,
    action TEXT NOT NULL, quiet_exempt INTEGER NOT NULL DEFAULT 0, priority INTEGER NOT NULL DEFAULT 0,
    enabled INTEGER NOT NULL DEFAULT 1);
CREATE TABLE IF NOT EXISTS audit (
    id INTEGER PRIMARY KEY, ts INTEGER NOT NULL, actor TEXT NOT NULL, action TEXT NOT NULL,
    subject TEXT, detail TEXT);
"#;

#[derive(Clone)]
pub struct Store {
    db: Arc<Mutex<Connection>>,
}

pub fn sha256(s: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        if !path.exists() {
            std::fs::File::create(path)?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        let db = Connection::open(path)?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000; PRAGMA foreign_keys=ON;")?;
        db.execute_batch(SCHEMA)?;
        if db.query_row("PRAGMA quick_check", [], |r| r.get::<_, String>(0))? != "ok" {
            return Err(anyhow!("{} failed its integrity check; refusing to start", path.display()));
        }
        Ok(Self { db: Arc::new(Mutex::new(db)) })
    }

    pub fn conn(&self) -> MutexGuard<'_, Connection> {
        self.db.lock().expect("store lock")
    }

    pub fn audit(&self, actor: &str, action: &str, subject: &str, detail: &str) {
        let _ = self.conn().execute(
            "INSERT INTO audit(ts, actor, action, subject, detail) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![now_ms(), actor, action, subject, detail],
        );
    }

    // ------------------------------------------------------------ inbound
    /// Persist a batch of platform events, then the cursor that acknowledges
    /// them, in one transaction: an event is never acked before it is stored.
    /// Duplicates (same adapter + event key) are dropped. Returns new row ids.
    pub fn ingest(&self, adapter: &str, events: &[Inbound], cursor: Option<&str>) -> Result<Vec<i64>> {
        let mut db = self.conn();
        let tx = db.transaction()?;
        let mut ids = Vec::new();
        for e in events {
            let n = tx.execute(
                "INSERT OR IGNORE INTO inbound(adapter, event_key, chat_id, sender_id, payload, received)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![adapter, e.event_key, e.chat_id, e.sender_id, serde_json::to_string(e)?, now_ms()],
            )?;
            if n == 1 {
                ids.push(tx.last_insert_rowid());
            }
        }
        if let Some(c) = cursor {
            tx.execute(
                "INSERT INTO cursors(adapter, value, updated) VALUES (?1, ?2, ?3)
                 ON CONFLICT(adapter) DO UPDATE SET value = excluded.value, updated = excluded.updated",
                params![adapter, c, now_ms()],
            )?;
        }
        tx.commit()?;
        Ok(ids)
    }

    pub fn cursor(&self, adapter: &str) -> Option<String> {
        self.conn()
            .query_row("SELECT value FROM cursors WHERE adapter = ?1", [adapter], |r| r.get(0))
            .optional()
            .ok()
            .flatten()
    }

    /// Inbound rows not yet routed, oldest first.
    pub fn pending_inbound(&self, limit: usize) -> Result<Vec<(i64, String, Inbound)>> {
        let db = self.conn();
        let mut st = db.prepare("SELECT id, adapter, payload FROM inbound WHERE state = 'new' ORDER BY id LIMIT ?1")?;
        let rows = st.query_map([limit as i64], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?;
        let mut out = Vec::new();
        for r in rows {
            let (id, a, p) = r?;
            out.push((id, a, serde_json::from_str(&p)?));
        }
        Ok(out)
    }

    pub fn mark_inbound(&self, id: i64, state: &str, note: &str) -> Result<()> {
        self.conn().execute("UPDATE inbound SET state = ?2, note = ?3 WHERE id = ?1", params![id, state, note])?;
        Ok(())
    }

    // ------------------------------------------------------------ echo detection
    pub fn remember_sent(&self, adapter: &str, chat: &str, platform_id: &str) -> Result<()> {
        self.conn().execute("INSERT OR IGNORE INTO sent_ids VALUES (?1, ?2, ?3)", params![adapter, chat, platform_id])?;
        Ok(())
    }

    pub fn was_sent_by_us(&self, adapter: &str, chat: &str, platform_id: &str) -> bool {
        self.conn()
            .query_row("SELECT 1 FROM sent_ids WHERE adapter = ?1 AND chat_id = ?2 AND platform_id = ?3",
                       params![adapter, chat, platform_id], |_| Ok(()))
            .optional()
            .ok()
            .flatten()
            .is_some()
    }

    // ------------------------------------------------------------ settings
    pub fn get_meta(&self, key: &str) -> Option<String> {
        self.conn().query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0)).optional().ok().flatten()
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn().execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn json_rows(&self, sql: &str, args: &[&dyn rusqlite::ToSql]) -> Result<Vec<Value>> {
        let db = self.conn();
        let mut st = db.prepare(sql)?;
        let names: Vec<String> = st.column_names().iter().map(|s| s.to_string()).collect();
        let rows = st.query_map(args, |r| {
            let mut m = serde_json::Map::new();
            for (i, n) in names.iter().enumerate() {
                let v: rusqlite::types::Value = r.get(i)?;
                m.insert(
                    n.clone(),
                    match v {
                        rusqlite::types::Value::Null => Value::Null,
                        rusqlite::types::Value::Integer(i) => Value::from(i),
                        rusqlite::types::Value::Real(f) => Value::from(f),
                        rusqlite::types::Value::Text(t) => Value::from(t),
                        rusqlite::types::Value::Blob(_) => Value::from("<blob>"),
                    },
                );
            }
            Ok(Value::Object(m))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}
