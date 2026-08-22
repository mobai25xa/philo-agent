//! Internal presentation colors. This is not a user-facing theme system.
//!
//! Surface colors are derived from the terminal background supplied by the
//! composition root (`TuiLaunchConfig.terminal_palette`); without it they
//! fall back to stable fixed values so tests and unknown terminals stay
//! deterministic.

use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style};

static PALETTE: OnceLock<Option<(u8, u8, u8)>> = OnceLock::new();

/// Called once per process by `run_async`. Later calls keep the first value.
pub(crate) fn init_palette(palette: Option<(u8, u8, u8)>) {
    let _ = PALETTE.set(palette);
}

fn bg_rgb() -> Option<(u8, u8, u8)> {
    *PALETTE.get_or_init(|| None)
}

const FALLBACK_BAND: Color = Color::Indexed(236);
const FALLBACK_DIFF_ADD: Color = Color::Rgb(16, 40, 16);
const FALLBACK_DIFF_DEL: Color = Color::Rgb(40, 16, 16);

/// Contract-pinned accent (tui.md §5): `•` / verb / `›` / Activity emphasis.
const ACCENT: Color = Color::Rgb(222, 137, 72);
const ACCENT_DARK: (u8, u8, u8) = (222, 137, 72);

const FALLBACK_MENU_SELECTED_BG: Color = Color::Rgb(64, 40, 22);

fn luma(bg: (u8, u8, u8)) -> f32 {
    0.2126 * f32::from(bg.0) + 0.7152 * f32::from(bg.1) + 0.0722 * f32::from(bg.2)
}

fn is_light(bg: (u8, u8, u8)) -> bool {
    luma(bg) >= 128.0
}

fn blend(top: (u8, u8, u8), bottom: (u8, u8, u8), alpha: f32) -> (u8, u8, u8) {
    let mix = |t: u8, b: u8| (f32::from(t) * alpha + f32::from(b) * (1.0 - alpha)).round() as u8;
    (
        mix(top.0, bottom.0),
        mix(top.1, bottom.1),
        mix(top.2, bottom.2),
    )
}

/// User/composer band: lifted toward white on dark themes, shaded toward
/// black on light ones, so the surface always reads as part of the theme.
pub(crate) fn band_rgb() -> Color {
    derive_band(bg_rgb())
}

pub(crate) fn derive_band(palette: Option<(u8, u8, u8)>) -> Color {
    match palette {
        Some(bg) => {
            let (top, alpha) = if is_light(bg) {
                ((0, 0, 0), 0.05)
            } else {
                ((255, 255, 255), 0.12)
            };
            let (r, g, b) = blend(top, bg, alpha);
            Color::Rgb(r, g, b)
        }
        None => FALLBACK_BAND,
    }
}

const DIFF_GREEN: (u8, u8, u8) = (63, 185, 80);
const DIFF_RED: (u8, u8, u8) = (248, 81, 73);

fn derive_diff_bg(top: (u8, u8, u8), palette: Option<(u8, u8, u8)>, fallback: Color) -> Color {
    match palette {
        Some(bg) => {
            let alpha = if is_light(bg) { 0.26 } else { 0.20 };
            let (r, g, b) = blend(top, bg, alpha);
            Color::Rgb(r, g, b)
        }
        None => fallback,
    }
}

pub(crate) fn user_band() -> Style {
    Style::default().bg(band_rgb())
}

pub(crate) fn user() -> Style {
    user_band()
}

pub(crate) fn user_gutter() -> Style {
    user_band().fg(ACCENT).add_modifier(Modifier::BOLD)
}

pub(crate) fn reasoning() -> Style {
    Style::default()
        .fg(Color::Rgb(130, 130, 155))
        .add_modifier(Modifier::ITALIC)
}

pub(crate) fn answer() -> Style {
    Style::default()
}

pub(crate) fn answer_gutter() -> Style {
    Style::default().fg(ACCENT)
}

pub(crate) fn tool() -> Style {
    Style::default().fg(ACCENT)
}

pub(crate) fn tool_ok() -> Style {
    Style::default().fg(Color::Green)
}

pub(crate) fn tool_err() -> Style {
    Style::default().fg(Color::Red)
}

pub(crate) fn diff_add() -> Style {
    Style::default()
        .fg(Color::Green)
        .bg(derive_diff_bg(DIFF_GREEN, bg_rgb(), FALLBACK_DIFF_ADD))
}

