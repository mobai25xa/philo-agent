//! Presentation palette for the philo-tui render layer.
//!
//! Two families live here:
//!
//! - The **anti-glare tunable set** (docs/philo-agent/suggest/new-color.md):
//!   seven foreground tokens (orange / green / blue / yellow / red /
//!   text-default / text-bold) resolve through a runtime [`ThemeState`] —
//!   either one of three hue-locked presets or a continuous
//!   saturation/lightness retune (`/theme`). Hue angles are absolutely
//!   locked; only S and L move.
//! - The **structural tokens**: canvas fills, borders, grays, scrollbar
//!   chrome, symbols, spinners and geometry — plain constants, unaffected
//!   by `/theme`.
//!
//! Composite style functions ([`meta`] / [`accent`] / [`diff_add`] …)
//! keep their contract names so callers swap values, never call sites.
//! Every resolution consults the global state per call: projection caches
//! are semantic spans, so a palette switch restyles the screen on the very
//! next frame with no re-wrap.

use std::sync::RwLock;

use ratatui::style::{Color, Modifier, Style};

// ---------------------------------------------------------------------------
// Anti-glare tunable set (new-color.md)
// ---------------------------------------------------------------------------

/// One glare preset. The md's tables are authoritative: selecting a preset
/// pins the exact hex row. Font weights are **not** part of a preset —
/// bold everywhere stays bold (§3's de-bold suggestions were declined);
/// only saturation/lightness ever move.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) enum ThemePreset {
    /// Old high-saturation row (`#FF6A00` family).
    Original,
    /// De-glare row (`#F27521` family): −20% glare. Startup default.
    #[default]
    Recommended,
    /// Comfort row (`#E47732` family) for all-day reading.
    Comfort,
}

impl ThemePreset {
    /// Preset words accepted by `/theme`.
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "original" => Some(Self::Original),
            "recommended" => Some(Self::Recommended),
            "comfort" => Some(Self::Comfort),
            _ => None,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Original => "original",
            Self::Recommended => "recommended",
            Self::Comfort => "comfort",
        }
    }

    /// Anchor of the preset on the continuous tuner (sat %, ΔL %, bold-gain
    /// %); the values `/theme sat|light|bold` resume from.
    fn anchor(self) -> (i32, i32, i32) {
        match self {
            Self::Original => (100, 0, 0),
            Self::Recommended => (88, -2, 35),
            Self::Comfort => (75, -4, 55),
        }
    }
}

/// Which tunable token resolves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tunable {
    Orange,
    Green,
    Blue,
    Yellow,
    Red,
    TextDefault,
    TextBold,
}

/// Locked hue-anchored bases (new-color.md §四 / HTML `BASE_TOKENS`):
/// `(hue°, saturation %, lightness %)`. Custom retunes transform these and
/// never touch the hue.
fn base_hsl(token: Tunable) -> (f64, f64, f64) {
    match token {
        Tunable::Orange => (25.0, 100.0, 50.0),
        Tunable::Green => (142.0, 69.0, 58.0),
        Tunable::Blue => (217.0, 91.0, 68.0),
        Tunable::Yellow => (42.0, 96.0, 56.0),
        Tunable::Red => (0.0, 91.0, 71.0),
        Tunable::TextDefault => (217.0, 22.0, 84.0),
        Tunable::TextBold => (217.0, 20.0, 96.0),
    }
}

/// Exact preset rows from the md comparison table (§二).
fn preset_hex(preset: ThemePreset, token: Tunable) -> (u8, u8, u8) {
    match (preset, token) {
        (ThemePreset::Original, Tunable::Orange) => (0xFF, 0x6A, 0x00),
        (ThemePreset::Original, Tunable::Green) => (0x4A, 0xDE, 0x80),
        (ThemePreset::Original, Tunable::Blue) => (0x60, 0xA5, 0xFA),
        (ThemePreset::Original, Tunable::Yellow) => (0xFB, 0xBF, 0x24),
        (ThemePreset::Original, Tunable::Red) => (0xF8, 0x71, 0x71),
        (ThemePreset::Original, Tunable::TextDefault) => (0xCD, 0xD5, 0xE0),
        (ThemePreset::Original, Tunable::TextBold) => (0xFF, 0xFF, 0xFF),

        (ThemePreset::Recommended, Tunable::Orange) => (0xF2, 0x75, 0x21),
        (ThemePreset::Recommended, Tunable::Green) => (0x51, 0xCD, 0x80),
        (ThemePreset::Recommended, Tunable::Blue) => (0x62, 0x99, 0xEA),
        (ThemePreset::Recommended, Tunable::Yellow) => (0xEB, 0xBA, 0x28),
        (ThemePreset::Recommended, Tunable::Red) => (0xEB, 0x65, 0x65),
        (ThemePreset::Recommended, Tunable::TextDefault) => (0xC5, 0xCD, 0xD9),
        (ThemePreset::Recommended, Tunable::TextBold) => (0xE8, 0xEE, 0xF5),

        (ThemePreset::Comfort, Tunable::Orange) => (0xE4, 0x77, 0x32),
        (ThemePreset::Comfort, Tunable::Green) => (0x58, 0xB9, 0x7D),
        (ThemePreset::Comfort, Tunable::Blue) => (0x64, 0x8F, 0xD7),
        (ThemePreset::Comfort, Tunable::Yellow) => (0xDD, 0xB2, 0x32),
        (ThemePreset::Comfort, Tunable::Red) => (0xDC, 0x6B, 0x6B),
        (ThemePreset::Comfort, Tunable::TextDefault) => (0xBC, 0xC4, 0xD0),
        (ThemePreset::Comfort, Tunable::TextBold) => (0xEF, 0xF3, 0xF8),
    }
}

