//! Bounded reliable / transient pipeline: live drain, backpressure, sink death.

mod support;

use std::sync::Arc;
use std::time::Duration;

use philo_agent_runtime::{
    AdmissionError, AgentEvent, ChannelBounds, GenerationConfig, ModelEvent, RuntimeConfig,
    RuntimeEvent, SequentialIdSource, SessionId, TokenUsage,
    UserMessage,
};
use philo_session::MemorySessionStore;
use support::fake_model::{FakeModel, ModelScript};
use support::fake_tool::{FakeTool, FakeToolResult};
use support::runtime::{
    EventProbe, empty_tools, event_cap_bounds, generation, start_with_bounds, submit_prompt,
    tiny_pipeline_bounds, wait_until_idle, wait_until_shutdown_leaves_running,
};

fn config() -> RuntimeConfig {
    RuntimeConfig {
        system_prompt: "sys".to_owned(),
        model_target: "fake".to_owned(),
        generation: GenerationConfig::default(),
        max_tool_rounds: 1,
        max_parallel_tool_calls: 1,
        operation_timeout: None,
        tool_cancel_grace: Duration::from_millis(300),
        compaction: Default::default(),
    }
}

fn echo() -> philo_agent_runtime::ToolDefinition {
    philo_agent_runtime::ToolDefinition::simple(
        "echo",
        "echo",
        philo_agent_runtime::EffectClass::ReadOnly,
    )
}

fn completed_text(text: &str) -> ModelEvent {
    philo_agent_runtime::ModelEvent::Completed {
        blocks: vec![philo_agent_runtime::ModelAssistantBlock::Text {
            text: text.to_owned(),
        }],
    }
}

fn alternating_stream(rounds: usize) -> ModelScript {
    let mut events = Vec::new();
    for index in 0..rounds {
        events.push(Ok(ModelEvent::TextDelta(format!("t{index}"))));
        events.push(Ok(ModelEvent::ReasoningDelta {
            text: format!("r{index}"),
        }));
        events.push(Ok(ModelEvent::UsageUpdated {
            usage: TokenUsage {
                input_tokens: Some(index as u64),
                output_tokens: Some(1),
                cache_read_tokens: None,
                cache_write_tokens: None,
                reasoning_tokens: None,
            },
        }));
    }
    events.push(Ok(completed_text("done")));
    ModelScript::Events(events)
}

fn has_settled(events: &[RuntimeEvent], operation_id: &philo_agent_runtime::OperationId) -> bool {
    events.iter().any(|event| {
        matches!(
            event,
            RuntimeEvent::OperationSettled { operation_id: id, .. } if id == operation_id
        )
    })
}

