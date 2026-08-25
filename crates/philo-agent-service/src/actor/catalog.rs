//! Session catalog: durable `SessionStore` summaries plus one ephemeral
//! current session, and manual renames via `TitleSet` transactions.

use philo_session::{
    SessionEntryKind, SessionError, SessionStore, SessionSummary, SessionTransaction,
};

use crate::error::CommandReject;
use crate::frontend::snapshot::FrontendSessionSummary;
use crate::frontend::update::FrontendUpdateKind;
use crate::ids::{FrontendEpoch, FrontendRequestId};
use crate::mapping;
use crate::runtime_api::{RuntimeEvents, RuntimePort};

use super::{AgentServiceActor, ServiceTaskResult};

/// One bounded re-read after a rename loses a revision race.
const RENAME_CONFLICT_RETRY_MAX: usize = 1;

impl<R, S> AgentServiceActor<R, S>
where
    R: RuntimePort,
    S: RuntimeEvents,
{
    pub(super) fn handle_list_sessions(&mut self, request_id: FrontendRequestId) {
        if !self.is_accepting_work() {
            self.reject_not_accepting(request_id);
            return;
        }
        if !self.can_spawn_work() {
            self.reject_child_capacity(request_id);
            return;
        }
        let sessions = self.sessions.clone();
        let epoch = self.epoch;
        self.spawn_work(request_id, async move {
            let result = sessions.list_session_summaries().await;
            ServiceTaskResult::ListSessions {
                request_id,
                epoch,
                result,
            }
        });
    }

    pub(super) fn handle_list_sessions_result(
        &mut self,
        request_id: FrontendRequestId,
        epoch: FrontendEpoch,
        result: Result<Vec<SessionSummary>, SessionError>,
    ) {
        if epoch != self.epoch {
            self.feed.cancel_request(request_id);
            return;
        }
        match result {
            Ok(durable) => {
                let sessions = compose_session_catalog(
                    durable,
                    self.snapshot.current_session.as_deref(),
                    self.snapshot.pending_load_session(),
                );
                self.emit(
                    Some(request_id),
                    FrontendUpdateKind::SessionListLoaded { sessions },
                );
            }
            Err(error) => self.emit(
                Some(request_id),
                FrontendUpdateKind::CommandRejected {
                    reason: CommandReject::InvalidInput {
                        reason: catalog_error_reason(&error),
                    },
                },
            ),
        }
    }

    /// A rename appends a `TitleSet` transaction through the shared store
    /// contract. It never touches the current session or the Runtime, so it
    /// is accepted in every availability state.
    pub(super) fn handle_rename_session(
        &mut self,
        request_id: FrontendRequestId,
        session_id: String,
        title: String,
    ) {
        if !self.is_accepting_work() {
            self.reject_not_accepting(request_id);
            return;
        }
        let trimmed = title.trim();
        if trimmed.is_empty() {
            self.emit(
                Some(request_id),
                FrontendUpdateKind::CommandRejected {
                    reason: CommandReject::InvalidInput {
                        reason: "session title must not be empty".into(),
                    },
                },
            );
            return;
        }
        if !self.can_spawn_work() {
            self.reject_child_capacity(request_id);
            return;
        }
        let sessions = self.sessions.clone();
        let epoch = self.epoch;
        let title = trimmed.to_owned();
        self.spawn_work(request_id, async move {
            let result = rename_session_once(sessions.as_ref(), &session_id, &title).await;
            ServiceTaskResult::RenameSession {
                request_id,
                epoch,
                result,
            }
        });
    }

    pub(super) fn handle_load_session(
        &mut self,
        request_id: FrontendRequestId,
        session_id: String,
    ) {
        if !self.is_accepting_work() {
            self.reject_not_accepting(request_id);
            return;
        }
        if !self.can_spawn_work() {
            self.reject_child_capacity(request_id);
            return;
        }
        self.start_session_load(request_id, session_id);
    }

    pub(super) fn handle_create_session(&mut self, request_id: FrontendRequestId) {
        if !self.is_accepting_work() {
            self.reject_not_accepting(request_id);
            return;
        }
        if !self.can_spawn_work() {
            self.reject_child_capacity(request_id);
            return;
        }
        self.session_seq += 1;
        let session_id = format!("sess-service-{}", self.session_seq);
        self.start_session_load(request_id, session_id);
    }
}

