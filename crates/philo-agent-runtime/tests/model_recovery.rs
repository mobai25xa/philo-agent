//! RUNTIME-RECOVERY: 回合内模型调用自动恢复（可恢复故障重试、致命故障快速失败）

mod support;

use std::sync::Arc;

use philo_agent_runtime::{
    AgentEvent, DEFAULT_MAX_TOOL_ROUNDS, FailureStage, GenerationConfig, RecoveryConfig,
    RuntimeConfig, ToolChoice,
};
use philo_session::{ContextMessage, MemorySessionStore, SessionStore};
use support::fake_model::{FakeModel, ModelScript};
use support::runtime::{Harness, empty_tools};

fn config(recovery: RecoveryConfig) -> RuntimeConfig {
    RuntimeConfig {
        system_prompt: "answer directly".to_owned(),
        model_target: "fake".to_owned(),
        generation: GenerationConfig {
            max_output_tokens: 1024,
            temperature: 0.0,
            reasoning_effort: None,
            tool_choice: ToolChoice::Auto,
        },
        max_tool_rounds: DEFAULT_MAX_TOOL_ROUNDS,
        max_parallel_tool_calls: 1,
        operation_timeout: None,
        tool_cancel_grace: std::time::Duration::from_millis(300),
        compaction: Default::default(),
        recovery,
    }
}

/// Fast budget so retry tests stay well under a second.
fn fast_recovery(max_retries: u32) -> RecoveryConfig {
    RecoveryConfig {
        enabled: true,
        max_retries,
        backoff_base_ms: 1,
        backoff_max_ms: 5,
    }
}

