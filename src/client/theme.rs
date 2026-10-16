use ratatui::style::{Color, Modifier, Style};

// The built-in fx-dark palette, without a fixed terminal background.
pub(super) fn muted() -> Style {
    Style::default().fg(Color::Indexed(245))
}

pub(super) fn strong() -> Style {
    Style::default()
        .fg(Color::Indexed(255))
        .add_modifier(Modifier::BOLD)
}

pub(super) fn divider() -> Style {
    Style::default().fg(Color::Indexed(240))
}

pub(super) fn warning() -> Style {
    Style::default().fg(Color::Indexed(252))
}

pub(super) fn notice() -> Style {
    Style::default().fg(Color::Indexed(250))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fx_palette_uses_exact_grays_without_dimming_or_backgrounds() {
        for (style, index) in [
            (divider(), 240),
            (muted(), 245),
            (notice(), 250),
            (warning(), 252),
            (strong(), 255),
        ] {
            assert_eq!(style.fg, Some(Color::Indexed(index)));
            assert_eq!(style.bg, None);
            assert!(!style.add_modifier.contains(Modifier::DIM));
        }
        assert!(strong().add_modifier.contains(Modifier::BOLD));
    }
}
