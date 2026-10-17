//! Approval gates. One decision core shared by the plain REPL (stderr prompt)
//! and the TUI (prompt request over a channel, answered with y/a/n).

use hasp_core::{Gate, GateDecision};
use hasp_tools::{escapes_cwd, is_simple, seed_allowlist};
use std::io::Write;
use std::sync::Mutex;
use std::sync::mpsc::Sender;

pub struct GateCore {
    pub yolo: bool,
    pub interactive: bool,
    pub allowlist: Vec<String>,
    pub tools: ToolPolicy,
    approved_bash: Vec<String>,
    approved_writes: Vec<String>,
    approved_tools: Vec<String>,
}

/// How MCP tools are gated. `auto` (the default) runs read-only tools without
/// asking and asks before anything else, so a session is not a wall of
/// prompts; `ask` asks for every tool; `yolo` asks for none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolMode {
    Auto,
    Ask,
    Yolo,
}

/// Loaded from `~/.hasp/policy.json`:
/// `{"tools": {"mode": "auto", "allow": ["mcp__home-mesh__mesh_status"], "ask": [], "deny": ["mcp__*__mesh_power"]}}`.
/// Patterns are tool names with `*` wildcards. `deny` beats `ask` beats
/// `allow`; `HASP_TOOL_MODE` overrides the mode.
#[derive(Clone, Debug)]
pub struct ToolPolicy {
    pub mode: ToolMode,
    pub allow: Vec<String>,
    pub ask: Vec<String>,
    pub deny: Vec<String>,
}

impl Default for ToolPolicy {
    fn default() -> Self {
        Self {
            mode: ToolMode::Auto,
            allow: Vec::new(),
            ask: Vec::new(),
            deny: Vec::new(),
        }
    }
}

impl ToolPolicy {
    pub fn load() -> Self {
        let mut policy = Self::default();
        if let Ok(home) = hasp_workspace::hasp_home() {
            if let Ok(text) = std::fs::read_to_string(home.join("policy.json")) {
                match serde_json::from_str::<serde_json::Value>(&text) {
                    Ok(v) => policy = Self::from_json(&v["tools"]),
                    Err(e) => eprintln!("hasp: ignoring ~/.hasp/policy.json: {e}"),
                }
            }
        }
        if let Ok(mode) = hasp_workspace::env_var("HASP_TOOL_MODE") {
            if let Some(m) = parse_mode(&mode) {
                policy.mode = m;
            }
        }
        policy
    }

    pub fn from_json(v: &serde_json::Value) -> Self {
        let list = |key: &str| -> Vec<String> {
            v[key]
                .as_array()
                .map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect())
                .unwrap_or_default()
        };
        Self {
            mode: v["mode"].as_str().and_then(parse_mode).unwrap_or(ToolMode::Auto),
            allow: list("allow"),
            ask: list("ask"),
            deny: list("deny"),
        }
    }
}

fn parse_mode(s: &str) -> Option<ToolMode> {
    match s.trim().to_ascii_lowercase().as_str() {
        "auto" => Some(ToolMode::Auto),
        "ask" => Some(ToolMode::Ask),
        "yolo" => Some(ToolMode::Yolo),
        _ => None,
    }
}

/// `*` matches any run of characters; everything else is literal.
fn glob_match(pattern: &str, name: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == name;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !name.starts_with(first) || name.len() < first.len() + last.len() || !name.ends_with(last) {
        return false;
    }
    let mut rest = &name[first.len()..name.len() - last.len()];
    for mid in &parts[1..parts.len() - 1] {
        match rest.find(mid) {
            Some(i) => rest = &rest[i + mid.len()..],
            None => return false,
        }
    }
    true
}

const READ_WORDS: &[&str] = &[
    "get", "list", "read", "ls", "status", "diag", "events", "jobs", "job", "attention",
    "storage", "screenshot", "sensors", "context", "search", "find", "describe", "show",
    "health", "state", "info", "query", "view", "check",
];
const WRITE_WORDS: &[&str] = &[
    "set", "write", "delete", "remove", "kill", "run", "exec", "shell", "power", "stop",
    "start", "launch", "click", "type", "press", "send", "create", "update", "put", "copy",
    "move", "install", "wake", "cycle", "computer", "ask", "review", "apply", "approve",
];

/// Read-only by name: the tool's own name (after `mcp__<server>__`) contains a
/// read verb and no write verb. `get_and_delete` asks; `mesh_status` runs.
pub fn looks_read_only(name: &str) -> bool {
    let tool = name.rsplit("__").next().unwrap_or(name).to_ascii_lowercase();
    let words: Vec<&str> = tool.split(|c: char| !c.is_ascii_alphanumeric()).collect();
    words.iter().any(|w| READ_WORDS.contains(w)) && !words.iter().any(|w| WRITE_WORDS.contains(w))
}

fn summarize_args(input: &serde_json::Value) -> String {
    let mut s = match input {
        serde_json::Value::Object(m) if m.is_empty() => String::new(),
        serde_json::Value::Null => String::new(),
        v => v.to_string(),
    };
    if s.chars().count() > 160 {
        s = s.chars().take(157).collect::<String>() + "...";
    }
    s
}

impl GateCore {
    pub fn new(yolo: bool, interactive: bool) -> Self {
        Self {
            yolo,
            interactive,
            allowlist: seed_allowlist().iter().map(|s| s.to_string()).collect(),
            tools: ToolPolicy::load(),
            approved_bash: Vec::new(),
            approved_writes: Vec::new(),
            approved_tools: Vec::new(),
        }
    }

    #[cfg(test)]
    pub fn with_tool_policy(mut self, tools: ToolPolicy) -> Self {
        self.tools = tools;
        self
    }

