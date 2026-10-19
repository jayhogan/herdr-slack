//! The only place colors are chosen (spec §9). ANSI 16 only: no Blue or Magenta; White appears
//! only in `hot_row`.

use ratatui::style::{Color, Modifier, Style};

use crate::model::Kind;

/// The auth-error banner.
pub fn hot_row() -> Style {
    Style::new()
        .bg(Color::Red)
        .fg(Color::White)
        .add_modifier(Modifier::BOLD)
}

pub fn dim() -> Style {
    Style::new().fg(Color::DarkGray)
}

pub fn bold() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

/// Header bar.
pub fn bar() -> Style {
    Style::new().add_modifier(Modifier::REVERSED)
}

/// Failure text in the status line.
pub fn warn() -> Style {
    Style::new().fg(Color::Yellow)
}

// The selected row is `▌` plus `bold()` with no background: a background would hide dim text
// on some themes.

pub fn kind(k: Kind) -> Style {
    match k {
        Kind::Watch => Style::new().fg(Color::Yellow),
        Kind::Mention => Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        Kind::Reply => Style::new().fg(Color::Cyan),
        Kind::Thread => Style::new().fg(Color::Green),
        Kind::Channel => Style::new().fg(Color::Reset).add_modifier(Modifier::BOLD),
        Kind::Dm => Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_use_the_canvas_palette() {
        assert_eq!(kind(Kind::Watch).fg, Some(Color::Yellow));
        assert_eq!(kind(Kind::Mention).fg, Some(Color::Red));
        assert!(kind(Kind::Mention).add_modifier.contains(Modifier::BOLD));
        assert_eq!(kind(Kind::Reply).fg, Some(Color::Cyan));
        assert_eq!(kind(Kind::Thread).fg, Some(Color::Green));
        assert_eq!(kind(Kind::Channel).fg, Some(Color::Reset));
        assert!(kind(Kind::Dm).add_modifier.contains(Modifier::BOLD));
        for k in [
            Kind::Watch,
            Kind::Mention,
            Kind::Reply,
            Kind::Thread,
            Kind::Channel,
            Kind::Dm,
        ] {
            assert!(!matches!(
                kind(k).fg,
                Some(Color::Blue | Color::Magenta | Color::White)
            ));
        }
    }
}
