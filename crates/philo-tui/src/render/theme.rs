//! Design tokens for the philo-tui presentation layer (redesign spec §2).
//! This is not a user-facing theme system.
//!
//! Single source for colors, symbols, and geometry:
//!
//! - §2.1 color tokens: [`surface`] / [`accent`] / [`primary`] / [`meta`] /
//!   [`reasoning`] / [`ok`-as-`diff_add`] / [`warn`] / [`err`] / diff tints;
//! - §2.2 symbol tokens: [`BAR`] / [`DETAIL`] / [`SPINNER_FRAMES`] /
//!   [`ELLIPSIS`];
//! - §2.3 geometry tokens: [`CONTENT_INSET`] / [`COMPOSER_ROWS`] /
//!   [`INPUT_ROWS`] / [`MENU_MAX_ROWS`] / [`picker_width`] /
//!   [`picker_height`];
//! - composite styles ([`notice`] / [`error`] / [`selection`] /
//!   [`placeholder`] / [`rule`] / [`panel_border`] /
//!   [`menu_selected_row`]) that derive from the tokens above;
//! - prose typography ([`inline_code`] / [`link`] / [`code_fg`]) — the
//!   answer body's style vocabulary (tui.md v0.39 §5.x). Heading rungs and
//!   structural tones are emitted as semantics by `app::prose` and realized
//!   here through [`corner_meta`] / [`meta`]; see
//!   `render::markdown::resolve`.
//!
//! Surface colors are derived from the terminal background supplied by the
//! composition root (`TuiLaunchConfig.terminal_palette`); without it they
//! fall back to stable fixed values so tests and unknown terminals stay
//! deterministic.

use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style};

// ---------------------------------------------------------------------------
// Background derivation (mechanism unchanged by the redesign)
// ---------------------------------------------------------------------------

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

/// Surface base for bands and float panels: lifted toward white on dark
/// themes, shaded toward black on light ones, so the surface always reads as
/// part of the theme.
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

const FALLBACK_MENU_SELECTED_BG: Color = Color::Rgb(64, 40, 22);

// ---------------------------------------------------------------------------
// §2.1 color tokens
// ---------------------------------------------------------------------------

/// Contract-pinned accent (tui.md §5): left bars, spinner, selected-row tint,
/// current-session marker, tool-card display names. Usage stays restrained.
const ACCENT: Color = Color::Rgb(222, 137, 72);
const ACCENT_DARK: (u8, u8, u8) = (222, 137, 72);

/// Bands: full-width surface behind user strips and the composer band.
/// Float panels sit on the native terminal background since v0.44.
pub(crate) fn surface() -> Style {
    Style::default().bg(band_rgb())
}

/// Default foreground: user messages, answers, composer drafts, state words,
/// the model name in the corner dashboard.
pub(crate) fn primary() -> Style {
    Style::default()
}

/// Accent foreground.
pub(crate) fn accent() -> Style {
    Style::default().fg(ACCENT)
}

/// Dimmed chrome: tool rows, system rows, secondary float info. Information
/// density comes from gray levels, not color.
pub(crate) fn meta() -> Style {
    Style::default().fg(Color::DarkGray)
}

/// Corner-dashboard chrome: one step brighter than [`meta`] so the four
/// corners read at arm's length while staying below [`primary`] weight.
pub(crate) fn corner_meta() -> Style {
    Style::default().fg(Color::Gray)
}

/// Think bodies and collapsed headers: gray-purple italic.
pub(crate) fn reasoning() -> Style {
    Style::default()
        .fg(Color::Rgb(130, 130, 155))
        .add_modifier(Modifier::ITALIC)
}

pub(crate) fn warn() -> Style {
    Style::default().fg(Color::Yellow)
}

pub(crate) fn err() -> Style {
    Style::default().fg(Color::Red)
}

/// Diff tints keep the background derivation: `+`/`-` backgrounds fill the
/// whole content column.
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

// ---------------------------------------------------------------------------
// Prose typography (tui.md v0.47 §5 Prose typography / prose v4)
//
// Element types read apart at a glance: brand orange marks the document
// skeleton (headings, quote bars, checked boxes, language chips, table
// headers), links stay blue, structural noise stays meta. Inline code is a
// soft green on the text itself — no background block (prose v4: a tinted
// pill fought the terminal and read as a highlighter smear). Every style
// below is what `render/markdown.rs` may spend on prose — no hardcoded
// colors survive there.
// ---------------------------------------------------------------------------

/// Stable indexed soft green for unknown palettes (#87d787).
const FALLBACK_CODE_FG: Color = Color::Indexed(108);

/// Code green on dark backgrounds (one-dark family).
const CODE_GREEN_DARK: (u8, u8, u8) = (152, 195, 121);

/// Code green on light backgrounds: same hue, dark enough for contrast.
const CODE_GREEN_LIGHT: (u8, u8, u8) = (88, 118, 60);