/// Bounds of the continuous tuner (new-color.md §四):
/// `S_final = base_S × ks`, `L_final = base_L + ΔL`. The bold-gain axis
/// damps bold+colored text: `S_bold = S × (1−g)`, `L_bold = L − damp`.
pub(crate) const SAT_MIN: i32 = 40;
pub(crate) const SAT_MAX: i32 = 115;
pub(crate) const LIGHT_MIN: i32 = -15;
pub(crate) const LIGHT_MAX: i32 = 15;
pub(crate) const BOLD_GAIN_MIN: i32 = 0;
pub(crate) const BOLD_GAIN_MAX: i32 = 60;

/// One axis of [`ThemeState::tune`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TuneAxis {
    Saturation,
    Lightness,
    BoldGain,
}

impl TuneAxis {
    fn label(self) -> &'static str {
        match self {
            Self::Saturation => "saturation",
            Self::Lightness => "lightness",
            Self::BoldGain => "bold gain",
        }
    }

    fn bounds(self) -> (i32, i32) {
        match self {
            Self::Saturation => (SAT_MIN, SAT_MAX),
            Self::Lightness => (LIGHT_MIN, LIGHT_MAX),
            Self::BoldGain => (BOLD_GAIN_MIN, BOLD_GAIN_MAX),
        }
    }
}

/// Fully-resolved presentation theme: where the seven tunables come from,
/// plus the bold-gain damping rule. Cheap to copy; read once per style
/// call. Font weights themselves are structural and never touched — the
/// gain only dims the *color* that bold+colored text wears.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ThemeState {
    /// `None` after a custom retune: the HSL formula owns the colors.
    preset: Option<ThemePreset>,
    sat_pct: i32,
    light: i32,
    /// Bold-gain percentage (0..=60): how much a bold run's saturation is
    /// scaled down and lightness pulled back. 0 = bold text wears the same
    /// color as regular text.
    bold_gain: i32,
}

const RECOMMENDED_STATE: ThemeState = ThemeState {
    preset: Some(ThemePreset::Recommended),
    sat_pct: 88,
    light: -2,
    bold_gain: 35,
};

impl Default for ThemeState {
    fn default() -> Self {
        RECOMMENDED_STATE
    }
}

impl ThemeState {
    /// Pins a preset row (and its tuner anchor).
    pub(crate) fn select_preset(mut self, preset: ThemePreset) -> Self {
        let (sat, light, bold_gain) = preset.anchor();
        self.preset = Some(preset);
        self.sat_pct = sat;
        self.light = light;
        self.bold_gain = bold_gain;
        self
    }

    /// Moves one slider axis; crossing out of the preset's anchor values
    /// leaves preset land (the formula takes over from the hue-locked
    /// bases).
    pub(crate) fn tune(mut self, axis: TuneAxis, value: i32) -> Result<Self, String> {
        let (low, high) = axis.bounds();
        if !(low..=high).contains(&value) {
            return Err(format!(
                "{label} must be {low}..={high}, got {value}",
                label = axis.label()
            ));
        }
        match axis {
            TuneAxis::Saturation => self.sat_pct = value,
            TuneAxis::Lightness => self.light = value,
            TuneAxis::BoldGain => self.bold_gain = value,
        }
        self.preset = None;
        Ok(self)
    }

    /// One-line status echoed by `/theme`.
    pub(crate) fn describe(&self) -> String {
        let source = self.preset.map_or_else(
            || "custom".to_owned(),
            |preset| preset.name().to_owned(),
        );
        format!(
            "color scheme: {source} · saturation {}% · lightness {:+}% · bold gain {}%",
            self.sat_pct, self.light, self.bold_gain
        )
    }

    /// Resolves one tunable: presets win verbatim; customs go through the
    /// hue-locked formula.
    fn resolved(self, token: Tunable) -> (u8, u8, u8) {
        match self.preset {
            Some(preset) => preset_hex(preset, token),
            None => {
                let (hue, sat, light) = base_hsl(token);
                hsl_to_rgb(
                    hue,
                    sat * f64::from(self.sat_pct) / 100.0,
                    light + f64::from(self.light),
                )
            }
        }
    }

    /// The bold+colored variant of one tunable: same hue, saturation
    /// scaled by `(1−gain)` and lightness pulled back by `gain/2` — bold
    /// strokes already add visual weight, so the color steps down a rung
    /// to keep the combined stimulus flat. Presets resolve their regular
    /// row first and damp *that* (so preset hex rows stay authoritative
    /// while still obeying the damping rule).
    fn resolved_bold(self, token: Tunable) -> (u8, u8, u8) {
        if self.bold_gain == 0 {
            return self.resolved(token);
        }
        let (hue, sat, light) = match self.preset {
            Some(preset) => {
                // Recover the preset row's HSL from its exact hex — the
                // table stays the single source of truth; the hue anchor
                // is only consulted on the custom path.
                let (r, g, b) = preset_hex(preset, token);
                rgb_to_hsl(r, g, b)
            }
            None => base_hsl(token),
        };
        let gain = f64::from(self.bold_gain) / 100.0;
        hsl_to_rgb(
            hue,
            sat * (1.0 - gain) * f64::from(self.sat_pct) / 100.0,
            light + f64::from(self.light) - gain * 50.0 / 2.0,
        )
    }
}

/// RGB → HSL (inverse of [`hsl_to_rgb`], for damping preset rows).
fn rgb_to_hsl(r: u8, g: u8, b: u8) -> (f64, f64, f64) {
    let r = f64::from(r) / 255.0;
    let g = f64::from(g) / 255.0;
    let b = f64::from(b) / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    if max == min {
        return (0.0, 0.0, l * 100.0);
    }
    let delta = max - min;
    let s = delta / (1.0 - (2.0 * l - 1.0).abs());
    let hue = if max == r {
        ((g - b) / delta).rem_euclid(6.0)
    } else if max == g {
        (b - r) / delta + 2.0
    } else {
        (r - g) / delta + 4.0
    } * 60.0;
    (hue, s * 100.0, l * 100.0)
}

