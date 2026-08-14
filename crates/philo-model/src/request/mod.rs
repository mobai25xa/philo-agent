//! Outbound runtime-to-SDK request mapping.
//!
//! This module owns only the assembly order. Message history, generation
//! settings, and tool exposure are mapped by focused child modules.

mod generation;
mod messages;
mod tools;

use philo::api::stable as sdk;
use philo_agent_runtime::{ModelCallSnapshot, ModelError};

use crate::replay::CachedReasoning;

/// Maps an immutable `ModelCallSnapshot` onto a provider-neutral SDK request.
pub(crate) fn map_request(
    snapshot: &ModelCallSnapshot,
    native_error_status: bool,
    replayed: &[Vec<CachedReasoning>],
) -> Result<sdk::ModelRequest, ModelError> {
    let mut request = generation::new_request(&snapshot.generation)?;
    messages::map_messages(
        &mut request,
        &snapshot.messages,
        native_error_status,
        replayed,
    )?;
    tools::map_tools(
        &mut request,
        &snapshot.generation.tool_choice,
        &snapshot.tools,
    )?;
    Ok(request)
}
