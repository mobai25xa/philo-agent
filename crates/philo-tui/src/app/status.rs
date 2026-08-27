//! Composer-dashboard projection: pure data to corner lines.
//!
//! The redesign moves the persistent instruments into the composer band's
//! four corners (redesign §2.4, tui.md §8). [`StatusData`] keeps the live
//! facts; the `*_corner_for` projections below are width-aware render
//! inputs with the contract's deterministic degradation order baked in.
//! [`StatusData::summary_line`] feeds the `/status` transcript dump
//! (contract §4: model, session, usage, context).

use philo_agent_service::FrontendTokenUsage;
use unicode_segmentation::UnicodeSegmentation;

use super::text;
use super::transcript::InfoLevel;

/// Everything the composer dashboard shows; the event loop keeps this
/// current.
#[derive(Clone, Debug, Default)]
pub struct StatusData {
    pub model: String,
    pub session: String,
    pub busy: bool,
    /// A manual compaction or automatic pre-turn compaction is active.
    pub compacting: bool,
    pub usage: Option<FrontendTokenUsage>,
    /// Context-budget hint: the configured or capability-derived window.
    pub context_window: Option<u64>,
    pub level: InfoLevel,
    /// Owning provider id (display), from the model catalog's current entry.
    pub provider: Option<String>,
    /// Active reasoning-effort label, from the installed generation.
    pub effort: Option<String>,
    /// Workspace root injected by the composition root; never probed here.
    pub workspace_root: String,
}

impl StatusData {
    pub fn new(model: impl Into<String>, session: impl Into<String>, level: InfoLevel) -> Self {
        Self {
            model: model.into(),
            session: session.into(),
            busy: false,
            compacting: false,
            usage: None,
            context_window: None,
            level,
            provider: None,
            effort: None,
            workspace_root: String::new(),
        }
    }

    /// First line of the `/status` dump (contract §4): model, session,
    /// latest-turn usage and context share. Tool names and effect classes
    /// follow as their own lines from the same reply.
    pub(crate) fn summary_line(&self) -> String {
        let mut parts = Vec::new();
        if !self.model.is_empty() {
            parts.push(self.model.clone());
        }
        if !self.session.is_empty() {
            parts.push(format!("session {}", self.session));
        }
        parts.push(self.usage_stages()[0].clone());
        parts.join(" · ")
    }

    /// Right-top corner (redesign §2.4): `({provider}) {model} · {effort}`.
    /// Unknown fields drop whole; degradation order is effort → provider →
    /// model truncation (tui.md §8).
    pub(crate) fn model_corner_for(&self, max_width: usize) -> Option<ModelCorner> {
        if self.model.is_empty() {
            return None;
        }
        let mut corner = ModelCorner {
            provider: self.provider.clone(),
            model: self.model.clone(),
            effort: self.effort.clone(),
        };
        while model_corner_width(&corner) > max_width {
            if corner.effort.take().is_some() {
                continue;
            }
            if corner.provider.take().is_some() {
                continue;
            }
            corner.model = tail_ellipsis(&corner.model, max_width);
            break;
        }
        Some(corner)
    }

    /// Bottom band row: workspace path left, latest-turn usage right.
    /// Degradation ladder (tui.md §8): verbatim path yields to its compact
    /// ellipsis before any usage field drops; then C%, R, ↑↓, and finally
    /// `{ctx%}/{window}`; the corners themselves vanish last.
    pub(crate) fn bottom_corners_for(&self, max_width: usize) -> (String, String) {
        let stages = self.usage_stages();
        if self.workspace_root.is_empty() {
            return (
                String::new(),
                stages
                    .into_iter()
                    .find(|stage| text::width(stage) <= max_width)
                    .unwrap_or_default(),
            );
        }
        let full = &stages[0];
        if text::width(&self.workspace_root) + CORNER_GAP + text::width(full) <= max_width {
            return (self.workspace_root.clone(), full.clone());
        }
        let compact = middle_ellipsis(&self.workspace_root, COMPACT_PATH_CELLS.min(max_width));
        for stage in &stages {
            if text::width(&compact) + CORNER_GAP + text::width(stage) <= max_width {
                return (compact.clone(), stage.clone());
            }
        }
        // The corners are the last to go: the path keeps whatever remains.
        (
            middle_ellipsis(&self.workspace_root, max_width),
            String::new(),
        )
    }