/// CSS-style HSL → RGB (same algorithm as the reference page).
fn hsl_to_rgb(h: f64, s: f64, l: f64) -> (u8, u8, u8) {
    let s = s.clamp(0.0, 100.0) / 100.0;
    let l = l.clamp(0.0, 100.0) / 100.0;
    let a = s * l.min(1.0 - l);
    let channel = |n: f64| -> u8 {
        let k = (n + h / 30.0) % 12.0;
        let x = (k - 3.0).min(9.0 - k).clamp(-1.0, 1.0);
        (255.0 * (l - a * x)).round() as u8
    };
    (channel(0.0), channel(8.0), channel(4.0))
}

// -- runtime state ----------------------------------------------------------

static STATE: RwLock<ThemeState> = RwLock::new(RECOMMENDED_STATE);

fn current_state() -> ThemeState {
    *STATE.read().expect("theme state poisoned")
}

fn install_state(state: ThemeState) {
    *STATE.write().expect("theme state poisoned") = state;
}

/// Applies a preset and returns its status line.
pub(crate) fn apply_preset(preset: ThemePreset) -> String {
    let next = current_state().select_preset(preset);
    install_state(next);
    next.describe()
}

/// Applies one slider axis and returns its status line.
pub(crate) fn apply_tune(axis: TuneAxis, value: i32) -> Result<String, String> {
    let next = current_state().tune(axis, value)?;
    install_state(next);
    Ok(next.describe())
}

pub(crate) fn current_description() -> String {
    current_state().describe()
}

/// Bold+colored text style: the accent hue damped by the theme's
/// bold-gain rule (headings, list markers, table headers, selected rows,
/// syntax keywords — every "bold + saturated" run lands here).
pub(crate) fn bold_accent() -> Style {
    Style::default()
        .fg(rgb(current_state().resolved_bold(Tunable::Orange)))
        .add_modifier(Modifier::BOLD)
}

/// Bold+blue variant of the damping rule (function-declaration names in
/// syntax highlighting).
pub(crate) fn bold_info() -> Style {
    Style::default()
        .fg(rgb(current_state().resolved_bold(Tunable::Blue)))
        .add_modifier(Modifier::BOLD)
}

// ---------------------------------------------------------------------------
// Structural palette (constants — `/theme` never touches these)
// ---------------------------------------------------------------------------

/// Main canvas fill. `draw()` paints the whole frame with this first.
pub(crate) const BASE_BG_RGB: (u8, u8, u8) = (0x0D, 0x10, 0x16);
/// Code / diff content backing.
const CODE_BG_RGB: (u8, u8, u8) = (0x16, 0x1B, 0x22);
/// Diff added-line background (`--bg-diff-add`).
const DIFF_ADD_BG_RGB: (u8, u8, u8) = (0x15, 0x26, 0x1A);
/// Diff deleted-line background (`--bg-diff-del`).
const DIFF_DEL_BG_RGB: (u8, u8, u8) = (0x26, 0x1A, 0x1A);
/// Annotation gray: comments, action names, meta info, telemetry identifiers.
const GRAY_RGB: (u8, u8, u8) = (0x7E, 0x8C, 0x9E);
/// Dark gray hints: line numbers, separators, timestamps, quote bars.
const DARK_GRAY_RGB: (u8, u8, u8) = (0x5A, 0x6A, 0x7C);
/// Borders and table dividers.
const BORDER_RGB: (u8, u8, u8) = (0x2A, 0x33, 0x3D);
/// Scrollbar track / footer divider.
const TRACK_RGB: (u8, u8, u8) = (0x1A, 0x21, 0x2A);
/// Scrollbar thumb at rest.
const THUMB_IDLE_RGB: (u8, u8, u8) = (0x3A, 0x43, 0x50);
/// Slash-menu solid panel background.
const PANEL_BG_RGB: (u8, u8, u8) = (0x11, 0x16, 0x1F);
/// Menu selected-row background.
const MENU_ACTIVE_BG_RGB: (u8, u8, u8) = (0x1A, 0x24, 0x33);
/// Footer band background.
const FOOTER_BG_RGB: (u8, u8, u8) = (0x0A, 0x0D, 0x12);
/// Intercept-confirmation box background.
const CONFIRM_BG_RGB: (u8, u8, u8) = (0x15, 0x0D, 0x0E);

fn rgb((r, g, b): (u8, u8, u8)) -> Color {
    Color::Rgb(r, g, b)
}

// ---------------------------------------------------------------------------
// Composite styles — contract names kept; tunables read the runtime state.
// ---------------------------------------------------------------------------

/// Base style for plain text on the brand canvas (default foreground).
pub(crate) fn primary() -> Style {
    Style::default().fg(rgb(current_state().resolved(Tunable::TextDefault)))
}

/// Brand accent foreground (syntax keywords, thinking spinner, fold bars).
pub(crate) fn accent() -> Style {
    Style::default().fg(rgb(current_state().resolved(Tunable::Orange)))
}

/// Syntax-keyword style: bold accent damped by the theme's bold-gain rule.
pub(crate) fn keyword_style() -> Style {
    bold_accent()
}

/// Dimmed chrome: tool rows, system rows, secondary float info.
pub(crate) fn meta() -> Style {
    Style::default().fg(rgb(GRAY_RGB))
}

/// Corner-dashboard chrome: one step dimmer than [`meta`].
pub(crate) fn corner_meta() -> Style {
    Style::default().fg(rgb(DARK_GRAY_RGB))
}

