//! One palette, used everywhere, so state reads the same on every screen.

use ratatui::style::{Color, Modifier, Style};

pub const ACCENT: Color = Color::Rgb(122, 162, 247);
pub const GOOD: Color = Color::Rgb(158, 206, 106);
pub const WARN: Color = Color::Rgb(224, 175, 104);
pub const BAD: Color = Color::Rgb(247, 118, 142);
pub const MUTED: Color = Color::Rgb(105, 112, 152);
pub const TEXT: Color = Color::Rgb(192, 202, 245);
pub const BORDER: Color = Color::Rgb(59, 66, 97);
pub const SELECTED_BG: Color = Color::Rgb(40, 46, 71);
pub const HEADER: Color = Color::Rgb(187, 154, 247);

pub fn base() -> Style {
    Style::default().fg(TEXT)
}

pub fn muted() -> Style {
    Style::default().fg(MUTED)
}

pub fn label() -> Style {
    Style::default().fg(MUTED)
}

pub fn value() -> Style {
    Style::default().fg(TEXT).add_modifier(Modifier::BOLD)
}

pub fn accent() -> Style {
    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
}

pub fn good() -> Style {
    Style::default().fg(GOOD)
}

pub fn warn() -> Style {
    Style::default().fg(WARN)
}

pub fn bad() -> Style {
    Style::default().fg(BAD)
}

pub fn border() -> Style {
    Style::default().fg(BORDER)
}

pub fn title() -> Style {
    Style::default().fg(HEADER).add_modifier(Modifier::BOLD)
}

pub fn selected() -> Style {
    Style::default()
        .bg(SELECTED_BG)
        .fg(TEXT)
        .add_modifier(Modifier::BOLD)
}

/// Green below `warn`, amber up to `bad`, red past it.
pub fn threshold(value: f64, warn_at: f64, bad_at: f64) -> Color {
    if value >= bad_at {
        BAD
    } else if value >= warn_at {
        WARN
    } else {
        GOOD
    }
}

pub fn state_color(state: crate::model::RigState) -> Color {
    use crate::model::RigState::*;
    match state {
        Mining => GOOD,
        Connecting | Authorizing => ACCENT,
        Retrying => WARN,
        Error => BAD,
        Stopped => MUTED,
        Unknown => WARN,
    }
}
