//! Turns: one agent run per inbound message (or event / follow-up), executed
//! one at a time per session, with the response saved before delivery.
//!
//! States: queued -> running -> responded (response + outbox rows written in
//! one transaction) -> delivered | failed | cancelled | interrupted.
//! After a crash: `running` turns become `interrupted` and are NOT re-run (the
//! effects of a half-finished turn are unknown); the person is told and can
//! `/resume`. Responded turns are delivered from the saved response, never by
//! asking the agent again.

use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::model::{Outbound, now_ms};
use crate::store::Store;

/// Where a turn's reply goes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Origin {
    pub adapter: String,
    pub chat_id: String,
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub reply_to: Option<String>,
    pub person: String,
}

#[derive(Debug, Clone)]
pub struct Turn {
    pub id: i64,
    pub session_key: String,
    pub origin: Origin,
    pub prompt: String,
    pub kind: String,
}

impl Store {
    /// Queue a turn. `inbound_id` makes it idempotent per inbound event.
    pub fn enqueue_turn(&self, session: &str, inbound_id: Option<i64>, origin: &Origin, prompt: &str, kind: &str) -> Result<Option<i64>> {
        let db = self.conn();
        let n = db.execute(
            "INSERT OR IGNORE INTO turns(session_key, inbound_id, origin, prompt, kind, state, created)
             VALUES (?1, ?2, ?3, ?4, ?5, 'queued', ?6)",
            params![session, inbound_id, serde_json::to_string(origin)?, prompt, kind, now_ms()],
        )?;
        Ok((n == 1).then(|| db.last_insert_rowid()))
    }

    pub fn session_busy(&self, session: &str) -> bool {
        self.conn()
            .query_row("SELECT 1 FROM turns WHERE session_key = ?1 AND state = 'running'", [session], |_| Ok(()))
            .optional()
            .ok()
            .flatten()
            .is_some()
    }

    /// Claim the next turn of every idle session (oldest queued first), marking
    /// them running in one transaction so two loops can't start the same one.
    pub fn claim_runnable(&self) -> Result<Vec<Turn>> {
        let mut db = self.conn();
        let tx = db.transaction()?;
        let picked: Vec<(i64, String, String, String, String)> = {
            let mut st = tx.prepare(
                "SELECT t.id, t.session_key, t.origin, t.prompt, t.kind FROM turns t
                 WHERE t.state = 'queued'
                   AND t.id = (SELECT MIN(id) FROM turns q WHERE q.session_key = t.session_key AND q.state = 'queued')
                   AND NOT EXISTS (SELECT 1 FROM turns r WHERE r.session_key = t.session_key AND r.state = 'running')",
            )?;
            let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?;
            rows.collect::<Result<_, _>>()?
        };
        let mut out = Vec::new();
        for (id, session, origin, prompt, kind) in picked {
            tx.execute("UPDATE turns SET state = 'running', started = ?2 WHERE id = ?1", params![id, now_ms()])?;
            out.push(Turn { id, session_key: session, origin: serde_json::from_str(&origin)?, prompt, kind });
        }
        tx.commit()?;
        Ok(out)
    }

    /// Save the response and its outbound messages atomically.
    pub fn respond(&self, turn: i64, response: &str, adapter: &str, messages: &[Outbound]) -> Result<()> {
        let mut db = self.conn();
        let tx = db.transaction()?;
        tx.execute(
            "UPDATE turns SET state = 'responded', response = ?2, finished = ?3 WHERE id = ?1 AND state = 'running'",
            params![turn, response, now_ms()],
        )?;
        for (i, m) in messages.iter().enumerate() {
            tx.execute(
                "INSERT OR IGNORE INTO outbox(dedupe_key, adapter, message, state, next_attempt, created, turn_id)
                 VALUES (?1, ?2, ?3, 'pending', 0, ?4, ?5)",
                params![format!("turn:{turn}:{i}"), adapter, serde_json::to_string(m)?, now_ms(), turn],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn finish_turn(&self, turn: i64, state: &str, error: Option<&str>) -> Result<()> {
        self.conn().execute(
            "UPDATE turns SET state = ?2, error = ?3, finished = COALESCE(finished, ?4) WHERE id = ?1",
            params![turn, state, error, now_ms()],
        )?;
        Ok(())
    }

    /// Startup recovery. Returns the turns that were interrupted (for notices).
    pub fn recover(&self) -> Result<Vec<Turn>> {
        let interrupted: Vec<Turn> = {
            let db = self.conn();
            let mut st = db.prepare("SELECT id, session_key, origin, prompt, kind FROM turns WHERE state = 'running'")?;
            let rows = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?)))?;
            rows.filter_map(|r| r.ok())
                .filter_map(|(id, s, o, p, k)| Some(Turn { id, session_key: s, origin: serde_json::from_str(&o).ok()?, prompt: p, kind: k }))
                .collect()
        };
        let db = self.conn();
        db.execute("UPDATE turns SET state = 'interrupted', error = 'gateway restarted while this was running' WHERE state = 'running'", [])?;
        // A send that was in flight when we died may or may not have arrived.
        db.execute("UPDATE outbox SET state = 'uncertain', last_error = 'gateway restarted mid-send' WHERE state = 'sending'", [])?;
        Ok(interrupted)
    }

    /// `/resume`: queue the session's most recent interrupted turn again (an
    /// explicit decision by the person, never automatic).
    pub fn resume_interrupted(&self, session: &str) -> Result<Option<i64>> {
        let db = self.conn();
        let row: Option<(i64, String, String, String)> = db
            .query_row(
                "SELECT id, origin, prompt, kind FROM turns WHERE session_key = ?1 AND state = 'interrupted' ORDER BY id DESC LIMIT 1",
                [session],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let Some((old, origin, prompt, kind)) = row else { return Ok(None) };
        db.execute("UPDATE turns SET state = 'resumed' WHERE id = ?1", [old])?;
        db.execute(
            "INSERT INTO turns(session_key, origin, prompt, kind, state, created) VALUES (?1, ?2, ?3, ?4, 'queued', ?5)",
            params![session, origin, prompt, kind, now_ms()],
        )?;
        Ok(Some(db.last_insert_rowid()))
    }

    pub fn turn_status(&self, session: &str) -> Result<Vec<serde_json::Value>> {
        self.json_rows(
            "SELECT id, kind, state, substr(prompt, 1, 80) AS prompt, created, started, finished FROM turns
             WHERE session_key = ?1 ORDER BY id DESC LIMIT 5",
            &[&session],
        )
    }
}
