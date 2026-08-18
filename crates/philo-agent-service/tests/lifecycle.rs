//! Exactly-once lifecycle facts: command replies are not a second producer.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use philo_agent_runtime::{
    AgentRuntime, ChannelBounds, EpochEndReason, MaintenanceId, MaintenanceResult,
    ModelAssistantBlock, ModelCallSnapshot, ModelError, ModelEvent, ModelEventStream, ModelPort,
    OperationId, OperationStatus, RuntimeConfig, RuntimeDeps, RuntimeEpoch, RuntimeFuture,
    RuntimeGeneration, SequentialIdSource, SessionId, SettlementDurability, SettlementRevision,
    ShutdownMode, ToolRegistry, TurnId,
};
use philo_agent_service::testing::{FakeAssembler, start_test_service};
use philo_agent_service::{
    AgentService, CommandDispatch, FrontendClient, FrontendCommand, FrontendMaintenancePhase,
    FrontendOperationEvent, FrontendUpdate, FrontendUpdateKind, RecvOutcome, RuntimeHandle,
    ServiceDeps, ServiceHealth, start,
};
use philo_session::MemorySessionStore;
use tokio::sync::Notify;

async fn recv_matching(
    client: &FrontendClient,
    mut pred: impl FnMut(&FrontendUpdate) -> bool,
) -> FrontendUpdate {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut seen = Vec::new();
    loop {
        match client
            .recv_until_async(Instant::now() + Duration::from_millis(50))
            .await
        {
            RecvOutcome::Update(update) if pred(&update) => return update,
            RecvOutcome::Update(update) => seen.push(format!("{:?}", update.kind)),
            RecvOutcome::Timeout if Instant::now() < deadline => continue,
            RecvOutcome::Timeout => panic!("timed out waiting for frontend update; seen={seen:?}"),
            RecvOutcome::Disconnected => {
                panic!("frontend disconnected while waiting; seen={seen:?}")
            }
        }
    }
}

async fn drain_briefly(client: &FrontendClient) -> Vec<FrontendUpdate> {
    let until = Instant::now() + Duration::from_millis(80);
    let mut updates = Vec::new();
    while Instant::now() < until {
        match client
            .recv_until_async(Instant::now() + Duration::from_millis(20))
            .await
        {
            RecvOutcome::Update(update) => updates.push(update),
            RecvOutcome::Timeout => break,
            RecvOutcome::Disconnected => panic!("disconnected"),
        }
    }
    updates
}

async fn collect_until(
    client: &FrontendClient,
    mut done: impl FnMut(&[FrontendUpdate]) -> bool,
) -> Vec<FrontendUpdate> {
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut updates = Vec::new();
    loop {
        match client
            .recv_until_async(Instant::now() + Duration::from_millis(50))
            .await
        {
            RecvOutcome::Update(update) => {
                updates.push(update);
                if done(&updates) {
                    return updates;
                }
            }
            RecvOutcome::Timeout if Instant::now() < deadline => continue,
            RecvOutcome::Timeout => {
                panic!("timed out collecting frontend updates; seen={updates:?}")
            }
            RecvOutcome::Disconnected => {
                panic!("frontend disconnected while collecting; seen={updates:?}")
            }
        }
    }
}

async fn load_session(client: &FrontendClient, session_id: &str) {
    assert!(matches!(
        client.try_command(FrontendCommand::LoadSession {
            session_id: session_id.into(),
        }),
        CommandDispatch::Enqueued(_)
    ));
    recv_matching(client, |update| {
        matches!(update.kind, FrontendUpdateKind::SessionLoaded { .. })
    })
    .await;
}

fn submit(client: &FrontendClient, draft: &str) -> philo_agent_service::FrontendRequestId {
    match client.try_command(FrontendCommand::Submit {
        draft: draft.into(),
        attachments: Vec::new(),
    }) {
        CommandDispatch::Enqueued(id) => id,
        other => panic!("submit enqueue {other:?}"),
    }
}

fn accepted_ids(updates: &[FrontendUpdate]) -> Vec<String> {
    updates
        .iter()
        .filter_map(|update| match &update.kind {
            FrontendUpdateKind::OperationAccepted { operation_id, .. } => {
                Some(operation_id.clone())
            }
            _ => None,
        })
        .collect()
}

