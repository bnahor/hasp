//! `hasp-edge exec`: what the Dagu job runs. Dagu may start it long after the
//! claim (queue, node asleep), so before anything else it re-validates the
//! lease *with the coordinator*: the task must still be ours with exactly the
//! fencing token we were given. A stale job exits without touching anything.
//!
//! Then it runs hasp on the task (a new session, so the task has its own
//! transcript), watches for cancellation, publishes the session as an artifact
//! and completes the task with a result that ties the ids together:
//! task_id <-> hasp session_id <-> dag run id <-> node.
//!
//! Limit: once hasp is running, an effect it already started (a command, a
//! file write) isn't undone if the lease is lost or the task is cancelled; the
//! process is stopped and the late completion is rejected by fencing.

use crate::threadline::{Grant, Threadline};
use anyhow::{Result, anyhow, bail};
use serde_json::json;
use std::path::PathBuf;
use std::time::Duration;

pub struct ExecArgs {
    pub node: String,
    pub grant: Grant,
    pub hasp: PathBuf,
    pub work_root: PathBuf,
    pub hasp_args: Vec<String>,
}

#[derive(Debug, PartialEq)]
pub enum Outcome {
    Completed { session_id: String },
    Stale(String),
    Cancelled,
    Failed(String),
}

/// Exit codes for Dagu: 0 done, 3 stale (nothing done), 4 cancelled, 1 failed.
pub fn exit_code(o: &Outcome) -> i32 {
    match o {
        Outcome::Completed { .. } => 0,
        Outcome::Stale(_) => 3,
        Outcome::Cancelled => 4,
        Outcome::Failed(_) => 1,
    }
}

async fn still_ours(tl: &Threadline, g: &Grant) -> Result<std::result::Result<serde_json::Value, String>> {
    let (authoritative, task) = tl.task(&g.task_id).await?;
    if !authoritative {
        return Ok(Err("coordinator unreachable: can't confirm the lease, not starting".into()));
    }
    if task["fencingToken"].as_u64() != Some(g.fencing_token) || task["lease"]["leaseId"].as_str() != Some(g.lease_id.as_str()) {
        return Ok(Err(format!("lease superseded (fencing {} now)", task["fencingToken"])));
    }
    if !matches!(task["status"].as_str(), Some("claimed" | "in_progress" | "blocked")) {
        return Ok(Err(format!("task is {}", task["status"])));
    }
    Ok(Ok(task))
}

pub async fn run(tl: &Threadline, a: &ExecArgs) -> Result<Outcome> {
    let task = match still_ours(tl, &a.grant).await? {
        Ok(t) => t,
        Err(why) => return Ok(Outcome::Stale(why)),
    };
    // The coordinator checks lease + fencing on this write too: the real fence.
    if let Err(e) = tl.update(&a.grant, "progress", &format!("started on {}", a.node), None).await {
        return Ok(Outcome::Stale(e.to_string()));
    }
    let workspace = a.work_root.join(&a.grant.task_id);
    std::fs::create_dir_all(&workspace)?;
    let ws = hasp_workspace::Workspace::resolve(&workspace).map_err(|e| anyhow!("{e}"))?;
    let before: Vec<String> = hasp_core::session::SessionManager::list_for(&ws).unwrap_or_default().into_iter().map(|l| l.id).collect();
    let prompt = format!("{}\n\n{}", task["title"].as_str().unwrap_or(""), task["description"].as_str().unwrap_or(""));
    let mut child = tokio::process::Command::new(&a.hasp)
        .arg("-p")
        .arg(prompt.trim())
        .arg("--new")
        .args(&a.hasp_args)
        .current_dir(&workspace)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdout = child.stdout.take().expect("piped");
    let collector = tokio::spawn(async move {
        let mut s = String::new();
        let _ = tokio::io::AsyncReadExt::read_to_string(&mut stdout, &mut s).await;
        s
    });
    let status = loop {
        tokio::select! {
            st = child.wait() => break st?,
            _ = tokio::time::sleep(Duration::from_millis(500)) => {
                match tl.task(&a.grant.task_id).await {
                    Ok((true, t)) if t["status"] == "cancelling" => {
                        let _ = child.kill().await;
                        let _ = tl.update(&a.grant, "cancelled", &format!("stopped on {}", a.node), None).await;
                        return Ok(Outcome::Cancelled);
                    }
                    Ok((true, t)) if t["fencingToken"].as_u64() != Some(a.grant.fencing_token) => {
                        let _ = child.kill().await;
                        return Ok(Outcome::Stale("lease lost while running; stopped".into()));
                    }
                    _ => {}
                }
            }
        }
    };
    let out = collector.await.unwrap_or_default();
    let session_id = hasp_core::session::SessionManager::list_for(&ws)
        .unwrap_or_default()
        .into_iter()
        .find(|l| !before.contains(&l.id))
        .map(|l| l.id)
        .unwrap_or_default();
    let dag_run = std::env::var("DAG_RUN_ID").or_else(|_| std::env::var("DAGU_RUN_ID")).unwrap_or_default();
    if !status.success() {
        let msg = format!("hasp exited {status}");
        let _ = tl.update(&a.grant, "blocked", &msg, None).await;
        return Ok(Outcome::Failed(msg));
    }
    tl.artifact(
        &a.grant,
        "hasp-session",
        &format!("hasp-session://{}/{}", a.node, session_id),
        "the task's transcript (canonical on that node)",
        json!({"node": a.node, "session_id": session_id, "workspace": workspace, "dag_run_id": dag_run}),
    )
    .await?;
    let summary: String = out.trim().chars().take(2000).collect();
    match tl
        .update(
            &a.grant,
            "completed",
            &summary,
            Some(json!({"node": a.node, "session_id": session_id, "dag_run_id": dag_run, "fencing_token": a.grant.fencing_token})),
        )
        .await
    {
        Ok(_) => Ok(Outcome::Completed { session_id }),
        Err(e) => {
            if e.to_string().contains("stale") || e.to_string().contains("lease") {
                return Ok(Outcome::Stale(format!("finished but the lease was gone: {e}")));
            }
            bail!(e)
        }
    }
}
