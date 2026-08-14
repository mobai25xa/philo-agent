//! Operation lifecycle machinery: scheduling, shared state, the public
//! handle, and event publication.

mod handle;
mod publisher;
mod scheduler;
mod shared;

pub use handle::OperationHandle;
pub(crate) use publisher::OperationPublisher;
pub(crate) use scheduler::{Admission, QueueClaim, Scheduler};
pub(crate) use shared::OperationShared;
