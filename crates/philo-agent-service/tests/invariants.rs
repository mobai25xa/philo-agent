//! Wave 1 service invariants: disconnect, backpressure, snapshot, identity, generation.

use std::time::{Duration, Instant};

use philo_agent_runtime::{
    AgentEvent, ModelCallId, OperationId, OperationStatus, SettlementDurability,
};
use philo_agent_service::testing::{FakeAssembler, start_test_service, start_test_service_with};
use philo_agent_service::{
    CommandSubmitResult, ConfirmationDecision, ConfirmationRequest, FrontendCommand,
    FrontendInstanceId, FrontendRevision, FrontendUpdate, FrontendUpdateKind, LIVE_TEXT_CHARS_MAX,
    RecvOutcome, RuntimeEvent,
};
use philo_session::{
    MemorySessionStore, OperationOutcome, SessionAssistantBlock, SessionEntryKind, SessionId,
    SessionRevision, SessionStore, SessionTransaction, SessionUserPart, TurnId, TurnOutcome,
};

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
async fn snapshot_is_session_view_plus_live_not_event_log() {
    let store = MemorySessionStore::new();
    seed_session(&store, "sess-snap").await;
    let (service, client, runtime) = start_test_service_with(FakeAssembler::new(), store);

    assert!(matches!(
        client.try_command(FrontendCommand::LoadSession {
            session_id: "sess-snap".into(),
        }),
        CommandSubmitResult::Accepted(_)
    ));
    recv_matching(&client, |update| {
        matches!(update.kind, FrontendUpdateKind::SessionLoaded { .. })
    })
    .await;

    runtime.emit(RuntimeEvent::OperationAccepted {
        operation_id: OperationId::new("op-live"),
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

    let request_id = client.request_snapshot(FrontendRevision::ZERO);
    assert!(request_id.is_valid());
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
        CommandSubmitResult::Accepted(id) => id,
        other => panic!("first preview {other:?}"),
    };
    let second = match client.try_command(FrontendCommand::PreviewSession {
        session_id: "sess-b".into(),
        request_generation: 2,
    }) {
        CommandSubmitResult::Accepted(id) => id,
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
        settlements: Vec::new(),
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
    assert!(matches!(late, CommandSubmitResult::Accepted(_)));
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
        CommandSubmitResult::Accepted(id) => id,
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

    let snapshot_id = client.request_snapshot(FrontendRevision::ZERO);
    assert!(snapshot_id.is_valid());
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
    assert!(matches!(slow, CommandSubmitResult::Accepted(_)));
    assert!(matches!(fast, CommandSubmitResult::Accepted(_)));

    let installed = recv_matching(&client, |update| {
        matches!(update.kind, FrontendUpdateKind::GenerationInstalled { .. })
    })
    .await;
    let FrontendUpdateKind::GenerationInstalled { display } = installed.kind else {
        unreachable!();
    };
    assert_eq!(display.model_name, "fast");

    tokio::time::sleep(Duration::from_millis(120)).await;
    let snapshot_id = client.request_snapshot(FrontendRevision::ZERO);
    assert!(snapshot_id.is_valid());
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
    let request_id = match client.try_command(FrontendCommand::Submit {
        session_id: "sess-1".into(),
        draft: "hello".into(),
        attachments: Vec::new(),
    }) {
        CommandSubmitResult::Accepted(id) => id,
        other => panic!("{other:?}"),
    };
    let accepted = recv_matching(&client, |update| {
        update.request_id == Some(request_id)
            && matches!(
                update.kind,
                FrontendUpdateKind::CommandAccepted | FrontendUpdateKind::OperationAccepted { .. }
            )
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

    runtime.emit_agent(AgentEvent::OperationSettled {
        operation_id: OperationId::new("op-1"),
        status: OperationStatus::Succeeded,
        durability: SettlementDurability::Confirmed,
        session_revision: None,
    });
    drop(service);
}

#[tokio::test(flavor = "multi_thread")]
async fn attach_replacement_denies_pending_confirmations() {
    let (service, client, _runtime) = start_test_service();
    let _ = client.try_command(FrontendCommand::FrontendAttached {
        frontend_instance_id: FrontendInstanceId::new("front-a"),
    });
    let gate = service.confirmation_gate();
    let decision = tokio::spawn(async move {
        gate.request(ConfirmationRequest {
            title: "q".into(),
            body: "b".into(),
        })
        .await
    });
    recv_matching(&client, |update| {
        matches!(
            update.kind,
            FrontendUpdateKind::ConfirmationRequested { .. }
        )
    })
    .await;

    let _ = client.try_command(FrontendCommand::FrontendAttached {
        frontend_instance_id: FrontendInstanceId::new("front-b"),
    });
    assert_eq!(decision.await.unwrap(), ConfirmationDecision::Deny);
    recv_matching(&client, |update| {
        matches!(
            update.kind,
            FrontendUpdateKind::ConfirmationResolved {
                decision: ConfirmationDecision::Deny,
                ..
            }
        )
    })
    .await;
    drop(service);
}

#[tokio::test(flavor = "multi_thread")]
async fn same_instance_reattach_keeps_pending_confirmation() {
    let (service, client, _runtime) = start_test_service();
    let instance = FrontendInstanceId::new("front-a");
    let _ = client.try_command(FrontendCommand::FrontendAttached {
        frontend_instance_id: instance.clone(),
    });
    let gate = service.confirmation_gate();
    let decision = tokio::spawn(async move {
        gate.request(ConfirmationRequest {
            title: "q".into(),
            body: "b".into(),
        })
        .await
    });
    let requested = recv_matching(&client, |update| {
        matches!(
            update.kind,
            FrontendUpdateKind::ConfirmationRequested { .. }
        )
    })
    .await;
    let FrontendUpdateKind::ConfirmationRequested {
        confirmation_id, ..
    } = requested.kind
    else {
        unreachable!();
    };

    let _ = client.try_command(FrontendCommand::FrontendAttached {
        frontend_instance_id: instance,
    });
    let _ = client.try_command(FrontendCommand::RespondConfirmation {
        confirmation_id,
        decision: ConfirmationDecision::Allow,
    });
    assert_eq!(decision.await.unwrap(), ConfirmationDecision::Allow);
    drop(service);
}
