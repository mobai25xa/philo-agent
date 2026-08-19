//! Session catalog: durable `SessionStore::list_sessions` plus one ephemeral current session.

use philo_session::{SessionError, SessionId};

use crate::error::CommandReject;
use crate::frontend::update::FrontendUpdateKind;
use crate::ids::{FrontendEpoch, FrontendRequestId};
use crate::runtime_api::{RuntimeEvents, RuntimePort};

use super::{AgentServiceActor, ServiceTaskResult};

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
            let result = sessions.list_sessions().await;
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
        result: Result<Vec<SessionId>, SessionError>,
    ) {
        if epoch != self.epoch {
            self.feed.cancel_request(request_id);
            return;
        }
        match result {
            Ok(durable) => {
                let session_ids = compose_session_catalog(
                    durable,
                    self.snapshot.current_session.as_deref(),
                    self.snapshot.pending_load_session(),
                );
                self.emit(
                    Some(request_id),
                    FrontendUpdateKind::SessionListLoaded { session_ids },
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

/// Durable ids plus committed current and in-flight pending load, then a stable sort.
pub(super) fn compose_session_catalog(
    durable: impl IntoIterator<Item = SessionId>,
    current_session: Option<&str>,
    pending_load: Option<&str>,
) -> Vec<String> {
    let mut session_ids: Vec<String> = durable
        .into_iter()
        .map(|session_id| session_id.as_str().to_owned())
        .collect();
    for extra in [current_session, pending_load] {
        if let Some(id) = extra.filter(|id| !id.is_empty())
            && !session_ids.iter().any(|existing| existing == id)
        {
            session_ids.push(id.to_owned());
        }
    }
    session_ids.sort();
    session_ids
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
    use super::{catalog_error_reason, compose_session_catalog};
    use philo_session::{SessionError, SessionId};

    #[test]
    fn compose_sorts_durable_ids() {
        let ids = compose_session_catalog(
            [
                SessionId::new("sess-z"),
                SessionId::new("sess-a"),
                SessionId::new("sess-m"),
            ],
            None,
            None,
        );
        assert_eq!(ids, ["sess-a", "sess-m", "sess-z"]);
    }

    #[test]
    fn compose_merges_uncommitted_current_once() {
        let ids = compose_session_catalog(
            [SessionId::new("sess-z"), SessionId::new("sess-a")],
            Some("sess-service-1"),
            None,
        );
        assert_eq!(ids, ["sess-a", "sess-service-1", "sess-z"]);
    }

    #[test]
    fn compose_does_not_duplicate_current_already_in_store() {
        let ids = compose_session_catalog(
            [SessionId::new("sess-a"), SessionId::new("sess-b")],
            Some("sess-a"),
            None,
        );
        assert_eq!(ids, ["sess-a", "sess-b"]);
    }

    #[test]
    fn compose_empty_store_without_current_is_empty() {
        assert!(compose_session_catalog([], None, None).is_empty());
    }

    #[test]
    fn compose_empty_store_keeps_only_current() {
        assert_eq!(
            compose_session_catalog([], Some("sess-service-1"), None),
            ["sess-service-1"]
        );
    }

    #[test]
    fn compose_merges_pending_load_without_duplicating() {
        let ids = compose_session_catalog(
            [SessionId::new("sess-a")],
            Some("sess-a"),
            Some("sess-pending"),
        );
        assert_eq!(ids, ["sess-a", "sess-pending"]);
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
