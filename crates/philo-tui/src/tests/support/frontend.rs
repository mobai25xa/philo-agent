//! Helpers for constructing frontend updates in tests.

use philo_agent_service::{FrontendEpoch, FrontendRevision, FrontendUpdate, FrontendUpdateKind};

pub(crate) fn frontend_update(revision: u64, kind: FrontendUpdateKind) -> FrontendUpdate {
    FrontendUpdate {
        epoch: FrontendEpoch::INITIAL,
        revision: FrontendRevision::new(revision),
        request_id: None,
        kind,
    }
}
