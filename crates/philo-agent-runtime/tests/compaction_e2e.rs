//! INTEGRATION-013: compaction across Runtime and the durable JSONL
//! backend, including append-only persistence, restart projection parity,
//! exact post-compaction model context, and summary failure recovery.

mod support;

use std::future::Future;
use std::path::PathBuf;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::task::{Context, Poll, Waker};

use philo_agent_runtime::{
    AgentEvent, AgentRuntime, CompactionConfig, GenerationConfig, ModelMessage, OperationOutcome,
    RuntimeConfig, SequentialIdSource, SessionId, UserMessage, UserPart,
};
use philo_session::{
    ContextMessage, OperationId, OperationOutcome as StoredOperationOutcome, SessionEntryKind,
    SessionStore, SessionTransaction, SessionUserPart, TurnId, TurnOutcome,
};
use philo_session_jsonl::JsonlSessionStore;
use support::fake_model::{FakeModel, ModelScript};

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return output;
        }
        std::thread::yield_now();
    }
}

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "philo-m13-integration-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("create temp root");
        Self { path }
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn session_id() -> SessionId {
    SessionId::new("m13-integration")
}

fn stored_session_id() -> philo_session::SessionId {
    philo_session::SessionId::new(session_id().as_str())
}

fn log_path(root: &TempRoot) -> PathBuf {
    root.path.join("s-m13-integration").join("log.jsonl")
}

fn seed_turn(store: &dyn SessionStore, index: usize) -> String {
    let revision = block_on(store.context_view(&stored_session_id()))
        .expect("seed context")
        .revision();
    let operation_id = OperationId::new(format!("seed-operation-{index}"));
    let turn_id = TurnId::new(format!("seed-turn-{index}"));
    let commit = block_on(store.commit(SessionTransaction::linear(
        stored_session_id(),
        revision,
        vec![
            SessionEntryKind::OperationStarted {
                operation_id: operation_id.clone(),
            },
            SessionEntryKind::TurnStarted {
                operation_id: operation_id.clone(),
                turn_id: turn_id.clone(),
            },
            SessionEntryKind::UserMessage {
                turn_id: turn_id.clone(),
                parts: SessionUserPart::text_parts(format!("seed question {index}")),
            },
            SessionEntryKind::AssistantMessage {
                turn_id: turn_id.clone(),
                content: format!("seed answer {index}"),
            },
            SessionEntryKind::TurnTerminated {
                turn_id,
                outcome: TurnOutcome::Succeeded,
            },
            SessionEntryKind::OperationSettled {
                operation_id,
                outcome: StoredOperationOutcome::Succeeded,
            },
        ],
    )))
    .expect("seed turn");
    commit.current_leaf().as_str().to_owned()
}

fn config() -> RuntimeConfig {
    RuntimeConfig {
        system_prompt: "system".to_owned(),
        model_target: "fake".to_owned(),
        generation: GenerationConfig::default(),
        max_tool_rounds: 0,
        max_parallel_tool_calls: 1,
        operation_timeout: None,
        compaction: CompactionConfig {
            context_budget: Some(1),
            auto_threshold: 0.8,
            keep_recent_turns: 1,
            estimate_bytes_per_token: 1,
        },
    }
}

fn text_part(text: &str) -> Vec<UserPart> {
    vec![UserPart::Text(text.to_owned())]
}

