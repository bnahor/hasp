//! The permission authority: deterministic code outside the agent's
//! workspace that decides whether a proposed action may happen.
//!
//! - Only the agent side can *request* (MCP tools, hasp permission prompts);
//!   only an authenticated owner can *decide*: a platform identity linked to
//!   the owner, or the owner token of the local web view. The agent has no
//!   path to `decide`, and messages the bot itself sent are dropped as echoes.
//! - A decision is bound to the card id, a one-time nonce shown only on the
//!   card, the exact action (its hash is re-checked at decision and at
//!   execution), the requesting session/task, a scope and an expiry.
//! - One use: the transition pending -> approved happens once (a conditional
//!   UPDATE); replays, late answers, other people and edited actions fail.
//! - Third-party messages are never decisions, whatever they say.

use anyhow::{Result, anyhow, bail};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

use crate::model::{Button, now_ms};
use crate::people::OWNER;
use crate::store::{Store, sha256};

#[derive(Debug, Clone)]
pub struct Card {
    pub id: String,
    pub nonce: String,
    pub text: String,
    pub buttons: Vec<Button>,
    pub expires: i64,
}

/// Who is deciding, as authenticated by the surface it arrived on.
#[derive(Debug, Clone)]
pub struct Decider {
    pub via: String, // "telegram", "signal", "whatsapp", "web", "cli"
    /// The linked person, if the surface authenticated one.
    pub person: Option<String>,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Approve,
    Deny,
}

#[derive(Debug)]
pub struct Decided {
    pub id: String,
    pub kind: String,
    pub action: Value,
    pub session_key: Option<String>,
    pub decision: Decision,
}

/// Canonical form: keys sorted (serde_json's map is ordered), no whitespace.
pub fn action_hash(kind: &str, action: &Value) -> String {
    sha256(&format!("{kind}\n{}", canonical(action)))
}

fn canonical(v: &Value) -> String {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys.iter().map(|k| format!("{}:{}", json!(k), canonical(&m[*k]))).collect();
            format!("{{{}}}", parts.join(","))
        }
        Value::Array(a) => format!("[{}]", a.iter().map(canonical).collect::<Vec<_>>().join(",")),
        other => other.to_string(),
    }
}

/// What the owner sees: destination and content verbatim, then context.
pub fn render(kind: &str, action: &Value, context: &str, expires_in_min: i64) -> String {
    let body = match kind {
        "send_message" => format!(
            "Send a message as you?\nTo: {} on {}\n---\n{}\n---",
            action["recipient"].as_str().unwrap_or("?"),
            action["adapter"].as_str().unwrap_or("?"),
            action["text"].as_str().unwrap_or("")
        ),
        "hasp_permission" => format!("The agent asks to:\n{}", action["question"].as_str().unwrap_or("")),
        "memory_rule" => format!("Save a standing rule?\n\"{}\"", action["text"].as_str().unwrap_or("")),
        _ => format!("{kind}:\n{}", serde_json::to_string_pretty(action).unwrap_or_default()),
    };
    let ctx = if context.is_empty() { String::new() } else { format!("\nWhy: {context}") };
    format!("{body}{ctx}\nExpires in {expires_in_min} min.")
}

