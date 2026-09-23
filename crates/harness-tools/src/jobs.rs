//! Background jobs: `bash` with `background: true` starts a long-running
//! command (a dev server, a watcher, a slow build) and returns at once; the
//! `job` tool lists jobs, pages their new output and kills them.
//!
//! A job is started exactly like a foreground command — same gate, same
//! metacharacter and confinement rules, same sandbox — then detached into its
//! own process group with stdout and stderr going to a log file under the
//! run's artifacts. Jobs belong to the session: the table kills every process
//! group it still owns when it is dropped, so nothing outlives the harness by
//! accident.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use harness_core::{Tool, ToolCtx, ToolOutput, ToolSpec};
use serde_json::{Value, json};

pub const JOB_TOOL_NAME: &str = "job";
/// Running jobs at once — a guard against a model that forgets to stop them.
pub const MAX_RUNNING_JOBS: usize = 8;
/// Most new output one `output` call returns; the rest stays in the log.
const OUTPUT_CHUNK: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub enum JobState {
    Running,
    Exited(i32),
    Killed,
}

impl JobState {
    fn label(&self) -> String {
        match self {
            JobState::Running => "running".into(),
            JobState::Exited(code) => format!("exited {code}"),
            JobState::Killed => "killed".into(),
        }
    }
}

struct Job {
    command: String,
    pid: Option<u32>,
    log: PathBuf,
    /// Workspace-relative log path, for the model.
    log_rel: String,
    state: Arc<Mutex<JobState>>,
    /// Bytes of the log already returned by `output`.
    read_to: u64,
}

/// A session's jobs. Shared by `bash` (which starts them) and `job`.
#[derive(Default)]
pub struct JobTable {
    jobs: Mutex<BTreeMap<u32, Job>>,
    next: Mutex<u32>,
}

impl Drop for JobTable {
    fn drop(&mut self) {
        if let Ok(jobs) = self.jobs.lock() {
            for job in jobs.values() {
                if *job.state.lock().expect("job state") == JobState::Running {
                    kill_group(job.pid);
                }
            }
        }
    }
}

/// How long a job gets to exit on SIGTERM before its group is SIGKILLed.
const KILL_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

#[cfg(unix)]
fn signal_group(pid: u32, signal: &str) {
    let _ = std::process::Command::new("kill")
        .args([signal, "--", &format!("-{pid}")])
        .stderr(std::process::Stdio::null())
        .status();
}

/// SIGTERM the job's process group, then SIGKILL it after [`KILL_GRACE`]: a
/// server that traps or ignores TERM (graceful shutdown, or a shell `trap`)
/// must still be gone. The escalation runs on its own thread so neither a
/// tool call nor a session's teardown waits for it.
#[cfg(unix)]
fn kill_group(pid: Option<u32>) {
    let Some(pid) = pid else {
        return;
    };
    signal_group(pid, "-TERM");
    std::thread::spawn(move || {
        std::thread::sleep(KILL_GRACE);
        signal_group(pid, "-KILL");
    });
}

#[cfg(not(unix))]
fn kill_group(_: Option<u32>) {}

impl JobTable {
    fn running(&self) -> usize {
        self.jobs
            .lock()
            .expect("jobs")
            .values()
            .filter(|j| *j.state.lock().expect("job state") == JobState::Running)
            .count()
    }

