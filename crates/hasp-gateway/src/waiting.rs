//! Follow-through: things a task is waiting on, persisted so a restart picks
//! them up where they were.
//!
//! - `user`: the agent asked the owner something. The answer is a reply to
//!   the question message (platform reply), or a message starting with the
//!   item's code (`W3f2a… yes`). It resumes the asking session, not the
//!   personal conversation, whatever order answers arrive in.
//! - `third_party`: waiting for someone else (a contact on the personal
//!   inbox). Their next message from that chat resumes the session, wrapped as
//!   untrusted content.
//! - `scheduled`: a follow-up at a time. Recurring schedules belong to hasp
//!   heartbeats / Dagu; this only tracks one-off due times of waiting items.
//!
//! When due: `remind` tells the owner once at the reply destination; `resume`
//! queues a turn in the session ("follow-up due: …").

use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::model::now_ms;
use crate::store::Store;
use crate::turns::Origin;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Correlate {
    #[serde(default)]
    pub adapter: Option<String>,
    #[serde(default)]
    pub chat_id: Option<String>,
    #[serde(default)]
    pub sender_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Waiting {
    pub id: String,
    pub kind: String,
    pub session_key: String,
    pub question: String,
    pub reply_dest: Origin,
    pub on_due: String,
}

impl Store {
    #[allow(clippy::too_many_arguments)]
    pub fn add_waiting(
        &self,
        kind: &str,
        session: &str,
        task_id: Option<&str>,
        question: &str,
        reply_dest: &Origin,
        correlate: &Correlate,
        due: Option<i64>,
        on_due: &str,
    ) -> Result<String> {
        let id = format!("W{}", &uuid::Uuid::new_v4().simple().to_string()[..6]);
        self.conn().execute(
            "INSERT INTO waiting(id, kind, session_key, task_id, question, reply_dest, correlate, due, on_due, state, created)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'open', ?10)",
            params![id, kind, session, task_id, question, serde_json::to_string(reply_dest)?, serde_json::to_string(correlate)?,
                    due, on_due, now_ms()],
        )?;
        Ok(id)
    }

    pub fn set_waiting_message(&self, id: &str, platform_id: &str) -> Result<()> {
        self.conn().execute("UPDATE waiting SET asked_platform_id = ?2 WHERE id = ?1", params![id, platform_id])?;
        Ok(())
    }

    fn waiting_row(&self, sql: &str, args: &[&dyn rusqlite::ToSql]) -> Option<Waiting> {
        self.conn()
            .query_row(sql, args, |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?, r.get::<_, String>(5)?))
            })
            .optional()
            .ok()
            .flatten()
            .and_then(|(id, kind, s, q, d, on_due)| {
                Some(Waiting { id, kind, session_key: s, question: q, reply_dest: serde_json::from_str(&d).ok()?, on_due })
            })
    }

    /// The open `user` item a message answers: by platform reply to the
    /// question, or by its code at the start of the text.
    pub fn match_user_reply(&self, adapter: &str, reply_to: Option<&str>, text: &str) -> Option<Waiting> {
        const COLS: &str = "SELECT id, kind, session_key, question, reply_dest, on_due FROM waiting";
        if let Some(r) = reply_to
            && let Some(w) = self.waiting_row(
                &format!("{COLS} WHERE state = 'open' AND kind = 'user' AND asked_platform_id = ?1 AND json_extract(reply_dest, '$.adapter') = ?2"),
                &[&r, &adapter],
            )
        {
            return Some(w);
        }
        let code = text.split_whitespace().next()?.trim_end_matches([':', ',']);
        if code.starts_with('W') && code.len() == 7 {
            return self.waiting_row(&format!("{COLS} WHERE state = 'open' AND kind = 'user' AND id = ?1"), &[&code]);
        }
        None
    }

    /// The open `third_party` item waiting on this chat/sender, oldest first.
    pub fn match_third_party(&self, adapter: &str, chat: &str, sender: &str) -> Option<Waiting> {
        self.waiting_row(
            "SELECT id, kind, session_key, question, reply_dest, on_due FROM waiting
             WHERE state = 'open' AND kind = 'third_party'
               AND json_extract(correlate, '$.adapter') = ?1
               AND (json_extract(correlate, '$.chat_id') = ?2 OR json_extract(correlate, '$.sender_id') = ?3)
             ORDER BY created LIMIT 1",
            &[&adapter, &chat, &sender],
        )
    }

    pub fn resolve_waiting(&self, id: &str, state: &str, resolution: &str) -> Result<bool> {
        let n = self.conn().execute(
            "UPDATE waiting SET state = ?2, resolved = ?3, resolution = ?4 WHERE id = ?1 AND state = 'open'",
            params![id, state, now_ms(), resolution],
        )?;
        Ok(n == 1)
    }

    /// Items whose due time passed (claimed: they move to 'due' so a restart
    /// doesn't act on them twice).
    pub fn take_due(&self, now: i64) -> Result<Vec<Waiting>> {
        let rows: Vec<Waiting> = {
            let db = self.conn();
            let mut st = db.prepare(
                "SELECT id, kind, session_key, question, reply_dest, on_due FROM waiting WHERE state = 'open' AND due IS NOT NULL AND due <= ?1",
            )?;
            let r = st.query_map([now], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?, r.get::<_, String>(5)?))
            })?;
            r.filter_map(|x| x.ok())
                .filter_map(|(id, kind, s, q, d, on_due)| Some(Waiting { id, kind, session_key: s, question: q, reply_dest: serde_json::from_str(&d).ok()?, on_due }))
                .collect()
        };
        let mut out = Vec::new();
        for w in rows {
            let n = self.conn().execute("UPDATE waiting SET state = 'due', resolved = ?2 WHERE id = ?1 AND state = 'open'", params![w.id, now])?;
            if n == 1 {
                out.push(w);
            }
        }
        Ok(out)
    }

    pub fn open_waiting(&self, session: Option<&str>) -> Result<Vec<serde_json::Value>> {
        match session {
            Some(s) => self.json_rows(
                "SELECT id, kind, session_key, question, due, state FROM waiting WHERE state = 'open' AND session_key = ?1 ORDER BY created",
                &[&s],
            ),
            None => self.json_rows("SELECT id, kind, session_key, question, due, state FROM waiting WHERE state = 'open' ORDER BY created", &[]),
        }
    }
}
