//! Status bar projection: pure data to one summary line.

use philo_agent_service::FrontendTokenUsage;

use super::text;
use super::transcript::InfoLevel;

/// Everything the status bar shows; the event loop keeps this current.
#[derive(Clone, Debug, Default)]
pub struct StatusData {
    pub model: String,
    pub session: String,
    pub busy: bool,
    /// A manual compaction or automatic pre-turn compaction is active.
    pub compacting: bool,
    compaction_spinner: usize,
    /// Prompts accepted while another operation was active (M6 FIFO).
    pub queued: usize,
    pub usage: Option<FrontendTokenUsage>,
    /// Context-budget hint: the configured or capability-derived window.
    pub context_window: Option<u64>,
    pub level: InfoLevel,
    /// A parsed config change is waiting for Idle before Runtime apply.
    pub config_reload_pending: bool,
    /// Input handle is waiting to rebuild. Shown once; not a transcript cell.
    pub input_rebuilding: bool,
}

impl StatusData {
    pub fn new(model: impl Into<String>, session: impl Into<String>, level: InfoLevel) -> Self {
        Self {
            model: model.into(),
            session: session.into(),
            busy: false,
            compacting: false,
            compaction_spinner: 0,
            queued: 0,
            usage: None,
            context_window: None,
            level,
            config_reload_pending: false,
            input_rebuilding: false,
        }
    }

    pub fn set_compacting(&mut self, compacting: bool) {
        self.compacting = compacting;
        if !compacting {
            self.compaction_spinner = 0;
        }
    }

    pub fn advance_spinner(&mut self) {
        if self.compacting {
            self.compaction_spinner = (self.compaction_spinner + 1) % 4;
        }
    }

    /// Renders the single status line.
    pub fn line(&self) -> String {
        self.line_for_width(usize::MAX)
    }

    /// Responsive status projection. Fields are admitted in preservation
    /// priority: compact/queue, session, model, usage, verbose.
    /// Idle/busy is color on the first field, not a word.
    pub(crate) fn line_for_width(&self, max_width: usize) -> String {
        let mut fields = vec![
            (0, 6, text::truncate(&self.model, 24)),
            (1, 2, text::truncate(&self.session, 12)),
        ];
        if self.compacting {
            fields.push((
                2,
                0,
                format!("compact {}", ["|", "/", "-", "\\"][self.compaction_spinner]),
            ));
        }
        if self.queued > 0 {
            fields.push((3, 1, format!("queued {}", self.queued)));
        }
        if let Some(usage) = &self.usage {
            if let (Some(input), Some(window)) = (usage.input_tokens, self.context_window) {
                fields.push((
                    4,
                    4,
                    format!(
                        "{}/{}{}",
                        compact_count(input),
                        compact_count(window),
                        cache_suffix(usage.cache_read_tokens)
                    ),
                ));
            } else if let Some(input) = usage.input_tokens {
                fields.push((
                    4,
                    4,
                    format!(
                        "in {}{}",
                        compact_count(input),
                        cache_suffix(usage.cache_read_tokens)
                    ),
                ));
            }
        }
        if self.level == InfoLevel::Verbose {
            fields.push((5, 5, "verbose".to_owned()));
        }
        if self.config_reload_pending {
            fields.push((6, 0, "reload".to_owned()));
        }
        if self.input_rebuilding {
            fields.push((7, 0, "input".to_owned()));
        }

        let mut admitted: Vec<(usize, String)> = Vec::new();
        fields.sort_by_key(|(_, priority, _)| *priority);
        for (order, _, candidate) in fields {
            let projected_width = admitted
                .iter()
                .map(|(_, value)| text::width(value))
                .sum::<usize>()
                + text::width(&candidate)
                + admitted.len() * 2;
            if projected_width <= max_width {
                admitted.push((order, candidate));
            }
        }
        admitted.sort_by_key(|(order, _)| *order);
        let line = admitted
            .into_iter()
            .map(|(_, value)| value)
            .collect::<Vec<_>>()
            .join("  ");
        if line.is_empty() {
            text::truncate(&self.model, max_width)
        } else {
            line
        }
    }
}

fn cache_suffix(cache_read: Option<u64>) -> String {
    match cache_read {
        Some(tokens) if tokens > 0 => format!(" cache {}", compact_count(tokens)),
        _ => String::new(),
    }
}

fn compact_count(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{:.1}m", value as f64 / 1_000_000.0)
    } else if value >= 1000 {
        format!("{:.1}k", value as f64 / 1000.0)
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_line_shows_model_session_and_state() {
        let mut status = StatusData::new("gpt-test", "s-1", InfoLevel::Default);
        assert_eq!(status.line(), "gpt-test  s-1");

        status.busy = true;
        status.queued = 2;
        status.usage = Some(FrontendTokenUsage {
            input_tokens: Some(1200),
            output_tokens: Some(340),
            ..FrontendTokenUsage::default()
        });
        status.context_window = Some(128_000);
        status.level = InfoLevel::Verbose;
        assert_eq!(
            status.line(),
            "gpt-test  s-1  queued 2  1.2k/128.0k  verbose"
        );

        status.usage = Some(FrontendTokenUsage {
            input_tokens: Some(1200),
            output_tokens: Some(340),
            cache_read_tokens: Some(900),
            ..FrontendTokenUsage::default()
        });
        assert_eq!(
            status.line(),
            "gpt-test  s-1  queued 2  1.2k/128.0k cache 900  verbose"
        );
    }

    #[test]
    fn narrow_status_preserves_state_before_secondary_fields() {
        let mut status = StatusData::new("a-very-long-model", "session-123", InfoLevel::Verbose);
        status.busy = true;
        status.queued = 3;
        let line = status.line_for_width(40);
        assert_eq!(line, "session-123  queued 3  verbose");
        assert!(text::width(&line) <= 40);
    }

    #[test]
    fn pending_reload_is_visible_on_the_status_line() {
        let mut status = StatusData::new("gpt-test", "s-1", InfoLevel::Default);
        status.busy = true;
        status.config_reload_pending = true;
        assert!(status.line().contains("reload"));
        assert!(status.line().contains("gpt-test"));
    }

    #[test]
    fn input_rebuild_is_visible_once_on_the_status_line() {
        let mut status = StatusData::new("gpt-test", "s-1", InfoLevel::Default);
        status.input_rebuilding = true;
        assert!(status.line().contains("input"));
        status.input_rebuilding = false;
        assert!(!status.line().contains("input"));
    }
}
