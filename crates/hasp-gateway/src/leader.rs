//! One active gateway. A platform token (Telegram) can only have one poller,
//! and two senders would double every reply, so a gateway polls and sends
//! only while it holds the leadership lease. A standby takes over when the
//! lease lapses, with a higher fencing token; every send records the token it
//! was made under, and a gateway that loses the lease stops at once.
//!
//! The lease is a Threadline task (`gateway-leader`), so the same coordinator
//! that fences task execution fences the gateway. Honest limit: a send already
//! handed to a platform when the lease is lost can't be called back; the new
//! leader treats such rows as uncertain.

use anyhow::Result;
use async_trait::async_trait;
use std::sync::Mutex;

use hasp_edge::threadline::{ClaimOutcome, Grant, Threadline};

use crate::model::now_ms;

#[async_trait]
pub trait Leader: Send + Sync {
    /// Acquire or renew. Returns the fencing token while we lead.
    async fn hold(&self) -> Result<Option<u64>>;
    /// The token if our lease is still good by our own clock (with margin).
    fn current(&self) -> Option<u64>;
    fn release_local(&self);
}

/// Single gateway, no standby (development, tests).
pub struct Solo;

#[async_trait]
impl Leader for Solo {
    async fn hold(&self) -> Result<Option<u64>> {
        Ok(Some(1))
    }
    fn current(&self) -> Option<u64> {
        Some(1)
    }
    fn release_local(&self) {}
}

pub struct ThreadlineLeader {
    tl: Threadline,
    ttl_s: u64,
    state: Mutex<Option<(Grant, i64)>>, // grant, locally-valid-until
    client: Mutex<Option<String>>,
    pub name: String,
}

pub const LEADER_LABEL: &str = "gateway-leader";

impl ThreadlineLeader {
    pub fn new(tl: Threadline, ttl_s: u64, name: &str) -> Self {
        Self { tl, ttl_s, state: Mutex::new(None), client: Mutex::new(None), name: name.into() }
    }

    async fn client_id(&self) -> Result<String> {
        if let Some(c) = self.client.lock().unwrap().clone() {
            return Ok(c);
        }
        let c = self.tl.register_client("hasp-gateway", &format!("gateway@{}", self.name)).await?;
        *self.client.lock().unwrap() = Some(c.clone());
        Ok(c)
    }

    /// The leader task: the oldest open task labelled gateway-leader (created
    /// if there is none; if two get created in a race, both sides pick the
    /// same one).
    async fn leader_task(&self, client: &str) -> Result<String> {
        let mut tasks: Vec<serde_json::Value> = self
            .tl
            .tasks()
            .await?
            .into_iter()
            .filter(|t| t["labels"].as_array().is_some_and(|l| l.iter().any(|x| x == LEADER_LABEL)))
            .filter(|t| !matches!(t["status"].as_str(), Some("completed" | "cancelled")))
            .collect();
        tasks.sort_by(|a, b| a["createdAt"].as_str().cmp(&b["createdAt"].as_str()).then(a["taskId"].as_str().cmp(&b["taskId"].as_str())));
        if let Some(t) = tasks.first() {
            return Ok(t["taskId"].as_str().unwrap_or("").into());
        }
        self.tl.create_task(client, "gateway-leader", "Lease: the active messaging gateway", &[LEADER_LABEL.into()]).await
    }

    fn margin_until(&self) -> i64 {
        now_ms() + (self.ttl_s as i64 * 1000) * 6 / 10
    }
}

#[async_trait]
impl Leader for ThreadlineLeader {
    async fn hold(&self) -> Result<Option<u64>> {
        let held = self.state.lock().unwrap().clone();
        if let Some((g, _)) = held {
            match self.tl.renew(&g, self.ttl_s).await {
                Ok(g2) => {
                    *self.state.lock().unwrap() = Some((g2.clone(), self.margin_until()));
                    return Ok(Some(g2.fencing_token));
                }
                Err(_) => {
                    *self.state.lock().unwrap() = None; // lost it: stop now, try to reacquire below
                }
            }
        }
        let client = self.client_id().await?;
        let task = self.leader_task(&client).await?;
        match self.tl.claim(&task, &client, self.ttl_s).await? {
            ClaimOutcome::Granted(g) => {
                let f = g.fencing_token;
                *self.state.lock().unwrap() = Some((g, self.margin_until()));
                Ok(Some(f))
            }
            _ => Ok(None), // someone else leads, or the coordinator is unreachable (queued claims never lead)
        }
    }

    fn current(&self) -> Option<u64> {
        self.state.lock().unwrap().as_ref().filter(|(_, until)| *until > now_ms()).map(|(g, _)| g.fencing_token)
    }

    fn release_local(&self) {
        *self.state.lock().unwrap() = None;
    }
}