fn has_accepted(events: &[RuntimeEvent], operation_id: &philo_agent_runtime::OperationId) -> bool {
    events.iter().any(|event| {
        matches!(
            event,
            RuntimeEvent::OperationAccepted { operation_id: id, .. } if id == operation_id
        )
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn alternating_transients_stay_within_coalescer_cap() {
    let model = Arc::new(FakeModel::new([alternating_stream(200)]));
    let sessions = Arc::new(MemorySessionStore::new());
    let bounds = tiny_pipeline_bounds();
    let (handle, sub) = start_with_bounds(
        sessions,
        Arc::new(SequentialIdSource::new()),
        bounds,
    )
    .await;
    let probe = EventProbe::start_paused(sub);
    submit_prompt(
        &handle,
        generation(model, empty_tools(), config()),
        "session",
        "hi",
    )
    .await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let stats = handle.outbound_stats().await;
        assert!(
            stats.transient_len <= stats.transient_cap,
            "transient coalescer grew past cap: {stats:?}"
        );
        assert!(
            stats.reliable_staging_len <= stats.reliable_staging_cap,
            "reliable staging grew past cap: {stats:?}"
        );
        if stats.reliable_staging_len >= stats.reliable_staging_cap.saturating_sub(1) {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let stats = handle.outbound_stats().await;
    assert!(stats.transient_len <= stats.transient_cap);
    assert!(stats.reliable_staging_len <= stats.reliable_staging_cap);

    let extra = handle
        .submit(philo_agent_runtime::OperationSpec {
            session_id: SessionId::new("session"),
            user_message: UserMessage::new("again"),
            generation: generation(
                Arc::new(FakeModel::succeeds(&["later"])),
                empty_tools(),
                config(),
            ),
            service_request_id: None,
        })
        .await;
    assert!(
        matches!(
            extra,
            Err(AdmissionError::Backpressured | AdmissionError::QueueFull)
        ),
        "producer must be backpressured while the outlet is paused, got {extra:?}"
    );
    drop(probe);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accepted_then_settled_once_under_live_drain() {
    let model = Arc::new(FakeModel::succeeds(&["hello"]));
    let sessions = Arc::new(MemorySessionStore::new());
    let (handle, sub) = start_with_bounds(
        sessions,
        Arc::new(SequentialIdSource::new()),
        event_cap_bounds(1),
    )
    .await;
    let probe = EventProbe::start(sub);
    let accepted = submit_prompt(
        &handle,
        generation(model, empty_tools(), config()),
        "session",
        "hi",
    )
    .await;
    let events = probe
        .wait_for(
            |events| has_settled(events, &accepted.operation_id),
            Duration::from_secs(5),
        )
        .await;
    let accepted_count = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                RuntimeEvent::OperationAccepted { operation_id, .. }
                    if operation_id == &accepted.operation_id
            )
        })
        .count();
    let settled_count = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                RuntimeEvent::OperationSettled { operation_id, .. }
                    if operation_id == &accepted.operation_id
            )
        })
        .count();
    assert_eq!(accepted_count, 1);
    assert_eq!(settled_count, 1);
    let accepted_at = events
        .iter()
        .position(|event| has_accepted(std::slice::from_ref(event), &accepted.operation_id))
        .expect("accepted position");
    let settled_at = events
        .iter()
        .position(|event| has_settled(std::slice::from_ref(event), &accepted.operation_id))
        .expect("settled position");
    assert!(accepted_at < settled_at);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_event_receiver_shuts_runtime_down() {
    let model = Arc::new(FakeModel::succeeds(&["hello"]));
    let sessions = Arc::new(MemorySessionStore::new());
    let (handle, sub) = start_with_bounds(
        sessions,
        Arc::new(SequentialIdSource::new()),
        ChannelBounds::default(),
    )
    .await;
    submit_prompt(
        &handle,
        generation(model, empty_tools(), config()),
        "session",
        "hi",
    )
    .await;
    drop(sub);
    wait_until_shutdown_leaves_running(&handle).await;
    let rejected = handle
        .submit(philo_agent_runtime::OperationSpec {
            session_id: SessionId::new("session"),
            user_message: UserMessage::new("again"),
            generation: generation(
                Arc::new(FakeModel::succeeds(&["later"])),
                empty_tools(),
                config(),
            ),
            service_request_id: None,
        })
        .await;
    assert!(
        matches!(
            rejected,
            Err(AdmissionError::ShuttingDown
                | AdmissionError::RuntimeStopped
                | AdmissionError::Backpressured)
        ),
        "submit after sink close must be rejected, got {rejected:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transient_does_not_starve_or_block_terminal() {
    let model = Arc::new(FakeModel::new([
        ModelScript::tool_call(0, Some("call-1"), Some("echo"), &["{}"]),
        ModelScript::text(&["done"]),
    ]));
    let chunks = std::iter::repeat("x").take(512);
    let tools = Arc::new(FakeTool::one(
        echo(),
        FakeToolResult::streaming_success(chunks, "ok"),
    ));
    let sessions = Arc::new(MemorySessionStore::new());
    let (handle, sub) = start_with_bounds(
        sessions,
        Arc::new(SequentialIdSource::new()),
        event_cap_bounds(1),
    )
    .await;
    let probe = EventProbe::start(sub);
    let accepted = handle
        .submit(philo_agent_runtime::OperationSpec {
            session_id: SessionId::new("s"),
            user_message: UserMessage::new("hi"),
            generation: generation(model, tools, config()),
            service_request_id: None,
        })
        .await
        .unwrap();
    let events = probe
        .wait_for(
            |events| has_settled(events, &accepted.operation_id),
            Duration::from_secs(5),
        )
        .await;
    let progress = events.iter().any(|event| {
        matches!(
            event,
            RuntimeEvent::Agent(AgentEvent::ToolExecutionProgress { .. })
        )
    });
    let started = events.iter().any(|event| {
        matches!(
            event,
            RuntimeEvent::Agent(AgentEvent::ToolExecutionStarted { .. })
        )
    });
    assert!(started, "reliable tool start must arrive");
    assert!(progress, "transient progress must get a send slot");
    assert!(has_settled(&events, &accepted.operation_id));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ledger_releases_when_settled_enters_staging() {
    let model = Arc::new(FakeModel::succeeds_sequence(vec![
        vec!["one"],
        vec!["two"],
        vec!["three"],
    ]));
    let sessions = Arc::new(MemorySessionStore::new());
    let (handle, sub) = start_with_bounds(
        sessions,
        Arc::new(SequentialIdSource::new()),
        ChannelBounds {
            command_cap: 8,
            control_cap: 8,
            event_cap: 1,
            queue_max: 4,
            driver_event_budget: 8,
            reliable_staging_cap: 16,
        },
    )
    .await;
    let probe = EventProbe::start_paused(sub);
    let first = submit_prompt(
        &handle,
        generation(model.clone(), empty_tools(), config()),
        "session",
        "one",
    )
    .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let snapshot = handle.snapshot().await;
        if snapshot
            .last_settled
            .iter()
            .any(|settled| settled.operation_id == first.operation_id)
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "first operation did not settle into staging while the consumer was paused"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let second = handle
        .submit(philo_agent_runtime::OperationSpec {
            session_id: SessionId::new("session"),
            user_message: UserMessage::new("two"),
            generation: generation(model.clone(), empty_tools(), config()),
            service_request_id: None,
        })
        .await;
    assert!(
        second.is_ok(),
        "admission must be released after settlement enters staging, got {second:?}"
    );
    drop(probe);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_drain_keeps_reliable_order_under_cap_one() {
    let model = Arc::new(FakeModel::succeeds(&["hello"]));
    let sessions = Arc::new(MemorySessionStore::new());
    let (handle, sub) = start_with_bounds(
        sessions,
        Arc::new(SequentialIdSource::new()),
        event_cap_bounds(1),
    )
    .await;
    let probe = EventProbe::start(sub);
    let accepted = submit_prompt(
        &handle,
        generation(model, empty_tools(), config()),
        "session",
        "hi",
    )
    .await;
    let events = probe
        .wait_for(
            |events| has_settled(events, &accepted.operation_id),
            Duration::from_secs(5),
        )
        .await;
    wait_until_idle(&handle).await;
    let started = events.iter().position(|event| {
        matches!(
            event,
            RuntimeEvent::Agent(AgentEvent::OperationStarted { .. })
        )
    });
    let completed = events.iter().position(|event| {
        matches!(
            event,
            RuntimeEvent::Agent(AgentEvent::AssistantMessageCompleted { .. })
        )
    });
    let settled = events
        .iter()
        .position(|event| has_settled(std::slice::from_ref(event), &accepted.operation_id));
    assert!(started.is_some() && completed.is_some() && settled.is_some());
    assert!(started.unwrap() < completed.unwrap());
    assert!(completed.unwrap() < settled.unwrap());
}