    pub fn decide_tool(
        &mut self,
        name: &str,
        input: &serde_json::Value,
        ask: impl FnOnce(&str) -> char,
    ) -> GateDecision {
        // An explicit deny holds even under --yolo.
        if self.tools.deny.iter().any(|p| glob_match(p, name)) {
            return GateDecision::Deny(format!("{name} is denied by ~/.hasp/policy.json"));
        }
        if self.yolo || self.tools.mode == ToolMode::Yolo {
            return GateDecision::Allow;
        }
        if self.approved_tools.iter().any(|t| t == name) {
            return GateDecision::Allow;
        }
        let must_ask = self.tools.ask.iter().any(|p| glob_match(p, name));
        if !must_ask {
            if self.tools.allow.iter().any(|p| glob_match(p, name)) {
                return GateDecision::Allow;
            }
            if self.tools.mode == ToolMode::Auto && looks_read_only(name) {
                return GateDecision::Allow;
            }
        }
        if !self.interactive {
            return GateDecision::Deny(format!(
                "non-interactive run: {name} needs approval (allow it in ~/.hasp/policy.json, or use --yolo)"
            ));
        }
        match ask(&format!("call `{name}({})`? [y]es / [a]lways / [n]o", summarize_args(input))) {
            'y' => GateDecision::Allow,
            'a' => {
                self.approved_tools.push(name.to_string());
                GateDecision::Allow
            }
            _ => GateDecision::Deny("user declined".into()),
        }
    }

    pub fn decide_bash(&mut self, command: &str, ask: impl FnOnce(&str) -> char) -> GateDecision {
        if self.yolo {
            return GateDecision::Allow;
        }
        let first = command.split_whitespace().next().unwrap_or("").to_string();
        let simple = is_simple(command);
        if simple && self.approved_bash.contains(&first) {
            return GateDecision::Allow;
        }
        if simple && self.allowlist.contains(&first) && !escapes_cwd(command) {
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

    fn check_tool(&mut self, name: &str, input: &serde_json::Value) -> GateDecision {
        self.core.decide_tool(name, input, prompt_char)
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

    fn check_tool(&mut self, name: &str, input: &serde_json::Value) -> GateDecision {
        self.core.decide_tool(name, input, |q| ask_via(&self.tx, q))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn core(interactive: bool, policy: serde_json::Value) -> GateCore {
        GateCore::new(false, interactive).with_tool_policy(ToolPolicy::from_json(&policy))
    }

    #[test]
    fn globs() {
        assert!(glob_match("mcp__*__mesh_power", "mcp__home-mesh__mesh_power"));
        assert!(glob_match("mcp__home-mesh__*", "mcp__home-mesh__mesh_ls"));
        assert!(glob_match("*", "anything"));
        assert!(!glob_match("mcp__*__mesh_power", "mcp__home-mesh__mesh_powerx"));
        assert!(!glob_match("a*b*c", "acb"));
    }

    #[test]
    fn read_only_names() {
        for n in ["mcp__home-mesh__mesh_status", "mcp__home-mesh__mesh_ls", "mcp__x__get_window_state", "mcp__home-mesh__mesh_screenshot"] {
            assert!(looks_read_only(n), "{n}");
        }
        for n in ["mcp__home-mesh__mesh_power", "mcp__home-mesh__mesh_shell", "mcp__home-mesh__mesh_run", "mcp__x__click", "mcp__x__get_and_delete", "mcp__home-mesh__mesh_computer", "mcp__home-mesh__mesh_esp"] {
            assert!(!looks_read_only(n), "{n}");
        }
    }

    #[test]
    fn auto_runs_reads_and_asks_for_the_rest() {
        let mut g = core(true, json!({}));
        let mut asked = 0;
        assert_eq!(g.decide_tool("mcp__m__mesh_status", &json!({}), |_| { asked += 1; 'n' }), GateDecision::Allow);
        assert_eq!(asked, 0);
        assert!(matches!(g.decide_tool("mcp__m__mesh_power", &json!({"node":"pc"}), |q| { assert!(q.contains("pc")); 'n' }), GateDecision::Deny(_)));
    }

    #[test]
    fn always_approves_for_the_session() {
        let mut g = core(true, json!({}));
        assert_eq!(g.decide_tool("mcp__m__mesh_run", &json!({}), |_| 'a'), GateDecision::Allow);
        assert_eq!(g.decide_tool("mcp__m__mesh_run", &json!({}), |_| panic!("asked twice")), GateDecision::Allow);
    }

    #[test]
    fn policy_lists() {
        let mut g = core(false, json!({"allow": ["mcp__m__mesh_run"], "ask": ["mcp__m__mesh_context"], "deny": ["mcp__*__mesh_power"]}));
        assert_eq!(g.decide_tool("mcp__m__mesh_run", &json!({}), |_| 'n'), GateDecision::Allow);
        assert!(matches!(g.decide_tool("mcp__m__mesh_context", &json!({}), |_| 'y'), GateDecision::Deny(_)));
        let mut y = GateCore::new(true, true).with_tool_policy(ToolPolicy::from_json(&json!({"deny": ["mcp__*__mesh_power"]})));
        assert!(matches!(y.decide_tool("mcp__m__mesh_power", &json!({}), |_| 'y'), GateDecision::Deny(_)));
    }

    #[test]
    fn modes() {
        let mut ask = core(false, json!({"mode": "ask"}));
        assert!(matches!(ask.decide_tool("mcp__m__mesh_status", &json!({}), |_| 'y'), GateDecision::Deny(_)));
        let mut yolo = core(false, json!({"mode": "yolo"}));
        assert_eq!(yolo.decide_tool("mcp__m__mesh_power", &json!({}), |_| 'n'), GateDecision::Allow);
    }
}