    /// Usage-corner candidates from most to least content: full → drop C →
    /// drop R → drop ↑↓ → empty. `{ctx%}/{window}` survives until the end.
    fn usage_stages(&self) -> [String; 5] {
        let usage = self.usage.unwrap_or_default();
        let arrows = format!(
            "↑{} ↓{}",
            dash_or(usage.input_tokens),
            dash_or(usage.output_tokens)
        );
        let reasoning = format!("R{}", dash_or(usage.reasoning_tokens));
        let cache = match (usage.cache_read_tokens, usage.input_tokens) {
            (Some(read), Some(input)) if input > 0 => format!("C{}", percent(read, input)),
            _ => "C-".to_owned(),
        };
        let context = format!(
            "{}/{}",
            match (usage.input_tokens, self.context_window) {
                (Some(input), Some(window)) if window > 0 => percent(input, window),
                _ => "-".to_owned(),
            },
            self.context_window.map_or_else(|| "-".to_owned(), tokens)
        );
        [
            format!("{arrows} {reasoning} {cache} {context}"),
            format!("{arrows} {reasoning} {context}"),
            format!("{arrows} {context}"),
            context.clone(),
            String::new(),
        ]
    }
}

/// Cells between the bottom row's left and right corners.
const CORNER_GAP: usize = 4;

/// Cell budget of the compact middle-ellipsized path (`D:\…\Pi`), the form
/// the path takes before any usage field degrades (design §3.10).
const COMPACT_PATH_CELLS: usize = 7;

/// Right-top corner after width degradation: provider/effort may have been
/// dropped and the model truncated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ModelCorner {
    pub(crate) provider: Option<String>,
    pub(crate) model: String,
    pub(crate) effort: Option<String>,
}

fn model_corner_width(corner: &ModelCorner) -> usize {
    let mut width = text::width(&corner.model);
    if let Some(provider) = &corner.provider {
        width += text::width(provider) + 3;
    }
    if let Some(effort) = &corner.effort {
        width += text::width(effort) + 3;
    }
    width
}

fn middle_ellipsis(value: &str, max_width: usize) -> String {
    if text::width(value) <= max_width {
        return value.to_owned();
    }
    if max_width == 0 {
        return String::new();
    }
    const MARK: &str = "…";
    let keep = max_width - text::width(MARK);
    let head_budget = keep.div_ceil(2);
    let tail_budget = keep - head_budget;
    let mut head = String::new();
    let mut used = 0;
    for grapheme in value.graphemes(true) {
        let width = text::width(grapheme);
        if used + width > head_budget {
            break;
        }
        head.push_str(grapheme);
        used += width;
    }
    let mut tail_graphemes = Vec::new();
    used = 0;
    for grapheme in value.graphemes(true).rev() {
        let width = text::width(grapheme);
        if used + width > tail_budget {
            break;
        }
        tail_graphemes.push(grapheme);
        used += width;
    }
    tail_graphemes.reverse();
    format!("{head}{MARK}{}", tail_graphemes.concat())
}

/// Head-keeping truncation with the design's `…` marker (§2.2): the last
/// resort for a model name too wide for its corner.
fn tail_ellipsis(value: &str, max_width: usize) -> String {
    if text::width(value) <= max_width {
        return value.to_owned();
    }
    if max_width == 0 {
        return String::new();
    }
    const MARK: &str = "…";
    let budget = max_width - text::width(MARK);
    let mut head = String::new();
    let mut used = 0;
    for grapheme in value.graphemes(true) {
        let width = text::width(grapheme);
        if used + width > budget {
            break;
        }
        head.push_str(grapheme);
        used += width;
    }
    format!("{head}{MARK}")
}

fn dash_or(value: Option<u64>) -> String {
    value.map_or_else(|| "-".to_owned(), tokens)
}

fn tokens(value: u64) -> String {
    if value >= 1_000_000 {
        abbreviate(value as f64 / 1_000_000.0, "m")
    } else if value >= 1000 {
        abbreviate(value as f64 / 1_000.0, "k")
    } else {
        value.to_string()
    }
}

#[allow(clippy::cast_precision_loss)]
fn percent(part: u64, whole: u64) -> String {
    if whole == 0 {
        return "-".to_owned();
    }
    abbreviate(part as f64 * 100.0 / whole as f64, "%")
}

