//! Pure cross-layer mapping: runtime <-> kernel <-> session <-> model.
//!
//! Every function here is a stateless projection between the vocabularies
//! of adjacent layers; nothing in this module performs IO or publishes
//! events.

pub(crate) mod entries;
pub(crate) mod failure;
pub(crate) mod messages;
pub(crate) mod parts;
pub(crate) mod tool;

use crate::TokenUsage;
use philo_session::SessionTokenUsage;

/// Maps the runtime's `TokenUsage` into the session's durable form.
pub(crate) fn session_usage(usage: TokenUsage) -> SessionTokenUsage {
    SessionTokenUsage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_read_tokens: usage.cache_read_tokens,
        cache_write_tokens: usage.cache_write_tokens,
        reasoning_tokens: usage.reasoning_tokens,
    }
}
