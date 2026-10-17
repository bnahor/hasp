//! Rebindable shortcuts. A binding is a combo string — modifiers then a key,
//! joined by `-`: `mod-shift-n`, `ctrl-tab`, `mod-,`. `mod` is ⌘ on macOS and
//! Ctrl elsewhere. Only changed bindings are stored; everything else is the
//! default, so new shortcuts reach existing users.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Every rebindable action, with its group, label and default combo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Shortcut {
    NewSession,
    OpenProject,
    CommandPalette,
    QuickOpen,
    Settings,
    ModelPicker,
    ToggleSidebar,
    ToggleChanges,
    ToggleFiles,
    NextSession,
    PreviousSession,
    CloseTab,
    ArchiveSession,
    FocusComposer,
}

impl Shortcut {
    pub const ALL: &'static [Shortcut] = &[
        Shortcut::NewSession,
        Shortcut::OpenProject,
        Shortcut::CommandPalette,
        Shortcut::QuickOpen,
        Shortcut::Settings,
        Shortcut::ModelPicker,
        Shortcut::ToggleSidebar,
        Shortcut::ToggleChanges,
        Shortcut::ToggleFiles,
        Shortcut::NextSession,
        Shortcut::PreviousSession,
        Shortcut::CloseTab,
        Shortcut::ArchiveSession,
        Shortcut::FocusComposer,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Shortcut::NewSession => "New session",
            Shortcut::OpenProject => "Open project",
            Shortcut::CommandPalette => "Command palette",
            Shortcut::QuickOpen => "Quick open file",
            Shortcut::Settings => "Settings",
            Shortcut::ModelPicker => "Model picker",
            Shortcut::ToggleSidebar => "Toggle sidebar",
            Shortcut::ToggleChanges => "Toggle changes",
            Shortcut::ToggleFiles => "Toggle files",
            Shortcut::NextSession => "Next session",
            Shortcut::PreviousSession => "Previous session",
            Shortcut::CloseTab => "Close tab",
            Shortcut::ArchiveSession => "Archive session",
            Shortcut::FocusComposer => "Focus composer",
        }
    }

    pub fn group(self) -> &'static str {
        match self {
            Shortcut::NewSession
            | Shortcut::NextSession
            | Shortcut::PreviousSession
            | Shortcut::CloseTab
            | Shortcut::ArchiveSession => "Sessions",
            Shortcut::ToggleSidebar | Shortcut::ToggleChanges | Shortcut::ToggleFiles => "Panels",
            _ => "App",
        }
    }

    pub fn default_combo(self) -> &'static str {
        match self {
            Shortcut::NewSession => "mod-n",
            Shortcut::OpenProject => "mod-o",
            Shortcut::CommandPalette => "mod-k",
            Shortcut::QuickOpen => "mod-p",
            Shortcut::Settings => "mod-,",
            Shortcut::ModelPicker => "mod-/",
            Shortcut::ToggleSidebar => "mod-b",
            Shortcut::ToggleChanges => "mod-j",
            Shortcut::ToggleFiles => "mod-shift-e",
            Shortcut::NextSession => "ctrl-tab",
            Shortcut::PreviousSession => "ctrl-shift-tab",
            Shortcut::CloseTab => "mod-w",
            Shortcut::ArchiveSession => "mod-shift-a",
            Shortcut::FocusComposer => "mod-l",
        }
    }
}

/// Combos that are not rebindable and cannot be taken by a shortcut.
pub const RESERVED: &[(&str, &str)] = &[
    ("mod-q", "Quit"),
    ("mod-c", "Copy"),
    ("mod-v", "Paste"),
    ("mod-x", "Cut"),
    ("mod-z", "Undo"),
    ("mod-a", "Select all"),
];

const MODIFIERS: &[&str] = &["mod", "ctrl", "alt", "shift"];