/// Think bodies and collapsed headers: gray italic.
pub(crate) fn reasoning() -> Style {
    Style::default()
        .fg(rgb(GRAY_RGB))
        .add_modifier(Modifier::ITALIC)
}

pub(crate) fn warn() -> Style {
    Style::default().fg(rgb(current_state().resolved(Tunable::Yellow)))
}

pub(crate) fn err() -> Style {
    Style::default().fg(rgb(current_state().resolved(Tunable::Red)))
}

/// Failure main line: red + bold (contract red line, kept).
pub(crate) fn error() -> Style {
    err().add_modifier(Modifier::BOLD)
}

/// Success green foreground: `✔ [Success]` system rows, tool statuses,
/// paths, strings.
pub(crate) fn ok() -> Style {
    Style::default().fg(rgb(current_state().resolved(Tunable::Green)))
}

/// Information blue foreground: `ℹ [Info]` system rows, model names.
pub(crate) fn info() -> Style {
    Style::default().fg(rgb(current_state().resolved(Tunable::Blue)))
}

/// Plain background-fill style in the brand base color (canvas铺底用).
pub(crate) fn base_fill() -> Style {
    Style::default().bg(rgb(BASE_BG_RGB))
}

/// Band/panel base background hook.
#[allow(dead_code)]
pub(crate) fn base_wash() -> Style {
    base_fill()
}

/// Diff tints: green/red foregrounds over the fixed v4.0 diff backgrounds.
pub(crate) fn diff_add() -> Style {
    Style::default()
        .fg(rgb(current_state().resolved(Tunable::Green)))
        .bg(rgb(DIFF_ADD_BG_RGB))
}

pub(crate) fn diff_del() -> Style {
    Style::default()
        .fg(rgb(current_state().resolved(Tunable::Red)))
        .bg(rgb(DIFF_DEL_BG_RGB))
}

/// Code/diff content backing surface (CODE_BG).
#[allow(dead_code)]
pub(crate) fn code_bg() -> Style {
    Style::default().bg(rgb(CODE_BG_RGB))
}

// ---------------------------------------------------------------------------
// Prose typography
// ---------------------------------------------------------------------------

/// Inline code: accent foreground, no background block (v4.0 §6).
pub(crate) fn inline_code() -> Style {
    accent()
}

/// The code accent, exposed for the prose span resolver and tests. All
/// consumers are bold runs (list bullets, task boxes), so this reports
/// the damped bold value.
#[cfg(test)]
pub(crate) fn code_fg() -> Color {
    rgb(current_state().resolved_bold(Tunable::Orange))
}

/// Links keep the universal blue convention.
pub(crate) fn link() -> Style {
    Style::default()
        .fg(rgb(current_state().resolved(Tunable::Blue)))
        .add_modifier(Modifier::UNDERLINED)
}

/// Text selection highlight. Unchanged by the reskin.
pub(crate) fn selection() -> Style {
    Style::default().add_modifier(Modifier::REVERSED)
}

/// Composer placeholder.
pub(crate) fn placeholder() -> Style {
    Style::default()
        .fg(rgb(DARK_GRAY_RGB))
        .add_modifier(Modifier::ITALIC)
}

/// Float-panel border glyphs (`╭╮╰╯─│`) ride the fixed BORDER token.
pub(crate) fn panel_border() -> Style {
    Style::default().fg(rgb(BORDER_RGB))
}

/// The BORDER token as a bare color (tool-card gutters and dot leaders).
pub(crate) fn border_color() -> Color {
    rgb(BORDER_RGB)
}

/// Selected-row tint: MENU_ACTIVE_BG fill with the bold-gain-damped accent.
pub(crate) fn menu_selected_row() -> Style {
    Style::default()
        .fg(rgb(current_state().resolved_bold(Tunable::Orange)))
        .bg(rgb(MENU_ACTIVE_BG_RGB))
        .add_modifier(Modifier::BOLD)
}

/// Bold emphasis white for H2 rungs and strong words; consumed by the P4
/// heading ladder. Bold runs ride the bold-gain rule — for the neutral
/// white that means a lightness step-down (no saturation to damp), so
/// `**strong**` reads as a soft white instead of a glare-white.
pub(crate) fn bold_white() -> Style {
    Style::default()
        .fg(rgb(current_state().resolved_bold(Tunable::TextBold)))
        .add_modifier(Modifier::BOLD)
}

/// Scrollbar track color (TRACK).
#[allow(dead_code)]
pub(crate) fn track_color() -> Color {
    rgb(TRACK_RGB)
}

/// Scrollbar thumb at rest (THUMB_IDLE).
#[allow(dead_code)]
pub(crate) fn thumb_idle_color() -> Color {
    rgb(THUMB_IDLE_RGB)
}

/// Footer band background (FOOTER_BG).
#[allow(dead_code)]
pub(crate) fn footer_bg_color() -> Color {
    rgb(FOOTER_BG_RGB)
}

/// Confirmation-box background (CONFIRM_BG).
#[allow(dead_code)]
pub(crate) fn confirm_bg_color() -> Color {
    rgb(CONFIRM_BG_RGB)
}

/// Solid float-panel background (PANEL_BG).
#[allow(dead_code)]
pub(crate) fn panel_bg_color() -> Color {
    rgb(PANEL_BG_RGB)
}

/// Menu selected-row background fill (MENU_ACTIVE_BG).
#[allow(dead_code)]
pub(crate) fn menu_active_bg_color() -> Color {
    rgb(MENU_ACTIVE_BG_RGB)
}

// ---------------------------------------------------------------------------
// Symbol tokens (v4.0 §10.2 / P1 task §2)
// ---------------------------------------------------------------------------

#[allow(dead_code)]
pub(crate) mod symbols {
    /// Left status bar of tool cards & fold bars (`▎`, one column).
    pub(crate) const STATUS_BAR: &str = "▎";

