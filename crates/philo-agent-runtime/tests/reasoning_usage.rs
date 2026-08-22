//! RUNTIME-006: reasoning/usage vocabulary, transient forwarding, and the
//! `#[non_exhaustive]` consumption stance of `AgentEvent`.

mod support;

use std::sync::Arc;

use philo_agent_runtime::{
    AgentEvent, AgentFailureKind, GenerationConfig, ModelAssistantBlock, ModelEvent,
    OperationOutcome, RuntimeConfig, TokenUsage,
};
use philo_session::{MemorySessionStore, SessionStore};
use support::fake_model::{FakeModel, ModelScript};
use support::runtime::{Harness, empty_tools};

fn config() -> RuntimeConfig {
    RuntimeConfig {
        system_prompt: "sys".to_owned(),
        model_target: "fake".to_owned(),
        generation: GenerationConfig::default(),
        max_tool_rounds: 0,
        max_parallel_tool_calls: 1,
        operation_timeout: None,
        tool_cancel_grace: std::time::Duration::from_millis(300),
        compaction: Default::default(),
        recovery: Default::default(),
    }
}

async fn run(
    model: Arc<FakeModel>,
) -> (Vec<AgentEvent>, OperationOutcome, Arc<MemorySessionStore>) {
    let sessions = Arc::new(MemorySessionStore::new());
    let mut harness =
        Harness::launch_default(model, sessions.clone(), empty_tools(), config()).await;
    let (events, outcome) = harness.run("s", "hi").await;
    (events, outcome, sessions)
}

fn usage(input: u64, output: u64) -> TokenUsage {
    TokenUsage {
        input_tokens: Some(input),
        output_tokens: Some(output),
        ..TokenUsage::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reasoning_deltas_are_forwarded_before_text_and_never_persisted() {
    let model = Arc::new(FakeModel::new([
        ModelScript::text(&["answer"]).with_reasoning(&["think ", "hard"])
    ]));
    let sessions = Arc::new(MemorySessionStore::new());
    let mut harness =
        Harness::launch_default(model, sessions.clone(), empty_tools(), config()).await;
    let (events, outcome) = harness.run("s", "hi").await;
    assert!(matches!(
        outcome,
        OperationOutcome::Succeeded { assistant } if assistant.content() == "answer"
    ));

    let reasoning: String = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ReasoningDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(reasoning, "think hard");

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

    let view = sessions
        .context_view(&philo_session::SessionId::new("s"))
        .await
        .unwrap();
    assert_eq!(view.revision(), philo_session::SessionRevision::new(2));
    assert_eq!(view.messages().len(), 2, "user and assistant only");
    assert!(matches!(
        &view.messages()[1],
        philo_session::ContextMessage::Assistant { blocks }
            if matches!(
                blocks.as_slice(),
                [philo_session::SessionAssistantBlock::Text { text }] if text == "answer"
            )
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_updates_are_forwarded_with_the_last_one_winning() {
    let model = Arc::new(FakeModel::new([ModelScript::Events(vec![
        Ok(ModelEvent::UsageUpdated {
            usage: usage(10, 0),
        }),
        Ok(ModelEvent::TextDelta("ok".to_owned())),
        Ok(ModelEvent::UsageUpdated {
            usage: usage(10, 7),
        }),
        Ok(ModelEvent::Completed {
            blocks: vec![ModelAssistantBlock::Text {
                text: "ok".to_owned(),
            }],
        }),
    ])]));
    let sessions = Arc::new(MemorySessionStore::new());
    let mut harness =
        Harness::launch_default(model, sessions.clone(), empty_tools(), config()).await;
    let (events, outcome) = harness.run("s", "hi").await;
    assert!(matches!(outcome, OperationOutcome::Succeeded { .. }));

    let usages: Vec<TokenUsage> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ModelUsageUpdated { usage, .. } => Some(*usage),
            _ => None,
        })
        .collect();
    assert_eq!(usages.last().copied(), Some(usage(10, 7)));
    assert_eq!(usages.last().unwrap().total_tokens(), Some(17));

    let view = sessions
        .context_view(&philo_session::SessionId::new("s"))
        .await
        .unwrap();
    assert_eq!(view.revision(), philo_session::SessionRevision::new(2));
    assert_eq!(view.messages().len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streams_without_new_events_keep_the_baseline_behavior() {
    let model = Arc::new(FakeModel::succeeds(&["plain"]));
    let (events, outcome, _) = run(model).await;
    assert!(matches!(
        outcome,
        OperationOutcome::Succeeded { assistant } if assistant.content() == "plain"
    ));
    assert!(!events.iter().any(|event| matches!(
        event,
        AgentEvent::ReasoningDelta { .. } | AgentEvent::ModelUsageUpdated { .. }
    )));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reasoning_after_completed_is_an_invalid_stream() {
    let model = Arc::new(FakeModel::new([ModelScript::Events(vec![
        Ok(ModelEvent::TextDelta("ok".to_owned())),
        Ok(ModelEvent::Completed {
            blocks: vec![ModelAssistantBlock::Text {
                text: "ok".to_owned(),
            }],
        }),
        Ok(ModelEvent::ReasoningDelta {
            text: "late".to_owned(),
        }),
    ])]));
    let (_, outcome, _) = run(model).await;
    assert!(matches!(
        outcome,
        OperationOutcome::Failed { failure, .. }
            if failure.kind() == AgentFailureKind::InvalidModelOutput
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_after_completed_is_an_invalid_stream() {
    let model = Arc::new(FakeModel::new([ModelScript::Events(vec![
        Ok(ModelEvent::TextDelta("ok".to_owned())),
        Ok(ModelEvent::Completed {
            blocks: vec![ModelAssistantBlock::Text {
                text: "ok".to_owned(),
            }],
        }),
        Ok(ModelEvent::UsageUpdated { usage: usage(1, 1) }),
    ])]));
    let (_, outcome, _) = run(model).await;
    assert!(matches!(
        outcome,
        OperationOutcome::Failed { failure, .. }
            if failure.kind() == AgentFailureKind::InvalidModelOutput
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn external_consumers_match_events_with_a_wildcard_arm() {
    let model = Arc::new(FakeModel::new([ModelScript::text(&["done"])
        .with_reasoning(&["r"])
        .with_usage(usage(2, 3))]));
    let (events, _, _) = run(model).await;
    let mut kinds = Vec::new();
    for event in events {
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
