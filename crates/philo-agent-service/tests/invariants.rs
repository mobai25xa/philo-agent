//! Wave 1 service invariants: disconnect, backpressure, snapshot, identity, generation.

use std::time::{Duration, Instant};

use philo_agent_runtime::{
    AgentEvent, EpochEndReason, ModelCallId, OperationId, OperationStatus, SettlementDurability,
};
use philo_agent_service::testing::{FakeAssembler, start_test_service, start_test_service_with};
use philo_agent_service::{
    AdmissionError, CommandDispatch, CommandReject, FrontendCommand, FrontendGeneration,
    FrontendRevision, FrontendUpdate, FrontendUpdateKind, LIVE_TEXT_CHARS_MAX, RecvOutcome,
    RuntimeEvent,
};
use philo_session::{
    MemorySessionStore, OperationOutcome, SessionAssistantBlock, SessionEntryKind, SessionId,
    SessionRevision, SessionStore, SessionTransaction, SessionUserPart, TurnId, TurnOutcome,
};

fn accept_snapshot(
    client: &philo_agent_service::FrontendClient,
) -> philo_agent_service::FrontendRequestId {
    match client.request_snapshot(FrontendRevision::ZERO) {
        CommandDispatch::Enqueued(id) => id,
        other => panic!("expected enqueued snapshot, got {other:?}"),
    }
}

async fn recv_matching(
    client: &philo_agent_service::FrontendClient,
    mut pred: impl FnMut(&FrontendUpdate) -> bool,
) -> FrontendUpdate {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut seen = Vec::new();
    loop {
        match client
            .recv_until_async(Instant::now() + Duration::from_millis(50))
            .await
        {
            RecvOutcome::Update(update) if pred(&update) => return update,
            RecvOutcome::Update(update) => {
                seen.push(format!("{:?}", update.kind));
            }
            RecvOutcome::Timeout if Instant::now() < deadline => continue,
            RecvOutcome::Timeout => panic!("timed out waiting for frontend update; seen={seen:?}"),
            RecvOutcome::Disconnected => {
                panic!("frontend disconnected while waiting; seen={seen:?}")
            }
        }
    }
}

async fn drain_briefly(client: &philo_agent_service::FrontendClient) -> Vec<FrontendUpdateKind> {
    let until = Instant::now() + Duration::from_millis(80);
    let mut kinds = Vec::new();
    while Instant::now() < until {
        match client
            .recv_until_async(Instant::now() + Duration::from_millis(20))
            .await
        {
            RecvOutcome::Update(update) => kinds.push(update.kind),
            RecvOutcome::Timeout => break,
            RecvOutcome::Disconnected => panic!("disconnected"),
        }
    }
    kinds
}

fn submit_hello(
    client: &philo_agent_service::FrontendClient,
) -> philo_agent_service::FrontendRequestId {
    match client.try_command(FrontendCommand::Submit {
        draft: "hello".into(),
        attachments: Vec::new(),
    }) {
        CommandDispatch::Enqueued(id) => id,
        other => panic!("submit enqueue {other:?}"),
    }
}

async fn seed_session(store: &MemorySessionStore, session_id: &str) {
    let session_id = SessionId::new(session_id);
    store
        .commit(SessionTransaction::linear(
            session_id,
            SessionRevision::ZERO,
            vec![
                SessionEntryKind::OperationStarted {
                    operation_id: philo_session::OperationId::new("op-seed"),
                },
                SessionEntryKind::TurnStarted {
                    operation_id: philo_session::OperationId::new("op-seed"),
                    turn_id: TurnId::new("turn-seed"),
                },
                SessionEntryKind::UserMessage {
                    turn_id: TurnId::new("turn-seed"),
                    parts: SessionUserPart::text_parts("hello from store"),
                },
                SessionEntryKind::AssistantMessage {
                    turn_id: TurnId::new("turn-seed"),
                    blocks: vec![SessionAssistantBlock::Text {
                        text: "durable answer".into(),
                    }],
                },
                SessionEntryKind::TurnTerminated {
                    turn_id: TurnId::new("turn-seed"),
                    outcome: TurnOutcome::Succeeded,
                },
                SessionEntryKind::OperationSettled {
                    operation_id: philo_session::OperationId::new("op-seed"),
                    outcome: OperationOutcome::Succeeded,
                },
            ],
        ))
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn frontend_disconnect_does_not_stop_runtime_consumption() {
    let (service, client, runtime) = start_test_service();
    drop(client);

    for index in 0..80 {
        runtime.emit_agent(AgentEvent::TextDelta {
            delta: format!("chunk-{index}"),
        });
    }

    let deadline = Instant::now() + Duration::from_secs(2);
    while runtime.consumed() < 80 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        runtime.consumed() >= 80,
        "service must keep draining RuntimeSubscription after frontend drop; consumed={}",
        runtime.consumed()
    );
    assert_eq!(runtime.cancel_calls(), 0);
    drop(service);
}