    /// Collapsed / expanded chevrons for fold bars.
    pub(crate) const CHEVRON_DOWN: &str = "▾";
    pub(crate) const CHEVRON_UP: &str = "▴";

    /// Card-inline success / standalone success glyph.
    pub(crate) const CHECK: &str = "✓";
    pub(crate) const HEAVY_CHECK: &str = "✔";

    /// Card-inline failure / error-line glyph.
    pub(crate) const CROSS: &str = "✗";
    pub(crate) const HEAVY_CROSS: &str = "✖";

    /// Warning glyph.
    pub(crate) const WARNING_SIGN: &str = "⚠";

    /// Info glyph.
    pub(crate) const INFO_SIGN: &str = "ℹ";

    /// Status indicator dot (Idle badge).
    pub(crate) const STATUS_DOT: &str = "●";

    /// Dot leaders filling card headers.
    pub(crate) const DOT_LEADER: &str = "·";

    /// Fold-bar side texture.
    pub(crate) const FOLD_RAIL: &str = "┈";

    /// Concurrency-tree branches.
    pub(crate) const BRANCH_FORK: &str = "├─";
    pub(crate) const BRANCH_LAST: &str = "└─";

    /// Input prompt symbol (green when ready, dark while busy).
    pub(crate) const PROMPT: &str = "❯";
}

/// Re-exports keep the call sites flat (`theme::PROMPT`).
#[allow(unused_imports)]
pub(crate) use symbols::{
    BRANCH_FORK, BRANCH_LAST, CHECK, CHEVRON_DOWN, CHEVRON_UP, CROSS, DOT_LEADER,
    FOLD_RAIL, HEAVY_CHECK, HEAVY_CROSS, INFO_SIGN, PROMPT, STATUS_BAR, STATUS_DOT,
    WARNING_SIGN,
};

/// v4.0 footer band tokens (P2 task §2).
pub(crate) fn footer_fill() -> Style {
    Style::default().bg(rgb(FOOTER_BG_RGB))
}

/// Footer separator line above the band (TRACK).
#[allow(dead_code)]
pub(crate) fn footer_rule() -> Style {
    Style::default().fg(rgb(TRACK_RGB))
}

/// Prompt symbol ready state: green `❯`.
pub(crate) fn prompt_ready() -> Style {
    Style::default()
        .fg(rgb(current_state().resolved(Tunable::Green)))
        .add_modifier(Modifier::BOLD)
}

/// Prompt symbol while a turn owns the wire: dimmed.
pub(crate) fn prompt_busy() -> Style {
    Style::default().fg(rgb(DARK_GRAY_RGB))
}

/// User message glyph `❯` — the same helper green as the idle prompt.
pub(crate) fn user_prompt() -> Style {
    prompt_ready()
}

/// User message body: bold emphasis white, damped by the bold-gain rule.
pub(crate) fn user_message() -> Style {
    Style::default()
        .fg(rgb(current_state().resolved_bold(Tunable::TextBold)))
        .add_modifier(Modifier::BOLD)
}

/// Model name in the footer's right badge column: information blue, bold.
pub(crate) fn model_name() -> Style {
    Style::default()
        .fg(rgb(current_state().resolved(Tunable::Blue)))
        .add_modifier(Modifier::BOLD)
}

/// Reasoning effort badge in the footer: warning yellow, always bold.
pub(crate) fn model_effort() -> Style {
    Style::default()
        .fg(rgb(current_state().resolved(Tunable::Yellow)))
        .add_modifier(Modifier::BOLD)
}

/// Workspace path on the footer telemetry row: helper green.
pub(crate) fn workspace_path() -> Style {
    Style::default().fg(rgb(current_state().resolved(Tunable::Green)))
}

/// Telemetry identifier glyphs (`↑ ↓ R C`, `/`): annotation gray.
pub(crate) fn telemetry_label() -> Style {
    Style::default().fg(rgb(GRAY_RGB))
}

/// Telemetry dynamic values: warning yellow, always bold (weights are
/// not themeable — only saturation/lightness move).
pub(crate) fn telemetry_value() -> Style {
    model_effort()
}

/// Idle badge dot `●`: helper green.
pub(crate) fn status_dot_idle() -> Style {
    Style::default().fg(rgb(current_state().resolved(Tunable::Green)))
}

/// Truncation marker / state-word suffix (`Thinking…`). Kept from v3.
pub(crate) const ELLIPSIS: &str = "…";

// ---------------------------------------------------------------------------
// Geometry tokens
// ---------------------------------------------------------------------------

/// Shared text column for transcript rows, floats, and the footer band.
pub(crate) const CONTENT_INSET: u16 = 4;

/// Footer band horizontal padding.
pub(crate) const FOOTER_PAD: u16 = 2;

/// Input box outer height when the draft is a single row (borders + text).
pub(crate) const COMPOSER_MIN_OUTER: u16 = 3;

/// Input box outer height cap with the draft scrolled inside (6 text rows).
pub(crate) const COMPOSER_MAX_OUTER: u16 = 8;

/// The v4.0 footer band's idle height (separator + badge + box + telemetry).
#[cfg(test)]
pub(crate) const FOOTER_ROWS: u16 = 1 + 1 + COMPOSER_MIN_OUTER + 1;

/// Upper bound on visible command-menu rows; the list scrolls inside it.
pub(crate) const MENU_MAX_ROWS: usize = 10;

/// Milliseconds a scrollbar thumb keeps its "recently scrolled" highlight.
pub(crate) const SCROLL_ACTIVE_MS: u64 = 800;

/// Vertical breathing room between transcript cells (Answer/Tool/Reasoning
/// boundaries). The single knob for cell-level density — v4.0 收紧后整体
/// 太密，这里集中调度；改一处全 TUI 节奏联动（与固定色板哲学同构）。
pub(crate) const GAP_CELL: usize = 1;

