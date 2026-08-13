use std::sync::Arc;

use philo::api::stable as sdk;
use philo_agent_runtime::{
    ModelCallSnapshot, ModelError, ModelEventStream, ModelPort, RuntimeFuture,
};

use crate::assemble::{ModelProtocol, PhiloModelBuilder};
use crate::error::model_error;
use crate::replay::ReplayChannel;
use crate::request::map_request;
use crate::stream::NormalizedStream;

/// `ModelPort` implementation backed by a `PhiloClient` and one `CallTarget`.
///
/// Each `start` maps the snapshot, opens exactly one SDK call, and returns
/// exactly one normalized event stream. SDK attempt-level retries stay inside
/// the SDK call and are invisible to the kernel; the adapter never retries or
/// replays a finished stream and never persists any fact.
///
/// The adapter's only mutable state is the turn-scoped reasoning replay side
/// channel: providers with signed reasoning require later calls of the same
/// turn to replay the earlier calls' reasoning state verbatim. The channel
/// rotates on the first call of a new turn and never crosses turn boundaries.
pub struct PhiloModelAdapter {
    client: sdk::PhiloClient,
    target: sdk::CallTarget,
    replay: Arc<ReplayChannel>,
}

impl PhiloModelAdapter {
    /// Wraps an already assembled client and target.
    pub fn new(client: sdk::PhiloClient, target: sdk::CallTarget) -> Self {
        Self {
            client,
            target,
            replay: Arc::new(ReplayChannel::new()),
        }
    }

    /// Starts the standard assembly: protocol adapter, provider profile,
    /// environment credential, and retry/timeout policies.
    pub fn builder(
        provider: impl Into<String>,
        protocol: ModelProtocol,
        model: impl Into<String>,
        endpoint: impl Into<String>,
    ) -> PhiloModelBuilder {
        PhiloModelBuilder::new(provider, protocol, model, endpoint)
    }

    /// Returns the configured call target.
    pub fn target(&self) -> &sdk::CallTarget {
        &self.target
    }
}

impl ModelPort for PhiloModelAdapter {
    fn start<'a>(
        &'a self,
        request: ModelCallSnapshot,
    ) -> RuntimeFuture<'a, Result<Box<dyn ModelEventStream>, ModelError>> {
        Box::pin(async move {
            // Zero-network capability query: protocols without a native tool
            // result error status receive errors as plain result text.
            let effective = self
                .client
                .capabilities(&self.target)
                .map_err(|error| model_error(&error))?;
            let native_error_status = effective
                .features()
                .contains(sdk::Capability::NativeToolResultErrorStatus);
            // Rotate the replay channel to this turn and collect the earlier
            // calls' reasoning state for verbatim injection.
            let turn_key = format!(
                "{}:{}",
                request.operation_id.as_str(),
                request.turn_id.as_str()
            );
            let replayed = self.replay.begin_call(&turn_key);
            let mapped = map_request(&request, native_error_status, &replayed)?;
            let call = self
                .client
                .call(&self.target, mapped, sdk::CallOptions::default())
                .await
                .map_err(|error| model_error(&error))?;
            Ok(Box::new(NormalizedStream::new(
                call,
                self.replay.clone(),
                turn_key,
                request.model_call_index,
            )) as Box<dyn ModelEventStream>)
        })
    }
}
