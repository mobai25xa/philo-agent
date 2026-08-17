//! Outbound runtime-to-SDK request mapping.
//!
//! This module owns only the assembly order. Message history, generation
//! settings, and tool exposure are mapped by focused child modules.

mod generation;
mod messages;
mod tools;

use philo::api::stable as sdk;
use philo_agent_runtime::{ModelCallSnapshot, ModelError};

use crate::replay::ReplayHistory;

/// Maps an immutable `ModelCallSnapshot` onto a provider-neutral SDK request.
pub(crate) fn map_request(
    snapshot: &ModelCallSnapshot,
    native_error_status: bool,
    replayed: &ReplayHistory,
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
        snapshot.max_parallel_tool_calls,
    )?;
    if let Ok(session) = sdk::CacheSessionId::new(snapshot.session_id.as_str()) {
        request.cache_session = Some(session);
    }
    request.cache_retention = sdk::CacheRetention::Short;
    Ok(request)
}
