//! RUNTIME-006: reasoning/usage vocabulary, transient forwarding, and the
//! `#[non_exhaustive]` consumption stance of `AgentEvent`.

mod support;

use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use philo_agent_runtime::{
    AgentEvent, AgentFailureKind, AgentRuntime, GenerationConfig, ModelEvent, OperationOutcome,
    RuntimeConfig, SequentialIdSource, SessionId, TokenUsage, UserMessage,
};
use philo_session::{MemorySessionStore, SessionStore};
use support::fake_model::{FakeModel, ModelScript};

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

fn runtime(model: Arc<FakeModel>, sessions: Arc<dyn SessionStore>) -> AgentRuntime {
    AgentRuntime::new(
        model,
        sessions,
        Arc::new(SequentialIdSource::new()),
        RuntimeConfig {
            system_prompt: "sys".to_owned(),
            model_target: "fake".to_owned(),
            generation: GenerationConfig::default(),
            max_tool_rounds: 0,
            max_parallel_tool_calls: 1,
            operation_timeout: None,
            compaction: Default::default(),
        },
    )
}

fn collect_events(mut handle: philo_agent_runtime::OperationHandle) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    while let Some(event) = block_on(handle.next_event()) {
        events.push(event);
    }
    events
}

fn usage(input: u64, output: u64) -> TokenUsage {
    TokenUsage {
        input_tokens: Some(input),
        output_tokens: Some(output),
        ..TokenUsage::default()
    }
}

#[test]
fn reasoning_deltas_are_forwarded_before_text_and_never_persisted() {
    let model = Arc::new(FakeModel::new([
        ModelScript::text(&["answer"]).with_reasoning(&["think ", "hard"])
    ]));
    let sessions = Arc::new(MemorySessionStore::new());
    let handle = block_on(
        runtime(model, sessions.clone()).prompt(SessionId::new("s"), UserMessage::new("hi")),
    )
    .unwrap();
    assert!(matches!(
        block_on(handle.wait()),
        OperationOutcome::Succeeded { assistant } if assistant.content() == "answer"
    ));

    let events = collect_events(handle);
    let reasoning: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ReasoningDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(reasoning, ["think ", "hard"]);

    // Order: after ModelCallStarted, before the assembled completion.
    let call_started = events
        .iter()
        .position(|event| matches!(event, AgentEvent::ModelCallStarted { .. }))
        .unwrap();
    let first_reasoning = events
        .iter()
        .position(|event| matches!(event, AgentEvent::ReasoningDelta { .. }))
        .unwrap();
    let first_text = events
        .iter()
        .position(|event| matches!(event, AgentEvent::TextDelta { .. }))
        .unwrap();
    let completed = events
        .iter()
        .position(|event| matches!(event, AgentEvent::AssistantMessageCompleted { .. }))
        .unwrap();
    assert!(call_started < first_reasoning);
    assert!(first_reasoning < first_text);
    assert!(first_text < completed);

    // Reasoning joins neither the assembled output nor the Session.
    let view = block_on(sessions.context_view(&philo_session::SessionId::new("s"))).unwrap();
    assert_eq!(view.revision(), philo_session::SessionRevision::new(2));
    assert_eq!(view.messages().len(), 2, "user and assistant only");
    assert!(matches!(
        &view.messages()[1],
        philo_session::ContextMessage::Assistant { content } if content == "answer"
    ));
}

#[test]
fn usage_updates_are_forwarded_with_the_last_one_winning() {
    let model = Arc::new(FakeModel::new([ModelScript::Events(vec![
        Ok(ModelEvent::UsageUpdated {
            usage: usage(10, 0),
        }),
        Ok(ModelEvent::TextDelta("ok".to_owned())),
        Ok(ModelEvent::UsageUpdated {
            usage: usage(10, 7),
        }),
        Ok(ModelEvent::Completed),
    ])]));
    let sessions = Arc::new(MemorySessionStore::new());
    let handle = block_on(
        runtime(model, sessions.clone()).prompt(SessionId::new("s"), UserMessage::new("hi")),
    )
    .unwrap();
    assert!(matches!(
        block_on(handle.wait()),
        OperationOutcome::Succeeded { .. }
    ));

    let events = collect_events(handle);
    let usages: Vec<TokenUsage> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ModelUsageUpdated { usage, .. } => Some(*usage),
            _ => None,
        })
        .collect();
    assert_eq!(usages, vec![usage(10, 0), usage(10, 7)]);
    assert_eq!(usages.last().unwrap().total_tokens(), Some(17));

    // Usage never enters the Session.
    let view = block_on(sessions.context_view(&philo_session::SessionId::new("s"))).unwrap();
    assert_eq!(view.revision(), philo_session::SessionRevision::new(2));
    assert_eq!(view.messages().len(), 2);
}

