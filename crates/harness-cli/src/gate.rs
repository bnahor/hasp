//! Approval gates. One decision core shared by the plain REPL (stderr prompt)
//! and the TUI (prompt request over a channel, answered with y/a/n).

use harness_core::{Gate, GateDecision};
use harness_tools::{escapes_cwd, is_simple, seed_allowlist};
use std::io::Write;
use std::sync::Mutex;
use std::sync::mpsc::Sender;

pub struct GateCore {
    pub yolo: bool,
    pub interactive: bool,
    pub allowlist: Vec<String>,
    approved_bash: Vec<String>,
    approved_writes: Vec<String>,
}

impl GateCore {
    pub fn new(yolo: bool, interactive: bool) -> Self {
        Self {
            yolo,
            interactive,
            allowlist: seed_allowlist().iter().map(|s| s.to_string()).collect(),
            approved_bash: Vec::new(),
            approved_writes: Vec::new(),
        }
    }

    pub fn decide_bash(&mut self, command: &str, ask: impl FnOnce(&str) -> char) -> GateDecision {
        if self.yolo {
            return GateDecision::Allow;
        }
        let first = command.split_whitespace().next().unwrap_or("").to_string();
        let simple = is_simple(command);
        if simple && self.approved_bash.iter().any(|w| *w == first) {
            return GateDecision::Allow;
        }
        if simple && self.allowlist.iter().any(|w| *w == first) && !escapes_cwd(command) {
            return GateDecision::Allow;
        }
        if !self.interactive {
            return GateDecision::Deny(
                "non-interactive run: only allowlisted read-only commands run (use --yolo to lift prompts)".into(),
            );
        }
        match ask(&format!("run `{command}`? [y]es / [a]lways / [n]o")) {
            'y' => GateDecision::Allow,
            'a' => {
                self.approved_bash.push(first);
                GateDecision::Allow
            }
            _ => GateDecision::Deny("user declined".into()),
        }
    }

    pub fn decide_write(&mut self, path: &str, ask: impl FnOnce(&str) -> char) -> GateDecision {
        if self.yolo {
            return GateDecision::Allow;
        }
        if self.approved_writes.iter().any(|p| *p == path) {
            return GateDecision::Allow;
        }
        if !self.interactive {
            return GateDecision::Deny(
                "non-interactive run: file writes need --yolo or an interactive session".into(),
            );
        }
        match ask(&format!("write `{path}`? [y]es / [a]lways / [n]o")) {
            'y' => GateDecision::Allow,
            'a' => {
                self.approved_writes.push(path.to_string());
                GateDecision::Allow
            }
            _ => GateDecision::Deny("user declined".into()),
        }
    }
}

fn prompt_char(question: &str) -> char {
    eprint!("{question} ");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return 'n';
    }
    line.trim().chars().next().unwrap_or('n')
}

pub struct PlainGate {
    pub core: GateCore,
}

impl Gate for PlainGate {
    fn check_bash(&mut self, command: &str) -> GateDecision {
        self.core.decide_bash(command, prompt_char)
    }

    fn check_write(&mut self, path: &str) -> GateDecision {
        self.core.decide_write(path, prompt_char)
    }
}

pub struct PromptRequest {
    pub question: String,
    pub respond: Sender<char>,
}

pub type PromptTx = Sender<PromptRequest>;

pub struct TuiGate {
    core: GateCore,
    tx: Mutex<PromptTx>,
}

impl TuiGate {
    pub fn new(core: GateCore, tx: PromptTx) -> Self {
        Self {
            core,
            tx: Mutex::new(tx),
        }
    }
}

/// Blocks the calling worker until the UI answers — `block_in_place` keeps the
/// rest of the runtime running while it waits.
fn ask_via(tx: &Mutex<PromptTx>, question: &str) -> char {
    let (respond, rx) = std::sync::mpsc::channel();
    let req = PromptRequest {
        question: question.to_string(),
        respond,
    };
    // Blocks a runtime worker until the human answers; `block_in_place` keeps
    // the rest of the runtime (and this worker's queued tasks) running.
    if tx.lock().expect("gate channel").send(req).is_err() {
        return 'n';
    }
    tokio::task::block_in_place(|| rx.recv().unwrap_or('n'))
}

impl Gate for TuiGate {
    fn check_bash(&mut self, command: &str) -> GateDecision {
        self.core.decide_bash(command, |q| ask_via(&self.tx, q))
    }

    fn check_write(&mut self, path: &str) -> GateDecision {
        self.core.decide_write(path, |q| ask_via(&self.tx, q))
    }
}