async fn launch(model: Arc<FakeModel>, recovery: RecoveryConfig) -> (Harness, Arc<MemorySessionStore>) {
    let sessions = Arc::new(MemorySessionStore::new());
    let harness = Harness::launch_default(
        model,
        sessions.clone(),
        empty_tools(),
        config(recovery),
    )
    .await;
    (harness, sessions)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recoverable_truncation_is_retried_and_the_turn_succeeds() {
    // Real scenario shape: first call dies mid-stream, second succeeds.
    let model = Arc::new(FakeModel::new([
        ModelScript::stream_truncated_after(&["par", "tial"], "incomplete_response"),
        ModelScript::text(&["hello"]),
    ]));
    let (mut harness, sessions) = launch(model.clone(), fast_recovery(3)).await;
    let (events, outcome) = harness.run("session", "hi").await;

    assert!(matches!(
        outcome,
        philo_agent_runtime::OperationOutcome::Succeeded { ref assistant } if assistant.content() == "hello"
    ));

    // One retry notification with sane fields and structured attribution.
    let retries: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ModelRetryScheduled {
                attempt,
                max_retries,
                delay_ms,
                failure,
                ..
            } => Some((
                *attempt,
                *max_retries,
                *delay_ms,
                failure.code().to_owned(),
                failure.domain(),
                failure.retry(),
            )),
            _ => None,
        })
        .collect();
    assert_eq!(retries.len(), 1);
    assert_eq!(retries[0].0, 1);
    assert_eq!(retries[0].1, 3);
    assert_eq!(retries[0].3, "model.incomplete_response");
    assert_eq!(retries[0].4, philo_agent_runtime::FailureDomain::Provider);
    assert!(matches!(
        retries[0].5,
        philo_agent_runtime::RetryDisposition::MayDuplicate { .. }
    ));

    // Exactly two attempts; both saw the identical message history and the
    // same logical model-call id.
    let calls = model.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].model_call_id, calls[1].model_call_id);
    assert_eq!(calls[0].messages, calls[1].messages);

    // The failed attempt's streamed deltas are transient facts only: the
    // durable context holds exactly one clean exchange.
    let stored = philo_session::SessionId::new("session");
    let view = sessions.context_view(&stored).await.unwrap();
    assert_eq!(
        view.messages(),
        &[
            ContextMessage::User {
                parts: philo_session::SessionUserPart::text_parts("hi"),
            },
            ContextMessage::Assistant {
                blocks: vec![philo_session::SessionAssistantBlock::Text {
                    text: "hello".to_owned(),
                }],
            },
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fatal_failure_never_retries() {
    let model = Arc::new(FakeModel::start_fails("invalid api key"));
    let (mut harness, _sessions) = launch(model.clone(), fast_recovery(3)).await;
    let (events, outcome) = harness.run("session", "hi").await;
    assert!(matches!(
        outcome,
        philo_agent_runtime::OperationOutcome::Failed { failure, .. }
            if failure.stage() == FailureStage::ModelPort
    ));
    assert_eq!(model.call_count(), 1);
    assert!(!events
        .iter()
        .any(|event| matches!(event, AgentEvent::ModelRetryScheduled { .. })));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recoverable_start_error_is_retried() {
    let model = Arc::new(FakeModel::new([
        ModelScript::RecoverableStartError("connection refused".to_owned()),
        ModelScript::text(&["ok"]),
    ]));    let (mut harness, _sessions) = launch(model.clone(), fast_recovery(3)).await;
    let (_events, outcome) = harness.run("session", "hi").await;
    assert!(matches!(
        outcome,
        philo_agent_runtime::OperationOutcome::Succeeded { ref assistant } if assistant.content() == "ok"
    ));
    assert_eq!(model.call_count(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_recovery_fails_fast() {
    let model = Arc::new(FakeModel::new([
        ModelScript::stream_truncated_after(&["partial"], "incomplete_response"),
    ]));
    let (mut harness, _sessions) =
        launch(model.clone(), RecoveryConfig { enabled: false, ..fast_recovery(3) }).await;
    let (events, outcome) = harness.run("session", "hi").await;
    assert!(matches!(outcome, philo_agent_runtime::OperationOutcome::Failed { .. }));
    assert_eq!(model.call_count(), 1);
    assert!(!events
        .iter()
        .any(|event| matches!(event, AgentEvent::ModelRetryScheduled { .. })));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exhausted_budget_settles_failed_with_every_retry_notified() {
    let truncated = || ModelScript::stream_truncated_after(&["x"], "incomplete_response");
    let model = Arc::new(FakeModel::new([truncated(), truncated(), truncated(), truncated()]));
    let (mut harness, _sessions) = launch(model.clone(), fast_recovery(3)).await;
    let (events, outcome) = harness.run("session", "hi").await;

    assert!(matches!(
        outcome,
        philo_agent_runtime::OperationOutcome::Failed { failure, .. }
            if failure.stage() == FailureStage::ModelPort
    ));
    // Original attempt plus exactly `max_retries` re-attempts.
    assert_eq!(model.call_count(), 4);
    let attempts: Vec<u32> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ModelRetryScheduled { attempt, .. } => Some(*attempt),
            _ => None,
        })
        .collect();
    assert_eq!(attempts, vec![1, 2, 3]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zero_budget_keeps_single_attempt() {
    let model = Arc::new(FakeModel::new([
        ModelScript::stream_truncated_after(&["partial"], "incomplete_response"),
    ]));
    let (mut harness, _sessions) = launch(model.clone(), fast_recovery(0)).await;
    let (_, outcome) = harness.run("session", "hi").await;
    assert!(matches!(outcome, philo_agent_runtime::OperationOutcome::Failed { .. }));
    assert_eq!(model.call_count(), 1);
}

/// The user-facing scenario that motivated the retry switch: a compatible
/// gateway emitting a non-conforming tool-call sequence (`invalid_sequence`,
/// `MayDuplicate`) used to be legacy-fatal; the SDK advice now drives the
/// decision, so an identical re-issue happens and a clean follow-up wins.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn protocol_decode_fault_is_retried_by_sdk_advice() {
    use support::fake_model::protocol_decode_error;
    let model = Arc::new(FakeModel::new([
        ModelScript::Events(vec![Err(protocol_decode_error("invalid_tool_call"))]),
        ModelScript::text(&["recovered"]),
    ]));
    let (mut harness, _sessions) = launch(model.clone(), fast_recovery(3)).await;
    let (events, outcome) = harness.run("session", "hi").await;
    assert!(matches!(
        outcome,
        philo_agent_runtime::OperationOutcome::Succeeded { ref assistant }
            if assistant.content() == "recovered"
    ));
    assert_eq!(model.call_count(), 2);
    let retries: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ModelRetryScheduled { failure, .. } => Some(failure.code().to_owned()),
            _ => None,
        })
        .collect();
    assert_eq!(retries, vec!["model.invalid_sequence"]);
}

/// Behavior correction pinned by the SDK code table: a TLS handshake
/// failure (`transport_tls`, `Never`) used to be legacy-recoverable; an
/// identical re-issue cannot fix a handshake, so it fails fast.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tls_handshake_failure_is_never_retried() {
    use support::fake_model::tls_error;
    let model = Arc::new(FakeModel::new([ModelScript::Events(vec![Err(tls_error(
        "certificate verify failed",
    ))])]));
    let (mut harness, _sessions) = launch(model.clone(), fast_recovery(3)).await;
    let (events, outcome) = harness.run("session", "hi").await;
    assert!(matches!(
        outcome,
        philo_agent_runtime::OperationOutcome::Failed { failure, .. }
            if failure.code() == "model.transport_tls"
                && failure.domain() == philo_agent_runtime::FailureDomain::Network
    ));
    assert_eq!(model.call_count(), 1);
    assert!(!events
        .iter()
        .any(|event| matches!(event, AgentEvent::ModelRetryScheduled { .. })));
}

#[test]
fn backoff_delay_stays_bounded_and_grows() {
    let recovery = RecoveryConfig {
        enabled: true,
        max_retries: 6,
        backoff_base_ms: 100,
        backoff_max_ms: 500,
    };
    let first = recovery.backoff_delay(1).as_millis();
    let second = recovery.backoff_delay(2).as_millis();
    let sixth = recovery.backoff_delay(6).as_millis();
    assert!(first <= 100, "first delay capped by base: {first}");
    assert!(second >= first.saturating_sub(first / 4), "grows after jitter");
    assert!(sixth <= 500, "capped by max: {sixth}");
}
