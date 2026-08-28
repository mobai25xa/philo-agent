//! Immutable generation snapshot frozen into each admitted operation.

use crate::{GenerationId, ModelPort, RuntimeConfig, ToolPort};
use std::sync::Arc;

/// Display metadata for a generation. Must not contain secrets.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GenerationDisplay {
    /// Owning provider id (display), e.g. `"openai"`. `None` only for legacy
    /// generations assembled before the field was tracked.
    pub provider: Option<String>,
    /// User-facing model name (display_name or wire name).
    pub model_name: String,
    /// Stable model identity (`{provider}/{model}` wire name). Persisted as
    /// the durable identity; `model_name` may be a friendlier alias.
    pub model_id: String,
    /// Whether the model accepts image input parts. Submits carrying image
    /// attachments are rejected while this is `false`.
    pub image_input: bool,
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