fn settled(updates: &[FrontendUpdate]) -> Vec<(String, String, String)> {
    updates
        .iter()
        .filter_map(|update| match &update.kind {
            FrontendUpdateKind::OperationEvent(FrontendOperationEvent::OperationSettled {
                operation_id,
                session_id,
                status,
                ..
            }) => Some((operation_id.clone(), session_id.clone(), status.clone())),
            _ => None,
        })
        .collect()
}

fn submit_accepted_count(updates: &[FrontendUpdate]) -> usize {
    updates
        .iter()
        .filter(|update| matches!(update.kind, FrontendUpdateKind::SubmitAccepted { .. }))
        .count()
}

fn maintenance_phases(updates: &[FrontendUpdate]) -> Vec<FrontendMaintenancePhase> {
    updates
        .iter()
        .filter_map(|update| match &update.kind {
            FrontendUpdateKind::MaintenanceChanged(maintenance) => Some(maintenance.phase.clone()),
            _ => None,
        })
        .collect()
}

struct CompletingModel {
    text: String,
}

impl ModelPort for CompletingModel {
    fn start<'a>(
        &'a self,
        _request: ModelCallSnapshot,
    ) -> RuntimeFuture<'a, Result<Box<dyn ModelEventStream>, ModelError>> {
        let text = self.text.clone();
        Box::pin(async move {
            Ok(Box::new(ScriptedStream {
                events: VecDeque::from([
                    Ok(ModelEvent::TextDelta(text.clone())),
                    Ok(ModelEvent::Completed {
                        blocks: vec![ModelAssistantBlock::Text { text }],
                    }),
                ]),
            }) as Box<dyn ModelEventStream>)
        })
    }
}

struct HeldModel {
    hold: Arc<Notify>,
}

impl ModelPort for HeldModel {
    fn start<'a>(
        &'a self,
        _request: ModelCallSnapshot,
    ) -> RuntimeFuture<'a, Result<Box<dyn ModelEventStream>, ModelError>> {
        let hold = self.hold.clone();
        Box::pin(async move {
            Ok(Box::new(HeldStream { hold, first: true }) as Box<dyn ModelEventStream>)
        })
    }
}

struct ScriptedStream {
    events: VecDeque<Result<ModelEvent, ModelError>>,
}

impl ModelEventStream for ScriptedStream {
    fn next<'a>(&'a mut self) -> RuntimeFuture<'a, Option<Result<ModelEvent, ModelError>>> {
        let event = self.events.pop_front();
        Box::pin(async move { event })
    }
}

struct HeldStream {
    hold: Arc<Notify>,
    first: bool,
}

impl ModelEventStream for HeldStream {
    fn next<'a>(&'a mut self) -> RuntimeFuture<'a, Option<Result<ModelEvent, ModelError>>> {
        if self.first {
            self.first = false;
            return Box::pin(async move { Some(Ok(ModelEvent::TextDelta("partial".into()))) });
        }
        let hold = self.hold.clone();
        Box::pin(async move {
            hold.notified().await;
            None
        })
    }
}

fn start_real_runtime_service(
    model: Arc<dyn ModelPort>,
) -> (AgentService, FrontendClient, RuntimeHandle) {
    let sessions = Arc::new(MemorySessionStore::new());
    let generation = Arc::new(RuntimeGeneration {
        generation_id: philo_agent_runtime::GenerationId::new("lifecycle-generation"),
        model,
        tools: Arc::new(ToolRegistry::empty()),
        runtime_config: RuntimeConfig {
            system_prompt: "sys".into(),
            model_target: "fake".into(),
            max_tool_rounds: 1,
            max_parallel_tool_calls: 1,
            ..RuntimeConfig::default()
        },
        display: philo_agent_runtime::GenerationDisplay {
            model_name: "fake".into(),
        },
    });
    let parts = AgentRuntime::start(RuntimeDeps {
        sessions: sessions.clone(),
        ids: Arc::new(SequentialIdSource::new()),
        bounds: ChannelBounds::default(),
    })
    .expect("start runtime");
    let handle = parts.handle.clone();
    let (service, client) = start(ServiceDeps {
        runtime: parts.handle,
        subscription: parts.events,
        sessions,
        assembler: Arc::new(FakeAssembler::new()),
        initial_generation: generation,
    });
    (service, client, handle)
}