fn derive_code_fg(palette: Option<(u8, u8, u8)>) -> Color {
    match palette {
        Some(bg) => {
            let (r, g, b) = if is_light(bg) {
                CODE_GREEN_LIGHT
            } else {
                CODE_GREEN_DARK
            };
            Color::Rgb(r, g, b)
        }
        None => FALLBACK_CODE_FG,
    }
}

/// Inline code: soft green text, no background block.
pub(crate) fn inline_code() -> Style {
    Style::default().fg(code_fg())
}

/// The code green, exposed for the prose span resolver.
pub(crate) fn code_fg() -> Color {
    derive_code_fg(bg_rgb())
}

/// Links keep the universal blue convention; accent stays reserved for
/// activity (§2.1 usage list is untouched).
pub(crate) fn link() -> Style {
    Style::default()
        .fg(Color::Blue)
        .add_modifier(Modifier::UNDERLINED)
}

/// Whether the detected terminal background is light (syntax-theme pick).
/// Without a palette this stays dark, keeping tests deterministic.
pub(crate) fn prefers_light() -> bool {
    bg_rgb().is_some_and(is_light)
}

// ---------------------------------------------------------------------------
// §2.2 symbol tokens
// ---------------------------------------------------------------------------

/// User strip / composer left bar (accent, one column).
pub(crate) const BAR: &str = "▌";

/// Tool-card detail-line prefix (first line prefixed, wrapped lines align).
pub(crate) const DETAIL: &str = "↳";

/// Braille spinner frames — the only animated element (top-left state word).
pub(crate) const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Truncation marker / state-word suffix (`Thinking…`).
pub(crate) const ELLIPSIS: &str = "…";

// ---------------------------------------------------------------------------
// §2.3 geometry tokens
// ---------------------------------------------------------------------------

/// Shared text column for transcript rows, corner dashboard, and floats.
pub(crate) const CONTENT_INSET: u16 = 4;

/// Horizontal inset for the "input" band (composer input box and user
/// message strips): one column wider than the content column on each side
/// (v2.3 — the band deliberately overhangs the corner rows by 1 col).
pub(crate) const BAND_INSET: u16 = CONTENT_INSET - 1;

/// Text inset inside the input band: the bar rides band column 0 and the
/// draft/message text starts two cells in, keeping one cell of air after
/// the bar.
pub(crate) const STRIP_TEXT_INSET: u16 = 2;

/// Composer band outer height: one dashboard text row per side hugging the
/// input box directly (v2.3 compaction — separators and slot breathing rows
/// are gone). The surface wash and accent bar wrap only the input box,
/// inset to the shared content column so its edges align with the corner
/// rows; the corners sit on the native terminal background.
pub(crate) const COMPOSER_ROWS: u16 = CORNER_ROWS * 2 + INPUT_ROWS;

/// Corner-slot height per side (§2.4): exactly the dashboard text row
/// (v2.3 compaction — the inner breathing row was removed with the
/// separators).
pub(crate) const CORNER_ROWS: u16 = 1;

/// Composer input height inside the band.
pub(crate) const INPUT_ROWS: u16 = 3;

/// Upper bound on visible command-menu rows; the list scrolls inside it.
pub(crate) const MENU_MAX_ROWS: usize = 10;

/// Proportional sizing of the session/model picker dialogs (v0.37 §4.2):
/// the dialog takes three quarters of the live band, capped by
/// [`PICKER_MAX_WIDTH`] / [`PICKER_MAX_HEIGHT`] and floored so small bands
/// keep a usable dialog. The outer extent never exceeds the live band.
pub(crate) const PICKER_SHARE: u32 = 3;
pub(crate) const PICKER_TOTAL_SHARE: u32 = 4;

pub(crate) const PICKER_MAX_WIDTH: u16 = 88;
pub(crate) const PICKER_MIN_WIDTH: u16 = 40;

pub(crate) const PICKER_MAX_HEIGHT: u16 = 24;
pub(crate) const PICKER_MIN_HEIGHT: u16 = 10;

fn picker_share(available: u16) -> u16 {
    u16::try_from(u32::from(available) * PICKER_SHARE / PICKER_TOTAL_SHARE)
        .unwrap_or(available)
        .min(available)
}

/// Outer picker width (borders included) for a terminal column count.
pub(crate) fn picker_width(available: u16) -> u16 {
    let share = picker_share(available);
    available.min(share.clamp(PICKER_MIN_WIDTH, PICKER_MAX_WIDTH))
}

/// Outer picker height (borders included) for a terminal row count.
pub(crate) fn picker_height(available: u16) -> u16 {
    let share = picker_share(available);
    available.min(share.clamp(PICKER_MIN_HEIGHT, PICKER_MAX_HEIGHT))
}

