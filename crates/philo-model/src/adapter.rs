use std::sync::Arc;

use philo::api::stable as sdk;
use philo_agent_runtime::{
    ModelCallSnapshot, ModelError, ModelEventStream, ModelPort, RuntimeFuture,
};

use crate::assemble::{ModelContinuationPolicy, ModelProtocol, PhiloModelBuilder};
use crate::error::model_error;
use crate::replay::{MemoryModelReplayStore, ModelReplayStore, ReplayCoordinator};
use crate::request::map_request;
use crate::stream::NormalizedStream;

/// `ModelPort` implementation backed by a `PhiloClient` and one `CallTarget`.
///
/// Each `start` maps the snapshot, opens exactly one SDK call, and returns
/// exactly one normalized event stream. SDK attempt-level retries stay inside
/// the SDK call and are invisible to the kernel; the adapter never retries or
/// replays a finished stream and never persists any fact.
///
/// Provider replay snapshots are restored through a narrow injected store;
/// they never enter the provider-neutral session log. Compatible protocols
/// that expose reasoning without a serializable token retain only a
/// same-process, same-turn fallback.
pub struct PhiloModelAdapter {
    client: sdk::PhiloClient,
    target: sdk::CallTarget,
    replay: Arc<ReplayCoordinator>,
    continuation_policy: ModelContinuationPolicy,
}

impl PhiloModelAdapter {
    /// Wraps an already assembled client and target.
    pub fn new(client: sdk::PhiloClient, target: sdk::CallTarget) -> Self {
        Self::with_replay_store(client, target, Arc::new(MemoryModelReplayStore::default()))
    }

    /// Wraps a client and target with an explicit replay sidecar store.
    pub fn with_replay_store(
        client: sdk::PhiloClient,
        target: sdk::CallTarget,
        replay_store: Arc<dyn ModelReplayStore>,
    ) -> Self {
        Self::with_configuration(
            client,
            target,
            replay_store,
            ModelContinuationPolicy::StatelessLocalReplay,
        )
    }

    pub(crate) fn with_configuration(
        client: sdk::PhiloClient,
        target: sdk::CallTarget,
        replay_store: Arc<dyn ModelReplayStore>,
        continuation_policy: ModelContinuationPolicy,
    ) -> Self {
        Self {
            client,
            target,
            replay: Arc::new(ReplayCoordinator::new(replay_store)),
            continuation_policy,
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
            let server_continuation = request.persist_replay
                && self.continuation_policy
                    == ModelContinuationPolicy::PreferPreviousResponseIdWithLocalFallback;
            let replayed = self
                .replay
                .load(&self.client, &self.target, &request, server_continuation)
                .await?;
            let mut mapped = map_request(&request, native_error_status, &replayed)?;
            let continuing =
                server_continuation && replayed.has_continuation() && mapped.continuation.is_some();
            if server_continuation && mapped.continuation.is_none() {
                mapped.continuation = Some(sdk::ResponseContinuation::start());
            }

            let first = self
                .client
                .call(&self.target, mapped.clone(), sdk::CallOptions::default())
                .await;
            let (call, retain_response_id) = match first {
                Ok(call) => (call, server_continuation),
                Err(error)
                    if continuing
                        && error.continuation_failure()
                            == Some(sdk::ContinuationFailure::PreviousResponseUnavailable) =>
                {
                    self.replay
                        .invalidate_continuation(request.session_id.as_str(), &replayed)
                        .await;
                    tracing::warn!(
                        code = "previous_response_unavailable",
                        fallback_attempt = 1_u8,
                        "stored response chain unavailable; retrying once with local replay"
                    );
                    mapped.continuation = None;
                    let call = self
                        .client
                        .call(&self.target, mapped, sdk::CallOptions::default())
                        .await
                        .map_err(|fallback| model_error(&fallback))?;
                    (call, false)
                }
                Err(error) => return Err(model_error(&error)),
            };
            Ok(Box::new(NormalizedStream::new(
                call,
                self.client.clone(),
                self.target.clone(),
                self.replay.clone(),
                request,
                retain_response_id,
            )) as Box<dyn ModelEventStream>)
        })
    }
}
