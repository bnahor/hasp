//! Environment definitions: what an isolated place to run tools looks like.
//!
//! ```toml
//! # .harness/environments/py312.toml   (or ~/.harness/environments/)
//! image   = "python:3.12-slim"
//! cpus    = 2
//! memory  = "4G"
//! network = "none"            # or "full" (default)
//! setup   = ["pip install -e ."]
//! [env]
//! PYTHONDONTWRITEBYTECODE = "1"
//! ```
//!
//! The project's definition wins over the home directory's under the same name.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    /// The backend's normal networking (outbound allowed).
    #[default]
    Full,
    /// No network at all inside the environment.
    None,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvSpec {
    /// The file stem it was loaded from.
    #[serde(skip)]
    pub name: String,
    /// Which backend runs it. `docker` today; microVM backends plug in here.
    #[serde(default = "default_backend")]
    pub backend: String,
    /// OCI image reference.
    pub image: String,
    /// e.g. `linux/amd64` for an image published for one architecture only.
    pub platform: Option<String>,
    /// The OCI runtime the container runs under: the backend's default
    /// (`runc`, a shared kernel) unless named — `runsc` (gVisor, a user-space
    /// kernel) or a Kata Containers runtime (a microVM per container), as
    /// registered with the Docker daemon.
    pub runtime: Option<String>,
    /// CPU limit (fractional allowed).
    pub cpus: Option<f64>,
    /// Memory limit: a number with an optional k/m/g suffix, e.g. `512m`, `4G`.
    pub memory: Option<String>,
    /// Process-count limit (a fork bomb stays inside).
    pub pids: Option<u32>,
    #[serde(default)]
    pub network: Network,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Shell commands run once, as root, in the workspace, before the agent.
    #[serde(default)]
    pub setup: Vec<String>,
}

fn default_backend() -> String {
    "docker".into()
}

impl EnvSpec {
    pub fn parse(name: &str, text: &str) -> Result<Self> {
        let mut spec: EnvSpec =
            toml::from_str(text).with_context(|| format!("environment {name}"))?;
        spec.name = name.to_string();
        spec.validate()?;
        Ok(spec)
    }

    fn validate(&self) -> Result<()> {
        if self.image.trim().is_empty() {
            bail!("environment {}: `image` is empty", self.name);
        }
        if let Some(c) = self.cpus
            && !(c > 0.0 && c <= 1024.0)
        {
            bail!("environment {}: `cpus` must be > 0 (got {c})", self.name);
        }
        if let Some(m) = &self.memory
            && memory_bytes(m).is_none()
        {
            bail!(
                "environment {}: `memory` must look like 512m or 4G (got {m:?})",
                self.name
            );
        }
        if let Some(rt) = &self.runtime
            && (rt.is_empty()
                || !rt
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)))
        {
            bail!("environment {}: bad runtime name {rt:?}", self.name);
        }
        for key in self.env.keys() {
            if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                bail!("environment {}: bad variable name {key:?}", self.name);
            }
        }
        Ok(())
    }

    /// One line for notes and listings.
    pub fn summary(&self) -> String {
        let mut parts = vec![match &self.runtime {
            Some(rt) => format!("{} ({}, {rt})", self.image, self.backend),
            None => format!("{} ({})", self.image, self.backend),
        }];
        if let Some(c) = self.cpus {
            parts.push(format!("{c} cpu"));
        }
        if let Some(m) = &self.memory {
            parts.push(format!("{m} memory"));
        }
        if self.network == Network::None {
            parts.push("no network".into());
        }
        parts.join(", ")
    }
}

/// `512m` / `4G` / `1.5g` / `1024` (bytes) → bytes.
pub fn memory_bytes(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    let s = s.strip_suffix('b').unwrap_or(&s);
    let (num, mult) = match s.chars().last()? {
        'k' => (&s[..s.len() - 1], 1u64 << 10),
        'm' => (&s[..s.len() - 1], 1 << 20),
        'g' => (&s[..s.len() - 1], 1 << 30),
        c if c.is_ascii_digit() => (s, 1),
        _ => return None,
    };
    let n: f64 = num.parse().ok()?;
    (n > 0.0).then_some((n * mult as f64) as u64)
}