/// Vertical breathing room inside one answer cell, around structural prose
/// blocks (headings, fenced code, tables). Idempotent over source blank
/// lines: the projection never amplifies a `\n\n` the author already wrote.
pub(crate) const GAP_BLOCK: usize = 1;

/// Proportional sizing of the session/model picker dialogs (v0.37 §4.2).
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
// Spinners (v4.0 §10.1 / P1 task §3)
// ---------------------------------------------------------------------------

/// One animation register: frame set plus its cadence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Spinner {
    pub frames: &'static [&'static str],
    pub frame_ms: u64,
}

impl Spinner {
    pub(crate) fn frame(&self, tick: usize) -> &'static str {
        self.frames[tick % self.frames.len()]
    }

    pub(crate) fn interval(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.frame_ms)
    }
}

/// Thinking/Writing/Retrying: braille at 80ms in brand orange.
pub(crate) const THINKING_SPINNER: Spinner = Spinner {
    frames: &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"],
    frame_ms: 80,
};

/// Running/Compacting: horizontal-dash frames at 100ms in warning yellow.
pub(crate) const RUNNING_SPINNER: Spinner = Spinner {
    frames: &["⠂", "-", "–", "—", "–", "-", "⠐"],
    frame_ms: 100,
};

#[cfg(test)]
mod tests {
    use super::*;

    fn install(state: ThemeState) {
        install_state(state);
    }

    fn restore_default() {
        install(RECOMMENDED_STATE);
    }

    #[test]
    fn structural_tokens_stay_constant() {
        assert_eq!(rgb(BASE_BG_RGB), Color::Rgb(0x0D, 0x10, 0x16));
        assert_eq!(rgb(CODE_BG_RGB), Color::Rgb(0x16, 0x1B, 0x22));
        assert_eq!(rgb(DIFF_ADD_BG_RGB), Color::Rgb(0x15, 0x26, 0x1A));
        assert_eq!(rgb(DIFF_DEL_BG_RGB), Color::Rgb(0x26, 0x1A, 0x1A));
        assert_eq!(rgb(GRAY_RGB), Color::Rgb(0x7E, 0x8C, 0x9E));
        assert_eq!(rgb(DARK_GRAY_RGB), Color::Rgb(0x5A, 0x6A, 0x7C));
        assert_eq!(rgb(BORDER_RGB), Color::Rgb(0x2A, 0x33, 0x3D));
        assert_eq!(rgb(TRACK_RGB), Color::Rgb(0x1A, 0x21, 0x2A));
        assert_eq!(rgb(THUMB_IDLE_RGB), Color::Rgb(0x3A, 0x43, 0x50));
        assert_eq!(rgb(PANEL_BG_RGB), Color::Rgb(0x11, 0x16, 0x1F));
        assert_eq!(rgb(MENU_ACTIVE_BG_RGB), Color::Rgb(0x1A, 0x24, 0x33));
        assert_eq!(rgb(FOOTER_BG_RGB), Color::Rgb(0x0A, 0x0D, 0x12));
        assert_eq!(rgb(CONFIRM_BG_RGB), Color::Rgb(0x15, 0x0D, 0x0E));

        assert_eq!(base_fill().bg, Some(Color::Rgb(13, 16, 22)));
        assert_eq!(footer_fill().bg, Some(Color::Rgb(10, 13, 18)));
        assert_eq!(placeholder().fg, Some(Color::Rgb(90, 106, 124)));
        assert_eq!(panel_border().fg, Some(Color::Rgb(42, 51, 61)));
    }

    #[test]
    fn preset_rows_match_the_new_color_md_tables() {
        let expected = [
            (
                ThemePreset::Original,
                [
                    (0xFF, 0x6A, 0x00),
                    (0x4A, 0xDE, 0x80),
                    (0x60, 0xA5, 0xFA),
                    (0xFB, 0xBF, 0x24),
                    (0xF8, 0x71, 0x71),
                    (0xCD, 0xD5, 0xE0),
                    (0xFF, 0xFF, 0xFF),
                ],
            ),
            (
                ThemePreset::Recommended,
                [
                    (0xF2, 0x75, 0x21),
                    (0x51, 0xCD, 0x80),
                    (0x62, 0x99, 0xEA),
                    (0xEB, 0xBA, 0x28),
                    (0xEB, 0x65, 0x65),
                    (0xC5, 0xCD, 0xD9),
                    (0xE8, 0xEE, 0xF5),
                ],
            ),
            (
                ThemePreset::Comfort,
                [
                    (0xE4, 0x77, 0x32),
                    (0x58, 0xB9, 0x7D),
                    (0x64, 0x8F, 0xD7),
                    (0xDD, 0xB2, 0x32),
                    (0xDC, 0x6B, 0x6B),
                    (0xBC, 0xC4, 0xD0),
                    (0xEF, 0xF3, 0xF8),
                ],
            ),
        ];
        let tokens = [
            Tunable::Orange,
            Tunable::Green,
            Tunable::Blue,
            Tunable::Yellow,
            Tunable::Red,
            Tunable::TextDefault,
            Tunable::TextBold,
        ];
        for (preset, row) in expected {
            for (token, want) in tokens.into_iter().zip(row) {
                assert_eq!(
                    preset_hex(preset, token),
                    want,
                    "{preset:?}/{} drifted",
                    token_label(token)
                );
                assert_eq!(
                    RECOMMENDED_STATE.select_preset(preset).resolved(token),
                    want,
                    "preset resolution leaked off the table"
                );
            }
        }
    }

