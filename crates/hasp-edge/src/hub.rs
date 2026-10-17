//! The mesh hub's edge endpoints. A node's own credential (node-scoped) may
//! only submit edge runs for itself; the hub turns them into Dagu jobs.

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

#[derive(Clone)]
pub struct Hub {
    base: String,
    token: String,
    http: reqwest::Client,
}

pub enum Submit {
    Queued { name: String, run_id: String, submission_id: String },
    /// No answer, or the hub said it may or may not be queued: retry with the
    /// same submission id (it never queues twice).
    Uncertain(String),
}

impl Hub {
    pub fn new(base: &str, token: &str) -> Self {
        Self { base: base.trim_end_matches('/').into(), token: token.into(), http: reqwest::Client::new() }
    }

    pub async fn submit(&self, task_id: &str, lease_id: &str, fencing: u64, client_id: &str) -> Result<Submit> {
        let body = json!({"task_id": task_id, "lease_id": lease_id, "fencing_token": fencing, "client_id": client_id});
        let r = match self.http.post(format!("{}/api/edge/submit", self.base)).bearer_auth(&self.token).json(&body).send().await {
            Ok(r) => r,
            Err(e) => return Ok(Submit::Uncertain(e.to_string())),
        };
        let status = r.status().as_u16();
        let v: Value = r.json().await.unwrap_or(Value::Null);
        if status == 200 {
            return Ok(Submit::Queued {
                name: v["name"].as_str().unwrap_or("").into(),
                run_id: v["dagRunId"].as_str().unwrap_or("").into(),
                submission_id: v["submission_id"].as_str().unwrap_or("").into(),
            });
        }
        let uncertain = v["detail"]["uncertain"].as_bool().unwrap_or(false) || matches!(status, 502..=504);
        if uncertain {
            return Ok(Submit::Uncertain(v["detail"].to_string()));
        }
        Err(anyhow!("hub {status}: {}", v["detail"]))
    }

    pub async fn stop(&self, task_id: &str, fencing: u64) -> Result<()> {
        let r = self
            .http
            .post(format!("{}/api/edge/stop", self.base))
            .bearer_auth(&self.token)
            .json(&json!({"task_id": task_id, "fencing_token": fencing}))
            .send()
            .await?;
        if r.status().as_u16() >= 400 && r.status().as_u16() != 404 {
            return Err(anyhow!("hub stop {}", r.status()));
        }
        Ok(())
    }
}