/// Reads the target revision and commits the rename; a lost race against a
/// concurrent commit is retried once with fresh state. Unknown sessions are
/// refused so renames cannot mint empty sessions.
async fn rename_session_once(
    sessions: &dyn SessionStore,
    session_id: &str,
    title: &str,
) -> Result<(), String> {
    let store_id = mapping::session_store_id(session_id);
    for attempt in 0..=RENAME_CONFLICT_RETRY_MAX {
        let view = match sessions.context_view(&store_id).await {
            Ok(view) => view,
            Err(error) => return Err(format!("cannot read the session: {error:?}")),
        };
        if view.revision().get() == 0 {
            return Err(format!("unknown session: {session_id}"));
        }
        let transaction = SessionTransaction::linear(
            store_id.clone(),
            view.revision(),
            vec![SessionEntryKind::TitleSet {
                title: title.to_owned(),
            }],
        );
        match sessions.commit(transaction).await {
            Ok(_) => return Ok(()),
            // The shared core canonicalizes to the trimmed form; a validation
            // failure here is a contract bug, surfaced verbatim otherwise.
            Err(SessionError::RevisionConflict { .. }) if attempt < RENAME_CONFLICT_RETRY_MAX => {}
            Err(error) => return Err(format!("{error:?}")),
        }
    }
    Err(format!(
        "session {session_id} kept changing while renaming; retry"
    ))
}

/// Durable summaries plus committed current and in-flight pending load.
/// Order is most-recent-first by the advisory timestamp, with unknown or
/// uncommitted sessions last, then stable by id. The order is explicit so
/// frontends never guess.
pub(super) fn compose_session_catalog(
    durable: Vec<SessionSummary>,
    current_session: Option<&str>,
    pending_load: Option<&str>,
) -> Vec<FrontendSessionSummary> {
    let mut sessions: Vec<FrontendSessionSummary> = durable
        .into_iter()
        .map(|summary| FrontendSessionSummary {
            session_id: summary.session_id.as_str().to_owned(),
            title: summary.title,
            updated_at: summary.updated_at,
        })
        .collect();
    for extra in [current_session, pending_load] {
        if let Some(id) = extra.filter(|id| !id.is_empty())
            && !sessions.iter().any(|existing| existing.session_id == id)
        {
            sessions.push(FrontendSessionSummary {
                session_id: id.to_owned(),
                title: None,
                updated_at: None,
            });
        }
    }
    sessions.sort_by(|left, right| {
        right
            .updated_at
            .cmp(&left.updated_at)
            .then_with(|| left.session_id.cmp(&right.session_id))
    });
    sessions
}