    fn token_label(token: Tunable) -> &'static str {
        match token {
            Tunable::Orange => "orange",
            Tunable::Green => "green",
            Tunable::Blue => "blue",
            Tunable::Yellow => "yellow",
            Tunable::Red => "red",
            Tunable::TextDefault => "text",
            Tunable::TextBold => "bold",
        }
    }

    #[test]
    fn neutral_custom_formula_lands_on_the_original_hues() {
        // At ks=100%, ΔL=0 the slider algebra reproduces the original
        // high-saturation row exactly — proof the hue angle is truly
        // locked (checked for orange and green; sampled from the JS refs).
        let neutral = ThemeState::default()
            .tune(TuneAxis::Saturation, 100)
            .and_then(|state| state.tune(TuneAxis::Lightness, 0))
            .expect("both axes in range");
        assert_eq!(neutral.preset, None);
        assert_eq!(neutral.resolved(Tunable::Orange), (0xFF, 0x6A, 0x00));
        assert_eq!(neutral.resolved(Tunable::Green), (0x4A, 0xDE, 0x80));
    }

    #[test]
    fn tuning_validates_bounds_and_leaves_preset_land() {
        assert_eq!(
            ThemeState::default().tune(TuneAxis::Saturation, 39).unwrap_err(),
            "saturation must be 40..=115, got 39"
        );
        assert_eq!(
            ThemeState::default().tune(TuneAxis::Saturation, 116).unwrap_err(),
            "saturation must be 40..=115, got 116"
        );
        assert_eq!(
            ThemeState::default().tune(TuneAxis::Lightness, -16).unwrap_err(),
            "lightness must be -15..=15, got -16"
        );
        assert!(ThemeState::default().tune(TuneAxis::Saturation, 40).is_ok());
        assert!(ThemeState::default().tune(TuneAxis::Lightness, 15).is_ok());
    }

    #[test]
    fn describe_reports_source_and_sliders() {
        assert_eq!(
            ThemeState::default().describe(),
            "color scheme: recommended · saturation 88% · lightness -2% · bold gain 35%"
        );
        let original = ThemeState::default().select_preset(ThemePreset::Original);
        assert_eq!(
            original.describe(),
            "color scheme: original · saturation 100% · lightness +0% · bold gain 0%"
        );
        let custom = original.tune(TuneAxis::Saturation, 65).expect("valid");
        assert_eq!(
            custom.describe(),
            "color scheme: custom · saturation 65% · lightness +0% · bold gain 0%"
        );
    }

    #[test]
    fn bold_gain_damps_only_the_bold_color_and_tracks_the_theme() {
        // Original: gain 0 — bold accent equals regular accent.
        let original = ThemeState::default().select_preset(ThemePreset::Original);
        assert_eq!(original.resolved_bold(Tunable::Orange), (0xFF, 0x6A, 0x00));

        // Recommended: gain 35 — same hue family, clearly darker/duller
        // than the regular row (#F27521 = 242,117,33).
        let recommended = ThemeState::default();
        let (r, g, b) = recommended.resolved_bold(Tunable::Orange);
        assert!(
            i32::from(r) < 0xF2 && i32::from(g) < 0x75 && i32::from(b) > 33,
            "35% damp dims saturation and lifts the blue channel: {r},{g},{b}"
        );

        // The damp tracks a saturation retune (the whole point of scheme
        // one: nothing is pinned).
        let custom = recommended
            .tune(TuneAxis::BoldGain, 0)
            .expect("valid");
        assert_eq!(
            custom.resolved_bold(Tunable::Orange),
            custom.resolved(Tunable::Orange),
            "gain 0 collapses back to the regular row"
        );
        let damped_more = recommended
            .tune(TuneAxis::BoldGain, 60)
            .expect("valid");
        let (r2, _, _) = damped_more.resolved_bold(Tunable::Orange);
        assert!(i32::from(r2) < i32::from(r), "more gain, darker still");

        // The neutral white damps too — for a hueless token that means a
        // pure lightness step-down (the glare lever for `**strong**`).
        let (wr, wg, wb) = recommended.resolved_bold(Tunable::TextBold);
        assert_eq!(
            (wr, wg, wb),
            (201, 210, 221),
            "recommended soft-white lands here"
        );
        let (wr2, _, wb2) = damped_more.resolved_bold(Tunable::TextBold);
        assert!(
            i32::from(wr2) < i32::from(wr) && i32::from(wb2) < i32::from(wb),
            "more gain, dimmer white"
        );
        let original_white = original.resolved_bold(Tunable::TextBold);
        assert_eq!(original_white, (0xFF, 0xFF, 0xFF), "gain 0 keeps pure white");
    }

    #[test]
    fn bold_styles_carry_the_damped_colors() {
        install(ThemeState::default());
        let damped_fg = bold_accent().fg.expect("accent fg");
        assert_ne!(Some(damped_fg), accent().fg, "bold accent steps down a rung");
        assert!(bold_accent().add_modifier.contains(Modifier::BOLD));
        assert_ne!(bold_info().fg, info().fg, "bold blue damps too");

        // Gain 0 (Original) collapses the distinction entirely.
        install(ThemeState::default().select_preset(ThemePreset::Original));
        assert_eq!(bold_accent().fg, accent().fg);
        assert_eq!(bold_info().fg, info().fg);
        restore_default();
    }

    #[test]
    fn weights_are_never_themeable() {
        // Bold is structural: keywords and telemetry stay bold regardless
        // of preset or slider position (only colors move).
        for state in [
            ThemeState::default().select_preset(ThemePreset::Original),
            ThemeState::default(),
            ThemeState::default().select_preset(ThemePreset::Comfort),
        ] {
            install(state);
            assert!(keyword_style().add_modifier.contains(Modifier::BOLD));
            assert!(telemetry_value().add_modifier.contains(Modifier::BOLD));
            assert!(model_effort().add_modifier.contains(Modifier::BOLD));
            assert!(
                error().add_modifier.contains(Modifier::BOLD),
                "the failure line keeps its contract weight"
            );
        }
        restore_default();
    }

    #[test]
    fn styles_resolve_through_the_live_state_and_decouple_from_effort() {
        // Telemetry rides the theme weight; the effort badge stays bold
        // regardless (weights are never themeable).
        install(RECOMMENDED_STATE.select_preset(ThemePreset::Original));
        assert!(telemetry_value().add_modifier.contains(Modifier::BOLD));
        assert!(keyword_style().add_modifier.contains(Modifier::BOLD));
        assert!(model_effort().add_modifier.contains(Modifier::BOLD));
        assert_eq!(
            accent().fg,
            Some(Color::Rgb(0xFF, 0x6A, 0x00)),
            "Original preset pins the legacy orange"
        );

        restore_default();
        assert!(telemetry_value().add_modifier.contains(Modifier::BOLD));
        assert!(keyword_style().add_modifier.contains(Modifier::BOLD));
        assert!(model_effort().add_modifier.contains(Modifier::BOLD));
        assert_eq!(accent().fg, Some(Color::Rgb(0xF2, 0x75, 0x21)));
        assert_eq!(ok().fg, Some(Color::Rgb(0x51, 0xCD, 0x80)));
        assert_eq!(info().fg, Some(Color::Rgb(0x62, 0x99, 0xEA)));
        assert_eq!(warn().fg, Some(Color::Rgb(0xEB, 0xBA, 0x28)));
        assert_eq!(err().fg, Some(Color::Rgb(0xEB, 0x65, 0x65)));
        assert_eq!(
            primary().fg,
            Some(Color::Rgb(0xC5, 0xCD, 0xD9)),
            "default startup lands on the Recommended row"
        );
        assert_eq!(
            bold_white().fg,
            Some(Color::Rgb(201, 210, 221)),
            "emphasis white damped by the 35% bold gain, still bold"
        );
        assert_eq!(
            user_message().fg,
            bold_white().fg,
            "the user echo shares the damped emphasis white"
        );

        // Diff washes pair tunable foregrounds with fixed backgrounds.
        let add = diff_add();
        assert_eq!(add.fg, Some(Color::Rgb(0x51, 0xCD, 0x80)));
        assert_eq!(add.bg, Some(Color::Rgb(21, 38, 26)));

        // Menu tint follows the damped bold accent while the panel fill
        // stays fixed.
        let selected = menu_selected_row();
        assert_eq!(selected.fg, bold_accent().fg);
        assert_eq!(selected.bg, Some(Color::Rgb(26, 36, 51)));

        // Custom retunes move the accent through the formula.
        apply_tune(TuneAxis::Saturation, 50).expect("in range");
        let dimmed = accent().fg.expect("accent");
        assert_ne!(dimmed, Color::Rgb(0xF2, 0x75, 0x21));
        restore_default();

        // Link/blue convention survives the reskin.
        assert_eq!(link().fg, Some(Color::Rgb(0x62, 0x99, 0xEA)));
        assert!(link().add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn preset_names_round_trip() {
        for name in ["original", "ORIGINAL", "recommended", "comfort"] {
            assert!(ThemePreset::from_name(name).is_some(), "{name}");
        }
        assert!(ThemePreset::from_name("nope").is_none());
        assert_eq!(ThemePreset::default(), ThemePreset::Recommended);
    }

    #[test]
    fn symbol_tokens_match_the_v40_spec() {
        assert_eq!(STATUS_BAR, "▎");
        assert_eq!(CHEVRON_DOWN, "▾");
        assert_eq!(CHEVRON_UP, "▴");
        assert_eq!(CHECK, "✓");
        assert_eq!(HEAVY_CHECK, "✔");
        assert_eq!(CROSS, "✗");
        assert_eq!(HEAVY_CROSS, "✖");
        assert_eq!(WARNING_SIGN, "⚠");
        assert_eq!(INFO_SIGN, "ℹ");
        assert_eq!(STATUS_DOT, "●");
        assert_eq!(DOT_LEADER, "·");
        assert_eq!(FOLD_RAIL, "┈");
        assert_eq!(BRANCH_FORK, "├─");
        assert_eq!(BRANCH_LAST, "└─");
        assert_eq!(PROMPT, "❯");
        assert_eq!(ELLIPSIS, "…");
    }

    #[test]
    fn both_spinners_carry_their_frame_sets_and_rates() {
        assert_eq!(THINKING_SPINNER.frames.len(), 10);
        assert_eq!(THINKING_SPINNER.frame_ms, 80);
        assert_eq!(THINKING_SPINNER.frame(usize::MAX), "⠴");

        assert_eq!(RUNNING_SPINNER.frames.len(), 7);
        assert_eq!(RUNNING_SPINNER.frame_ms, 100);
        assert_eq!(RUNNING_SPINNER.frame(10), "—");
        assert_eq!(
            THINKING_SPINNER.interval(),
            std::time::Duration::from_millis(80)
        );
        assert_eq!(
            RUNNING_SPINNER.interval(),
            std::time::Duration::from_millis(100)
        );
    }

    #[test]
    fn geometry_tokens_survive_unchanged() {
        assert_eq!(CONTENT_INSET, 4);
        assert_eq!(FOOTER_PAD, 2);
        assert_eq!(COMPOSER_MIN_OUTER, 3);
        assert_eq!(COMPOSER_MAX_OUTER, 8);
        assert_eq!(FOOTER_ROWS, 6);
        assert_eq!(MENU_MAX_ROWS, 10);
        assert_eq!(SCROLL_ACTIVE_MS, 800);
        assert_eq!(picker_width(200), PICKER_MAX_WIDTH);
        assert_eq!(picker_height(100), PICKER_MAX_HEIGHT);
    }
}
