//! A child process spoken to in newline-delimited JSON — the transport all
//! three protocols share.

use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use crate::Opts;

/// Last lines of the child's stderr, for error messages ("not logged in").
const STDERR_KEEP: usize = 20;

pub struct Proc {
    child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    stderr: Arc<Mutex<VecDeque<String>>>,
    name: String,
}

impl Proc {
    pub fn spawn(name: &str, command: &str, args: &[String], opts: &Opts) -> Result<Proc> {
        let mut child = Command::new(command)
            .args(args)
            .current_dir(&opts.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| {
                format!("starting {name} (`{command}`) — is it installed and on PATH?")
            })?;
        let stdin = child.stdin.take().context("child stdin")?;
        let stdout = BufReader::new(child.stdout.take().context("child stdout")?).lines();
        let stderr = Arc::new(Mutex::new(VecDeque::new()));
        let mut err_lines = BufReader::new(child.stderr.take().context("child stderr")?).lines();
        let keep = stderr.clone();
        tokio::spawn(async move {
            while let Ok(Some(line)) = err_lines.next_line().await {
                let mut k = keep.lock().expect("stderr buffer");
                if k.len() == STDERR_KEEP {
                    k.pop_front();
                }
                k.push_back(line);
            }
        });
        Ok(Proc {
            child,
            stdin,
            stdout,
            stderr,
            name: name.to_string(),
        })
    }

    pub async fn send(&mut self, msg: &Value) -> Result<()> {
        let mut line = serde_json::to_string(msg)?;
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .await
            .with_context(|| format!("{} stopped reading", self.name))?;
        self.stdin.flush().await?;
        Ok(())
    }

    /// The next JSON message. Lines that are not JSON (banners, logs some
    /// agents print on stdout) are skipped. End of output is an error that
    /// carries the agent's last stderr lines.
    pub async fn next(&mut self) -> Result<Value> {
        loop {
            match self.stdout.next_line().await? {
                Some(line) => {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    if let Ok(v) = serde_json::from_str::<Value>(line) {
                        return Ok(v);
                    }
                }
                None => {
                    let status = self.child.wait().await.ok();
                    let tail = self.stderr_tail();
                    bail!(
                        "{} exited{}{}",
                        self.name,
                        status.map(|s| format!(" ({s})")).unwrap_or_default(),
                        if tail.is_empty() {
                            String::new()
                        } else {
                            format!(":\n{tail}")
                        }
                    );
                }
            }
        }
    }

    pub fn stderr_tail(&self) -> String {
        self.stderr
            .lock()
            .map(|k| k.iter().cloned().collect::<Vec<_>>().join("\n"))
            .unwrap_or_default()
    }
}

/// JSON-RPC ids are numbers or strings; compare them as either.
pub fn same_id(v: &Value, id: u64) -> bool {
    v.as_u64() == Some(id) || v.as_str() == Some(&id.to_string())
}