#[tokio::test(flavor = "multi_thread")]
async fn submit_reply_does_not_emit_operation_accepted() {
    let (service, client, runtime) = start_test_service();
    load_session(&client, "sess-1").await;

    let hold = runtime.hold_children();
    let request_id = submit(&client, "hello");
    runtime.wait_child_started(1).await;
    let early = drain_briefly(&client).await;
    assert!(
        accepted_ids(&early).is_empty(),
        "no OperationAccepted before runtime.submit returns: {early:?}"
    );
    assert_eq!(submit_accepted_count(&early), 0);

    hold.release();
    let accepted = recv_matching(&client, |update| {
        update.request_id == Some(request_id)
            && matches!(update.kind, FrontendUpdateKind::SubmitAccepted { .. })
    })
    .await;
    match &accepted.kind {
        FrontendUpdateKind::SubmitAccepted { operation_id, .. } => {
            assert_eq!(operation_id, "op-1");
        }
        other => panic!("{other:?}"),
    }
    let extras = drain_briefly(&client).await;
    assert!(
        accepted_ids(&extras).is_empty(),
        "command reply must not publish OperationAccepted: {extras:?}"
    );

    runtime.emit_operation_accepted(
        OperationId::new("op-1"),
        SessionId::new("sess-1"),
        TurnId::new("turn-1"),
    );
    let lifecycle = recv_matching(&client, |update| {
        matches!(update.kind, FrontendUpdateKind::OperationAccepted { .. })
    })
    .await;
    assert_eq!(lifecycle.request_id, None);
    match &lifecycle.kind {
        FrontendUpdateKind::OperationAccepted {
            operation_id,
            session_id,
            turn_id,
        } => {
            assert_eq!(operation_id, "op-1");
            assert_eq!(session_id, "sess-1");
            assert_eq!(turn_id, "turn-1");
        }
        other => panic!("{other:?}"),
    }
    let more = drain_briefly(&client).await;
    assert!(
        accepted_ids(&more).is_empty(),
        "exactly one OperationAccepted: {more:?}"
    );
    drop(service);
}

#[tokio::test(flavor = "multi_thread")]
async fn settled_only_from_runtime_event() {
    let (service, client, runtime) = start_test_service();
    load_session(&client, "sess-1").await;
    let request_id = submit(&client, "hello");
    recv_matching(&client, |update| {
        update.request_id == Some(request_id)
            && matches!(update.kind, FrontendUpdateKind::SubmitAccepted { .. })
    })
    .await;
    let extras = drain_briefly(&client).await;
    assert!(
        settled(&extras).is_empty(),
        "submit reply must not settle: {extras:?}"
    );

    runtime.emit_operation_settled(
        OperationId::new("op-1"),
        SessionId::new("sess-1"),
        OperationStatus::Succeeded,
        SettlementDurability::Confirmed,
        SettlementRevision::Unchanged,
    );
    let first = recv_matching(&client, |update| {
        matches!(
            update.kind,
            FrontendUpdateKind::OperationEvent(FrontendOperationEvent::OperationSettled { .. })
        )
    })
    .await;
    match &first.kind {
        FrontendUpdateKind::OperationEvent(FrontendOperationEvent::OperationSettled {
            operation_id,
            session_id,
            ..
        }) => {
            assert_eq!(operation_id, "op-1");
            assert_eq!(session_id, "sess-1");
        }
        other => panic!("{other:?}"),
    }
    let more = drain_briefly(&client).await;
    assert!(
        settled(&more).is_empty(),
        "exactly one settlement from one event: {more:?}"
    );
    drop(service);
}

#[tokio::test(flavor = "multi_thread")]
async fn maintenance_reply_is_not_lifecycle() {
    let (service, client, runtime) = start_test_service();
    load_session(&client, "sess-1").await;
    let request_id = match client.try_command(FrontendCommand::StartCompaction {
        session_id: "sess-1".into(),
    }) {
        CommandDispatch::Enqueued(id) => id,
        other => panic!("{other:?}"),
    };
    recv_matching(&client, |update| {
        update.request_id == Some(request_id)
            && matches!(update.kind, FrontendUpdateKind::CompactionAccepted { .. })
    })
    .await;
    let extras = drain_briefly(&client).await;
    assert!(
        maintenance_phases(&extras).is_empty(),
        "compaction reply must not emit MaintenanceChanged: {extras:?}"
    );

    runtime.emit_maintenance_accepted(MaintenanceId::new("maint-1"), SessionId::new("sess-1"));
    recv_matching(&client, |update| {
        matches!(
            update.kind,
            FrontendUpdateKind::MaintenanceChanged(ref maintenance)
                if maintenance.phase == FrontendMaintenancePhase::Accepted
        )
    })
    .await;

    runtime.emit_maintenance_settled(
        MaintenanceId::new("maint-1"),
        SessionId::new("sess-1"),
        MaintenanceResult::Cancelled,
    );
    recv_matching(&client, |update| {
        matches!(
            update.kind,
            FrontendUpdateKind::MaintenanceChanged(ref maintenance)
                if maintenance.phase == FrontendMaintenancePhase::Cancelled
        )
    })
    .await;
    let more = drain_briefly(&client).await;
    assert!(
        maintenance_phases(&more).is_empty(),
        "maintenance accepted/settled each once: {more:?}"
    );
    drop(service);
}

