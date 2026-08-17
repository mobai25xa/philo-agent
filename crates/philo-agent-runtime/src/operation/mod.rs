//! Operation lifecycle machinery: shared driver state and event publication.

mod publisher;
mod shared;

pub(crate) use publisher::OperationPublisher;
pub(crate) use shared::{DriverEvent, MaintenanceCancel, OperationShared};