impl Store {
    /// Create a pending card for the owner. Returns the card to deliver.
    #[allow(clippy::too_many_arguments)]
    pub fn propose(
        &self,
        kind: &str,
        action: &Value,
        context: &str,
        session_key: Option<&str>,
        task_id: Option<&str>,
        scope: &str,
        requested_by: &str,
        ttl_ms: i64,
    ) -> Result<Card> {
        let id = format!("A{}", &uuid::Uuid::new_v4().simple().to_string()[..7]);
        let nonce = uuid::Uuid::new_v4().simple().to_string()[..6].to_string();
        let expires = now_ms() + ttl_ms;
        self.conn().execute(
            "INSERT INTO approvals(id, nonce_hash, kind, action, action_hash, session_key, task_id, scope,
                                   requested_by, owner_person, created, expires, state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 'pending')",
            params![id, sha256(&nonce), kind, canonical(action), action_hash(kind, action), session_key, task_id, scope,
                    requested_by, OWNER, now_ms(), expires],
        )?;
        self.audit(requested_by, "approval_requested", &id, &format!("{kind} {scope}"));
        let text = format!(
            "🔐 {}\n\nReply /approve {id} {nonce} or /deny {id} {nonce}",
            render(kind, action, context, ttl_ms / 60_000)
        );
        Ok(Card {
            buttons: vec![
                Button { label: "Approve".into(), data: format!("ap:{id}:{nonce}") },
                Button { label: "Deny".into(), data: format!("dn:{id}:{nonce}") },
            ],
            id,
            nonce,
            text,
            expires,
        })
    }

    /// The only way a card is decided.
    pub fn decide(&self, id: &str, nonce: &str, decision: Decision, who: &Decider) -> Result<Decided> {
        let mut db = self.conn();
        let tx = db.transaction()?;
        let row: Option<(String, String, String, String, Option<String>, String, i64, String)> = tx
            .query_row(
                "SELECT nonce_hash, kind, action, action_hash, session_key, owner_person, expires, state FROM approvals WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?)),
            )
            .optional()?;
        let Some((nonce_hash, kind, action, hash, session_key, owner, expires, state)) = row else {
            bail!("no such approval");
        };
        let refuse = |why: &str| -> Result<Decided> {
            Err(anyhow!("{why}"))
        };
        if who.person.as_deref() != Some(owner.as_str()) {
            drop(tx);
            drop(db);
            self.audit(&who.label, "approval_refused", id, "not the owner");
            return refuse("only the owner can decide this");
        }
        if sha256(nonce.trim()) != nonce_hash {
            drop(tx);
            drop(db);
            self.audit(&who.label, "approval_refused", id, "wrong code");
            return refuse("wrong approval code");
        }
        if state != "pending" {
            return refuse(&format!("already {state}"));
        }
        if expires < now_ms() {
            tx.execute("UPDATE approvals SET state = 'expired' WHERE id = ?1 AND state = 'pending'", [id])?;
            tx.commit()?;
            drop(db);
            self.audit(&who.label, "approval_refused", id, "expired");
            return refuse("expired");
        }
        let action: Value = serde_json::from_str(&action)?;
        if action_hash(&kind, &action) != hash {
            return refuse("the action changed after it was proposed");
        }
        let new_state = if decision == Decision::Approve { "approved" } else { "denied" };
        let n = tx.execute(
            "UPDATE approvals SET state = ?2, decided_by = ?3, decided_via = ?4, decided = ?5 WHERE id = ?1 AND state = 'pending'",
            params![id, new_state, who.label, who.via, now_ms()],
        )?;
        if n != 1 {
            return refuse("already decided");
        }
        tx.commit()?;
        drop(db);
        self.audit(&who.label, &format!("approval_{new_state}"), id, &who.via);
        Ok(Decided { id: id.into(), kind, action, session_key, decision })
    }

    /// Execution claims an approved card exactly once and re-checks its hash.
    pub fn claim_for_execution(&self, id: &str) -> Result<(String, Value)> {
        let db = self.conn();
        let (kind, action, hash): (String, String, String) =
            db.query_row("SELECT kind, action, action_hash FROM approvals WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        let action: Value = serde_json::from_str(&action)?;
        if action_hash(&kind, &action) != hash {
            bail!("action hash mismatch");
        }
        let n = db.execute("UPDATE approvals SET state = 'executed' WHERE id = ?1 AND state = 'approved'", [id])?;
        if n != 1 {
            bail!("not approved, or already used");
        }
        Ok((kind, action))
    }

    pub fn approval_state(&self, id: &str) -> Option<String> {
        self.conn().query_row("SELECT state FROM approvals WHERE id = ?1", [id], |r| r.get(0)).optional().ok().flatten()
    }

    pub fn expire_approvals(&self, now: i64) -> Result<usize> {
        Ok(self.conn().execute("UPDATE approvals SET state = 'expired' WHERE state = 'pending' AND expires < ?1", [now])?)
    }
}

/// `ap:<id>:<nonce>` / `dn:<id>:<nonce>` from a button, or the command form.
pub fn parse_decision(data: &str) -> Option<(Decision, String, String)> {
    let mut parts = data.splitn(3, ':');
    let d = match parts.next()? {
        "ap" => Decision::Approve,
        "dn" => Decision::Deny,
        _ => return None,
    };
    Some((d, parts.next()?.to_string(), parts.next()?.to_string()))
}
