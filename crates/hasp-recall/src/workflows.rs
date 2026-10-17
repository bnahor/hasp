//! Reusable workflows as versioned skill drafts.
//!
//! No new executor: an active workflow *is* a skill (`~/.hasp/skills/<name>/SKILL.md`),
//! loaded by the existing `skill` tool and run under the session's normal gate
//! and tool policy. This module only manages the path from "this task keeps
//! coming back" to an owner-approved skill:
//!
//! - `candidates`: deterministic repetition detection over the recall index
//!   (same normalized request in N+ sessions), no model involved.
//! - `propose`: write a draft version to `~/.hasp/workflows/<name>/v<N>/SKILL.md`
//!   with `meta.json` (sources, status). Drafts are outside every skill root,
//!   so a draft can't run. The agent may propose; it can't activate.
//! - `activate` (owner only): copy a version into the skills directory with its
//!   version stamped in the front matter. `retire` removes it again.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DraftMeta {
    pub name: String,
    pub version: u32,
    pub status: String, // draft | active | retired
    pub description: String,
    pub source_sessions: Vec<String>,
    pub proposed_by: String, // "agent" | "owner"
    pub created_ms: i64,
    #[serde(default)]
    pub activated_by: Option<String>,
    pub sha256: String,
}

pub struct Workflows {
    root: PathBuf,   // ~/.hasp/workflows
    skills: PathBuf, // ~/.hasp/skills
}

fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 64 && n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

impl Workflows {
    pub fn new(hasp_home: &Path) -> Self {
        Self { root: hasp_home.join("workflows"), skills: hasp_home.join("skills") }
    }

    fn dir(&self, name: &str, v: u32) -> PathBuf {
        self.root.join(name).join(format!("v{v}"))
    }

    pub fn versions(&self, name: &str) -> Vec<DraftMeta> {
        let mut out: Vec<DraftMeta> = std::fs::read_dir(self.root.join(name))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| std::fs::read_to_string(e.path().join("meta.json")).ok())
            .filter_map(|t| serde_json::from_str(&t).ok())
            .collect();
        out.sort_by_key(|m| m.version);
        out
    }

    pub fn list(&self) -> Vec<DraftMeta> {
        let mut names: Vec<String> = std::fs::read_dir(&self.root)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        names.iter().flat_map(|n| self.versions(n)).collect()
    }

    /// New draft version. Never active, whoever proposes it.
    pub fn propose(&self, name: &str, description: &str, body: &str, source_sessions: &[String], by: &str) -> Result<DraftMeta> {
        if !valid_name(name) {
            bail!("workflow names are lowercase letters, digits and '-' (max 64)");
        }
        if body.trim().is_empty() {
            bail!("empty workflow");
        }
        let version = self.versions(name).last().map_or(1, |m| m.version + 1);
        let dir = self.dir(name, version);
        std::fs::create_dir_all(&dir)?;
        let skill = format!(
            "---\nname: {name}\ndescription: {}\n---\n\n{}\n",
            description.replace('\n', " "),
            body.trim()
        );
        std::fs::write(dir.join("SKILL.md"), &skill)?;
        let meta = DraftMeta {
            name: name.into(),
            version,
            status: "draft".into(),
            description: description.into(),
            source_sessions: source_sessions.to_vec(),
            proposed_by: by.into(),
            created_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or_default(),
            activated_by: None,
            sha256: crate::sha256_hex(skill.as_bytes()),
        };
        std::fs::write(dir.join("meta.json"), serde_json::to_string_pretty(&meta)?)?;
        Ok(meta)
    }

    /// Owner-only: install version `v` as the skill. `expected_sha256` must
    /// match the draft the owner reviewed (so a draft edited after review
    /// can't slip in). Any previously active version is marked retired.
    pub fn activate(&self, name: &str, v: u32, expected_sha256: &str, owner: &str) -> Result<()> {
        let dir = self.dir(name, v);
        let skill = std::fs::read_to_string(dir.join("SKILL.md")).with_context(|| format!("no draft {name} v{v}"))?;
        if crate::sha256_hex(skill.as_bytes()) != expected_sha256 {
            bail!("{name} v{v} changed since it was reviewed; review it again");
        }
        for mut m in self.versions(name) {
            if m.status == "active" && m.version != v {
                m.status = "retired".into();
                std::fs::write(self.dir(name, m.version).join("meta.json"), serde_json::to_string_pretty(&m)?)?;
            }
        }
        let target = self.skills.join(name);
        std::fs::create_dir_all(&target)?;
        let stamped = skill.replacen("---\n", &format!("---\nworkflow-version: {v}\n"), 1);
        std::fs::write(target.join("SKILL.md"), stamped)?;
        let mut meta: DraftMeta = serde_json::from_str(&std::fs::read_to_string(dir.join("meta.json"))?)?;
        meta.status = "active".into();
        meta.activated_by = Some(owner.into());
        std::fs::write(dir.join("meta.json"), serde_json::to_string_pretty(&meta)?)?;
        Ok(())
    }

    /// Owner-only: stop offering it as a skill (drafts stay for the record).
    pub fn retire(&self, name: &str) -> Result<()> {
        let target = self.skills.join(name).join("SKILL.md");
        let text = std::fs::read_to_string(&target).unwrap_or_default();
        if !text.contains("workflow-version:") {
            bail!("{name} is not an activated workflow (hand-written skills are left alone)");
        }
        std::fs::remove_file(target)?;
        for mut m in self.versions(name) {
            if m.status == "active" {
                m.status = "retired".into();
                std::fs::write(self.dir(name, m.version).join("meta.json"), serde_json::to_string_pretty(&m)?)?;
            }
        }
        Ok(())
    }
}

/// A request that recurs across sessions: (normalized text, session ids).
#[derive(Debug, Serialize, PartialEq)]
pub struct Candidate {
    pub request: String,
    pub sessions: Vec<String>,
}

/// Deterministic: user messages whose normalized form (lowercase, digits and
/// punctuation removed, whitespace collapsed) appears in at least `min`
/// different sessions.
pub fn candidates(index: &crate::Index, min: usize) -> Result<Vec<Candidate>> {
    let rows = index.user_messages()?;
    let mut groups: std::collections::BTreeMap<String, std::collections::BTreeSet<String>> = Default::default();
    for (session, text) in rows {
        let norm: String = text
            .to_lowercase()
            .chars()
            .map(|c| if c.is_alphabetic() || c.is_whitespace() { c } else { ' ' })
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if norm.split(' ').count() >= 2 {
            groups.entry(norm).or_default().insert(session);
        }
    }
    Ok(groups
        .into_iter()
        .filter(|(_, s)| s.len() >= min)
        .map(|(request, s)| Candidate { request, sessions: s.into_iter().collect() })
        .collect())
}