fn dirs(project: &Path) -> Vec<PathBuf> {
    let mut out = vec![project.join(".harness").join("environments")];
    if let Ok(home) = harness_workspace::harness_home() {
        out.push(home.join("environments"));
    }
    out
}

/// Load the named environment, project definitions first.
pub fn load(project: &Path, name: &str) -> Result<EnvSpec> {
    if name.is_empty() || name.contains(['/', '\\']) || name.starts_with('.') {
        bail!("bad environment name {name:?}");
    }
    for dir in dirs(project) {
        let path = dir.join(format!("{name}.toml"));
        if let Ok(text) = std::fs::read_to_string(&path) {
            return EnvSpec::parse(name, &text).with_context(|| path.display().to_string());
        }
    }
    bail!(
        "no environment named {name} (looked for {name}.toml in .harness/environments/ \
         and ~/.harness/environments/)"
    )
}

/// Every environment visible from `project`, name-sorted; broken files are
/// reported as errors in place rather than hidden.
pub fn list(project: &Path) -> Vec<(String, Result<EnvSpec>)> {
    let mut out: Vec<(String, Result<EnvSpec>)> = Vec::new();
    for dir in dirs(project) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().is_none_or(|x| x != "toml") {
                continue;
            }
            let name = path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            if out.iter().any(|(n, _)| *n == name) {
                continue;
            }
            let spec = std::fs::read_to_string(&path)
                .map_err(anyhow::Error::from)
                .and_then(|t| EnvSpec::parse(&name, &t));
            out.push((name, spec));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_full_spec_and_rejects_nonsense() {
        let s = EnvSpec::parse(
            "py",
            "image = \"python:3.12\"\ncpus = 2\nmemory = \"4G\"\nnetwork = \"none\"\n\
             setup = [\"pip install -e .\"]\n[env]\nA_B = \"1\"\n",
        )
        .unwrap();
        assert_eq!(s.cpus, Some(2.0));
        assert_eq!(s.network, Network::None);
        assert_eq!(s.backend, "docker");
        assert_eq!(
            s.summary(),
            "python:3.12 (docker), 2 cpu, 4G memory, no network"
        );
        let g = EnvSpec::parse("g", "image = \"x\"\nruntime = \"runsc\"").unwrap();
        assert_eq!(g.summary(), "x (docker, runsc)");
        assert!(EnvSpec::parse("g", "image = \"x\"\nruntime = \"a b\"").is_err());

        for bad in [
            "image = \"\"",
            "image = \"x\"\ncpus = 0",
            "image = \"x\"\nmemory = \"lots\"",
            "image = \"x\"\nnetwork = \"sometimes\"",
            "image = \"x\"\ntypo = 1",
            "image = \"x\"\n[env]\n\"A B\" = \"1\"",
        ] {
            assert!(EnvSpec::parse("bad", bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn memory_sizes() {
        assert_eq!(memory_bytes("512m"), Some(512 << 20));
        assert_eq!(memory_bytes("4G"), Some(4 << 30));
        assert_eq!(memory_bytes("1.5g"), Some(3 << 29));
        assert_eq!(memory_bytes("2gb"), Some(2 << 30));
        assert_eq!(memory_bytes("0"), None);
        assert_eq!(memory_bytes("-1g"), None);
    }

    #[test]
    fn project_definitions_win_and_names_are_checked() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".harness/environments");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("dev.toml"), "image = \"alpine\"").unwrap();
        std::fs::write(dir.join("broken.toml"), "image = 3").unwrap();
        assert_eq!(load(tmp.path(), "dev").unwrap().image, "alpine");
        assert!(load(tmp.path(), "../etc").is_err());
        assert!(load(tmp.path(), "missing").is_err());
        let names: Vec<(String, bool)> = list(tmp.path())
            .into_iter()
            .filter(|(n, _)| n == "dev" || n == "broken")
            .map(|(n, r)| (n, r.is_ok()))
            .collect();
        assert_eq!(names, vec![("broken".into(), false), ("dev".into(), true)]);
    }
}
