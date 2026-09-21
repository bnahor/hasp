//! `mcp.json` config, merged user-wide then per project.

use harness_workspace::Workspace;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct McpServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Working directory for the server, for project-scoped servers.
    #[serde(default)]
    pub cwd: Option<std::path::PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
struct File {
    #[serde(default)]
    servers: BTreeMap<String, McpServerConfig>,
}

/// `~/.harness/mcp.json`, then `<root>/.harness/mcp.json` — the project wins on
/// a name collision. A malformed file is reported and ignored: a typo in one
/// config must not stop the agent from starting.
pub fn load_config(ws: &Workspace) -> BTreeMap<String, McpServerConfig> {
    let mut paths: Vec<PathBuf> = Vec::new();
    if let Ok(home) = harness_workspace::harness_home() {
        paths.push(home.join("mcp.json"));
    }
    paths.push(ws.root().join(".harness").join("mcp.json"));
    merge_files(paths)
}

/// Later files win on a name collision.
fn merge_files(paths: impl IntoIterator<Item = PathBuf>) -> BTreeMap<String, McpServerConfig> {
    let mut out = BTreeMap::new();
    for path in paths {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        match serde_json::from_str::<File>(&text) {
            Ok(file) => out.extend(file.servers),
            Err(e) => eprintln!("▪ ignoring {}: {e}", path.display()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn project_config_overrides_user_and_broken_files_are_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let user = write(
            tmp.path(),
            "user.json",
            r#"{"servers":{"a":{"command":"ua"},"b":{"command":"ub"}}}"#,
        );
        let project = write(
            tmp.path(),
            "project.json",
            r#"{"servers":{"b":{"command":"pb"},"c":{"command":"pc"}}}"#,
        );
        let broken = write(tmp.path(), "broken.json", "{not json");

        let merged = merge_files([user, project, broken]);
        assert_eq!(merged.len(), 3);
        assert_eq!(merged["a"].command, "ua");
        assert_eq!(merged["b"].command, "pb", "project overrides user");
        assert_eq!(merged["c"].command, "pc");
    }

    #[test]
    fn missing_files_are_not_an_error() {
        let missing = PathBuf::from("/nonexistent/mcp.json");
        assert!(merge_files([missing]).is_empty());
    }
}
