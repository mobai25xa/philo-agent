//! Canonical failure rendering shared by every frontend.
//!
//! The wording of failure and retry lines is a single source of truth: TUI
//! and CLI call these pure functions and only apply their own visual shell
//! (line kinds / colors versus stderr outputs). No I/O, no terminal access.

use crate::frontend::snapshot::FrontendFailure;

/// Visual role of one rendered presentation line; frontends map this onto
/// their own styling vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureLineStyle {
    /// Primary human line (error emphasis).
    Error,
    /// Attribution tags line (dim meta).
    Meta,
}

/// One rendered presentation line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FailureLine {
    pub style: FailureLineStyle,
    pub text: String,
}

fn tags_line(failure: &FrontendFailure) -> String {
    format!("[{} · {} · {}]", failure.domain, failure.stage, failure.code)
}

/// Renders one durable turn failure in three tiers:
/// primary human summary, attribution tags, bounded diagnostic detail.
pub fn turn_failed_lines(failure: &FrontendFailure) -> Vec<FailureLine> {
    vec![
        FailureLine {
            style: FailureLineStyle::Error,
            text: format!("turn failed: {}", failure.summary),
        },
        FailureLine {
            style: FailureLineStyle::Meta,
            text: tags_line(failure),
        },
        FailureLine {
            style: FailureLineStyle::Meta,
            text: format!("detail: {}", failure.diagnostic),
        },
    ]
}

/// Renders one transient retry notification in two tiers: the primary line
/// names the summary plus attempt/max and the wait; the tags line carries
/// attribution. No detail tier — the full diagnostic stays on the eventual
/// terminal failure if retries exhaust.
pub fn retry_scheduled_lines(
    failure: &FrontendFailure,
    attempt: u32,
    max_retries: u32,
    delay_ms: u64,
) -> Vec<FailureLine> {
    let seconds = format!("{:.1}", delay_ms as f64 / 1000.0);
    vec![
        FailureLine {
            style: FailureLineStyle::Error,
            text: format!(
                "model call interrupted; retrying (attempt {attempt}/{max_retries}, \
                 waiting {seconds}s): {}",
                failure.summary
            ),
        },
        FailureLine {
            style: FailureLineStyle::Meta,
            text: tags_line(failure),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> FrontendFailure {
        FrontendFailure {
            code: "model.invalid_sequence".to_owned(),
            domain: "provider".to_owned(),
            stage: "model-port".to_owned(),
            retry: "may-duplicate".to_owned(),
            summary: "the provider produced an incomplete tool call".to_owned(),
            diagnostic: "kind=ProtocolDecode stage=Decode code=invalid_sequence".to_owned(),
        }
    }

    #[test]
    fn turn_failure_renders_three_tiers() {
        let lines = turn_failed_lines(&sample());

        assert_eq!(lines.len(), 3);
        assert_eq!(
            lines[0].text,
            "turn failed: the provider produced an incomplete tool call"
        );
        assert_eq!(lines[0].style, FailureLineStyle::Error);
        assert_eq!(
            lines[1].text,
            "[provider · model-port · model.invalid_sequence]"
        );
        assert_eq!(lines[1].style, FailureLineStyle::Meta);
        assert!(lines[2].text.starts_with("detail: kind=ProtocolDecode"));
    }

    #[test]
    fn retry_notification_renders_two_tiers_with_wait() {
        let lines = retry_scheduled_lines(&sample(), 1, 3, 500);

        assert_eq!(lines.len(), 2);
        assert!(lines[0]
            .text
            .starts_with("model call interrupted; retrying (attempt 1/3, waiting 0.5s)"));
        assert!(lines[0].text.ends_with("incomplete tool call"));
        assert_eq!(lines[1].text, "[provider · model-port · model.invalid_sequence]");
    }
}
