//! The public runtime entry point: construction, availability, admission.

use crate::engine::{self, EngineContext};
use crate::operation::{Admission, OperationHandle, OperationShared, Scheduler};
use crate::{
    AgentAvailability, AgentError, AgentEvent, IdSource, OperationPhase, RuntimeConfig, SessionId,
    UserMessage,
};
use philo_session as session;
use philo_tools::{ToolPort, ToolRegistry};
use std::sync::Arc;

pub struct AgentRuntime {
    model: Arc<dyn crate::ModelPort>,
    sessions: Arc<dyn session::SessionStore>,
    ids: Arc<dyn IdSource>,
    tools: Arc<dyn ToolPort>,
    config: RuntimeConfig,
    scheduler: Arc<Scheduler>,
}

impl AgentRuntime {
    /// M1-compatible constructor with an empty immutable registry.
    pub fn new(
        model: Arc<dyn crate::ModelPort>,
        sessions: Arc<dyn session::SessionStore>,
        ids: Arc<dyn IdSource>,
        config: RuntimeConfig,
    ) -> Self {
        Self::with_tools(
            model,
            sessions,
            ids,
            config,
            Arc::new(ToolRegistry::empty()),
        )
    }

    /// Constructs a runtime with a frozen tool port.
    pub fn with_tools(
        model: Arc<dyn crate::ModelPort>,
        sessions: Arc<dyn session::SessionStore>,
        ids: Arc<dyn IdSource>,
        config: RuntimeConfig,
        tools: Arc<dyn ToolPort>,
    ) -> Self {
        Self {
            model,
            sessions,
            ids,
            tools,
            config,
            scheduler: Scheduler::new(),
        }
    }

    /// Read-only availability observation: `Busy` names the operation the
    /// runtime is actively driving; queued operations are observed through
    /// their own handles.
    pub fn availability(&self) -> AgentAvailability {
        self.scheduler.availability()
    }

    fn engine_context(&self) -> EngineContext {
        EngineContext {
            model: self.model.clone(),
            sessions: self.sessions.clone(),
            tools: self.tools.clone(),
            config: self.config.clone(),
            scheduler: self.scheduler.clone(),
        }
    }

    /// Admits one user prompt and returns its handle immediately (M10:
    /// acceptance never drives the work inside `prompt()`). The operation is
    /// driven while the handle is polled (`next_event` / `wait`); events are
    /// consumable the moment they are published, and `cancel()` is reachable
    /// at any point while the operation runs.
    ///
    /// While another operation is active the new operation queues FIFO with
    /// phase `Queued`; queue entries are never persisted.
    pub async fn prompt(
        &self,
        session_id: SessionId,
        user_message: UserMessage,
    ) -> Result<OperationHandle, AgentError> {
        let operation_id = self.ids.next_operation_id();
        let turn_id = self.ids.next_turn_id();
        match self.scheduler.admit(&operation_id) {
            Admission::Direct => {
                let shared = Arc::new(OperationShared::new(
                    operation_id,
                    turn_id,
                    self.scheduler.clone(),
                    OperationPhase::PreparingTurn,
                ));
                let engine = Box::pin(engine::drive_claimed(
                    self.engine_context(),
                    shared.clone(),
                    session_id,
                    user_message,
                ));
                Ok(OperationHandle::with_engine(shared, engine))
            }
            Admission::Queued => {
                let shared = Arc::new(OperationShared::new(
                    operation_id.clone(),
                    turn_id,
                    self.scheduler.clone(),
                    OperationPhase::Queued,
                ));
                shared.publish(AgentEvent::OperationQueued { operation_id });
                let engine = Box::pin(engine::run_queued(
                    self.engine_context(),
                    shared.clone(),
                    session_id,
                    user_message,
                ));
                Ok(OperationHandle::with_engine(shared, engine))
            }
        }
    }
}