    /// Start `command` (already approved by the caller) in the background.
    pub fn start(&self, command: &str, ctx: &ToolCtx<'_>) -> Result<String, String> {
        if self.running() >= MAX_RUNNING_JOBS {
            return Err(format!(
                "{MAX_RUNNING_JOBS} background jobs are already running; kill one with the job tool first"
            ));
        }
        let id = {
            let mut n = self.next.lock().expect("job counter");
            *n += 1;
            *n
        };
        let log_rel = ctx
            .artifacts
            .store(&format!("job-{id}"), b"")
            .map_err(|e| format!("could not create the job log: {e}"))?;
        let log = ctx.workspace.root().join(&log_rel);
        let file = std::fs::OpenOptions::new()
            .append(true)
            .open(&log)
            .map_err(|e| e.to_string())?;
        let err_file = file.try_clone().map_err(|e| e.to_string())?;
        let mut cmd = crate::sandbox::command(command, ctx.workspace.root());
        cmd.current_dir(ctx.workspace.root())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(file))
            .stderr(std::process::Stdio::from(err_file))
            .kill_on_drop(false);
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd.spawn().map_err(|e| e.to_string())?;
        let pid = child.id();
        let state = Arc::new(Mutex::new(JobState::Running));
        let watcher = state.clone();
        tokio::spawn(async move {
            let status = child.wait().await;
            let mut s = watcher.lock().expect("job state");
            if *s == JobState::Running {
                *s = JobState::Exited(status.ok().and_then(|st| st.code()).unwrap_or(-1));
            }
        });
        self.jobs.lock().expect("jobs").insert(
            id,
            Job {
                command: command.to_string(),
                pid,
                log,
                log_rel: log_rel.clone(),
                state,
                read_to: 0,
            },
        );
        Ok(format!(
            "started background job {id} (pid {}); output → {log_rel}. Check it with the job \
             tool (action \"output\"), stop it with action \"kill\".",
            pid.map(|p| p.to_string()).unwrap_or_else(|| "?".into())
        ))
    }

    fn list(&self) -> String {
        let jobs = self.jobs.lock().expect("jobs");
        if jobs.is_empty() {
            return "no background jobs".into();
        }
        jobs.iter()
            .map(|(id, j)| {
                format!(
                    "{id}  {}  {}  → {}",
                    j.state.lock().expect("job state").label(),
                    j.command.lines().next().unwrap_or_default(),
                    j.log_rel
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// New output since the last call, plus the job's state.
    fn output(&self, id: u32) -> Result<String, String> {
        let mut jobs = self.jobs.lock().expect("jobs");
        let job = jobs.get_mut(&id).ok_or_else(|| format!("no job {id}"))?;
        let bytes = std::fs::read(&job.log).map_err(|e| e.to_string())?;
        let from = (job.read_to as usize).min(bytes.len());
        let to = (from + OUTPUT_CHUNK).min(bytes.len());
        job.read_to = to as u64;
        let state = job.state.lock().expect("job state").label();
        let mut out = format!("job {id}: {state}\n");
        if from == to {
            out.push_str("(no new output)");
        } else {
            out.push_str(&String::from_utf8_lossy(&bytes[from..to]));
            if to < bytes.len() {
                out.push_str(&format!(
                    "\n[{} more bytes — call output again, or read {}]",
                    bytes.len() - to,
                    job.log_rel
                ));
            }
        }
        Ok(out)
    }

    fn kill(&self, id: u32) -> Result<String, String> {
        let jobs = self.jobs.lock().expect("jobs");
        let job = jobs.get(&id).ok_or_else(|| format!("no job {id}"))?;
        let mut state = job.state.lock().expect("job state");
        if *state != JobState::Running {
            return Ok(format!("job {id} already {}", state.label()));
        }
        kill_group(job.pid);
        *state = JobState::Killed;
        Ok(format!("job {id} killed"))
    }
}

pub struct JobTool {
    jobs: Arc<JobTable>,
}

impl JobTool {
    pub fn new(jobs: Arc<JobTable>) -> Self {
        Self { jobs }
    }
}

#[async_trait]
impl Tool for JobTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: JOB_TOOL_NAME.into(),
            description: "Manage background jobs started with bash `background: true`: \
                          `list` them, get a job's new `output` since you last checked (and \
                          whether it is still running), or `kill` it. Kill jobs you no longer \
                          need."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["list", "output", "kill"]},
                    "id": {"type": "integer", "description": "Job id (output, kill)"}
                },
                "required": ["action"]
            }),
        }
    }

    fn concurrency_safe(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, _ctx: &mut ToolCtx<'_>) -> ToolOutput {
        let id = args["id"].as_u64().map(|n| n as u32);
        let result = match (args["action"].as_str(), id) {
            (Some("list"), _) => Ok(self.jobs.list()),
            (Some("output"), Some(id)) => self.jobs.output(id),
            (Some("kill"), Some(id)) => self.jobs.kill(id),
            (Some("output" | "kill"), None) => Err("`id` is required".into()),
            _ => Err("action must be list, output or kill".into()),
        };
        match result {
            Ok(text) => ToolOutput::text(text),
            Err(e) => ToolOutput::error(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::{ArtifactStore, Gate, GateDecision};
    use harness_workspace::Workspace;
    use std::time::Duration;

    struct AllowGate;
    impl Gate for AllowGate {
        fn check_bash(&mut self, _: &str) -> GateDecision {
            GateDecision::Allow
        }
        fn check_write(&mut self, _: &str) -> GateDecision {
            GateDecision::Allow
        }
    }

    fn ctx_in<'a>(path: &std::path::Path, gate: &'a mut dyn Gate) -> ToolCtx<'a> {
        let ws: &'static Workspace = Box::leak(Box::new(Workspace::resolve(path).unwrap()));
        let artifacts: &'static ArtifactStore = Box::leak(Box::new(
            ArtifactStore::new(path.join(".harness/artifacts/test"), "test".into()).unwrap(),
        ));
        ToolCtx {
            workspace: ws,
            gate,
            artifacts,
            bus: None,
        }
    }

    /// Running, as opposed to gone or a zombie awaiting its reaper (which
    /// `kill -0` would still report as alive on Linux).
    fn alive(pid: u32) -> bool {
        std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .is_ok_and(|o| {
                let stat = String::from_utf8_lossy(&o.stdout);
                let stat = stat.trim();
                !stat.is_empty() && !stat.starts_with('Z')
            })
    }

    /// Wait (generously — CI runners are slow under parallel tests) for `pid`
    /// to stop running.
    async fn gone(pid: u32) -> bool {
        // KILL_GRACE, then time for the SIGKILL to land on a slow runner.
        for _ in 0..160 {
            if !alive(pid) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    async fn settle(table: &JobTable, id: u32) {
        for _ in 0..100 {
            let state = table.jobs.lock().unwrap()[&id]
                .state
                .lock()
                .unwrap()
                .clone();
            if state != JobState::Running {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("job {id} never finished");
    }

    #[tokio::test]
    async fn background_bash_returns_at_once_and_output_pages_forward() {
        let tmp = tempfile::tempdir().unwrap();
        let jobs = Arc::new(JobTable::default());
        let bash = crate::BashTool::with_jobs(jobs.clone());
        let tool = JobTool::new(jobs.clone());
        let mut gate = AllowGate;
        let mut ctx = ctx_in(tmp.path(), &mut gate);

        let t = std::time::Instant::now();
        let out = bash
            .execute(
                json!({"command": "echo first; sleep 0.3; echo second >&2", "background": true}),
                &mut ctx,
            )
            .await;
        assert!(
            t.elapsed() < Duration::from_millis(250),
            "returned before the command ended"
        );
        assert!(
            out.content.contains("started background job 1"),
            "{}",
            out.content
        );

        settle(&jobs, 1).await;
        let out = tool
            .execute(json!({"action": "output", "id": 1}), &mut ctx)
            .await;
        assert!(
            out.content.starts_with("job 1: exited 0"),
            "{}",
            out.content
        );
        assert!(out.content.contains("first") && out.content.contains("second"));
        let again = tool
            .execute(json!({"action": "output", "id": 1}), &mut ctx)
            .await;
        assert!(
            again.content.contains("(no new output)"),
            "{}",
            again.content
        );
        let list = tool.execute(json!({"action": "list"}), &mut ctx).await;
        assert!(list.content.contains("exited 0"), "{}", list.content);
    }

    #[tokio::test]
    async fn kill_and_drop_stop_the_whole_process_group() {
        let tmp = tempfile::tempdir().unwrap();
        let jobs = Arc::new(JobTable::default());
        let tool = JobTool::new(jobs.clone());
        let mut gate = AllowGate;
        let mut ctx = ctx_in(tmp.path(), &mut gate);

        jobs.start("sleep 30", &ctx).unwrap();
        let pid = jobs.jobs.lock().unwrap()[&1].pid.unwrap();
        assert!(alive(pid));
        let out = tool
            .execute(json!({"action": "kill", "id": 1}), &mut ctx)
            .await;
        assert_eq!(out.content, "job 1 killed");
        assert!(gone(pid).await, "killed job's process is gone");

        // A job that ignores SIGTERM is SIGKILLed after the grace period.
        jobs.start("trap '' TERM; sleep 30 & wait", &ctx).unwrap();
        let pid = jobs.jobs.lock().unwrap()[&2].pid.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        tool.execute(json!({"action": "kill", "id": 2}), &mut ctx)
            .await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(alive(pid), "TERM alone does not stop it");
        assert!(gone(pid).await, "SIGKILL after the grace period does");

        // A grandchild in the job's group dies with the session's table too.
        jobs.start("sleep 30 & wait", &ctx).unwrap();
        let pid = jobs.jobs.lock().unwrap()[&3].pid.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(tool);
        drop(jobs);
        assert!(gone(pid).await, "dropping the table stops running jobs");
    }

    #[tokio::test]
    async fn running_jobs_are_capped() {
        let tmp = tempfile::tempdir().unwrap();
        let jobs = JobTable::default();
        let mut gate = AllowGate;
        let ctx = ctx_in(tmp.path(), &mut gate);
        for _ in 0..MAX_RUNNING_JOBS {
            jobs.start("sleep 30", &ctx).unwrap();
        }
        let err = jobs.start("sleep 30", &ctx).unwrap_err();
        assert!(err.contains("already running"), "{err}");
    }
}
