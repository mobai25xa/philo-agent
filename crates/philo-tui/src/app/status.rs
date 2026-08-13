//! Status bar projection: pure data to one summary line.

use philo_agent_runtime::TokenUsage;

use super::transcript::InfoLevel;

/// Everything the status bar shows; the event loop keeps this current.
#[derive(Clone, Debug, Default)]
pub struct StatusData {
    pub model: String,
    pub session: String,
    pub busy: bool,
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
            queued: 0,
            usage: None,
            context_window: None,
            level,
        }
    }

    /// Renders the single status line.
    pub fn line(&self) -> String {
        let mut parts = vec![
            format!("model {}", self.model),
            format!("session {}", self.session),
            if self.busy {
                "busy".to_owned()
            } else {
                "idle".to_owned()
            },
        ];
        if self.queued > 0 {
            parts.push(format!("queued {}", self.queued));
        }
        if let Some(usage) = &self.usage {
            let value =
                |value: Option<u64>| value.map_or_else(|| "-".to_owned(), |v| v.to_string());
            let mut tokens = format!(
                "tokens in {} out {}",
                value(usage.input_tokens),
                value(usage.output_tokens)
            );
            if let (Some(input), Some(window)) = (usage.input_tokens, self.context_window) {
                tokens.push_str(&format!(" | ctx {input}/{window}"));
            }
            parts.push(tokens);
        }
        if self.level == InfoLevel::Verbose {
            parts.push("verbose".to_owned());
        }
        parts.join(" | ")
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
}