#[tokio::test(flavor = "multi_thread")]
async fn epoch_diagnostic_emits_no_settlement() {
    let (service, client, runtime) = start_test_service();
    runtime.emit_epoch_ended(
        RuntimeEpoch::new("epoch-x"),
        EpochEndReason::CoordinatorFault,
        2,
    );
    recv_matching(&client, |update| {
        matches!(
            update.kind,
            FrontendUpdateKind::ServiceHealthChanged {
                health: ServiceHealth::RuntimeEpochEnded { .. }
            }
        )
    })
    .await;
    let extras = drain_briefly(&client).await;
    assert!(
        settled(&extras).is_empty(),
        "EpochEnded must not synthesize settlements: {extras:?}"
    );
    drop(service);
}

#[tokio::test(flavor = "multi_thread")]
async fn fake_forwards_duplicate_settled_events() {
    let (service, client, runtime) = start_test_service();
    for _ in 0..2 {
        runtime.emit_operation_settled(
            OperationId::new("op-dup"),
            SessionId::new("sess-1"),
            OperationStatus::Succeeded,
            SettlementDurability::Confirmed,
            SettlementRevision::Unchanged,
        );
    }
    let first = recv_matching(&client, |update| {
        !settled(std::slice::from_ref(update)).is_empty()
    })
    .await;
    let second = recv_matching(&client, |update| {
        !settled(std::slice::from_ref(update)).is_empty()
    })
    .await;
    assert_eq!(settled(&[first, second]).len(), 2);
    drop(service);
}

#[tokio::test(flavor = "multi_thread")]
async fn real_submit_counts_accepted_and_settled_once() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let (service, client, _handle) =
            start_real_runtime_service(Arc::new(CompletingModel { text: "ok".into() }));
        load_session(&client, "sess-1").await;
        let request_id = submit(&client, "hello");
        let updates = collect_until(&client, |updates| {
            submit_accepted_count(updates) >= 1 && settled(updates).len() >= 1
        })
        .await;
        assert_eq!(submit_accepted_count(&updates), 1);
        let accepted = accepted_ids(&updates);
        assert_eq!(
            accepted.len(),
            1,
            "accepted={accepted:?} updates={updates:?}"
        );
        let terminals = settled(&updates);
        assert_eq!(
            terminals.len(),
            1,
            "settled={terminals:?} updates={updates:?}"
        );
        assert_eq!(terminals[0].0, accepted[0]);
        assert_eq!(terminals[0].1, "sess-1");
        assert_eq!(terminals[0].2, "Succeeded");
        assert!(updates.iter().any(|update| {
            update.request_id == Some(request_id)
                && matches!(update.kind, FrontendUpdateKind::SubmitAccepted { .. })
        }));
        let extra = drain_briefly(&client).await;
        assert!(
            accepted_ids(&extra).is_empty() && settled(&extra).is_empty(),
            "no duplicate lifecycle after completion: {extra:?}"
        );
        drop(service);
    })
    .await
    .expect("real submit timed out");
}