fn catalog_error_reason(error: &SessionError) -> String {
    match error {
        SessionError::StoreBusy { reason } => format!("session catalog is busy: {reason}"),
        SessionError::StoreUnavailable { reason } => {
            format!("session catalog is unavailable: {reason}")
        }
        other => format!("session catalog failed: {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};

    use super::{catalog_error_reason, compose_session_catalog, rename_session_once};
    use crate::frontend::snapshot::FrontendSessionSummary;
    use philo_session::{
        MemorySessionStore, OperationId, SessionEntryKind, SessionError, SessionId,
        SessionRevision, SessionStore, SessionSummary, SessionTransaction, SessionUserPart, TurnId,
    };

    fn block_on<F: Future>(future: F) -> F::Output {
        let mut future = pin!(future);
        let mut context = Context::from_waker(Waker::noop());
        loop {
            if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
                return value;
            }
        }
    }

    fn summary(id: &str, title: Option<&str>) -> SessionSummary {
        SessionSummary {
            session_id: SessionId::new(id),
            title: title.map(str::to_owned),
            updated_at: None,
        }
    }

    fn dto(id: &str, title: Option<&str>) -> FrontendSessionSummary {
        FrontendSessionSummary {
            session_id: id.to_owned(),
            title: title.map(str::to_owned),
            updated_at: None,
        }
    }

    #[test]
    fn compose_orders_most_recent_first_with_unknowns_last() {
        let ids = compose_session_catalog(
            vec![
                {
                    let mut old = summary("sess-old", Some("old"));
                    old.updated_at = Some(100);
                    old
                },
                {
                    let mut new = summary("sess-new", None);
                    new.updated_at = Some(200);
                    new
                },
                summary("sess-unknown", None),
            ],
            None,
            None,
        );
        assert_eq!(
            ids.iter()
                .map(|s| s.session_id.as_str())
                .collect::<Vec<_>>(),
            ["sess-new", "sess-old", "sess-unknown"]
        );
    }

    #[test]
    fn compose_ties_break_by_id() {
        let ids = compose_session_catalog(
            vec![
                {
                    let mut b = summary("sess-b", None);
                    b.updated_at = Some(100);
                    b
                },
                {
                    let mut a = summary("sess-a", None);
                    a.updated_at = Some(100);
                    a
                },
            ],
            None,
            None,
        );
        assert_eq!(
            ids.iter()
                .map(|s| s.session_id.as_str())
                .collect::<Vec<_>>(),
            ["sess-a", "sess-b"]
        );
    }

    #[test]
    fn compose_sorts_durable_ids_and_keeps_titles() {
        let ids = compose_session_catalog(
            vec![
                summary("sess-z", Some("zulu")),
                summary("sess-a", None),
                summary("sess-m", Some("mike")),
            ],
            None,
            None,
        );
        assert_eq!(
            ids,
            [
                dto("sess-a", None),
                dto("sess-m", Some("mike")),
                dto("sess-z", Some("zulu")),
            ]
        );
    }

    #[test]
    fn compose_merges_uncommitted_current_once() {
        let ids = compose_session_catalog(
            vec![summary("sess-z", None), summary("sess-a", None)],
            Some("sess-service-1"),
            None,
        );
        assert_eq!(
            ids.iter()
                .map(|s| s.session_id.as_str())
                .collect::<Vec<_>>(),
            ["sess-a", "sess-service-1", "sess-z"]
        );
    }

    #[test]
    fn compose_does_not_duplicate_current_already_in_store() {
        let ids = compose_session_catalog(
            vec![summary("sess-a", None), summary("sess-b", None)],
            Some("sess-a"),
            None,
        );
        assert_eq!(
            ids.iter()
                .map(|s| s.session_id.as_str())
                .collect::<Vec<_>>(),
            ["sess-a", "sess-b"]
        );
    }

    #[test]
    fn compose_empty_store_without_current_is_empty() {
        assert!(compose_session_catalog(Vec::new(), None, None).is_empty());
    }

    #[test]
    fn compose_empty_store_keeps_only_current() {
        assert_eq!(
            compose_session_catalog(Vec::new(), Some("sess-service-1"), None)
                .iter()
                .map(|s| s.session_id.as_str())
                .collect::<Vec<_>>(),
            ["sess-service-1"]
        );
    }

    #[test]
    fn compose_merges_pending_load_without_duplicating() {
        let ids = compose_session_catalog(
            vec![summary("sess-a", None)],
            Some("sess-a"),
            Some("sess-pending"),
        );
        assert_eq!(
            ids.iter()
                .map(|s| s.session_id.as_str())
                .collect::<Vec<_>>(),
            ["sess-a", "sess-pending"]
        );
    }

    #[test]
    fn catalog_errors_are_not_empty_success() {
        assert_eq!(
            catalog_error_reason(&SessionError::store_busy("queue full")),
            "session catalog is busy: queue full"
        );
        assert_eq!(
            catalog_error_reason(&SessionError::store_unavailable("actor stopped")),
            "session catalog is unavailable: actor stopped"
        );
    }

    #[tokio::test]
    async fn rename_appends_a_title_and_refuses_unknown_sessions() {
        let store = MemorySessionStore::new();
        block_on(store.commit(SessionTransaction::linear(
            SessionId::new("sess-a"),
            SessionRevision::ZERO,
            vec![
                SessionEntryKind::OperationStarted {
                    operation_id: OperationId::new("op-1"),
                },
                SessionEntryKind::TurnStarted {
                    operation_id: OperationId::new("op-1"),
                    turn_id: TurnId::new("turn-1"),
                },
                SessionEntryKind::UserMessage {
                    turn_id: TurnId::new("turn-1"),
                    parts: SessionUserPart::text_parts("hello"),
                },
            ],
        )))
        .expect("seed");

        rename_session_once(&store, "sess-a", "renamed")
            .await
            .expect("rename");
        let summaries = block_on(store.list_session_summaries()).expect("summaries");
        assert_eq!(summaries[0].title.as_deref(), Some("renamed"));

        let error = rename_session_once(&store, "sess-missing", "nope")
            .await
            .expect_err("unknown");
        assert!(error.contains("unknown session"), "{error}");
    }

    #[test]
    fn process_cache_catalog_is_deleted() {
        let needle = ["known", "sessions"].join("_");
        for source in [
            include_str!("catalog.rs"),
            include_str!("mod.rs"),
            include_str!("commands.rs"),
        ] {
            let production = source.split("#[cfg(test)]").next().unwrap_or(source);
            assert!(
                !production.contains(&needle),
                "{needle} must stay deleted from the service actor"
            );
        }
    }
}
