//! Current `RuntimeGeneration` ownership. Failed installs keep the old Arc.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use philo_agent_runtime::{
    GenerationDisplay, GenerationId, ModelPort, RuntimeConfig, RuntimeGeneration,
};
use philo_tools::ToolPort;

use crate::frontend::snapshot::FrontendGeneration;
use crate::ids::FrontendRequestId;
use crate::mapping;

/// Background assembly request produced by `InstallModel`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssembleRequest {
    /// Requested model name.
    pub name: String,
}

/// Successful assembly result. The service assigns the new [`GenerationId`].
pub struct AssembledGeneration {
    /// Newly constructed model port.
    pub model: Arc<dyn ModelPort>,
    /// Tool port to freeze into the generation.
    pub tools: Arc<dyn ToolPort>,
    /// Runtime config to freeze into the generation.
    pub runtime_config: RuntimeConfig,
    /// User-facing model name. Never a secret.
    pub model_name: String,
}

/// Why generation assembly failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssembleError {
    /// Stable diagnostic text.
    pub message: String,
}

impl AssembleError {
    /// Creates an assembly error.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Injected by CLI (Wave 2) or tests. The service crate does not build models.
pub trait GenerationAssembler: Send + Sync {
    /// Constructs a candidate generation in the background.
    fn assemble(
        &self,
        request: AssembleRequest,
    ) -> Pin<Box<dyn Future<Output = Result<AssembledGeneration, AssembleError>> + Send + '_>>;
}

/// Current generation cell. Install is atomic; failures leave the previous Arc.
pub(crate) struct CurrentGeneration {
    current: Arc<RuntimeGeneration>,
    latest_install: Option<FrontendRequestId>,
    seq: u64,
}

impl CurrentGeneration {
    pub(crate) fn new(current: Arc<RuntimeGeneration>) -> Self {
        Self {
            current,
            latest_install: None,
            seq: 0,
        }
    }

    pub(crate) fn current(&self) -> Arc<RuntimeGeneration> {
        self.current.clone()
    }

    pub(crate) fn display(&self) -> FrontendGeneration {
        mapping::frontend_generation(&self.current)
    }

    pub(crate) fn note_install(&mut self, request_id: FrontendRequestId) {
        if self
            .latest_install
            .is_none_or(|latest| request_id >= latest)
        {
            self.latest_install = Some(request_id);
        }
    }

    pub(crate) fn is_current_install(&self, request_id: FrontendRequestId) -> bool {
        self.latest_install
            .is_none_or(|latest| request_id == latest)
    }

    pub(crate) fn next_id(&mut self) -> GenerationId {
        self.seq += 1;
        GenerationId::new(format!("generation-{}", self.seq))
    }

    /// Swaps on success when `request_id` is still the latest install.
    /// Returns `true` when the new generation became current.
    pub(crate) fn install_success(
        &mut self,
        request_id: FrontendRequestId,
        assembled: AssembledGeneration,
    ) -> Option<Arc<RuntimeGeneration>> {
        if !self.is_current_install(request_id) {
            return None;
        }
        let generation_id = self.next_id();
        let next = Arc::new(RuntimeGeneration {
            generation_id,
            model: assembled.model,
            tools: assembled.tools,
            runtime_config: assembled.runtime_config,
            display: GenerationDisplay {
                model_name: assembled.model_name,
            },
        });
        self.current = next.clone();
        Some(next)
    }

    /// Records a failed install. The previous generation stays current.
    pub(crate) fn install_failure(&mut self, request_id: FrontendRequestId) -> bool {
        self.is_current_install(request_id)
    }

    /// Installs a same-ports generation that differs only by reasoning effort.
    pub(crate) fn install_reasoning(
        &mut self,
        effort: philo_agent_runtime::ReasoningEffort,
    ) -> Arc<RuntimeGeneration> {
        let current = self.current();
        let mut runtime_config = current.runtime_config.clone();
        runtime_config.generation.reasoning_effort = Some(effort);
        let generation_id = self.next_id();
        let next = Arc::new(RuntimeGeneration {
            generation_id,
            model: current.model.clone(),
            tools: current.tools.clone(),
            runtime_config,
            display: current.display.clone(),
        });
        self.current = next.clone();
        next
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{UnavailableModel, empty_tools, test_generation};

    fn assembled(name: &str) -> AssembledGeneration {
        AssembledGeneration {
            model: Arc::new(UnavailableModel),
            tools: empty_tools(),
            runtime_config: RuntimeConfig::default(),
            model_name: name.to_owned(),
        }
    }

    fn cell() -> CurrentGeneration {
        CurrentGeneration::new(test_generation("base"))
    }

    #[test]
    fn failure_keeps_previous_generation() {
        let mut cell = cell();
        let before = cell.current();
        cell.note_install(FrontendRequestId::new(1));
        assert!(cell.install_failure(FrontendRequestId::new(1)));
        assert!(Arc::ptr_eq(&before, &cell.current()));
        assert_eq!(cell.display().model_name, "base");
    }

    #[test]
    fn stale_success_does_not_overwrite_newer_request() {
        let mut cell = cell();
        cell.note_install(FrontendRequestId::new(1));
        cell.note_install(FrontendRequestId::new(2));
        assert!(
            cell.install_success(FrontendRequestId::new(2), assembled("fast"))
                .is_some()
        );
        assert!(
            cell.install_success(FrontendRequestId::new(1), assembled("slow"))
                .is_none()
        );
        assert_eq!(cell.display().model_name, "fast");
    }
}
