//! Settings state shared by the CLI, the daemon and the desktop app.
//!
//! Everything here lives in `~/.hasp` (see [`hasp_workspace::hasp_home`]):
//!
//! | file | what |
//! |---|---|
//! | `ui-settings.json` | [`UiSettings`]: appearance, files, notifications, shortcuts |
//! | `agent-prefs.json` | [`AgentPrefs`]: which agents are enabled, executable overrides |
//! | `device.json` | [`Device`]: this machine's id and display name |
//! | `archived.json` | [`Archive`]: archived session ids (journals are append-only) |
//!
//! Files are read leniently — unknown fields are ignored, missing ones take
//! defaults, and every number is clamped — so a hand-edited or newer file can
//! never break the app. Writes are atomic (temp file, then rename).

pub mod keymap;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub use keymap::{Keymap, Shortcut};

/// Current [`UiSettings`] schema version.
pub const UI_SETTINGS_VERSION: u32 = 1;

// ---- files ----

fn home() -> std::io::Result<PathBuf> {
    hasp_workspace::hasp_home().map_err(|e| std::io::Error::other(e.to_string()))
}

/// Read a JSON file, or the default when it is missing or unreadable.
pub fn read_or_default<T: DeserializeOwned + Default>(path: &Path) -> T {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Write JSON atomically: a crash mid-write leaves the old file, not half a
/// new one.
pub fn write_atomic<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
    std::fs::rename(tmp, path)
}

// ---- UI settings ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Motion {
    #[default]
    System,
    Reduced,
    Full,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Font {
    /// Empty means the platform default for this role.
    pub family: String,
    pub size: f32,
}

impl Font {
    fn new(size: f32) -> Self {
        Self {
            family: String::new(),
            size,
        }
    }
}

