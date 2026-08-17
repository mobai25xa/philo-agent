//! On-disk schema v2 records and their explicit mapping to `philo-session`.
//!
//! Records remain owned by this crate, while the codec isolates conversion
//! from the durable format definition. Entry and parent IDs are always
//! persisted as opaque strings.

mod codec;
mod record;

pub(crate) use codec::{PendingArtifact, decode_entry, encode_entry};
pub(crate) use record::TransactionRecord;

/// Envelope schema version written by this crate.
pub(crate) const SCHEMA_VERSION: u64 = 2;