#[test]
fn automatic_compaction_is_append_only_and_reopens_to_the_exact_model_projection() {
    let root = TempRoot::new();
    let store = Arc::new(JsonlSessionStore::open(&root.path).expect("open store"));
    seed_turn(store.as_ref(), 1);
    let expected_boundary = seed_turn(store.as_ref(), 2);
    seed_turn(store.as_ref(), 3);
    let before = std::fs::read_to_string(log_path(&root)).expect("read original log");

    let model = Arc::new(FakeModel::new([
        ModelScript::summary("durable summary"),
        ModelScript::text(&["continued answer"]),
    ]));
    let agent = AgentRuntime::new(
        model.clone(),
        store.clone(),
        Arc::new(SequentialIdSource::new()),
        config(),
    );
    let mut handle = block_on(agent.prompt(session_id(), UserMessage::new("continue")))
        .expect("prompt accepted");
    let mut events = Vec::new();
    while let Some(event) = block_on(handle.next_event()) {
        events.push(event);
    }
    assert!(matches!(
        block_on(handle.wait()),
        OperationOutcome::Succeeded { ref assistant }
            if assistant.content() == "continued answer"
    ));
    assert!(events.iter().any(|event| {
        matches!(
            event,
            AgentEvent::ContextCompactionCompleted { covers_up_to }
                if covers_up_to == &expected_boundary
        )
    }));

    let calls = model.calls();
    assert_eq!(calls.len(), 2, "one summary call followed by one turn call");
    assert_eq!(
        calls[1].messages,
        vec![
            ModelMessage::System {
                content: "system".to_owned(),
            },
            ModelMessage::Summary {
                text: "durable summary".to_owned(),
            },
            ModelMessage::User {
                parts: text_part("seed question 3"),
            },
            ModelMessage::Assistant {
                content: "seed answer 3".to_owned(),
            },
            ModelMessage::User {
                parts: text_part("continue"),
            },
        ],
        "the turn sees exactly system + durable summary + retained tail + new input"
    );

    let after = std::fs::read_to_string(log_path(&root)).expect("read compacted log");
    assert!(
        after.as_bytes().starts_with(before.as_bytes()),
        "compaction must preserve every original byte and append new facts"
    );
    let original_line_count = before.lines().count();
    let lines = after.lines().collect::<Vec<_>>();
    assert_eq!(
        lines.len(),
        original_line_count + 3,
        "one compaction transaction and two normal turn transactions are appended"
    );
    let compaction_line = lines[original_line_count];
    assert_eq!(compaction_line.matches(r#""kind":{"type":""#).count(), 1);
    assert_eq!(compaction_line.matches(r#""type":"compaction""#).count(), 1);
    assert!(compaction_line.contains(r#""summary":"durable summary""#));
    assert!(compaction_line.contains(&format!(r#""covers_up_to":"{expected_boundary}""#)));

    drop(handle);
    drop(agent);
    drop(store);
    let reopened = JsonlSessionStore::open(&root.path).expect("reopen store");
    let view = block_on(reopened.context_view(&stored_session_id())).expect("replayed context");
    assert_eq!(
        view.messages(),
        [
            ContextMessage::Summary {
                text: "durable summary".to_owned(),
            },
            ContextMessage::User {
                parts: SessionUserPart::text_parts("seed question 3"),
            },
            ContextMessage::Assistant {
                content: "seed answer 3".to_owned(),
            },
            ContextMessage::User {
                parts: SessionUserPart::text_parts("continue"),
            },
            ContextMessage::Assistant {
                content: "continued answer".to_owned(),
            },
        ],
        "restart projection must equal the model-visible durable context"
    );
}

#[test]
fn summary_model_failure_warns_without_blocking_the_turn_or_leaving_a_trace() {
    let root = TempRoot::new();
    let store = Arc::new(JsonlSessionStore::open(&root.path).expect("open store"));
    seed_turn(store.as_ref(), 1);
    seed_turn(store.as_ref(), 2);
    let before = std::fs::read_to_string(log_path(&root)).expect("read original log");
    let model = Arc::new(FakeModel::new([
        ModelScript::error("summary provider unavailable"),
        ModelScript::text(&["normal answer"]),
    ]));
    let agent = AgentRuntime::new(
        model,
        store.clone(),
        Arc::new(SequentialIdSource::new()),
        RuntimeConfig {
            compaction: CompactionConfig {
                keep_recent_turns: 0,
                ..config().compaction
            },
            ..config()
        },
    );

    let mut handle = block_on(agent.prompt(session_id(), UserMessage::new("continue")))
        .expect("prompt accepted");
    let mut events = Vec::new();
    while let Some(event) = block_on(handle.next_event()) {
        events.push(event);
    }
    assert!(matches!(
        block_on(handle.wait()),
        OperationOutcome::Succeeded { ref assistant } if assistant.content() == "normal answer"
    ));
    let failed = events.iter().position(|event| {
        matches!(
            event,
            AgentEvent::ContextCompactionFailed { message }
                if message.contains("summary provider unavailable")
        )
    });
    let turn_started = events
        .iter()
        .position(|event| matches!(event, AgentEvent::TurnStarted { .. }));
    assert!(failed.is_some_and(|failed| turn_started.is_some_and(|turn| failed < turn)));

    let after = std::fs::read_to_string(log_path(&root)).expect("read final log");
    assert!(after.as_bytes().starts_with(before.as_bytes()));
    assert!(!after.contains(r#""type":"compaction""#));
    let view = block_on(store.context_view(&stored_session_id())).expect("context");
    assert!(view.latest_compaction_boundary().is_none());
}
