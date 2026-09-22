//! Color themes. One `Theme` struct feeds every content color the TUI uses
//! (transcript kinds, markdown, syntax, diff), so light mode is a palette
//! swap, not a scattering of conditionals. Selected by `HARNESS_THEME`
//! (`dark` default, `light`), switchable at runtime with `/theme`.

use ratatui::style::Color;
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    pub name: &'static str,
    // transcript line kinds
    pub user: Color,
    pub meta: Color,
    pub tool: Color,
    pub thinking: Color,
    // markdown
    pub heading1: Color,
    pub code: Color,
    pub inline_code: Color,
    pub quote: Color,
    // syntax
    pub keyword: Color,
    pub string: Color,
    pub number: Color,
    pub comment: Color,
    // diffs
    pub diff_add: Color,
    pub diff_del: Color,
    pub diff_hunk: Color,
}

impl Theme {
    /// The historical colors — dark background assumed, nothing changes for
    /// anyone who never asks for a theme.
    pub fn dark() -> Self {
        Self {
            name: "dark",
            user: Color::Cyan,
            meta: Color::Yellow,
            tool: Color::Gray,
            thinking: Color::DarkGray,
            heading1: Color::Cyan,
            code: Color::Rgb(180, 190, 170),
            inline_code: Color::Yellow,
            quote: Color::Rgb(140, 150, 170),
            keyword: Color::Magenta,
            string: Color::Green,
            number: Color::Yellow,
            comment: Color::DarkGray,
            diff_add: Color::Green,
            diff_del: Color::Red,
            diff_hunk: Color::Cyan,
        }
    }

    /// Darkened tones that keep the same hierarchy on a white background —
    /// ANSI yellow/cyan as-is would wash out.
    pub fn light() -> Self {
        Self {
            name: "light",
            user: Color::Rgb(0, 90, 160),
            meta: Color::Rgb(150, 90, 0),
            tool: Color::Rgb(90, 90, 90),
            thinking: Color::Rgb(120, 120, 120),
            heading1: Color::Rgb(0, 90, 160),
            code: Color::Rgb(80, 100, 90),
            inline_code: Color::Rgb(140, 90, 0),
            quote: Color::Rgb(100, 100, 120),
            keyword: Color::Rgb(130, 40, 140),
            string: Color::Rgb(0, 110, 40),
            number: Color::Rgb(180, 90, 0),
            comment: Color::Rgb(130, 130, 130),
            diff_add: Color::Rgb(0, 120, 40),
            diff_del: Color::Rgb(190, 30, 30),
            diff_hunk: Color::Rgb(0, 90, 160),
        }
    }

    fn from_env() -> Self {
        match std::env::var("HARNESS_THEME").as_deref() {
            Ok("light") => Self::light(),
            _ => Self::dark(),
        }
    }
}

static THEME: Mutex<Option<Theme>> = Mutex::new(None);

/// The active theme (initialized from `HARNESS_THEME` on first use).
pub fn current() -> Theme {
    let mut slot = THEME.lock().expect("theme lock");
    *slot.get_or_insert_with(Theme::from_env)
}

/// Switch at runtime (`/theme light|dark`).
pub fn set(theme: Theme) {
    *THEME.lock().expect("theme lock") = Some(theme);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palettes_differ_where_it_matters() {
        let (dark, light) = (Theme::dark(), Theme::light());
        // The point of light mode: dark's bright ANSI colors are unreadable on
        // white, so every shared slot must move.
        for (d, l) in [
            (dark.user, light.user),
            (dark.meta, light.meta),
            (dark.inline_code, light.inline_code),
            (dark.string, light.string),
        ] {
            assert_ne!(d, l);
        }
        assert_eq!(dark.name, "dark");
        assert_eq!(light.name, "light");
    }
}
