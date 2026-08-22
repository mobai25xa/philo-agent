use std::fmt::Write as _;

use philo::api::stable::PhiloError;
use philo_agent_runtime::{ModelFailureClass, ModelError};

/// Normalizes any SDK failure into the runtime `ModelError`, carrying a
/// redacted diagnostic summary. Every `PhiloErrorKind` takes this single path;
/// no new runtime failure path is introduced.
pub(crate) fn model_error(error: &PhiloError) -> ModelError {
    let class = failure_class(error);
    let mut message = format!(
        "philo model call failed: kind={:?} stage={:?} code={} class={:?}",
        error.kind(),
        error.context().stage(),
        error.code(),
        class
    );

    // ErrorDetails, ErrorContext, and RetryReport are the SDK's explicitly
    // redacted diagnostic surface. Preserve them so callers can distinguish
    // connection, delivery, provider, protocol, and retry failures.
    write!(message, " details={:?}", error.details()).expect("writing to String cannot fail");
    write!(message, " context={:?}", error.context()).expect("writing to String cannot fail");
    if let Some(retry) = error.retry_report() {
        write!(message, " retry={retry:?}").expect("writing to String cannot fail");
    }
    if let Some(summary) = error.provider_summary() {
        if let Some(value) = summary.message() {
            write!(
                message,
                " provider_message={:?}",
                bounded_single_line(value)
            )
            .expect("writing to String cannot fail");
        }
        if let Some(value) = summary.code() {
            write!(message, " provider_code={:?}", bounded_single_line(value))
                .expect("writing to String cannot fail");
        }
        if let Some(value) = summary.kind() {
            write!(message, " provider_kind={:?}", bounded_single_line(value))
                .expect("writing to String cannot fail");
        }
        if let Some(value) = summary.param() {
            write!(message, " provider_param={:?}", bounded_single_line(value))
                .expect("writing to String cannot fail");
        }
        if let Some(value) = summary.request_id() {
            write!(
                message,
                " provider_request_id={:?}",
                bounded_single_line(value)
            )
            .expect("writing to String cannot fail");
        }
        if summary.body_truncated() {
            message.push_str(" provider_body_truncated=true");
        }
    }

    // Transport sources carry the actionable OS/DNS/TLS reason that the
    // stable classification cannot express. Only expose the deepest cause,
    // bounded to one line, and redact URL query strings before rendering it.
    if matches!(error.kind(), philo::api::stable::PhiloErrorKind::Transport)
        && let Some(cause) = deepest_cause(error)
    {
        write!(message, " cause={cause:?}").expect("writing to String cannot fail");
    }

    ModelError::with_class(message, class)
}

/// Maps the SDK's redacted classification onto the runtime's recovery
/// vocabulary. The SDK owns attempt-level retries for everything before a
/// 2xx response; anything surfacing here either exhausted that policy or
/// happened mid-stream, where only delivery faults are worth one fresh
/// identical attempt by the caller.
fn failure_class(error: &PhiloError) -> ModelFailureClass {
    use philo::api::stable::{ErrorDetails as Details, ProtocolStage as Protocol, TimeoutKind as Timeout};

    match error.details() {
        // Connect/DNS/TLS/write failures and mid-body drops: a fresh attempt
        // rebuilds the whole request, so ambiguity is harmless at this layer.
        Details::Transport { .. } => ModelFailureClass::Recoverable,
        // Stream cut before the terminal event (e.g. `incomplete_response`).
        Details::Protocol { stage: Protocol::Termination, .. } => ModelFailureClass::Recoverable,
        // Provider sent malformed or contradictory events; structural.
        Details::Protocol { .. } | Details::Configuration { .. }
        | Details::Validation { .. } | Details::Capability { .. }
        | Details::Credential | Details::ProtocolEncode { .. }
        | Details::Replay { .. } => ModelFailureClass::Fatal,
        // Provider aborted the stream remotely: transient.
        Details::RemoteStream { .. } => ModelFailureClass::Recoverable,
        // A stalled stream is a delivery fault regardless of which deadline
        // expired; a fresh attempt gets its own budgets.
        Details::Timeout { kind: Timeout::StreamIdle | Timeout::ResponseHead } => {
            ModelFailureClass::Recoverable
        }
        // Whole-call budget exhausted: retrying here would fight the
        // caller's own operation timeout.
        Details::Timeout { kind: Timeout::Total } => ModelFailureClass::Fatal,
        Details::Http { status, .. } => {
            let code = status.as_u16();
            if code == 408 || code == 409 || code == 429 || code >= 500 {
                ModelFailureClass::Recoverable
            } else {
                ModelFailureClass::Fatal
            }
        }
        // Cancellation is a decision, not a fault to retry through.
        Details::Cancelled => ModelFailureClass::Fatal,
        // The SDK enum is non-exhaustive: unknown future details stay fatal
        // until classified deliberately.
        _ => ModelFailureClass::Fatal,
    }
}

