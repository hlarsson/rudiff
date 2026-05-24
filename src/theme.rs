//! Color palette.
//!
//! The handoff asks for a palette that works in both light and dark terminals
//! and degrades from truecolor to 256-color. We adapt along two axes detected
//! from the environment:
//!
//! * **truecolor** — `COLORTERM=truecolor|24bit`. When absent we fall back to
//!   named ANSI / indexed-256 colors, which the terminal renders in its own
//!   palette (so they remain theme-appropriate by construction).
//! * **light vs dark** — `COLORFGBG` (e.g. `15;0` is light-on-dark). Only used
//!   to pick truecolor background tints; defaults to dark when unknown.

use ratatui::style::{Color, Modifier, Style};

#[derive(Clone, Copy)]
pub struct Theme {
    // Foreground accents.
    pub added: Color,
    pub removed: Color,
    pub modified: Color,
    pub renamed: Color,
    /// Dimmed text — viewed rows, secondary hints.
    pub tertiary: Color,
    pub secondary: Color,
    pub accent: Color,
    // Backgrounds.
    pub bg_success: Color,
    pub bg_danger: Color,
    /// Stronger backgrounds for intra-line (changed-character) spans.
    pub bg_success_strong: Color,
    pub bg_danger_strong: Color,
    /// Selected-row / focused-line background.
    pub bg_selected: Color,
    /// Subtle separator / chrome color.
    pub chrome: Color,
}

impl Theme {
    pub fn detect() -> Theme {
        let truecolor = std::env::var("COLORTERM")
            .map(|v| v == "truecolor" || v == "24bit")
            .unwrap_or(false);
        let dark = is_dark_background();
        if truecolor {
            if dark {
                Theme::dark_truecolor()
            } else {
                Theme::light_truecolor()
            }
        } else {
            Theme::ansi(dark)
        }
    }

    fn dark_truecolor() -> Theme {
        Theme {
            added: Color::Rgb(126, 211, 135),
            removed: Color::Rgb(224, 108, 117),
            modified: Color::Rgb(229, 192, 123),
            renamed: Color::Rgb(97, 175, 239),
            tertiary: Color::Rgb(110, 118, 129),
            secondary: Color::Rgb(150, 158, 169),
            accent: Color::Rgb(198, 160, 246),
            bg_success: Color::Rgb(26, 42, 31),
            bg_danger: Color::Rgb(51, 30, 33),
            bg_success_strong: Color::Rgb(43, 86, 53),
            bg_danger_strong: Color::Rgb(95, 41, 47),
            bg_selected: Color::Rgb(45, 50, 60),
            chrome: Color::Rgb(80, 86, 96),
        }
    }

    fn light_truecolor() -> Theme {
        Theme {
            added: Color::Rgb(34, 134, 58),
            removed: Color::Rgb(207, 34, 46),
            modified: Color::Rgb(154, 103, 0),
            renamed: Color::Rgb(9, 105, 218),
            tertiary: Color::Rgb(140, 149, 159),
            secondary: Color::Rgb(90, 99, 110),
            accent: Color::Rgb(130, 80, 223),
            bg_success: Color::Rgb(218, 246, 222),
            bg_danger: Color::Rgb(255, 224, 224),
            bg_success_strong: Color::Rgb(171, 230, 181),
            bg_danger_strong: Color::Rgb(255, 188, 188),
            bg_selected: Color::Rgb(221, 226, 233),
            chrome: Color::Rgb(190, 197, 205),
        }
    }

