//! Event sources (read-only connectors). Each turns a snapshot or feed into
//! durable events with stable dedupe keys; the gateway's rules decide what
//! happens next. No model is called here.
//!
//! - `ThreadlineTasks`: task status changes (completed / blocked / cancelled)
//!   from the node's threadlined: how edge tasks report back.
//! - `MeshContext`: the mesh hub's personal-context API (calendar, reminders
//!   read from the Mac's iCloud-synced databases by its node agent).
//! - Email: no live connector yet (needs an account/OAuth); the `EventSource`
//!   trait is the interface, and tests use a fake.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};

use crate::inbox::{Event, EventSource};

pub struct ThreadlineTasks {
    pub tl: hasp_edge::threadline::Threadline,
    /// Only tasks carrying this label (e.g. "edge").
    pub label: String,
}

#[async_trait]
impl EventSource for ThreadlineTasks {
    fn name(&self) -> &str {
        "threadline"
    }

    async fn poll(&self, cursor: Option<String>) -> Result<(Vec<Event>, Option<String>)> {
        let mut out = Vec::new();
        for t in self.tl.tasks().await? {
            if !t["labels"].as_array().is_some_and(|l| l.iter().any(|x| x == self.label.as_str())) {
                continue;
            }
            let status = t["status"].as_str().unwrap_or("");
            let kind = match status {
                "completed" => "task_completed",
                "blocked" => "task_blocked",
                "cancelled" => "task_cancelled",
                _ => continue, // progress is for /status, not notifications
            };
            let id = t["taskId"].as_str().unwrap_or("");
            out.push(Event {
                source: "threadline".into(),
                // once per (task, status, grant): a re-run under a new lease reports again
                dedupe_key: format!("{id}:{status}:{}", t["fencingToken"]),
                kind: kind.into(),
                occurred_ms: crate::model::now_ms(),
                title: t["title"].as_str().unwrap_or("").into(),
                body: t["completionSummary"].as_str().or(t["blockReason"].as_str()).or(t["cancelReason"].as_str()).unwrap_or("").chars().take(2000).collect(),
                data: json!({"task_id": id, "result": t["completionResult"], "fencing": t["fencingToken"]}),
                trust: "system".into(),
            });
        }
        Ok((out, cursor))
    }
}

pub struct MeshContext {
    pub url: String,
    pub token: String,
    pub node: String,
    /// "calendar" or "reminders"
    pub source: String,
    pub http: reqwest::Client,
}

impl MeshContext {
    pub fn events_from(&self, v: &Value) -> Vec<Event> {
        let items = match self.source.as_str() {
            "calendar" => v["events"].as_array(),
            _ => v["open"].as_array(),
        };
        items
            .into_iter()
            .flatten()
            .map(|e| {
                let title = e["title"].as_str().unwrap_or("").to_string();
                let when = e["start"].as_i64().or(e["due"].as_i64()).unwrap_or(0);
                Event {
                    source: self.source.clone(),
                    // the same item at the same time is one event; a moved one is new
                    dedupe_key: crate::store::sha256(&format!("{title}|{when}|{}", e["calendar"].as_str().or(e["list"].as_str()).unwrap_or(""))),
                    kind: if self.source == "calendar" { "calendar_upcoming".into() } else { "reminder_open".into() },
                    occurred_ms: when * 1000,
                    title,
                    body: e["location"].as_str().or(e["notes"].as_str()).unwrap_or("").into(),
                    data: e.clone(),
                    // calendar invites can carry other people's text
                    trust: "third_party".into(),
                }
            })
            .collect()
    }
}

#[async_trait]
impl EventSource for MeshContext {
    fn name(&self) -> &str {
        &self.source
    }

    async fn poll(&self, cursor: Option<String>) -> Result<(Vec<Event>, Option<String>)> {
        let r = self
            .http
            .get(format!("{}/api/context/{}?source={}&limit=50", self.url, self.node, self.source))
            .bearer_auth(&self.token)
            .send()
            .await?;
        if !r.status().is_success() {
            anyhow::bail!("mesh context {}: {}", self.source, r.status());
        }
        Ok((self.events_from(&r.json().await?), cursor))
    }
}