fn deepest_cause(error: &(dyn std::error::Error + 'static)) -> Option<String> {
    let mut current = error.source()?;
    for _ in 0..15 {
        let Some(next) = current.source() else {
            break;
        };
        current = next;
    }
    let cause = bounded_single_line(&current.to_string());
    (!cause.is_empty() && !cause.starts_with("transport ")).then_some(cause)
}

fn bounded_single_line(value: &str) -> String {
    const MAX_CHARS: usize = 512;

    let mut normalized = String::with_capacity(value.len().min(MAX_CHARS));
    let mut truncated = false;
    for (index, character) in value.chars().enumerate() {
        if index == MAX_CHARS {
            truncated = true;
            break;
        }
        normalized.push(if character.is_control() {
            ' '
        } else {
            character
        });
    }
    if truncated {
        normalized.push_str("...");
    }
    redact_url_secrets(&normalized)
}

fn redact_url_secrets(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(scheme_offset) = rest.find("://") {
        let prefix_end = scheme_offset + 3;
        output.push_str(&rest[..prefix_end]);
        rest = &rest[prefix_end..];

        let token_end = rest
            .find(|character: char| {
                character.is_whitespace() || matches!(character, ')' | ']' | '}')
            })
            .unwrap_or(rest.len());
        let (url_tail, suffix) = rest.split_at(token_end);
        let secret_offset = url_tail.find(['?', '#']).unwrap_or(url_tail.len());
        let visible_url = &url_tail[..secret_offset];
        let authority_end = visible_url.find('/').unwrap_or(visible_url.len());
        if let Some(user_info_end) = visible_url[..authority_end].rfind('@') {
            output.push_str("<redacted>@");
            output.push_str(&visible_url[user_info_end + 1..]);
        } else {
            output.push_str(visible_url);
        }
        if secret_offset < url_tail.len() {
            output.push(url_tail.as_bytes()[secret_offset] as char);
            output.push_str("<redacted>");
        }
        rest = suffix;
    }
    output.push_str(rest);
    output
}

#[cfg(test)]
mod tests {
    use std::fmt;

    use philo::api::stable::{DeliveryState, PhiloError, TransportStage};

    use super::{bounded_single_line, model_error};

    #[derive(Debug)]
    struct DiagnosticSource(&'static str);

    impl fmt::Display for DiagnosticSource {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.0)
        }
    }

    impl std::error::Error for DiagnosticSource {}

    #[test]
    fn transport_error_preserves_safe_details_and_root_cause() {
        let error = PhiloError::transport(TransportStage::Connect, DeliveryState::NotSent)
            .with_source(DiagnosticSource("connection refused (os error 10061)"));

        let normalized = model_error(&error);

        assert!(normalized.message().contains("code=transport_connect"));
        assert!(
            normalized
                .message()
                .contains("details=Transport { stage: Connect, delivery: NotSent }")
        );
        assert!(
            normalized
                .message()
                .contains("cause=\"connection refused (os error 10061)\"")
        );
    }

    #[test]
    fn diagnostic_text_is_single_line_bounded_and_redacts_url_queries() {
        let value = format!(
            "request to https://alice:password@example.test/path?api_key=secret failed\n{}",
            "x".repeat(600)
        );

        let normalized = bounded_single_line(&value);

        assert!(!normalized.contains("secret"));
        assert!(!normalized.contains("password"));
        assert!(!normalized.contains('\n'));
        assert!(normalized.contains("https://<redacted>@example.test/path?<redacted>"));
        assert!(normalized.ends_with("..."));
    }
}
