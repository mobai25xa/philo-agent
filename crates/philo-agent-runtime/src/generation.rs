//! Immutable generation snapshot frozen into each admitted operation.

use crate::{GenerationId, ModelPort, RuntimeConfig, ToolPort};
use std::sync::Arc;

/// Display metadata for a generation. Must not contain secrets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenerationDisplay {
    pub model_name: String,
}

impl Default for GenerationDisplay {
    fn default() -> Self {
        Self {
            model_name: String::new(),
        }
    }
}

/// Immutable model/tools/config snapshot used at admission and throughout
/// the resulting operation or maintenance task.
pub struct RuntimeGeneration {
    pub generation_id: GenerationId,
    pub model: Arc<dyn ModelPort>,
    pub tools: Arc<dyn ToolPort>,
    pub runtime_config: RuntimeConfig,
    pub display: GenerationDisplay,
}

impl std::fmt::Debug for RuntimeGeneration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeGeneration")
            .field("generation_id", &self.generation_id)
            .field("display", &self.display)
            .field("runtime_config", &self.runtime_config)
            .finish_non_exhaustive()
    }
}