impl Default for Font {
    fn default() -> Self {
        Self::new(14.0)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Appearance {
    pub mode: Mode,
    pub light_theme: String,
    pub dark_theme: String,
    /// `None` uses the theme's own accent.
    pub accent: Option<String>,
    pub ui_font: Font,
    pub code_font: Font,
    pub terminal_font: Font,
    /// Width of the conversation column, in logical pixels.
    pub conversation_width: u32,
    /// Fold reasoning, tool calls and narration into one row each.
    pub compact: bool,
    pub motion: Motion,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            mode: Mode::System,
            light_theme: "hasp-light".into(),
            dark_theme: "hasp-dark".into(),
            accent: None,
            ui_font: Font::new(14.0),
            code_font: Font::new(13.0),
            terminal_font: Font::new(13.0),
            conversation_width: 768,
            compact: false,
            motion: Motion::System,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Files {
    /// Include hidden and gitignored files in file trees.
    pub show_all: bool,
    pub word_wrap: bool,
    pub autosave: bool,
    pub autosave_delay_ms: u32,
    /// Where "new project" starts browsing; `None` is the home directory.
    pub default_project_dir: Option<PathBuf>,
}

impl Default for Files {
    fn default() -> Self {
        Self {
            show_all: false,
            word_wrap: true,
            autosave: false,
            autosave_delay_ms: 900,
            default_project_dir: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Notifications {
    pub sounds: bool,
    pub sound_on_done: bool,
    pub sound_on_input: bool,
    pub sound_on_error: bool,
    pub desktop: bool,
    /// Desktop notifications only when the app is not focused.
    pub only_in_background: bool,
}

impl Default for Notifications {
    fn default() -> Self {
        Self {
            sounds: true,
            sound_on_done: true,
            sound_on_input: true,
            sound_on_error: true,
            desktop: true,
            only_in_background: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SendWith {
    #[default]
    Enter,
    ModEnter,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Behaviour {
    pub send_with: SendWith,
    /// Escape stops the running turn (otherwise only the stop button does).
    pub escape_stops_agent: bool,
    /// Closing a tab archives its session.
    pub archive_on_close: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiSettings {
    pub version: u32,
    pub appearance: Appearance,
    pub files: Files,
    pub notifications: Notifications,
    pub behaviour: Behaviour,
    pub keymap: Keymap,
    /// The settings section shown last, reopened next time.
    pub last_section: String,
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            version: UI_SETTINGS_VERSION,
            appearance: Appearance::default(),
            files: Files::default(),
            notifications: Notifications::default(),
            behaviour: Behaviour::default(),
            keymap: Keymap::default(),
            last_section: "general".into(),
        }
    }
}

impl UiSettings {
    pub fn path() -> std::io::Result<PathBuf> {
        Ok(home()?.join("ui-settings.json"))
    }

    pub fn load() -> Self {
        Self::path()
            .map(|p| Self::load_from(&p))
            .unwrap_or_default()
    }

    pub fn load_from(path: &Path) -> Self {
        let mut s: Self = read_or_default(path);
        s.clamp();
        s
    }

    pub fn save(&self) -> std::io::Result<()> {
        self.save_to(&Self::path()?)
    }

    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        let mut s = self.clone();
        s.version = UI_SETTINGS_VERSION;
        s.clamp();
        write_atomic(path, &s)
    }

    /// Bring every number back into its range, and drop shortcut overrides
    /// that no longer parse or that collide.
    pub fn clamp(&mut self) {
        let a = &mut self.appearance;
        for f in [&mut a.ui_font, &mut a.code_font, &mut a.terminal_font] {
            f.size = if f.size.is_finite() {
                f.size.clamp(9.0, 28.0)
            } else {
                14.0
            };
        }
        a.conversation_width = (a.conversation_width.clamp(560, 1200) / 16) * 16;
        self.files.autosave_delay_ms = self.files.autosave_delay_ms.clamp(100, 10_000);
        self.keymap.repair();
    }
}

// ---- agents ----

/// Per-agent preferences. Agents not listed take [`AgentPrefs::default_enabled`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AgentPrefs {
    pub agents: BTreeMap<String, AgentPref>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AgentPref {
    pub enabled: Option<bool>,
    /// Use this binary instead of the one on PATH (a custom build).
    pub executable: Option<String>,
}

impl AgentPrefs {
    /// The agents on by default once installed: our own engine and the two
    /// most common subscriptions. Everything else is opt-in.
    pub const DEFAULT_ON: &'static [&'static str] = &["hasp", "claude", "codex"];

    pub fn path() -> std::io::Result<PathBuf> {
        Ok(home()?.join("agent-prefs.json"))
    }

    pub fn load() -> Self {
        Self::path()
            .map(|p| read_or_default(&p))
            .unwrap_or_default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        write_atomic(&Self::path()?, self)
    }

    pub fn default_enabled(name: &str) -> bool {
        Self::DEFAULT_ON.contains(&name)
    }

    pub fn enabled(&self, name: &str) -> bool {
        self.agents
            .get(name)
            .and_then(|p| p.enabled)
            .unwrap_or_else(|| Self::default_enabled(name))
    }

    /// Turn an agent on or off. Refuses to turn off the last enabled one of
    /// `available` — the app must always have something to run.
    pub fn set_enabled(&mut self, name: &str, on: bool, available: &[&str]) -> Result<(), String> {
        if !on {
            let others_on = available.iter().any(|a| *a != name && self.enabled(a));
            if !others_on {
                return Err(format!(
                    "{name} is the only enabled agent; enable another first"
                ));
            }
        }
        self.agents.entry(name.to_string()).or_default().enabled = Some(on);
        Ok(())
    }

    pub fn executable(&self, name: &str) -> Option<&str> {
        self.agents
            .get(name)?
            .executable
            .as_deref()
            .filter(|e| !e.is_empty())
    }

    pub fn set_executable(&mut self, name: &str, exe: Option<String>) {
        self.agents.entry(name.to_string()).or_default().executable =
            exe.filter(|e| !e.trim().is_empty());
    }
}

// ---- device ----

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Device {
    pub id: String,
    /// Display name; empty means the hostname.
    pub name: String,
    pub added_ms: i64,
}

impl Device {
    pub fn path() -> std::io::Result<PathBuf> {
        Ok(home()?.join("device.json"))
    }

    /// This machine's record, created on first use.
    pub fn this() -> std::io::Result<Self> {
        Self::this_at(&Self::path()?)
    }

    pub fn this_at(path: &Path) -> std::io::Result<Self> {
        let mut d: Self = read_or_default(path);
        if d.id.is_empty() {
            d.id = random_id();
            d.added_ms = now_ms();
            write_atomic(path, &d)?;
        }
        Ok(d)
    }

    pub fn display_name(&self) -> String {
        if self.name.trim().is_empty() {
            hostname()
        } else {
            self.name.clone()
        }
    }

    pub fn rename(&mut self, name: &str) -> std::io::Result<()> {
        self.rename_at(&Self::path()?, name)
    }

    pub fn rename_at(&mut self, path: &Path, name: &str) -> std::io::Result<()> {
        self.name = name.trim().chars().take(64).collect();
        write_atomic(path, self)
    }

    /// `abcd1234…wxyz`: enough to tell devices apart at a glance.
    pub fn short_id(&self) -> String {
        if self.id.len() <= 12 {
            return self.id.clone();
        }
        format!("{}…{}", &self.id[..8], &self.id[self.id.len() - 4..])
    }
}

pub fn hostname() -> String {
    std::process::Command::new("hostname")
        .arg("-s")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "this device".into())
}

pub fn os_label() -> &'static str {
    match std::env::consts::OS {
        "macos" => "macOS",
        "linux" => "Linux",
        "windows" => "Windows",
        other => other,
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn random_id() -> String {
    let mut bytes = [0u8; 16];
    let ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut bytes))
        .is_ok();
    if !ok {
        let seed = (now_ms() as u128) << 32 | std::process::id() as u128;
        bytes.copy_from_slice(&seed.to_le_bytes());
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ---- archive ----

/// Archived sessions, by id. A separate index rather than a journal entry:
/// archiving is a view preference, and journals are append-only history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Archive {
    pub sessions: BTreeSet<String>,
}

impl Archive {
    pub fn path() -> std::io::Result<PathBuf> {
        Ok(home()?.join("archived.json"))
    }

    pub fn load() -> Self {
        Self::path()
            .map(|p| read_or_default(&p))
            .unwrap_or_default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        write_atomic(&Self::path()?, self)
    }

    pub fn contains(&self, id: &str) -> bool {
        self.sessions.contains(id)
    }

    pub fn set(&mut self, id: &str, archived: bool) {
        if archived {
            self.sessions.insert(id.to_string());
        } else {
            self.sessions.remove(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hand_edited_file_is_clamped_not_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("ui-settings.json");
        std::fs::write(
            &path,
            r#"{"appearance": {"mode": "dark", "ui_font": {"size": 400},
                "conversation_width": 9000, "future_field": true},
               "files": {"autosave_delay_ms": 3},
               "something_new": [1, 2]}"#,
        )
        .unwrap();
        let s = UiSettings::load_from(&path);
        assert_eq!(s.appearance.mode, Mode::Dark);
        assert_eq!(s.appearance.ui_font.size, 28.0);
        assert_eq!(s.appearance.conversation_width, 1200);
        assert_eq!(s.files.autosave_delay_ms, 100);
        assert!(s.notifications.sounds, "missing sections take defaults");
    }

    #[test]
    fn garbage_falls_back_to_defaults_and_saves_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("ui-settings.json");
        std::fs::write(&path, "not json").unwrap();
        let mut s = UiSettings::load_from(&path);
        assert_eq!(s, UiSettings::default());
        s.appearance.compact = true;
        s.files.show_all = true;
        s.save_to(&path).unwrap();
        assert_eq!(UiSettings::load_from(&path), s);
    }

    #[test]
    fn the_last_enabled_agent_cannot_be_turned_off() {
        let mut p = AgentPrefs::default();
        let available = ["hasp", "claude", "codex", "gemini"];
        assert!(p.enabled("claude") && !p.enabled("gemini"), "defaults");
        p.set_enabled("claude", false, &available).unwrap();
        p.set_enabled("codex", false, &available).unwrap();
        let err = p.set_enabled("hasp", false, &available).unwrap_err();
        assert!(err.contains("only enabled agent"), "{err}");
        p.set_enabled("gemini", true, &available).unwrap();
        p.set_enabled("hasp", false, &available).unwrap();
        assert!(!p.enabled("hasp") && p.enabled("gemini"));
    }

    #[test]
    fn executable_overrides_ignore_blank_input() {
        let mut p = AgentPrefs::default();
        p.set_executable("codex", Some("/opt/codex".into()));
        assert_eq!(p.executable("codex"), Some("/opt/codex"));
        p.set_executable("codex", Some("  ".into()));
        assert_eq!(p.executable("codex"), None);
    }

    #[test]
    fn a_device_gets_one_stable_id_and_can_be_renamed() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("device.json");
        let mut d = Device::this_at(&path).unwrap();
        assert_eq!(d.id.len(), 32);
        assert_eq!(Device::this_at(&path).unwrap().id, d.id, "stable");
        assert_eq!(d.display_name(), hostname(), "hostname until renamed");
        d.rename_at(&path, "  Studio Mac  ").unwrap();
        assert_eq!(Device::this_at(&path).unwrap().display_name(), "Studio Mac");
        assert!(d.short_id().contains('…'));
    }

    #[test]
    fn archive_toggles() {
        let mut a = Archive::default();
        a.set("s1", true);
        assert!(a.contains("s1"));
        a.set("s1", false);
        assert!(!a.contains("s1"));
    }
}
