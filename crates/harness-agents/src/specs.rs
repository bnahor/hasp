//! Which agents exist and how to start them: built-ins plus
//! `~/.harness/agents.json`:
//!
//! ```json
//! { "agents": { "kimi": { "command": "kimi", "args": ["--acp"], "protocol": "acp" } } }
//! ```

use std::collections::BTreeMap;

use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Codex,
    Claude,
    Acp,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Spec {
    #[serde(skip)]
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "acp")]
    pub protocol: Protocol,
    /// How to log in, for error messages and `harness agents`.
    #[serde(default)]
    pub login: String,
    /// Whose plan this spends, for listings.
    #[serde(default)]
    pub plan: String,
}

fn acp() -> Protocol {
    Protocol::Acp
}

fn spec(
    name: &str,
    command: &str,
    args: &[&str],
    protocol: Protocol,
    login: &str,
    plan: &str,
) -> Spec {
    Spec {
        name: name.into(),
        command: command.into(),
        args: args.iter().map(|s| s.to_string()).collect(),
        protocol,
        login: login.into(),
        plan: plan.into(),
    }
}

pub fn builtin() -> Vec<Spec> {
    vec![
        spec(
            "claude",
            "claude",
            &[],
            Protocol::Claude,
            "claude auth login",
            "Claude Pro / Max, or an Anthropic key",
        ),
        spec(
            "codex",
            "codex",
            &["app-server"],
            Protocol::Codex,
            "codex login",
            "ChatGPT Plus / Pro, or an OpenAI key",
        ),
        spec(
            "gemini",
            "gemini",
            &["--experimental-acp"],
            Protocol::Acp,
            "gemini (sign in with Google on first run)",
            "Google account / Gemini plan",
        ),
        spec(
            "opencode",
            "opencode",
            &["acp"],
            Protocol::Acp,
            "opencode auth login",
            "whatever providers OpenCode is logged into",
        ),
    ]
}

#[derive(Deserialize)]
struct File {
    #[serde(default)]
    agents: BTreeMap<String, Spec>,
}

fn parse(text: &str) -> Result<Vec<Spec>, serde_json::Error> {
    let f: File = serde_json::from_str(text)?;
    Ok(f.agents
        .into_iter()
        .map(|(name, mut s)| {
            s.name = name;
            s
        })
        .collect())
}

pub fn custom() -> Vec<Spec> {
    let Ok(home) = harness_workspace::harness_home() else {
        return Vec::new();
    };
    let path = home.join("agents.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    parse(&text).unwrap_or_else(|e| {
        eprintln!("▪ ignoring {}: {e}", path.display());
        Vec::new()
    })
}

/// Built-ins, then custom agents; a custom agent with a built-in's name
/// replaces it.
pub fn all() -> Vec<Spec> {
    let custom = custom();
    let mut out: Vec<Spec> = builtin()
        .into_iter()
        .filter(|b| !custom.iter().any(|c| c.name == b.name))
        .collect();
    out.extend(custom);
    out
}

pub fn find(name: &str) -> Option<Spec> {
    all().into_iter().find(|s| s.name == name)
}

/// Whether the agent's command is on PATH.
pub fn installed(spec: &Spec) -> bool {
    let cmd = std::path::Path::new(&spec.command);
    if cmd.components().count() > 1 {
        return cmd.is_file();
    }
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(&spec.command).is_file()))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_agents_default_to_acp() {
        let v = parse(r#"{"agents": {"k": {"command": "kimi", "args": ["--acp"]}}}"#).unwrap();
        assert_eq!(v[0].name, "k");
        assert_eq!(v[0].protocol, Protocol::Acp);
    }
}
