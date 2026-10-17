//! `hasp-edge worker`: claims Threadline tasks this node can run, hands each
//! to Dagu (via the hub), and keeps the lease alive while it waits or runs.
//! It schedules nothing itself: Dagu decides when the job runs.
//!
//! Routing labels on a task: `needs:<cap>` (all must be in this node's
//! capabilities) and `node:<name>` (pinned). Leases this worker holds are
//! persisted, so a restarted worker keeps renewing them instead of letting
//! work it already handed to Dagu lapse.

use crate::hub::{Hub, Submit};
use crate::threadline::{ClaimOutcome, Grant, Threadline};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

pub struct Config {
    pub node: String,
    pub caps: Vec<String>,
    pub ttl_s: u64,
    pub max_held: usize,
    pub state: PathBuf,
}

#[derive(Default, Serialize, Deserialize)]
pub struct Held {
    pub client_id: Option<String>,
    pub tasks: BTreeMap<String, HeldTask>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct HeldTask {
    pub grant: Grant,
    pub submitted: Option<(String, String)>, // (dag name, dag run id)
    pub last_renew_ms: i64,
}

fn now_ms() -> i64 {
    hasp_core::session::now_ms()
}

impl Held {
    pub fn load(p: &PathBuf) -> Self {
        std::fs::read_to_string(p).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }
    pub fn save(&self, p: &PathBuf) -> Result<()> {
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        let tmp = p.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(tmp, p)?;
        Ok(())
    }
}

pub fn eligible(task: &serde_json::Value, node: &str, caps: &[String]) -> bool {
    if !task["claimable"].as_bool().unwrap_or(false) {
        return false;
    }
    let labels: Vec<&str> = task["labels"].as_array().map(|a| a.iter().filter_map(|l| l.as_str()).collect()).unwrap_or_default();
    labels.iter().all(|l| match l.split_once(':') {
        Some(("needs", c)) => caps.iter().any(|x| x == c),
        Some(("node", n)) => n == node,
        _ => true,
    }) && labels.iter().any(|l| *l == "edge")
}

/// One pass. Returns a log of what it did (also printed by the binary).
pub async fn tick(cfg: &Config, tl: &Threadline, hub: &Hub) -> Result<Vec<String>> {
    let mut log = Vec::new();
    let mut held = Held::load(&cfg.state);
    let client_id = match &held.client_id {
        Some(c) => c.clone(),
        None => {
            let c = tl.register_client("hasp-edge", &format!("worker@{}", cfg.node)).await?;
            held.client_id = Some(c.clone());
            held.save(&cfg.state)?;
            c
        }
    };

    // 1. Leases we hold: renew, hand to Dagu if not yet, follow cancels.
    for (task_id, h) in held.tasks.clone() {
        let (authoritative, task) = match tl.task(&task_id).await {
            Ok(x) => x,
            Err(e) => {
                log.push(format!("{task_id}: can't read task ({e}); keeping lease until it expires"));
                continue;
            }
        };
        let status = task["status"].as_str().unwrap_or("");
        let ours = task["lease"]["leaseId"].as_str() == Some(h.grant.lease_id.as_str())
            && task["fencingToken"].as_u64() == Some(h.grant.fencing_token);
        if authoritative && (matches!(status, "completed" | "cancelled") || !ours) {
            if !ours && !matches!(status, "completed" | "cancelled") {
                log.push(format!("{task_id}: lease lost (fencing now {}); stopping our run", task["fencingToken"]));
                let _ = hub.stop(&task_id, h.grant.fencing_token).await;
            }
            held.tasks.remove(&task_id);
            continue;
        }
        if status == "cancelling" {
            let _ = hub.stop(&task_id, h.grant.fencing_token).await;
            // Ack the cancel. If the job already acked it, this is a harmless invalid_transition.
            let _ = tl.update(&h.grant, "cancelled", &format!("cancelled on {}", cfg.node), None).await;
            log.push(format!("{task_id}: cancelled"));
            held.tasks.remove(&task_id);
            continue;
        }
        let mut h = h;
        if now_ms() - h.last_renew_ms > (cfg.ttl_s as i64 * 1000) / 3 {
            match tl.renew(&h.grant, cfg.ttl_s).await {
                Ok(g) => {
                    h.grant = g;
                    h.last_renew_ms = now_ms();
                }
                Err(e) => log.push(format!("{task_id}: renew failed: {e}")),
            }
        }
        if h.submitted.is_none() {
            match hub.submit(&task_id, &h.grant.lease_id, h.grant.fencing_token, &client_id).await {
                Ok(Submit::Queued { name, run_id, .. }) => {
                    log.push(format!("{task_id}: queued as {name}/{run_id}"));
                    h.submitted = Some((name, run_id));
                }
                Ok(Submit::Uncertain(e)) => log.push(format!("{task_id}: submission unconfirmed ({e}); will resend the same id")),
                Err(e) => log.push(format!("{task_id}: hub refused: {e}")),
            }
        }
        held.tasks.insert(task_id, h);
    }
    held.save(&cfg.state)?;

    // 2. New work, within capacity.
    if held.tasks.len() < cfg.max_held {
        for task in tl.tasks().await? {
            if held.tasks.len() >= cfg.max_held {
                break;
            }
            let Some(task_id) = task["taskId"].as_str() else { continue };
            if held.tasks.contains_key(task_id) || !eligible(&task, &cfg.node, &cfg.caps) {
                continue;
            }
            match tl.claim(task_id, &client_id, cfg.ttl_s).await? {
                ClaimOutcome::Granted(g) => {
                    log.push(format!("{task_id}: claimed (fencing {})", g.fencing_token));
                    let mut h = HeldTask { grant: g, submitted: None, last_renew_ms: now_ms() };
                    // Persist the lease before handing it to Dagu.
                    held.tasks.insert(task_id.into(), h.clone());
                    held.save(&cfg.state)?;
                    if let Ok(Submit::Queued { name, run_id, .. }) =
                        hub.submit(task_id, &h.grant.lease_id, h.grant.fencing_token, &client_id).await
                    {
                        log.push(format!("{task_id}: queued as {name}/{run_id}"));
                        h.submitted = Some((name, run_id));
                        held.tasks.insert(task_id.into(), h);
                        held.save(&cfg.state)?;
                    }
                }
                ClaimOutcome::Queued { event_id } => {
                    log.push(format!("{task_id}: claim queued offline ({event_id}); not starting anything"))
                }
                ClaimOutcome::Rejected { code, .. } => log.push(format!("{task_id}: claim rejected ({code})")),
            }
        }
    }
    Ok(log)
}