#[tokio::test(flavor = "multi_thread")]
async fn feed_overflow_stays_bounded_and_requests_resync() {
    let (service, client, runtime) = start_test_service();

    for index in 0..200 {
        runtime.emit_agent(AgentEvent::TextDelta {
            delta: format!("x{index}"),
        });
    }

    let deadline = Instant::now() + Duration::from_secs(2);
    while runtime.consumed() < 200 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(runtime.consumed() >= 200);

    let mut saw_resync = false;
    let drain_until = Instant::now() + Duration::from_secs(2);
    let mut received = 0usize;
    while Instant::now() < drain_until {
        match client
            .recv_until_async(Instant::now() + Duration::from_millis(50))
            .await
        {
            RecvOutcome::Update(update) => {
                received += 1;
                if matches!(update.kind, FrontendUpdateKind::ResyncRequired { .. }) {
                    saw_resync = true;
                    break;
                }
            }
            RecvOutcome::Timeout => {
                if saw_resync || received > 0 {
                    break;
                }
            }
            RecvOutcome::Disconnected => panic!("disconnected"),
        }
    }
    assert!(
        saw_resync,
        "overflow must eventually surface ResyncRequired; received={received}"
    );
    assert!(
        received <= philo_agent_service::FRONTEND_UPDATE_CAP + 2,
        "feed must stay bounded; received={received}"
    );
    drop(service);
}

#[tokio::test(flavor = "multi_thread")]
async fn request_terminals_survive_an_overflowed_ordinary_feed() {
    let (service, client, runtime) = start_test_service();
    for index in 0..200 {
        runtime.emit_agent(AgentEvent::TextDelta {
            delta: format!("overflow-{index}"),
        });
    }
    let consumed_deadline = Instant::now() + Duration::from_secs(2);
    while runtime.consumed() < 200 && Instant::now() < consumed_deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(runtime.consumed() >= 200);

    let submit_id = match client.try_command(FrontendCommand::Submit {
        draft: "no current session".into(),
        attachments: Vec::new(),
    }) {
        CommandDispatch::Enqueued(id) => id,
        other => panic!("submit dispatch failed: {other:?}"),
    };
    let cancel_id = match client.try_command(FrontendCommand::CancelOperation {
        operation_id: "missing-operation".into(),
    }) {
        CommandDispatch::Enqueued(id) => id,
        other => panic!("cancel dispatch failed: {other:?}"),
    };

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut saw_submit = false;
    let mut saw_cancel = false;
    while !(saw_submit && saw_cancel) && Instant::now() < deadline {
        match client
            .recv_until_async(Instant::now() + Duration::from_millis(50))
            .await
        {
            RecvOutcome::Update(update) if update.request_id == Some(submit_id) => {
                saw_submit = matches!(update.kind, FrontendUpdateKind::CommandRejected { .. });
            }
            RecvOutcome::Update(update) if update.request_id == Some(cancel_id) => {
                saw_cancel = matches!(
                    update.kind,
                    FrontendUpdateKind::CommandAccepted
                        | FrontendUpdateKind::CommandRejected { .. }
                );
            }
            RecvOutcome::Update(_) | RecvOutcome::Timeout => {}
            RecvOutcome::Disconnected => panic!("frontend disconnected"),
        }
    }
    assert!(saw_submit, "submit rejection was lost during resync");
    assert!(saw_cancel, "cancel terminal was lost during resync");
    drop(service);
}