pub(crate) fn diff_del() -> Style {
    Style::default()
        .fg(Color::Red)
        .bg(derive_diff_bg(DIFF_RED, bg_rgb(), FALLBACK_DIFF_DEL))
}

pub(crate) fn notice() -> Style {
    Style::default().fg(Color::Yellow)
}

pub(crate) fn error() -> Style {
    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
}

pub(crate) fn selection() -> Style {
    Style::default().add_modifier(Modifier::REVERSED)
}

pub(crate) fn meta() -> Style {
    Style::default().fg(Color::DarkGray)
}

pub(crate) fn placeholder() -> Style {
    Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::ITALIC)
}

pub(crate) fn rule() -> Style {
    Style::default().fg(Color::DarkGray)
}

pub(crate) fn activity_normal() -> Style {
    Style::default().fg(ACCENT)
}

pub(crate) fn activity_reasoning() -> Style {
    Style::default().fg(Color::Rgb(130, 130, 155))
}

pub(crate) fn activity_tool() -> Style {
    Style::default().fg(ACCENT)
}

pub(crate) fn activity_warning() -> Style {
    Style::default().fg(Color::Yellow)
}

pub(crate) fn status_idle() -> Style {
    Style::default().fg(Color::DarkGray)
}

pub(crate) fn status_busy() -> Style {
    Style::default().fg(ACCENT)
}

pub(crate) fn menu_usage() -> Style {
    Style::default()
}

/// The command menu floats on its own full-width surface so it reads apart
/// from the transcript; it shares the band family with the composer it sits
/// directly above.
pub(crate) fn menu_panel() -> Style {
    user_band()
}

fn derive_menu_selected_bg(palette: Option<(u8, u8, u8)>) -> Color {
    match palette {
        Some(bg) => {
            let alpha = if is_light(bg) { 0.24 } else { 0.34 };
            let (r, g, b) = blend(ACCENT_DARK, bg, alpha);
            Color::Rgb(r, g, b)
        }
        None => FALLBACK_MENU_SELECTED_BG,
    }
}

pub(crate) fn menu_selected_row() -> Style {
    let bg = derive_menu_selected_bg(bg_rgb());
    let fg = match bg_rgb() {
        Some(bg_rgb) if is_light(bg_rgb) => Color::Rgb(120, 62, 16),
        _ => ACCENT,
    };
    Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blend_interpolates_each_channel() {
        assert_eq!(blend((255, 255, 255), (0, 0, 0), 0.5), (128, 128, 128));
        assert_eq!(blend((0, 0, 0), (100, 100, 100), 0.1), (90, 90, 90));
        assert_eq!(blend((10, 20, 30), (10, 20, 30), 0.9), (10, 20, 30));
    }

    #[test]
    fn light_classification_follows_luma() {
        assert!(is_light((250, 250, 250)));
        assert!(!is_light((20, 22, 28)));
        assert!(is_light((140, 140, 140)));
        assert!(!is_light((120, 120, 120)));
    }

    #[test]
    fn band_without_palette_is_the_stable_fallback() {
        assert_eq!(derive_band(None), FALLBACK_BAND);
        assert_eq!(
            diff_add().bg.unwrap(),
            FALLBACK_DIFF_ADD,
            "diff fallbacks stay fixed"
        );
        assert_eq!(diff_del().bg.unwrap(), FALLBACK_DIFF_DEL);
    }

    #[test]
    fn band_tracks_a_detected_background() {
        let dark = derive_band(Some((18, 20, 26)));
        let Color::Rgb(r, g, b) = dark else {
            panic!("derived band must be rgb, got {dark:?}");
        };
        assert!(r > 18 && g > 20 && b > 26, "dark theme lifts toward white");

        let light = derive_band(Some((240, 238, 230)));
        let Color::Rgb(r, g, b) = light else {
            panic!("derived band must be rgb");
        };
        assert!(r < 240 && g < 238 && b < 230, "light theme shades the band");

        let add = derive_diff_bg(DIFF_GREEN, Some((18, 20, 26)), FALLBACK_DIFF_ADD);
        assert_ne!(add, FALLBACK_DIFF_ADD);
        let del = derive_diff_bg(DIFF_RED, Some((18, 20, 26)), FALLBACK_DIFF_DEL);
        assert_ne!(del, FALLBACK_DIFF_DEL);
        assert!(
            matches!(add, Color::Rgb(..)) && matches!(del, Color::Rgb(..)),
            "diff tints track the background too"
        );
    }
}
