//! Resident sessions that survive a daemon restart.
//!
//! A resident lives in the daemon, not a client (see `daemon.rs`). Before this,
//! a restarted daemon came up empty: its sessions and their schedules waited
//! until some client happened to reopen the workspace, and one-shot schedules
//! were never re-armed at all. Now a session is restored only when someone
//! *explicitly* enabled it (`hasp resident enable`), recorded in
//! `~/.hasp/residents.json`:
//!
//! ```json
//! {"version": 1, "residents": [{"cwd": "/Users/me/code/app",
//!   "session": "/Users/me/.hasp/sessions/<key>/<ms>-<id>.jsonl",
//!   "environment": null, "enabled_ms": 1760000000000}]}
//! ```
//!
//! What a restored resident does, and doesn't do:
//! - It reopens exactly the recorded session file (not "the latest").
//! - A turn that was running when the daemon died is not resumed or replayed:
//!   its unanswered tool calls are closed as "interrupted" (the journal's
//!   normal repair), and nothing runs until a client prompts or a schedule
//!   comes due. Effects that tool had already started may or may not have
//!   happened; the transcript says so instead of guessing.
//! - Interval schedules: next run one interval after the restart. Runs missed
//!   while the daemon was down are skipped, not caught up.
//! - Cron schedules: next occurrence after the restart; missed ones skipped.
//! - One-shots (`inMs`): due at `set + inMs`. Still in the future: armed for
//!   that time. Already past: not run late; journaled as `expired` and
//!   reported. One-shots are journaled `fired` before they run, so one that
//!   fired before the crash never fires again.
//! - Paused schedules stay paused.

use anyhow::{Context, Result};
use hasp_core::session::HeartbeatState;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResidentEntry {
    pub cwd: PathBuf,
    pub session: PathBuf,
    #[serde(default)]
    pub environment: Option<String>,
    pub enabled_ms: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub residents: Vec<ResidentEntry>,
}

pub fn manifest_path() -> Result<PathBuf> {
    Ok(hasp_workspace::hasp_home()
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .join("residents.json"))
}

impl Manifest {
    /// A missing file is an empty manifest; an unreadable one is an error (the
    /// daemon reports it and restores nothing rather than guessing).
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self { version: 1, residents: vec![] }),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// One entry per workspace: enabling again replaces the session it points at.
    pub fn enable(&mut self, entry: ResidentEntry) {
        self.version = 1;
        self.residents.retain(|r| r.cwd != entry.cwd);
        self.residents.push(entry);
    }

    pub fn disable(&mut self, cwd: &Path) -> bool {
        let before = self.residents.len();
        self.residents.retain(|r| r.cwd != cwd);
        before != self.residents.len()
    }
}

/// What to do with each journaled schedule when a resident is (re)built at `now`.
#[derive(Debug, PartialEq)]
pub enum Arm {
    /// Arm with this first due time (ms).
    At(i64),
    /// A one-shot whose time passed while nobody was running it: don't fire.
    Expired { due_ms: i64 },
}

pub fn plan_schedule(hb: &HeartbeatState, cron: Option<&hasp_core::cron::CronSpec>, now: i64) -> Arm {
    if hb.every_ms.is_some() || hb.cron.is_some() {
        return Arm::At(crate::daemon::initial_due(now, cron, hb.every_ms));
    }
    let due = hb.set_ms.saturating_add(hb.in_ms.unwrap_or_default() as i64);
    if due >= now {
        Arm::At(due)
    } else {
        Arm::Expired { due_ms: due }
    }
}

/// `hasp resident enable|disable|list`. Writes the manifest only; a running
/// daemon picks it up at its next start (it is already holding whatever
/// residents its clients opened).
pub fn command(args: &[String]) -> Result<()> {
    let path = manifest_path()?;
    let mut m = Manifest::load(&path)?;
    match args.first().map(String::as_str) {
        Some("enable") => {
            let cwd = std::env::current_dir()?;
            let ws = hasp_workspace::Workspace::resolve(&cwd).map_err(|e| anyhow::anyhow!("{e}"))?;
            let session = match args.iter().position(|a| a == "--session") {
                Some(i) => PathBuf::from(args.get(i + 1).context("--session needs a path")?),
                None => hasp_core::session::SessionManager::list_for(&ws)?
                    .into_iter()
                    .next()
                    .map(|l| l.path)
                    .context("no session for this workspace yet: start one, then enable it")?,
            };
            let environment = args
                .iter()
                .position(|a| a == "--env")
                .and_then(|i| args.get(i + 1).cloned());
            m.enable(ResidentEntry {
                cwd: ws.root().to_path_buf(),
                session: session.clone(),
                environment,
                enabled_ms: hasp_core::session::now_ms(),
            });
            m.save(&path)?;
            println!("resident enabled: {} -> {}", ws.root().display(), session.display());
            println!("the daemon restores it (and its schedules) whenever it starts");
        }
        Some("disable") => {
            let cwd = std::env::current_dir()?;
            let root = hasp_workspace::Workspace::resolve(&cwd)
                .map(|w| w.root().to_path_buf())
                .unwrap_or(cwd);
            let gone = m.disable(&root);
            m.save(&path)?;
            println!("{}", if gone { "resident disabled" } else { "this workspace was not enabled" });
        }
        Some("list") | None => {
            if m.residents.is_empty() {
                println!("no resident sessions enabled ({})", path.display());
            }
            for r in &m.residents {
                println!("{}  {}", r.cwd.display(), r.session.display());
            }
        }
        Some(other) => anyhow::bail!("unknown resident subcommand {other}: enable [--session PATH] [--env NAME] | disable | list"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hb(every: Option<u64>, in_ms: Option<u64>, cron: Option<&str>, set_ms: i64) -> HeartbeatState {
        HeartbeatState {
            id: "h".into(),
            instruction: "x".into(),
            mode: "steer".into(),
            every_ms: every,
            in_ms,
            cron: cron.map(String::from),
            paused: false,
            set_ms,
        }
    }

    #[test]
    fn intervals_restart_one_period_out_without_catch_up() {
        assert_eq!(plan_schedule(&hb(Some(60_000), None, None, 0), None, 1_000_000), Arm::At(1_060_000));
    }

    #[test]
    fn cron_jumps_to_its_next_occurrence() {
        let spec = hasp_core::cron::parse("0 9 * * *").unwrap();
        match plan_schedule(&hb(None, None, Some("0 9 * * *"), 0), Some(&spec), 1_000_000) {
            Arm::At(t) => assert!(t > 1_000_000),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn one_shots_keep_their_time_or_expire() {
        assert_eq!(plan_schedule(&hb(None, Some(5_000), None, 10_000), None, 12_000), Arm::At(15_000));
        assert_eq!(
            plan_schedule(&hb(None, Some(5_000), None, 10_000), None, 20_000),
            Arm::Expired { due_ms: 15_000 }
        );
    }

    #[test]
    fn manifest_round_trips_one_entry_per_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("residents.json");
        assert!(Manifest::load(&path).unwrap().residents.is_empty());
        let mut m = Manifest::default();
        let e = |s: &str| ResidentEntry { cwd: "/w".into(), session: s.into(), environment: None, enabled_ms: 1 };
        m.enable(e("/a.jsonl"));
        m.enable(e("/b.jsonl"));
        m.save(&path).unwrap();
        let back = Manifest::load(&path).unwrap();
        assert_eq!(back.residents, vec![e("/b.jsonl")]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        std::fs::write(&path, "{broken").unwrap();
        assert!(Manifest::load(&path).is_err(), "a corrupt manifest is reported, not treated as empty");
    }
}