/// Canonical form of a combo: lower case, `cmd`/`⌘`/`command`/`super` as
/// `mod`, `option`/`opt` as `alt`, modifiers in a fixed order, exactly one
/// non-modifier key. `None` if it is not a usable combo.
pub fn normalize(combo: &str) -> Option<String> {
    let mut mods = Vec::new();
    let mut key: Option<String> = None;
    // A trailing "-" means the key itself is minus: "mod--".
    let (body, minus) = match combo.strip_suffix("--") {
        Some(b) => (b, true),
        None => (combo, false),
    };
    for part in body.split('-').filter(|p| !p.is_empty()) {
        let p = part.trim().to_lowercase();
        let m = match p.as_str() {
            "cmd" | "⌘" | "command" | "super" | "meta" | "mod" => Some("mod"),
            "ctrl" | "control" | "⌃" => Some("ctrl"),
            "alt" | "option" | "opt" | "⌥" => Some("alt"),
            "shift" | "⇧" => Some("shift"),
            _ => None,
        };
        match m {
            Some(m) if !mods.contains(&m) => mods.push(m),
            Some(_) => {}
            None if key.is_none() => key = Some(p),
            None => return None, // two keys
        }
    }
    if minus {
        if key.is_some() {
            return None;
        }
        key = Some("-".into());
    }
    let key = key?;
    // A bare key would fire while typing; every shortcut needs a modifier.
    if mods.is_empty() {
        return None;
    }
    mods.sort_by_key(|m| MODIFIERS.iter().position(|x| x == m));
    Some(format!("{}-{key}", mods.join("-")))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Keymap {
    /// Only bindings that differ from the defaults.
    pub overrides: BTreeMap<Shortcut, String>,
}

impl Keymap {
    /// The combo currently bound to `s`.
    pub fn combo(&self, s: Shortcut) -> String {
        self.overrides
            .get(&s)
            .cloned()
            .unwrap_or_else(|| s.default_combo().to_string())
    }

    /// Which action (or reserved system function) holds `combo`, ignoring `except`.
    fn holder(&self, combo: &str, except: Shortcut) -> Option<&'static str> {
        if let Some((_, label)) = RESERVED.iter().find(|(c, _)| *c == combo) {
            return Some(label);
        }
        Shortcut::ALL
            .iter()
            .find(|s| **s != except && self.combo(**s) == combo)
            .map(|s| s.label())
    }

    /// Bind `s` to `combo`. Refused — and nothing saved — when the combo is
    /// malformed or already taken.
    pub fn bind(&mut self, s: Shortcut, combo: &str) -> Result<(), String> {
        let combo = normalize(combo).ok_or_else(|| {
            format!("{combo:?} is not a shortcut (it needs a modifier and one key)")
        })?;
        if let Some(label) = self.holder(&combo, s) {
            return Err(format!("{combo} is already assigned to {label}."));
        }
        if combo == s.default_combo() {
            self.overrides.remove(&s);
        } else {
            self.overrides.insert(s, combo);
        }
        Ok(())
    }

    pub fn reset(&mut self, s: Shortcut) {
        self.overrides.remove(&s);
    }

    pub fn reset_all(&mut self) {
        self.overrides.clear();
    }

    /// Drop overrides that do not parse or that collide (with a reserved
    /// combo, a default, or another override), so a hand-edited file can
    /// never leave two actions on one key.
    pub fn repair(&mut self) {
        let mut kept = Keymap::default();
        for (s, combo) in std::mem::take(&mut self.overrides) {
            let _ = kept.bind(s, &combo);
        }
        *self = kept;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combos_normalize() {
        assert_eq!(normalize("Shift-Cmd-N").as_deref(), Some("mod-shift-n"));
        assert_eq!(normalize("⌘-,").as_deref(), Some("mod-,"));
        assert_eq!(
            normalize("ctrl-option-tab").as_deref(),
            Some("ctrl-alt-tab")
        );
        assert_eq!(normalize("mod--").as_deref(), Some("mod--"));
        assert_eq!(normalize("n"), None, "needs a modifier");
        assert_eq!(normalize("mod-a-b"), None, "one key");
    }

    #[test]
    fn conflicts_are_refused_and_nothing_is_saved() {
        let mut k = Keymap::default();
        let err = k.bind(Shortcut::QuickOpen, "cmd-k").unwrap_err();
        assert_eq!(err, "mod-k is already assigned to Command palette.");
        assert!(k.overrides.is_empty());
        let err = k.bind(Shortcut::QuickOpen, "mod-c").unwrap_err();
        assert!(err.contains("Copy"), "{err}");
        k.bind(Shortcut::QuickOpen, "mod-shift-p").unwrap();
        assert_eq!(k.combo(Shortcut::QuickOpen), "mod-shift-p");
        // Its old combo is free again.
        k.bind(Shortcut::CommandPalette, "mod-p").unwrap();
        k.reset_all();
        assert_eq!(k.combo(Shortcut::QuickOpen), "mod-p");
    }

    #[test]
    fn rebinding_to_the_default_stores_nothing() {
        let mut k = Keymap::default();
        k.bind(Shortcut::Settings, "mod-shift-s").unwrap();
        k.bind(Shortcut::Settings, "mod-,").unwrap();
        assert!(k.overrides.is_empty());
    }

    #[test]
    fn repair_drops_collisions_from_a_hand_edited_file() {
        let mut k = Keymap::default();
        k.overrides.insert(Shortcut::QuickOpen, "mod-k".into()); // taken by default
        k.overrides.insert(Shortcut::Settings, "not a combo".into());
        k.overrides
            .insert(Shortcut::ModelPicker, "mod-shift-m".into());
        k.repair();
        assert_eq!(k.combo(Shortcut::QuickOpen), "mod-p");
        assert_eq!(k.combo(Shortcut::Settings), "mod-,");
        assert_eq!(k.combo(Shortcut::ModelPicker), "mod-shift-m");
    }

    #[test]
    fn defaults_do_not_collide() {
        let mut seen = std::collections::HashSet::new();
        for s in Shortcut::ALL {
            assert!(seen.insert(s.default_combo()), "{s:?}");
            assert!(
                normalize(s.default_combo()).as_deref() == Some(s.default_combo()),
                "{s:?}"
            );
        }
    }
}