    /// Named/indexed fallback for terminals without truecolor. Foregrounds use
    /// ANSI names so they track the user's theme; backgrounds use subtle
    /// indexed-256 tints chosen per light/dark.
    fn ansi(dark: bool) -> Theme {
        let (bg_success, bg_danger, bg_success_strong, bg_danger_strong, bg_selected) = if dark {
            (
                Color::Indexed(22),
                Color::Indexed(52),
                Color::Indexed(28),
                Color::Indexed(88),
                Color::Indexed(237),
            )
        } else {
            (
                Color::Indexed(194),
                Color::Indexed(224),
                Color::Indexed(157),
                Color::Indexed(217),
                Color::Indexed(254),
            )
        };
        Theme {
            added: Color::Green,
            removed: Color::Red,
            modified: Color::Yellow,
            renamed: Color::Blue,
            tertiary: Color::DarkGray,
            secondary: Color::Gray,
            accent: Color::Magenta,
            bg_success,
            bg_danger,
            bg_success_strong,
            bg_danger_strong,
            bg_selected,
            chrome: Color::DarkGray,
        }
    }

    // ---- Style helpers ----

    pub fn added_style(&self) -> Style {
        Style::default().fg(self.added)
    }
    pub fn removed_style(&self) -> Style {
        Style::default().fg(self.removed)
    }
    pub fn dim(&self) -> Style {
        Style::default().fg(self.tertiary)
    }
    pub fn chrome_style(&self) -> Style {
        Style::default().fg(self.chrome)
    }
    pub fn header_style(&self) -> Style {
        Style::default()
            .fg(self.accent)
            .add_modifier(Modifier::BOLD)
    }
    /// Map a tree-sitter highlight capture name (already reduced to one of our
    /// recognized base names) to a style. Unknown names render with no color.
    pub fn syntax_style(&self, name: &str) -> Style {
        let truecolor = matches!(self.accent, Color::Rgb(..));
        // Pick a color per capture group, adapting to the available palette.
        let color = if truecolor {
            match name {
                "keyword" => Color::Rgb(198, 120, 221),
                "function" | "constructor" => Color::Rgb(97, 175, 239),
                "string" | "string.special" => Color::Rgb(152, 195, 121),
                "comment" => self.tertiary,
                "type" => Color::Rgb(229, 192, 123),
                "number" | "constant" => Color::Rgb(209, 154, 102),
                "operator" | "punctuation" => self.secondary,
                "tag" | "attribute" => Color::Rgb(224, 108, 117),
                "property" => Color::Rgb(86, 182, 194),
                "module" | "label" => Color::Rgb(86, 182, 194),
                "escape" => Color::Rgb(86, 182, 194),
                _ => return Style::default(),
            }
        } else {
            match name {
                "keyword" => Color::Magenta,
                "function" | "constructor" => Color::Blue,
                "string" | "string.special" => Color::Green,
                "comment" => self.tertiary,
                "type" => Color::Yellow,
                "number" | "constant" => Color::Cyan,
                "operator" | "punctuation" => self.secondary,
                "tag" | "attribute" => Color::Red,
                "property" | "module" | "label" | "escape" => Color::Cyan,
                _ => return Style::default(),
            }
        };
        let style = Style::default().fg(color);
        if name == "comment" {
            style.add_modifier(Modifier::ITALIC)
        } else {
            style
        }
    }

    pub fn status_style(&self, s: crate::git::model::FileStatus) -> Style {
        use crate::git::model::FileStatus::*;
        let c = match s {
            Added => self.added,
            Deleted => self.removed,
            Modified => self.modified,
            Renamed | Copied => self.renamed,
        };
        Style::default().fg(c).add_modifier(Modifier::BOLD)
    }
}

/// Best-effort light/dark detection from `COLORFGBG` (`fg;bg`). A background
/// index of 0–6 or 8 is treated as dark; 7 or 15 (and the light variants) as
/// light. Unknown => dark, the common developer default.
fn is_dark_background() -> bool {
    let Ok(v) = std::env::var("COLORFGBG") else {
        return true;
    };
    let Some(bg) = v.split(';').next_back() else {
        return true;
    };
    match bg.trim().parse::<u8>() {
        Ok(7) | Ok(15) => false,
        Ok(_) => true,
        Err(_) => true,
    }
}