#[tokio::test(flavor = "multi_thread")]
async fn real_cancel_counts_one_settled() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let hold = Arc::new(Notify::new());
        let (service, client, _handle) =
            start_real_runtime_service(Arc::new(HeldModel { hold: hold.clone() }));
        load_session(&client, "sess-1").await;
        submit(&client, "hang");
        let accepted = recv_matching(&client, |update| {
            matches!(update.kind, FrontendUpdateKind::OperationAccepted { .. })
        })
        .await;
        let FrontendUpdateKind::OperationAccepted { operation_id, .. } = accepted.kind else {
            unreachable!();
        };
        assert!(matches!(
            client.try_command(FrontendCommand::CancelOperation {
                operation_id: operation_id.clone(),
            }),
            CommandDispatch::Enqueued(_)
        ));
        let updates = collect_until(&client, |updates| !settled(updates).is_empty()).await;
        let terminals = settled(&updates);
        assert_eq!(
            terminals.len(),
            1,
            "cancel must settle once: {terminals:?} updates={updates:?}"
        );
        assert_eq!(terminals[0].0, operation_id);
        assert_eq!(terminals[0].1, "sess-1");
        assert_eq!(terminals[0].2, "Cancelled");
        let extra = drain_briefly(&client).await;
        assert!(
            settled(&extra).is_empty(),
            "no second settlement after cancel: {extra:?}"
        );
        drop(hold);
        drop(service);
    })
    .await
    .expect("real cancel timed out");
}

#[tokio::test(flavor = "multi_thread")]
async fn real_shutdown_counts_one_settled() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let hold = Arc::new(Notify::new());
        let (service, client, handle) =
            start_real_runtime_service(Arc::new(HeldModel { hold: hold.clone() }));
        load_session(&client, "sess-1").await;
        submit(&client, "hang");
        let accepted = recv_matching(&client, |update| {
            matches!(update.kind, FrontendUpdateKind::OperationAccepted { .. })
        })
        .await;
        let FrontendUpdateKind::OperationAccepted { operation_id, .. } = accepted.kind else {
            unreachable!();
        };
        handle
            .shutdown(
                ShutdownMode::Forced,
                Instant::now() + Duration::from_secs(2),
            )
            .await
            .expect("forced shutdown");
        let updates = collect_until(&client, |updates| {
            !settled(updates).is_empty()
                && updates.iter().any(|update| {
                    matches!(
                        update.kind,
                        FrontendUpdateKind::ServiceHealthChanged {
                            health: ServiceHealth::RuntimeEpochEnded { .. }
                        }
                    )
                })
        })
        .await;
        let terminals = settled(&updates);
        assert_eq!(
            terminals.len(),
            1,
            "shutdown must settle once: {terminals:?} updates={updates:?}"
        );
        assert_eq!(terminals[0].0, operation_id);
        assert_eq!(terminals[0].1, "sess-1");
        let extra = drain_briefly(&client).await;
        assert!(
            settled(&extra).is_empty(),
            "EpochEnded must not add another settlement: {extra:?}"
        );
        drop(hold);
        drop(service);
    })
    .await
    .expect("real shutdown timed out");
}

// Panic after a settlement has already entered Runtime staging is covered by
// `philo-agent-runtime` `paused_outlet_settled_then_coordinator_panic_still_publishes_one_settlement`.
// Service always drains the Runtime outlet, so a Service-level pause would be flaky.

#[tokio::test(flavor = "multi_thread")]
async fn real_coordinator_panic_one_forced_settled() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let hold = Arc::new(Notify::new());
        let (service, client, handle) =
            start_real_runtime_service(Arc::new(HeldModel { hold: hold.clone() }));
        load_session(&client, "sess-1").await;
        submit(&client, "hang");
        let accepted = recv_matching(&client, |update| {
            matches!(update.kind, FrontendUpdateKind::OperationAccepted { .. })
        })
        .await;
        let FrontendUpdateKind::OperationAccepted { operation_id, .. } = accepted.kind else {
            unreachable!();
        };
        handle.inject_coordinator_panic().await;
        let updates = collect_until(&client, |updates| {
            !settled(updates).is_empty()
                && updates.iter().any(|update| {
                    matches!(
                        update.kind,
                        FrontendUpdateKind::ServiceHealthChanged {
                            health: ServiceHealth::RuntimeEpochEnded { .. }
                        }
                    )
                })
        })
        .await;
        let terminals = settled(&updates);
        assert_eq!(
            terminals.len(),
            1,
            "panic forced settlement must be unique: {terminals:?} updates={updates:?}"
        );
        assert_eq!(terminals[0].0, operation_id);
        assert_eq!(terminals[0].1, "sess-1");
        let extra = drain_briefly(&client).await;
        assert!(
            settled(&extra).is_empty(),
            "epoch diagnostic must not map another settlement: {extra:?}"
        );
        drop(hold);
        drop(service);
    })
    .await
    .expect("real panic timed out");
}