#[tokio::test(flavor = "multi_thread")]
async fn snapshot_is_session_view_plus_live_not_event_log() {
    let store = MemorySessionStore::new();
    seed_session(&store, "sess-snap").await;
    let (service, client, runtime) = start_test_service_with(FakeAssembler::new(), store);

    assert!(matches!(
        client.try_command(FrontendCommand::LoadSession {
            session_id: "sess-snap".into(),
        }),
        CommandDispatch::Enqueued(_)
    ));
    recv_matching(&client, |update| {
        matches!(update.kind, FrontendUpdateKind::SessionLoaded { .. })
    })
    .await;

    runtime.emit(RuntimeEvent::OperationAccepted {
        operation_id: OperationId::new("op-live"),
        session_id: philo_agent_runtime::SessionId::new("sess-snap"),
        turn_id: philo_agent_runtime::TurnId::new("turn-live"),
    });
    runtime.emit_agent(AgentEvent::OperationStarted {
        operation_id: OperationId::new("op-live"),
    });
    runtime.emit_agent(AgentEvent::ModelCallStarted {
        model_call_id: ModelCallId::new("mc-1"),
    });
    for _ in 0..40 {
        runtime.emit_agent(AgentEvent::TextDelta {
            delta: "partial ".into(),
        });
    }

    let deadline = Instant::now() + Duration::from_secs(2);
    while runtime.consumed() < 43 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let _request_id = accept_snapshot(&client);
    let update = recv_matching(&client, |update| {
        matches!(update.kind, FrontendUpdateKind::SnapshotReady(_))
    })
    .await;
    let FrontendUpdateKind::SnapshotReady(snapshot) = update.kind else {
        unreachable!();
    };

    let durable = snapshot.durable_session_view.expect("session was loaded");
    assert_eq!(durable.session_id, "sess-snap");
    assert_eq!(
        durable.messages.len(),
        2,
        "durable view is the store projection, not the live delta count"
    );
    assert!(
        snapshot.live.text.contains("partial"),
        "live snapshot carries in-flight text"
    );
    assert!(snapshot.live.text.chars().count() <= LIVE_TEXT_CHARS_MAX);
    assert_ne!(
        durable.messages.len(),
        snapshot.live.text.matches("partial").count(),
        "service must not flatten the event stream into the durable view"
    );
    drop(service);
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_preview_and_epoch_results_are_discarded() {
    let store = MemorySessionStore::new();
    seed_session(&store, "sess-a").await;
    seed_session(&store, "sess-b").await;
    let (service, client, runtime) = start_test_service_with(FakeAssembler::new(), store);

    let first = match client.try_command(FrontendCommand::PreviewSession {
        session_id: "sess-a".into(),
        request_generation: 1,
    }) {
        CommandDispatch::Enqueued(id) => id,
        other => panic!("first preview {other:?}"),
    };
    let second = match client.try_command(FrontendCommand::PreviewSession {
        session_id: "sess-b".into(),
        request_generation: 2,
    }) {
        CommandDispatch::Enqueued(id) => id,
        other => panic!("second preview {other:?}"),
    };
    assert!(second > first);

    let previewed = recv_matching(&client, |update| match &update.kind {
        FrontendUpdateKind::SessionPreviewed { session_id, .. } => {
            update.request_id != Some(first) && session_id == "sess-b"
        }
        _ => false,
    })
    .await;
    let FrontendUpdateKind::SessionPreviewed { session_id, .. } = &previewed.kind else {
        unreachable!();
    };
    assert_eq!(session_id, "sess-b");
    assert_eq!(previewed.request_id, Some(second));

    runtime.emit(RuntimeEvent::EpochEnded {
        epoch: philo_agent_runtime::RuntimeEpoch::new("1"),
        reason: EpochEndReason::CoordinatorFault,
        forced_count: 0,
    });
    let health = recv_matching(&client, |update| {
        matches!(
            update.kind,
            FrontendUpdateKind::ServiceHealthChanged {
                health: philo_agent_service::ServiceHealth::RuntimeEpochEnded { .. }
            }
        )
    })
    .await;
    assert!(health.epoch > philo_agent_service::FrontendEpoch::INITIAL);

    let late = client.try_command(FrontendCommand::PreviewSession {
        session_id: "sess-a".into(),
        request_generation: 1,
    });
    assert!(matches!(late, CommandDispatch::Enqueued(_)));
    let rejected_or_health = recv_matching(&client, |update| {
        matches!(
            update.kind,
            FrontendUpdateKind::CommandRejected { .. }
                | FrontendUpdateKind::SessionPreviewed { .. }
                | FrontendUpdateKind::ServiceHealthChanged { .. }
                | FrontendUpdateKind::ResyncRequired { .. }
        )
    })
    .await;
    if let FrontendUpdateKind::SessionPreviewed { .. } = rejected_or_health.kind {
        assert!(
            rejected_or_health.epoch > philo_agent_service::FrontendEpoch::INITIAL,
            "late work after epoch end must carry the new epoch"
        );
    }
    drop(service);
}

#[tokio::test(flavor = "multi_thread")]
async fn generation_install_failure_keeps_previous() {
    let assembler = FakeAssembler::failing(&["broken"]);
    let (service, client, _runtime) = start_test_service_with(assembler, MemorySessionStore::new());

    let request_id = match client.try_command(FrontendCommand::InstallModel {
        name: "broken".into(),
    }) {
        CommandDispatch::Enqueued(id) => id,
        other => panic!("{other:?}"),
    };
    let failed = recv_matching(&client, |update| {
        matches!(
            update.kind,
            FrontendUpdateKind::GenerationInstallFailed { .. }
        )
    })
    .await;
    assert_eq!(failed.request_id, Some(request_id));

    let _snapshot_id = accept_snapshot(&client);
    let update = recv_matching(&client, |update| {
        matches!(update.kind, FrontendUpdateKind::SnapshotReady(_))
    })
    .await;
    let FrontendUpdateKind::SnapshotReady(snapshot) = update.kind else {
        unreachable!();
    };
    assert_eq!(snapshot.generation.model_name, "base");
    drop(service);
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_install_success_does_not_overwrite_newer_generation() {
    let assembler = FakeAssembler::new().with_delay(&["slow"], Duration::from_millis(80));
    let (service, client, _runtime) = start_test_service_with(assembler, MemorySessionStore::new());

    let slow = client.try_command(FrontendCommand::InstallModel {
        name: "slow".into(),
    });
    let fast = client.try_command(FrontendCommand::InstallModel {
        name: "fast".into(),
    });
    assert!(matches!(slow, CommandDispatch::Enqueued(_)));
    assert!(matches!(fast, CommandDispatch::Enqueued(_)));

    let installed = recv_matching(&client, |update| {
        matches!(update.kind, FrontendUpdateKind::GenerationInstalled { .. })
    })
    .await;
    let FrontendUpdateKind::GenerationInstalled { display } = installed.kind else {
        unreachable!();
    };
    assert_eq!(display.model_name, "fast");

    tokio::time::sleep(Duration::from_millis(120)).await;
    let _snapshot_id = accept_snapshot(&client);
    let update = recv_matching(&client, |update| {
        matches!(update.kind, FrontendUpdateKind::SnapshotReady(_))
    })
    .await;
    let FrontendUpdateKind::SnapshotReady(snapshot) = update.kind else {
        unreachable!();
    };
    assert_eq!(
        snapshot.generation.model_name, "fast",
        "late success of the superseded install must not replace the newer generation"
    );
    drop(service);
}

#[tokio::test(flavor = "multi_thread")]
async fn submit_assigns_request_identity_and_freezes_current_generation() {
    let (service, client, runtime) = start_test_service();
    assert!(matches!(
        client.try_command(FrontendCommand::LoadSession {
            session_id: "sess-1".into(),
        }),
        CommandDispatch::Enqueued(_)
    ));
    recv_matching(&client, |update| {
        matches!(update.kind, FrontendUpdateKind::SessionLoaded { .. })
    })
    .await;
    let request_id = match client.try_command(FrontendCommand::Submit {
        draft: "hello".into(),
        attachments: Vec::new(),
    }) {
        CommandDispatch::Enqueued(id) => id,
        other => panic!("{other:?}"),
    };
    let accepted = recv_matching(&client, |update| {
        update.request_id == Some(request_id)
            && matches!(update.kind, FrontendUpdateKind::SubmitAccepted { .. })
    })
    .await;
    assert_eq!(accepted.request_id, Some(request_id));

    let deadline = Instant::now() + Duration::from_secs(1);
    while runtime.submitted() == 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(runtime.submitted(), 1);
    assert_eq!(
        runtime.last_submitted_generation().as_deref(),
        Some("generation-0")
    );

    runtime.emit(RuntimeEvent::OperationSettled {
        operation_id: OperationId::new("op-1"),
        session_id: philo_agent_runtime::SessionId::new("sess-1"),
        status: OperationStatus::Succeeded,
        durability: SettlementDurability::Confirmed,
        session_revision: philo_agent_runtime::SettlementRevision::Unchanged,
    });
    drop(service);
}

#[test]
fn frontend_generation_still_has_model_name() {
    let display = FrontendGeneration {
        generation_id: "g-1".into(),
        model_name: "base".into(),
        reasoning_effort: None,
        image_input: true,
        tool_names: Vec::new(),
    };
    assert_eq!(display.model_name, "base");
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_event_settlement_is_not_a_frontend_lifecycle() {
    let (service, client, runtime) = start_test_service();
    runtime.emit_agent(AgentEvent::OperationSettled {
        operation_id: OperationId::new("op-agent"),
        status: OperationStatus::Succeeded,
        durability: SettlementDurability::Confirmed,
        session_revision: philo_agent_runtime::SettlementRevision::Committed(SessionRevision::new(
            7,
        )),
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    while runtime.consumed() == 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(runtime.consumed() >= 1);

    let drain_until = Instant::now() + Duration::from_millis(150);
    while Instant::now() < drain_until {
        match client
            .recv_until_async(Instant::now() + Duration::from_millis(40))
            .await
        {
            RecvOutcome::Update(update) => {
                assert!(
                    !matches!(
                        update.kind,
                        FrontendUpdateKind::OperationEvent(
                            philo_agent_service::FrontendOperationEvent::OperationSettled { .. }
                        )
                    ),
                    "AgentEvent::OperationSettled must not settle the frontend"
                );
            }
            RecvOutcome::Timeout => break,
            RecvOutcome::Disconnected => panic!("disconnected"),
        }
    }
    drop(service);
}

#[tokio::test(flavor = "multi_thread")]
async fn runtime_event_settlement_is_the_public_frontend_terminal() {
    let (service, client, runtime) = start_test_service();
    runtime.emit_operation_accepted(
        OperationId::new("op-1"),
        philo_agent_runtime::SessionId::new("sess-1"),
        philo_agent_runtime::TurnId::new("turn-1"),
    );
    recv_matching(&client, |update| {
        matches!(update.kind, FrontendUpdateKind::OperationAccepted { .. })
    })
    .await;
    runtime.emit(RuntimeEvent::OperationSettled {
        operation_id: OperationId::new("op-1"),
        session_id: philo_agent_runtime::SessionId::new("sess-1"),
        status: OperationStatus::Succeeded,
        durability: SettlementDurability::Confirmed,
        session_revision: philo_agent_runtime::SettlementRevision::Committed(SessionRevision::new(
            7,
        )),
    });
    let update = recv_matching(&client, |update| {
        matches!(
            update.kind,
            FrontendUpdateKind::OperationEvent(
                philo_agent_service::FrontendOperationEvent::OperationSettled { .. }
            )
        )
    })
    .await;
    let FrontendUpdateKind::OperationEvent(
        philo_agent_service::FrontendOperationEvent::OperationSettled {
            operation_id,
            session_id,
            status,
            durability,
            session_revision,
        },
    ) = update.kind
    else {
        unreachable!();
    };
    assert_eq!(operation_id, "op-1");
    assert_eq!(session_id, "sess-1");
    assert_eq!(status, "Succeeded");
    assert_eq!(durability, "Confirmed");
    assert_eq!(
        session_revision,
        philo_agent_runtime::SettlementRevision::Committed(SessionRevision::new(7))
    );

    let extra_until = Instant::now() + Duration::from_millis(80);
    while Instant::now() < extra_until {
        match client
            .recv_until_async(Instant::now() + Duration::from_millis(30))
            .await
        {
            RecvOutcome::Update(update) => {
                assert!(
                    !matches!(
                        update.kind,
                        FrontendUpdateKind::OperationEvent(
                            philo_agent_service::FrontendOperationEvent::OperationSettled { .. }
                        )
                    ),
                    "exactly one frontend settled event"
                );
            }
            RecvOutcome::Timeout => break,
            RecvOutcome::Disconnected => panic!("disconnected"),
        }
    }
    drop(service);
}

/// No successful current: Submit is `NoCurrentSession`. First pending load
/// (no current yet) is covered by
/// `snapshot::submit_during_first_pending_load_is_no_current_session`.
/// Replacement load while current A exists submits to A.
#[tokio::test(flavor = "multi_thread")]
async fn submit_without_current_session_is_rejected() {
    let (service, client, _runtime) = start_test_service();
    let request_id = submit_hello(&client);
    let rejected = recv_matching(&client, |update| {
        update.request_id == Some(request_id)
            && matches!(update.kind, FrontendUpdateKind::CommandRejected { .. })
    })
    .await;
    match &rejected.kind {
        FrontendUpdateKind::CommandRejected {
            reason: CommandReject::NoCurrentSession,
        } => {}
        other => panic!("expected NoCurrentSession, got {other:?}"),
    }
    let extras = drain_briefly(&client).await;
    assert!(
        extras.iter().all(|kind| {
            !matches!(
                kind,
                FrontendUpdateKind::CommandAccepted | FrontendUpdateKind::SubmitAccepted { .. }
            )
        }),
        "no-session submit must not accept: {extras:?}"
    );
    drop(service);
}

#[tokio::test(flavor = "multi_thread")]
async fn submit_accepted_waits_for_runtime_and_keeps_lifecycle_separate() {
    let (service, client, runtime) = start_test_service();
    assert!(matches!(
        client.try_command(FrontendCommand::LoadSession {
            session_id: "sess-1".into(),
        }),
        CommandDispatch::Enqueued(_)
    ));
    recv_matching(&client, |update| {
        matches!(update.kind, FrontendUpdateKind::SessionLoaded { .. })
    })
    .await;

    let hold = runtime.hold_children();
    let request_id = submit_hello(&client);
    runtime.wait_child_started(1).await;
    let early = drain_briefly(&client).await;
    assert!(
        early.iter().all(|kind| {
            !matches!(
                kind,
                FrontendUpdateKind::SubmitAccepted { .. }
                    | FrontendUpdateKind::CommandAccepted
                    | FrontendUpdateKind::OperationAccepted { .. }
            )
        }),
        "runtime.submit must finish before SubmitAccepted: {early:?}"
    );

    hold.release();
    let accepted = recv_matching(&client, |update| {
        update.request_id == Some(request_id)
            && matches!(update.kind, FrontendUpdateKind::SubmitAccepted { .. })
    })
    .await;
    match &accepted.kind {
        FrontendUpdateKind::SubmitAccepted {
            operation_id,
            turn_id,
        } => {
            assert_eq!(operation_id, "op-1");
            assert_eq!(turn_id, "turn-1");
        }
        other => panic!("{other:?}"),
    }

    let extras = drain_briefly(&client).await;
    assert!(
        extras
            .iter()
            .all(|kind| !matches!(kind, FrontendUpdateKind::SubmitAccepted { .. })),
        "exactly one SubmitAccepted: {extras:?}"
    );

    runtime.emit(RuntimeEvent::OperationAccepted {
        operation_id: OperationId::new("op-1"),
        session_id: philo_agent_runtime::SessionId::new("sess-1"),
        turn_id: philo_agent_runtime::TurnId::new("turn-1"),
    });
    let lifecycle = recv_matching(&client, |update| {
        matches!(update.kind, FrontendUpdateKind::OperationAccepted { .. })
    })
    .await;
    assert_eq!(lifecycle.request_id, None);
    drop(service);
}

#[tokio::test(flavor = "multi_thread")]
async fn submit_admission_failure_is_rejected() {
    let (service, client, runtime) = start_test_service();
    assert!(matches!(
        client.try_command(FrontendCommand::LoadSession {
            session_id: "sess-1".into(),
        }),
        CommandDispatch::Enqueued(_)
    ));
    recv_matching(&client, |update| {
        matches!(update.kind, FrontendUpdateKind::SessionLoaded { .. })
    })
    .await;

    runtime.fail_next_submit(AdmissionError::QueueFull);
    let request_id = submit_hello(&client);
    let rejected = recv_matching(&client, |update| {
        update.request_id == Some(request_id)
            && matches!(update.kind, FrontendUpdateKind::CommandRejected { .. })
    })
    .await;
    match &rejected.kind {
        FrontendUpdateKind::CommandRejected {
            reason: CommandReject::AdmissionFailed { message },
        } => {
            assert!(message.contains("full"), "{message}");
        }
        other => panic!("expected AdmissionFailed, got {other:?}"),
    }
    let extras = drain_briefly(&client).await;
    assert!(
        extras
            .iter()
            .all(|kind| !matches!(kind, FrontendUpdateKind::SubmitAccepted { .. })),
        "failed submit must not emit SubmitAccepted: {extras:?}"
    );
    drop(service);
}

#[tokio::test(flavor = "multi_thread")]
async fn image_attachments_are_rejected_when_the_model_has_no_image_input() {
    use std::sync::Arc;

    use philo_agent_service::FrontendAttachment;
    use philo_agent_service::testing::start_test_service_with_generation;
    use philo_agent_runtime::{GenerationDisplay, RuntimeGeneration};

    let text_only = Arc::new(RuntimeGeneration {
        generation_id: philo_agent_runtime::GenerationId::new("generation-0"),
        model: Arc::new(philo_agent_service::testing::UnavailableModel),
        tools: philo_agent_service::testing::empty_tools(),
        runtime_config: Default::default(),
        display: GenerationDisplay {
            model_name: "text-only".to_owned(),
            image_input: false,
        },
    });
    let (service, client, _runtime) = start_test_service_with_generation(
        FakeAssembler::new(),
        MemorySessionStore::new(),
        text_only,
    );
    assert!(matches!(
        client.try_command(FrontendCommand::LoadSession {
            session_id: "sess-1".into(),
        }),
        CommandDispatch::Enqueued(_)
    ));
    recv_matching(&client, |update| {
        matches!(update.kind, FrontendUpdateKind::SessionLoaded { .. })
    })
    .await;

    let request_id = match client.try_command(FrontendCommand::Submit {
        draft: "look at this".into(),
        attachments: vec![FrontendAttachment {
            media_type: "image/png".into(),
            bytes: vec![0x89, 0x50, 0x4E, 0x47],
        }],
    }) {
        CommandDispatch::Enqueued(id) => id,
        other => panic!("submit enqueue {other:?}"),
    };
    let rejected = recv_matching(&client, |update| {
        update.request_id == Some(request_id)
            && matches!(update.kind, FrontendUpdateKind::CommandRejected { .. })
    })
    .await;
    match &rejected.kind {
        FrontendUpdateKind::CommandRejected {
            reason: CommandReject::InvalidInput { reason },
        } => {
            assert!(
                reason.contains("does not accept image attachments"),
                "{reason}"
            );
            assert!(reason.contains("text-only"), "{reason}");
        }
        other => panic!("expected InvalidInput for image attachment, got {other:?}"),
    }

    // Text-only submits still pass the capability gate.
    submit_hello(&client);
    drop(service);
}

#[test]
fn lease_types_remain_public_and_distinct_from_generation_display() {
    let _ = std::any::type_name::<philo_agent_service::FrontendLease>();
    let _ = std::any::type_name::<philo_agent_service::FrontendLeaseGeneration>();
    let _ = std::any::type_name::<philo_agent_service::FrontendGeneration>();
    let _ = std::any::type_name::<philo_agent_service::SupervisorCommand>();
    let _ = std::any::type_name::<philo_agent_service::AttachError>();
    let _ = std::any::type_name::<philo_agent_service::DetachError>();
    let _ = std::any::type_name::<philo_agent_service::DetachReport>();
}
