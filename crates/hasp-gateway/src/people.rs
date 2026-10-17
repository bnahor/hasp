//! Who is talking: people, their linked platform identities, conversations.
//!
//! Identity is only ever established by a platform-authenticated sender id
//! (Telegram user id, Signal ACI, WhatsApp JID) being linked to a person by
//! the owner, through a one-time code issued from an owner surface (the CLI on
//! the gateway machine, or an already-linked owner chat). Display names are
//! never used: two accounts called "Mum" are two strangers until linked.

use anyhow::{Result, bail};
use rusqlite::{OptionalExtension, params};

use crate::model::now_ms;
use crate::store::{Store, sha256};

pub const OWNER: &str = "owner";

#[derive(Debug, Clone, PartialEq)]
pub struct Person {
    pub id: String,
    pub role: String, // owner | member
}

impl Store {
    pub fn ensure_owner(&self, label: &str) -> Result<()> {
        self.conn().execute(
            "INSERT OR IGNORE INTO persons(id, label, role, created) VALUES (?1, ?2, 'owner', ?3)",
            params![OWNER, label, now_ms()],
        )?;
        Ok(())
    }

    pub fn add_member(&self, id: &str, label: &str) -> Result<()> {
        if id == OWNER {
            bail!("reserved id");
        }
        self.conn().execute(
            "INSERT OR IGNORE INTO persons(id, label, role, created) VALUES (?1, ?2, 'member', ?3)",
            params![id, label, now_ms()],
        )?;
        Ok(())
    }

    /// A one-time code that links whichever platform account sends it to
    /// `person_id`. Only its hash is stored.
    pub fn issue_link_code(&self, person_id: &str, ttl_ms: i64, by: &str) -> Result<String> {
        let code: String = uuid::Uuid::new_v4().simple().to_string()[..10].to_uppercase();
        self.conn().execute(
            "INSERT INTO link_codes(code_hash, person_id, expires) VALUES (?1, ?2, ?3)",
            params![sha256(&code), person_id, now_ms() + ttl_ms],
        )?;
        self.audit(by, "link_code_issued", person_id, "");
        Ok(code)
    }

    pub fn redeem_link_code(&self, adapter: &str, sender_id: &str, code: &str) -> Result<String> {
        let mut db = self.conn();
        let tx = db.transaction()?;
        let row: Option<(String, i64, Option<i64>)> = tx
            .query_row(
                "SELECT person_id, expires, used FROM link_codes WHERE code_hash = ?1",
                [sha256(code.trim().to_uppercase().as_str())],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((person, expires, used)) = row else { bail!("unknown code") };
        if used.is_some() {
            bail!("code already used");
        }
        if expires < now_ms() {
            bail!("code expired");
        }
        if let Some(existing) = tx
            .query_row("SELECT person_id FROM identities WHERE adapter = ?1 AND sender_id = ?2", params![adapter, sender_id], |r| {
                r.get::<_, String>(0)
            })
            .optional()?
            && existing != person
        {
            bail!("this account is already linked to someone else");
        }
        tx.execute("UPDATE link_codes SET used = ?2 WHERE code_hash = ?1", params![sha256(code.trim().to_uppercase().as_str()), now_ms()])?;
        tx.execute(
            "INSERT OR REPLACE INTO identities(adapter, sender_id, person_id, linked_by, linked) VALUES (?1, ?2, ?3, 'code', ?4)",
            params![adapter, sender_id, person, now_ms()],
        )?;
        tx.commit()?;
        drop(db);
        self.audit(&format!("{adapter}:{sender_id}"), "identity_linked", &person, "");
        Ok(person)
    }

    pub fn person_for(&self, adapter: &str, sender_id: &str) -> Option<Person> {
        self.conn()
            .query_row(
                "SELECT p.id, p.role FROM identities i JOIN persons p ON p.id = i.person_id
                 WHERE i.adapter = ?1 AND i.sender_id = ?2",
                params![adapter, sender_id],
                |r| Ok(Person { id: r.get(0)?, role: r.get(1)? }),
            )
            .optional()
            .ok()
            .flatten()
    }

    /// For surfaces that authenticate the owner themselves (the web view's
    /// owner token): bind that surface's fixed sender id to the owner.
    pub fn link_surface(&self, adapter: &str, sender_id: &str, by: &str) -> Result<()> {
        self.conn().execute(
            "INSERT OR IGNORE INTO identities(adapter, sender_id, person_id, linked_by, linked) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![adapter, sender_id, OWNER, by, now_ms()],
        )?;
        Ok(())
    }

    pub fn unlink(&self, adapter: &str, sender_id: &str, by: &str) -> Result<()> {
        self.conn().execute("DELETE FROM identities WHERE adapter = ?1 AND sender_id = ?2", params![adapter, sender_id])?;
        self.audit(by, "identity_unlinked", &format!("{adapter}:{sender_id}"), "");
        Ok(())
    }

    /// The session a chat talks to: the person's one personal conversation
    /// unless they selected another session in this chat.
    pub fn session_for(&self, adapter: &str, chat: &str, thread: Option<&str>, person: &str) -> String {
        self.conn()
            .query_row(
                "SELECT session_key FROM conversations WHERE adapter = ?1 AND chat_id = ?2 AND thread_id = ?3 AND person_id = ?4",
                params![adapter, chat, thread.unwrap_or(""), person],
                |r| r.get(0),
            )
            .optional()
            .ok()
            .flatten()
            .unwrap_or_else(|| personal_session(person))
    }

    pub fn select_session(&self, adapter: &str, chat: &str, thread: Option<&str>, person: &str, key: &str) -> Result<()> {
        if !key.starts_with(&format!("personal:{person}")) && !key.starts_with(&format!("named:{person}:")) && !key.starts_with("task:") {
            bail!("not one of your sessions");
        }
        self.conn().execute(
            "INSERT INTO conversations(adapter, chat_id, thread_id, person_id, session_key, updated) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(adapter, chat_id, thread_id) DO UPDATE SET session_key = excluded.session_key, person_id = excluded.person_id, updated = excluded.updated",
            params![adapter, chat, thread.unwrap_or(""), person, key, now_ms()],
        )?;
        Ok(())
    }

    pub fn sessions_of(&self, person: &str) -> Vec<String> {
        let mut out = vec![personal_session(person)];
        if let Ok(rows) = self.json_rows(
            "SELECT DISTINCT session_key FROM turns WHERE session_key LIKE ?1 OR session_key LIKE 'task:%' ORDER BY session_key",
            &[&format!("named:{person}:%")],
        ) {
            out.extend(rows.into_iter().filter_map(|r| r["session_key"].as_str().map(String::from)));
        }
        out.dedup();
        out
    }
}

pub fn personal_session(person: &str) -> String {
    format!("personal:{person}")
}
