//! Status bar projection: pure data to one summary line.

use philo_agent_runtime::TokenUsage;

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
    pub usage: Option<TokenUsage>,
    /// Context-budget hint: the configured or capability-derived window.
    pub context_window: Option<u64>,
    pub level: InfoLevel,
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
    /// priority: state, queue, session, model, usage/context, verbose.
    pub(crate) fn line_for_width(&self, max_width: usize) -> String {
        let state = if self.compacting {
            format!(
                "compacting [{}]",
                ["|", "/", "-", "\\"][self.compaction_spinner]
            )
        } else if self.busy {
            "busy".to_owned()
        } else {
            "idle".to_owned()
        };
        let mut fields = vec![
            (0, 3, format!("model {}", self.model)),
            (1, 2, format!("session {}", self.session)),
            (2, 0, state),
        ];
        if self.queued > 0 {
            fields.push((3, 1, format!("queued {}", self.queued)));
        }
        if let Some(usage) = &self.usage {
            let value =
                |value: Option<u64>| value.map_or_else(|| "-".to_owned(), |v| v.to_string());
            fields.push((
                4,
                4,
                format!(
                    "tokens in {} out {}",
                    value(usage.input_tokens),
                    value(usage.output_tokens)
                ),
            ));
            if let (Some(input), Some(window)) = (usage.input_tokens, self.context_window) {
                fields.push((5, 5, format!("ctx {input}/{window}")));
            }
        }
        if self.level == InfoLevel::Verbose {
            fields.push((6, 6, "verbose".to_owned()));
        }

        let mut admitted: Vec<(usize, String)> = Vec::new();
        fields.sort_by_key(|(_, priority, _)| *priority);
        for (order, _, candidate) in fields {
            let projected_width = admitted
                .iter()
                .map(|(_, value)| text::width(value))
                .sum::<usize>()
                + text::width(&candidate)
                + admitted.len() * 3;
            if projected_width <= max_width {
                admitted.push((order, candidate));
            }
        }
        admitted.sort_by_key(|(order, _)| *order);
        let line = admitted
            .into_iter()
            .map(|(_, value)| value)
            .collect::<Vec<_>>()
            .join(" | ");
        if line.is_empty() {
            text::truncate(
                if self.compacting {
                    "compacting"
                } else if self.busy {
                    "busy"
                } else {
                    "idle"
                },
                max_width,
            )
        } else {
            line
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_line_shows_model_session_and_state() {
        let mut status = StatusData::new("gpt-test", "s-1", InfoLevel::Default);
        assert_eq!(status.line(), "model gpt-test | session s-1 | idle");

        status.busy = true;
        status.queued = 2;
        status.usage = Some(TokenUsage {
            input_tokens: Some(1200),
            output_tokens: Some(340),
            ..TokenUsage::default()
        });
        status.context_window = Some(128_000);
        status.level = InfoLevel::Verbose;
        assert_eq!(
            status.line(),
            "model gpt-test | session s-1 | busy | queued 2 | \
             tokens in 1200 out 340 | ctx 1200/128000 | verbose"
        );
    }

    #[test]
    fn narrow_status_preserves_state_before_secondary_fields() {
        let mut status = StatusData::new("a-very-long-model", "session-123", InfoLevel::Verbose);
        status.busy = true;
        status.queued = 3;
        let line = status.line_for_width(40);
        assert_eq!(line, "session session-123 | busy | queued 3");
        assert!(text::width(&line) <= 40);
    }
}
