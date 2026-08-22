//! RUNTIME-RECOVERY: 回合内模型调用自动恢复（可恢复故障重试、致命故障快速失败）

mod support;

use std::sync::Arc;

use philo_agent_runtime::{
    AgentEvent, AgentFailureKind, DEFAULT_MAX_TOOL_ROUNDS, GenerationConfig, RecoveryConfig,
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

    // One retry notification with sane fields.
    let retries: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ModelRetryScheduled {
                attempt,
                max_retries,
                delay_ms,
                reason,
                ..
            } => Some((*attempt, *max_retries, *delay_ms, reason.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(retries.len(), 1);
    assert_eq!(retries[0].0, 1);
    assert_eq!(retries[0].1, 3);

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
            if failure.kind() == AgentFailureKind::ModelCall
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
            if failure.kind() == AgentFailureKind::ModelCall
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