// ---------------------------------------------------------------------------
// Composite styles
//
// Derived from the tokens above; each names its contract role.
// ---------------------------------------------------------------------------

/// Warning rows (health degradation, delivery failures, `/quit` while
/// active). Meta covers ordinary system rows; warn stays reserved for
/// warnings/failures (§2.1).
pub(crate) fn notice() -> Style {
    warn()
}

/// Failure main line: red + bold (contract red line, kept).
pub(crate) fn error() -> Style {
    err().add_modifier(Modifier::BOLD)
}

/// Text selection highlight. Unchanged by the redesign.
pub(crate) fn selection() -> Style {
    Style::default().add_modifier(Modifier::REVERSED)
}

/// Composer placeholder.
pub(crate) fn placeholder() -> Style {
    Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::ITALIC)
}

/// Dim separator / markdown gutter tone.
pub(crate) fn rule() -> Style {
    meta()
}

/// Float-panel border glyphs (`╭╮╰╯─│`): dim chrome on the native terminal
/// background (v0.44 — floats lost their surface wash).
pub(crate) fn panel_border() -> Style {
    meta()
}

/// Selected-row accent tint. Kept by the redesign (selected rows fill their
/// full row width with this tint).
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

    #[test]
    fn color_tokens_carry_the_design_values() {
        assert_eq!(accent(), Style::default().fg(ACCENT));
        assert_eq!(primary(), Style::default());
        assert_eq!(meta(), Style::default().fg(Color::DarkGray));
        assert_eq!(
            corner_meta(),
            Style::default().fg(Color::Gray),
            "corner chrome is one step brighter than meta"
        );
        assert_eq!(warn(), Style::default().fg(Color::Yellow));
        assert_eq!(err(), Style::default().fg(Color::Red));

        let think = reasoning();
        assert_eq!(think.fg, Some(Color::Rgb(130, 130, 155)));
        assert!(think.add_modifier.contains(Modifier::ITALIC));

        let face = surface();
        assert_eq!(
            face.bg,
            Some(band_rgb()),
            "surface rides the band derivation"
        );
    }

    #[test]
    fn composite_styles_derive_from_tokens() {
        assert_eq!(notice(), warn());
        assert_eq!(error(), err().add_modifier(Modifier::BOLD));
        assert_eq!(
            panel_border().bg,
            None,
            "float borders sit on the native background (v0.44)"
        );
        assert_eq!(panel_border().fg, meta().fg);
        assert_eq!(rule(), meta());
    }

    #[test]
    fn prose_tokens_carry_the_typography_ladder() {
        // Heading rungs are realized as semantics in `app::prose`
        // (bold/underline flags + the corner-meta color); the render side
        // spends these tokens.
        let h3_rung = corner_meta();
        assert_eq!(h3_rung.fg, Some(Color::Gray));

        assert_eq!(link().fg, Some(Color::Blue));
        assert!(link().add_modifier.contains(Modifier::UNDERLINED));

        // Structural prose tones (markers, bars, dashes) ride meta.
        assert_eq!(rule(), meta());
    }

    #[test]
    fn inline_code_rides_a_soft_green_with_no_background() {
        assert_eq!(
            derive_code_fg(None),
            FALLBACK_CODE_FG,
            "code green falls back to the stable indexed green"
        );
        assert_eq!(derive_code_fg(Some((18, 20, 26))), Color::Rgb(152, 195, 121));
        assert_eq!(derive_code_fg(Some((240, 238, 230))), Color::Rgb(88, 118, 60));

        let style = inline_code();
        assert_eq!(style.fg, Some(derive_code_fg(bg_rgb())));
        assert_eq!(style.bg, None, "code is font color only — no highlighter smear");
    }

    #[test]
    fn light_preference_without_a_palette_stays_dark() {
        // The process-global PALETTE is never initialized in tests.
        assert!(!prefers_light());
    }

    #[test]
    fn symbol_and_geometry_tokens_match_the_spec() {
        assert_eq!(BAR, "▌");
        assert_eq!(DETAIL, "↳");
        assert_eq!(ELLIPSIS, "…");
        assert_eq!(SPINNER_FRAMES.len(), 10);
        assert_eq!(SPINNER_FRAMES.first(), Some(&"⠋"));
        assert_eq!(SPINNER_FRAMES.last(), Some(&"⠏"));

        assert_eq!(CONTENT_INSET, 4);
        assert_eq!(BAND_INSET, 3);
        assert_eq!(STRIP_TEXT_INSET, 2);
        assert_eq!(COMPOSER_ROWS, 5);
        assert_eq!(CORNER_ROWS, 1);
        assert_eq!(INPUT_ROWS, 3);
        assert_eq!(MENU_MAX_ROWS, 10);
        assert_eq!(picker_width(200), PICKER_MAX_WIDTH);
        assert_eq!(picker_height(100), PICKER_MAX_HEIGHT);
    }
}