#[test]
fn streams_without_new_events_keep_the_baseline_behavior() {
    let model = Arc::new(FakeModel::succeeds(&["plain"]));
    let handle = block_on(
        runtime(model, Arc::new(MemorySessionStore::new()))
            .prompt(SessionId::new("s"), UserMessage::new("hi")),
    )
    .unwrap();
    assert!(matches!(
        block_on(handle.wait()),
        OperationOutcome::Succeeded { assistant } if assistant.content() == "plain"
    ));
    let events = collect_events(handle);
    assert!(!events.iter().any(|event| matches!(
        event,
        AgentEvent::ReasoningDelta { .. } | AgentEvent::ModelUsageUpdated { .. }
    )));
}

#[test]
fn reasoning_after_completed_is_an_invalid_stream() {
    let model = Arc::new(FakeModel::new([ModelScript::Events(vec![
        Ok(ModelEvent::TextDelta("ok".to_owned())),
        Ok(ModelEvent::Completed),
        Ok(ModelEvent::ReasoningDelta {
            text: "late".to_owned(),
        }),
    ])]));
    let handle = block_on(
        runtime(model, Arc::new(MemorySessionStore::new()))
            .prompt(SessionId::new("s"), UserMessage::new("hi")),
    )
    .unwrap();
    assert!(matches!(
        block_on(handle.wait()),
        OperationOutcome::Failed { failure, .. }
            if failure.kind() == AgentFailureKind::InvalidModelOutput
    ));
}

#[test]
fn usage_after_completed_is_an_invalid_stream() {
    let model = Arc::new(FakeModel::new([ModelScript::Events(vec![
        Ok(ModelEvent::TextDelta("ok".to_owned())),
        Ok(ModelEvent::Completed),
        Ok(ModelEvent::UsageUpdated { usage: usage(1, 1) }),
    ])]));
    let handle = block_on(
        runtime(model, Arc::new(MemorySessionStore::new()))
            .prompt(SessionId::new("s"), UserMessage::new("hi")),
    )
    .unwrap();
    assert!(matches!(
        block_on(handle.wait()),
        OperationOutcome::Failed { failure, .. }
            if failure.kind() == AgentFailureKind::InvalidModelOutput
    ));
}

/// M7-006: this integration test crate is an external consumer, so this
/// wildcard-armed match is the compile-time proof of the
/// `#[non_exhaustive]` consumption stance.
#[test]
fn external_consumers_match_events_with_a_wildcard_arm() {
    let model = Arc::new(FakeModel::new([ModelScript::text(&["done"])
        .with_reasoning(&["r"])
        .with_usage(usage(2, 3))]));
    let handle = block_on(
        runtime(model, Arc::new(MemorySessionStore::new()))
            .prompt(SessionId::new("s"), UserMessage::new("hi")),
    )
    .unwrap();
    let mut kinds = Vec::new();
    for event in collect_events(handle) {
        let kind = match event {
            AgentEvent::OperationStarted { .. } => "operation",
            AgentEvent::TurnStarted { .. } => "turn",
            AgentEvent::ReasoningDelta { .. } => "reasoning",
            AgentEvent::ModelUsageUpdated { .. } => "usage",
            AgentEvent::TextDelta { .. } => "text",
            AgentEvent::OperationSettled { .. } => "settled",
            _ => "other",
        };
        kinds.push(kind);
    }
    assert!(kinds.contains(&"reasoning"));
    assert!(kinds.contains(&"usage"));
    assert!(kinds.contains(&"settled"));
}