fn abbreviate(value: f64, unit: &str) -> String {
    #[allow(clippy::cast_precision_loss)]
    let text = format!("{value:.1}");
    let trimmed = text.strip_suffix(".0").unwrap_or(&text).to_owned();
    format!("{trimmed}{unit}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage() -> FrontendTokenUsage {
        FrontendTokenUsage {
            input_tokens: Some(11_000),
            output_tokens: Some(4_800),
            cache_read_tokens: Some(5_640),
            reasoning_tokens: Some(14_000),
            ..FrontendTokenUsage::default()
        }
    }

    fn dashboard() -> StatusData {
        let mut status = StatusData::new("gpt-5.2", "s-1", InfoLevel::Default);
        status.provider = Some("openai".to_owned());
        status.effort = Some("high".to_owned());
        status.workspace_root = r"D:\Code\Zed\Year2026\Jul0706\Pi".to_owned();
        status.usage = Some(usage());
        status.context_window = Some(500_000);
        status
    }

    #[test]
    fn summary_line_lists_model_session_and_latest_turn_usage() {
        assert_eq!(
            dashboard().summary_line(),
            "gpt-5.2 · session s-1 · ↑11k ↓4.8k R14k C51.3% 2.2%/500k"
        );

        // A fresh session renders unknowns as dashes instead of inventing
        // numbers (§2.4).
        let fresh = StatusData::new("m", "", InfoLevel::Default);
        assert_eq!(fresh.summary_line(), "m · ↑- ↓- R- C- -/-");
    }

    #[test]
    fn tokens_abbreviate_without_trailing_zeros() {
        assert_eq!(tokens(999), "999");
        assert_eq!(tokens(11_000), "11k");
        assert_eq!(tokens(4_800), "4.8k");
        assert_eq!(tokens(1_600_000), "1.6m");
    }

    #[test]
    fn usage_corner_renders_the_latest_turn_with_dashes_for_unknowns() {
        assert_eq!(
            dashboard().bottom_corners_for(80).1,
            "↑11k ↓4.8k R14k C51.3% 2.2%/500k"
        );
        let fresh = StatusData::new("m", "s", InfoLevel::Default);
        assert_eq!(fresh.bottom_corners_for(80).1, "↑- ↓- R- C- -/-");
    }

    #[test]
    fn usage_corner_degrades_c_then_r_then_arrows_then_context() {
        let mut status = dashboard();
        status.workspace_root.clear();
        let full = status.bottom_corners_for(80).1;
        assert_eq!(full, "↑11k ↓4.8k R14k C51.3% 2.2%/500k");

        let without_cache = status.bottom_corners_for(text::width(&full) - 1).1;
        assert_eq!(without_cache, "↑11k ↓4.8k R14k 2.2%/500k");

        let without_reasoning = status.bottom_corners_for(text::width(&without_cache) - 1).1;
        assert_eq!(without_reasoning, "↑11k ↓4.8k 2.2%/500k");

        let without_arrows = status
            .bottom_corners_for(text::width(&without_reasoning) - 1)
            .1;
        assert_eq!(without_arrows, "2.2%/500k");
    }

    #[test]
    fn model_corner_keeps_all_fields_until_width_forces_degradation() {
        let status = dashboard();
        let full = status.model_corner_for(80).expect("model corner");
        assert_eq!(full.provider.as_deref(), Some("openai"));
        assert_eq!(full.model, "gpt-5.2");
        assert_eq!(full.effort.as_deref(), Some("high"));

        // Effort drops before the provider, the provider before truncation.
        let narrow = status.model_corner_for(20).expect("model corner");
        assert_eq!(narrow.provider.as_deref(), Some("openai"));
        assert_eq!(narrow.effort, None);

        let tighter = status.model_corner_for(12).expect("model corner");
        assert_eq!(tighter.provider, None);
        assert_eq!(tighter.model, "gpt-5.2");

        let truncated = status.model_corner_for(6).expect("model corner");
        assert!(text::width(&truncated.model) <= 6);
        assert!(truncated.model.ends_with('…'));
    }

    #[test]
    fn workspace_paths_middle_ellipsize_and_short_paths_stay_verbatim() {
        let long = r"D:\Code\Zed\Year2026\Jul0706\Pi";
        assert_eq!(middle_ellipsis(long, text::width(long)), long);
        assert_eq!(middle_ellipsis(long, 7), r"D:\…\Pi");
        assert_eq!(middle_ellipsis(long, 1), "…");
        assert_eq!(middle_ellipsis(long, 0), "");
        let short = r"src\main.rs";
        assert_eq!(middle_ellipsis(short, 20), short);
    }

    #[test]
    fn bottom_corners_keep_full_usage_and_squeeze_the_path_first() {
        let status = dashboard();
        let wide = status.bottom_corners_for(72);
        assert_eq!(wide.0, status.workspace_root);
        assert_eq!(wide.1, "↑11k ↓4.8k R14k C51.3% 2.2%/500k");

        // The design's 40-column mock: path ellipsized, then C and R gone,
        // arrows and ctx/window last (§3.10).
        let narrow = status.bottom_corners_for(32);
        assert_eq!(narrow.0, r"D:\…\Pi");
        assert_eq!(narrow.1, "↑11k ↓4.8k 2.2%/500k");

        let (path, usage) = status.bottom_corners_for(200);
        assert_eq!(path, status.workspace_root);
        assert_eq!(usage, "↑11k ↓4.8k R14k C51.3% 2.2%/500k");
    }

    #[test]
    fn bottom_corners_without_a_root_show_usage_only() {
        let mut status = dashboard();
        status.workspace_root.clear();
        let (path, usage) = status.bottom_corners_for(28);
        assert!(path.is_empty());
        assert_eq!(usage, "↑11k ↓4.8k R14k 2.2%/500k");
    }
}
