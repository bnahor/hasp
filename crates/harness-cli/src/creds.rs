//! Credentials: `harness login [provider]` stores API keys at
//! `~/.harness/credentials.json` (0600). Resolution per provider: env
//! (`ANTHROPIC_API_KEY` / `ZAI_API_KEY`) → file. The shape leaves room for
//! OAuth tokens later without a migration.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Credentials {
    pub anthropic_api_key: Option<String>,
    #[serde(default)]
    pub zai_api_key: Option<String>,
    #[serde(default)]
    pub deepseek_api_key: Option<String>,
    /// Keys for every other provider (coding plans, custom endpoints), by id.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub keys: std::collections::BTreeMap<String, String>,
}

fn home() -> std::io::Result<PathBuf> {
    harness_workspace::harness_home().map_err(|e| std::io::Error::other(e.to_string()))
}

fn file_name() -> PathBuf {
    Path::new("credentials.json").to_path_buf()
}

/// Full path of the credentials file.
pub fn path() -> std::io::Result<PathBuf> {
    Ok(home()?.join(file_name()))
}

/// Credentials as stored on this machine (env is handled by `resolve_key`).
pub fn load() -> Credentials {
    home().map(|h| load_at(&h)).unwrap_or_default()
}

pub fn load_at(dir: &Path) -> Credentials {
    std::fs::read_to_string(dir.join(file_name()))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save_at(dir: &Path, provider: &str, key: &str) -> std::io::Result<PathBuf> {
    let mut creds = load_at(dir);
    match provider {
        "zai" => creds.zai_api_key = Some(key.to_string()),
        "deepseek" => creds.deepseek_api_key = Some(key.to_string()),
        "anthropic" => creds.anthropic_api_key = Some(key.to_string()),
        other => {
            creds.keys.insert(other.to_string(), key.to_string());
        }
    }
    let target = dir.join(file_name());
    let mut f = std::fs::File::create(&target)?;
    serde_json::to_writer_pretty(&mut f, &creds)?;
    f.flush()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(target)
}

pub fn resolve_api_key() -> Option<String> {
    std::env::var("ANTHROPIC_API_KEY")
        .ok()
        .filter(|k| !k.is_empty())
        .or_else(|| load().anthropic_api_key)
}

pub fn resolve_key(provider: &str) -> Option<String> {
    match provider {
        "zai" => std::env::var("ZAI_API_KEY")
            .ok()
            .filter(|k| !k.is_empty())
            .or_else(|| load().zai_api_key),
        "deepseek" => std::env::var("DEEPSEEK_API_KEY")
            .ok()
            .filter(|k| !k.is_empty())
            .or_else(|| load().deepseek_api_key),
        // Keyless local endpoints are legal: a configured URL is credential
        // enough, and the placeholder is ignored by the server.
        "compat" | "openai-compat" => std::env::var("HARNESS_COMPAT_KEY")
            .ok()
            .filter(|k| !k.is_empty())
            .or_else(|| {
                std::env::var("HARNESS_COMPAT_URL")
                    .ok()
                    .filter(|u| !u.is_empty())
                    .map(|_| "none".into())
            }),
        // Local by construction; the key header is a placeholder.
        "ollama" => Some("ollama".into()),
        "anthropic" => resolve_api_key(),
        other => match harness_provider::plans::find(other) {
            Some(plan) => std::env::var(&plan.key_env)
                .ok()
                .filter(|k| !k.is_empty())
                .or_else(|| load().keys.get(other).cloned()),
            None => resolve_api_key(),
        },
    }
}

/// Interactive login; also used as the first-run fallback when no key is found.
pub fn login_interactive(provider: &str) -> Result<()> {
    let plan = harness_provider::plans::find(provider);
    let label = match provider {
        "zai" => "z.ai".to_string(),
        "deepseek" => "DeepSeek".to_string(),
        "anthropic" => "Anthropic".to_string(),
        other => match &plan {
            Some(p) => {
                if !p.signup.is_empty() {
                    println!("get a key at {}", p.signup);
                }
                p.label.clone()
            }
            None => anyhow::bail!(
                "unknown provider {other} — `harness providers` lists them, and \
                 ~/.harness/providers.json adds your own"
            ),
        },
    };
    let key = rpassword::prompt_password(format!("{label} API key (input hidden): "))?;
    let key = key.trim();
    if key.is_empty() {
        anyhow::bail!("empty key; nothing saved");
    }
    let home =
        harness_workspace::harness_home().map_err(|e| std::io::Error::other(e.to_string()))?;
    let saved = save_at(&home, provider, key)?;
    println!("saved to {} (mode 0600)", saved.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_points_at_the_file_load_at_reads() {
        // regression: `path()` yields the file, while `load_at` takes its
        // directory — composing them directly double-joined the file name and
        // silently read nothing.
        let p = path().unwrap();
        assert_eq!(p.parent().unwrap().join(file_name()), p);
    }

    #[test]
    fn roundtrip_with_tight_permissions() {
        let dir = tempfile::tempdir().unwrap();
        save_at(dir.path(), "anthropic", "sk-ant-test").unwrap();
        save_at(dir.path(), "zai", "zai-key").unwrap();
        save_at(dir.path(), "deepseek", "ds-key").unwrap();
        let creds = load_at(dir.path());
        assert_eq!(creds.anthropic_api_key.as_deref(), Some("sk-ant-test"));
        assert_eq!(creds.zai_api_key.as_deref(), Some("zai-key"));
        assert_eq!(creds.deepseek_api_key.as_deref(), Some("ds-key"));

        save_at(dir.path(), "anthropic", "sk-ant-second").unwrap();
        let creds = load_at(dir.path());
        assert_eq!(creds.anthropic_api_key.as_deref(), Some("sk-ant-second"));
        assert_eq!(creds.zai_api_key.as_deref(), Some("zai-key"));
        assert_eq!(creds.deepseek_api_key.as_deref(), Some("ds-key"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::metadata(dir.path().join("credentials.json"))
                .unwrap()
                .permissions();
            assert_eq!(perms.mode() & 0o777, 0o600);
        }
    }
}
