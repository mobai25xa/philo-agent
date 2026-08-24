use std::fmt::Write as _;

use philo::api::stable::PhiloError;
use philo_agent_runtime::{FailureDomain, ModelError, RetryDisposition};

/// Normalizes any SDK failure into the runtime's structured `ModelError`.
/// Every `PhiloErrorKind` takes this single path; no new runtime failure
/// path is introduced.
///
/// The four-question facts come straight from the SDK's frozen code table:
/// the stable code passes through, `fault_domain()` names who is
/// responsible (SDK `Sdk` is absorbed into [`FailureDomain::Internal`]),
/// `retry_advice()` records intrinsic re-issue advice, and
/// `semantic_summary()` supplies the human one-liner. The bounded redacted
/// Debug surface stays available as `diagnostic` for developers.
pub(crate) fn model_error(error: &PhiloError) -> ModelError {
    let mut diagnostic = format!(
        "philo model call failed: kind={:?} stage={:?} code={} class={:?}",
        error.kind(),
        error.context().stage(),
        error.code(),
        sdk_retry_word(error),
    );

    // ErrorDetails, ErrorContext, and RetryReport are the SDK's explicitly
    // redacted diagnostic surface. Preserve them so callers can distinguish
    // connection, delivery, provider, protocol, and retry failures.
    write!(diagnostic, " details={:?}", error.details()).expect("writing to String cannot fail");
    write!(diagnostic, " context={:?}", error.context()).expect("writing to String cannot fail");
    if let Some(retry) = error.retry_report() {
        write!(diagnostic, " retry={retry:?}").expect("writing to String cannot fail");
    }
    if let Some(summary) = error.provider_summary() {
        if let Some(value) = summary.message() {
            write!(
                diagnostic,
                " provider_message={:?}",
                bounded_single_line(value)
            )
            .expect("writing to String cannot fail");
        }
        if let Some(value) = summary.code() {
            write!(diagnostic, " provider_code={:?}", bounded_single_line(value))
                .expect("writing to String cannot fail");
        }
        if let Some(value) = summary.kind() {
            write!(diagnostic, " provider_kind={:?}", bounded_single_line(value))
                .expect("writing to String cannot fail");
        }
        if let Some(value) = summary.param() {
            write!(diagnostic, " provider_param={:?}", bounded_single_line(value))
                .expect("writing to String cannot fail");
        }
        if let Some(value) = summary.request_id() {
            write!(
                diagnostic,
                " provider_request_id={:?}",
                bounded_single_line(value)
            )
            .expect("writing to String cannot fail");
        }
        if summary.body_truncated() {
            diagnostic.push_str(" provider_body_truncated=true");
        }
    }

    // Transport sources carry the actionable OS/DNS/TLS reason that the
    // stable classification cannot express. Only expose the deepest cause,
    // bounded to one line, and redact URL query strings before rendering it.
    if matches!(error.kind(), philo::api::stable::PhiloErrorKind::Transport)
        && let Some(cause) = deepest_cause(error)
    {
        write!(diagnostic, " cause={cause:?}").expect("writing to String cannot fail");
    }

    ModelError::new(
        format!("model.{}", error.code()),
        map_domain(error.fault_domain()),
        map_disposition(error.retry_advice()),
        error.semantic_summary(),
        diagnostic,
    )
}

fn map_domain(domain: philo::api::stable::FaultDomain) -> FailureDomain {
    match domain {
        philo::api::stable::FaultDomain::Provider => FailureDomain::Provider,
        philo::api::stable::FaultDomain::Network => FailureDomain::Network,
        philo::api::stable::FaultDomain::Caller => FailureDomain::Caller,
        // An SDK defect is still "our stack is at fault" from the user's
        // point of view; the diagnostic preserves the sdk attribution for
        // upstream reports.
        philo::api::stable::FaultDomain::Sdk => FailureDomain::Internal,
        _ => FailureDomain::Internal,
    }
}

fn map_disposition(advice: philo::api::stable::RetryDisposition) -> RetryDisposition {
    let retry_after_ms =
        advice
            .retry_after()
            .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX));
    match advice {
        philo::api::stable::RetryDisposition::Never => RetryDisposition::Never,
        philo::api::stable::RetryDisposition::Safe { .. } => {
            RetryDisposition::Safe { retry_after_ms }
        }
        philo::api::stable::RetryDisposition::MayDuplicate { .. } => {
            RetryDisposition::MayDuplicate { retry_after_ms }
        }
        _ => RetryDisposition::Never,
    }
}

/// Agent-side adapter failure classified by the local code table
/// (`docs/philo-agent/error-codes.md`, `model.` prefix): deterministic,
/// never retryable. The message doubles as the bounded summary and
/// diagnostic.
pub(crate) fn caller_error(code: &'static str, message: impl Into<String>) -> ModelError {
    let message = message.into();
    ModelError::new(
        code.to_owned(),
        FailureDomain::Caller,
        RetryDisposition::Never,
        message.clone(),
        message,
    )
}

/// Agent-side adapter failure caused by non-conforming provider data
/// observed outside the SDK decoder (Provider domain); regeneration is
/// intrinsically safe, so the recorded advice is MayDuplicate.
pub(crate) fn provider_stream_error(code: &'static str, message: impl Into<String>) -> ModelError {
    let message = message.into();
    ModelError::new(
        code.to_owned(),
        FailureDomain::Provider,
        RetryDisposition::MayDuplicate { retry_after_ms: None },
        message.clone(),
        message,
    )
}

/// One-word stand-in for the former two-value class inside diagnostics.
fn sdk_retry_word(error: &PhiloError) -> &'static str {
    match error.retry_advice() {
        philo::api::stable::RetryDisposition::Never => "Never",
        philo::api::stable::RetryDisposition::Safe { .. } => "Safe",
        philo::api::stable::RetryDisposition::MayDuplicate { .. } => "MayDuplicate",
        _ => "Never",
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

pub(crate) fn bounded_single_line(value: &str) -> String {
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
    use philo_agent_runtime::{FailureDomain, RetryDisposition};

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

        assert_eq!(normalized.code(), "model.transport_connect");
        assert_eq!(normalized.domain(), FailureDomain::Network);
        assert!(matches!(
            normalized.retry(),
            RetryDisposition::Safe { retry_after_ms: None }
        ));
        assert!(normalized.diagnostic().contains("code=transport_connect"));
        assert!(
            normalized
                .diagnostic()
                .contains("details=Transport { stage: Connect, delivery: NotSent }")
        );
        assert!(
            normalized
                .diagnostic()
                .contains("cause=\"connection refused (os error 10061)\"")
        );
        assert!(!normalized.summary().is_empty());
    }

    #[test]
    fn protocol_decode_is_provider_domain_and_retryable() {
        let error = PhiloError::protocol(
            philo::api::stable::ProtocolStage::State,
            Some("choices".to_owned()),
        );

        let normalized = model_error(&error);

        assert_eq!(normalized.code(), "model.invalid_sequence");
        assert_eq!(normalized.domain(), FailureDomain::Provider);
        assert!(matches!(
            normalized.retry(),
            RetryDisposition::MayDuplicate { .. }
        ));
        // The SDK advice is the recovery decision's single source: a
        // non-conforming provider sequence is worth an identical re-issue.
        assert!(matches!(
            normalized.retry(),
            RetryDisposition::MayDuplicate { .. } | RetryDisposition::Safe { .. }
        ));
        assert!(!normalized.summary().is_empty());
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
