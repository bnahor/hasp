//! Durable outbound delivery: at-least-once with visible uncertainty.
//!
//! pending -> sending (persisted, with the leader's fencing token, *before*
//! the network call) -> sent (platform id saved, also used for echo
//! detection) | pending again (retryable, with backoff / retry_after) |
//! failed (permanent, or out of attempts) | uncertain (the request went out
//! and no answer came back, or the gateway died mid-send).
//!
//! Uncertain messages are never silently resent as if they hadn't been sent:
//! `resend_uncertain` creates one marked copy ("↻ resent; you may see this
//! twice") and keeps the original row as uncertain for the record.

use anyhow::Result;
use rusqlite::{OptionalExtension, params};

use crate::model::{Outbound, now_ms};
use crate::store::Store;

pub const MAX_ATTEMPTS: i64 = 8;
pub const GIVE_UP_AFTER_MS: i64 = 24 * 3600 * 1000;
pub const RESEND_MARKER: &str = "↻ resent after a connection problem; you may see this twice\n\n";

#[derive(Debug, Clone)]
pub struct Pending {
    pub id: i64,
    pub adapter: String,
    pub message: Outbound,
    pub attempts: i64,
    pub created: i64,
}

impl Store {
    pub fn enqueue_out(&self, adapter: &str, msg: &Outbound, dedupe_key: &str) -> Result<bool> {
        let n = self.conn().execute(
            "INSERT OR IGNORE INTO outbox(dedupe_key, adapter, message, state, next_attempt, created)
             VALUES (?1, ?2, ?3, 'pending', 0, ?4)",
            params![dedupe_key, adapter, serde_json::to_string(msg)?, now_ms()],
        )?;
        Ok(n == 1)
    }

    pub fn due_out(&self, adapter: &str, now: i64, limit: usize) -> Result<Vec<Pending>> {
        let db = self.conn();
        let mut st = db.prepare(
            "SELECT id, adapter, message, attempts, created FROM outbox
             WHERE adapter = ?1 AND state = 'pending' AND next_attempt <= ?2 ORDER BY id LIMIT ?3",
        )?;
        let rows = st.query_map(params![adapter, now, limit as i64], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?, r.get::<_, i64>(4)?))
        })?;
        let mut out = Vec::new();
        for r in rows {
            let (id, adapter, m, attempts, created) = r?;
            out.push(Pending { id, adapter, message: serde_json::from_str(&m)?, attempts, created });
        }
        Ok(out)
    }

    /// Persist "about to send" with the fencing token we hold. Returns false
    /// if someone else already moved this row (another leader).
    pub fn mark_sending(&self, id: i64, fencing: u64) -> Result<bool> {
        let n = self.conn().execute(
            "UPDATE outbox SET state = 'sending', attempts = attempts + 1, fencing = ?2 WHERE id = ?1 AND state = 'pending'",
            params![id, fencing as i64],
        )?;
        Ok(n == 1)
    }

    pub fn mark_sent(&self, id: i64, platform_id: &str) -> Result<()> {
        let db = self.conn();
        db.execute("UPDATE outbox SET state = 'sent', platform_id = ?2, sent = ?3 WHERE id = ?1", params![id, platform_id, now_ms()])?;
        let (adapter, msg): (String, String) = db.query_row("SELECT adapter, message FROM outbox WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let m: Outbound = serde_json::from_str(&msg)?;
        db.execute("INSERT OR IGNORE INTO sent_ids VALUES (?1, ?2, ?3)", params![adapter, m.chat_id, platform_id])?;
        // A turn is delivered once all of its messages are sent.
        db.execute(
            "UPDATE turns SET state = 'delivered' WHERE id = (SELECT turn_id FROM outbox WHERE id = ?1)
               AND NOT EXISTS (SELECT 1 FROM outbox o WHERE o.turn_id = turns.id AND o.state != 'sent')",
            [id],
        )?;
        Ok(())
    }

    pub fn mark_retry(&self, p: &Pending, after_ms: u64, why: &str) -> Result<()> {
        let give_up = p.attempts + 1 >= MAX_ATTEMPTS || now_ms() - p.created > GIVE_UP_AFTER_MS;
        if give_up {
            return self.mark_failed(p.id, &format!("gave up after {} attempts: {why}", p.attempts + 1));
        }
        // exponential backoff, at least what the platform asked for
        let backoff = (1_000u64 << p.attempts.min(10)).min(10 * 60_000).max(after_ms);
        self.conn().execute(
            "UPDATE outbox SET state = 'pending', next_attempt = ?2, last_error = ?3 WHERE id = ?1",
            params![p.id, now_ms() + backoff as i64, why],
        )?;
        Ok(())
    }

    pub fn mark_failed(&self, id: i64, why: &str) -> Result<()> {
        self.conn().execute("UPDATE outbox SET state = 'failed', last_error = ?2 WHERE id = ?1", params![id, why])?;
        Ok(())
    }

    pub fn mark_uncertain(&self, id: i64, why: &str) -> Result<()> {
        self.conn().execute("UPDATE outbox SET state = 'uncertain', last_error = ?2 WHERE id = ?1", params![id, why])?;
        Ok(())
    }

    /// One marked resend per uncertain message (at-least-once, visibly).
    pub fn resend_uncertain(&self) -> Result<usize> {
        let rows: Vec<(i64, String, String)> = {
            let db = self.conn();
            let mut st = db.prepare(
                "SELECT id, adapter, message FROM outbox o WHERE state = 'uncertain'
                 AND NOT EXISTS (SELECT 1 FROM outbox r WHERE r.resend_of = o.id)",
            )?;
            let r = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            r.collect::<Result<_, _>>()?
        };
        for (id, adapter, m) in &rows {
            let mut msg: Outbound = serde_json::from_str(m)?;
            msg.text = format!("{RESEND_MARKER}{}", msg.text);
            self.conn().execute(
                "INSERT OR IGNORE INTO outbox(dedupe_key, adapter, message, state, next_attempt, created, resend_of)
                 VALUES (?1, ?2, ?3, 'pending', 0, ?4, ?5)",
                params![format!("resend:{id}"), adapter, serde_json::to_string(&msg)?, now_ms(), id],
            )?;
        }
        Ok(rows.len())
    }

    pub fn outbox_state(&self, dedupe_key: &str) -> Option<String> {
        self.conn().query_row("SELECT state FROM outbox WHERE dedupe_key = ?1", [dedupe_key], |r| r.get(0)).optional().ok().flatten()
    }
}
