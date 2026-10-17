//! Client for the node-local `threadlined` HTTP API (it holds this node's
//! coordinator credential; we hold only its local daemon token).

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

#[derive(Clone)]
pub struct Threadline {
    base: String,
    token: String,
    http: reqwest::Client,
}

/// A committed lease. Only these authorize work; a queued claim never does.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Grant {
    pub task_id: String,
    pub lease_id: String,
    pub fencing_token: u64,
    pub client_id: String,
    pub expires_at: String,
}

pub enum ClaimOutcome {
    Granted(Grant),
    /// Accepted locally while the coordinator was unreachable: no grant, no work.
    Queued { event_id: String },
    Rejected { code: String, error: String },
}

impl Threadline {
    pub fn new(base: &str, token: &str) -> Self {
        Self { base: base.trim_end_matches('/').into(), token: token.into(), http: reqwest::Client::new() }
    }

    async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<(u16, Value)> {
        let mut req = self.http.request(method, format!("{}{path}", self.base)).bearer_auth(&self.token);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let r = req.send().await?;
        let status = r.status().as_u16();
        let v = r.json::<Value>().await.unwrap_or(Value::Null);
        Ok((status, v))
    }

    fn ok(status: u16, v: Value) -> Result<Value> {
        if status >= 400 {
            Err(anyhow!("threadline {status} {}: {}", v["code"].as_str().unwrap_or(""), v["error"].as_str().unwrap_or("")))
        } else {
            Ok(v)
        }
    }

    pub async fn register_client(&self, tool: &str, label: &str) -> Result<String> {
        let (s, v) = self.call(reqwest::Method::POST, "/v1/clients/register", Some(json!({"toolName": tool, "sessionLabel": label}))).await?;
        Self::ok(s, v)?["clientId"].as_str().map(String::from).ok_or_else(|| anyhow!("no clientId"))
    }

    /// The daemon's cached view: advisory only (fine for finding candidates).
    pub async fn tasks(&self) -> Result<Vec<Value>> {
        let (s, v) = self.call(reqwest::Method::GET, "/v1/tasks", None).await?;
        Ok(Self::ok(s, v)?.as_array().cloned().unwrap_or_default())
    }

    /// `(authoritative, task)`: authoritative only when the coordinator answered.
    pub async fn task(&self, task_id: &str) -> Result<(bool, Value)> {
        let (s, v) = self.call(reqwest::Method::GET, &format!("/v1/tasks/{task_id}"), None).await?;
        let v = Self::ok(s, v)?;
        Ok((v["authoritative"].as_bool().unwrap_or(false), v["task"].clone()))
    }

    pub async fn create_task(&self, client_id: &str, title: &str, description: &str, labels: &[String]) -> Result<String> {
        let (s, v) = self
            .call(reqwest::Method::POST, "/v1/tasks", Some(json!({"clientId": client_id, "title": title, "description": description, "labels": labels})))
            .await?;
        Self::ok(s, v)?["taskId"].as_str().map(String::from).ok_or_else(|| anyhow!("no taskId"))
    }

    pub async fn claim(&self, task_id: &str, client_id: &str, ttl: u64) -> Result<ClaimOutcome> {
        let (s, v) = self
            .call(reqwest::Method::POST, &format!("/v1/tasks/{task_id}/claim"), Some(json!({"clientId": client_id, "ttlSeconds": ttl})))
            .await?;
        if s >= 400 {
            return Ok(ClaimOutcome::Rejected {
                code: v["code"].as_str().unwrap_or("").into(),
                error: v["error"].as_str().unwrap_or("").into(),
            });
        }
        if v["status"] == "committed" {
            let g = &v["grant"];
            return Ok(ClaimOutcome::Granted(Grant {
                task_id: task_id.into(),
                lease_id: g["leaseId"].as_str().unwrap_or("").into(),
                fencing_token: g["fencingToken"].as_u64().unwrap_or(0),
                client_id: client_id.into(),
                expires_at: g["expiresAt"].as_str().unwrap_or("").into(),
            }));
        }
        Ok(ClaimOutcome::Queued { event_id: v["eventId"].as_str().unwrap_or("").into() })
    }

    fn lease_body(g: &Grant, extra: Value) -> Value {
        let mut b = json!({"clientId": g.client_id, "leaseId": g.lease_id, "fencingToken": g.fencing_token});
        if let (Some(b), Some(e)) = (b.as_object_mut(), extra.as_object()) {
            b.extend(e.clone());
        }
        b
    }

    /// Lease-guarded call; Err carries the coordinator's code (e.g. stale_fencing_token).
    async fn guarded(&self, path: String, g: &Grant, extra: Value) -> Result<Value> {
        let (s, v) = self.call(reqwest::Method::POST, &path, Some(Self::lease_body(g, extra))).await?;
        if s == 202 {
            // Queued while offline: not confirmed by the coordinator.
            return Err(anyhow!("threadline queued (coordinator unreachable): not confirmed"));
        }
        Self::ok(s, v)
    }

    pub async fn renew(&self, g: &Grant, ttl: u64) -> Result<Grant> {
        let v = self.guarded(format!("/v1/tasks/{}/renew", g.task_id), g, json!({"ttlSeconds": ttl})).await?;
        let mut out = g.clone();
        if let Some(e) = v["grant"]["expiresAt"].as_str() {
            out.expires_at = e.into();
        }
        Ok(out)
    }

    pub async fn update(&self, g: &Grant, status: &str, message: &str, result: Option<Value>) -> Result<Value> {
        let mut extra = json!({"status": status, "message": message});
        if let Some(r) = result {
            extra["result"] = r;
        }
        self.guarded(format!("/v1/tasks/{}/update", g.task_id), g, extra).await
    }

    pub async fn release(&self, g: &Grant, reason: &str) -> Result<Value> {
        self.guarded(format!("/v1/tasks/{}/release", g.task_id), g, json!({"reason": reason})).await
    }

    pub async fn cancel(&self, task_id: &str, client_id: &str, reason: &str) -> Result<Value> {
        let (s, v) = self
            .call(reqwest::Method::POST, &format!("/v1/tasks/{task_id}/cancel"), Some(json!({"clientId": client_id, "reason": reason})))
            .await?;
        Self::ok(s, v)
    }

    pub async fn artifact(&self, g: &Grant, kind: &str, uri: &str, summary: &str, metadata: Value) -> Result<Value> {
        let (s, v) = self
            .call(
                reqwest::Method::POST,
                "/v1/artifacts",
                Some(Self::lease_body(g, json!({"taskId": g.task_id, "kind": kind, "uri": uri, "summary": summary, "metadata": metadata}))),
            )
            .await?;
        Self::ok(s, v)
    }
}
