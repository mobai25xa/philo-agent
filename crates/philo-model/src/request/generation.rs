use std::num::NonZeroU32;

use philo::api::stable as sdk;
use philo_agent_runtime::{GenerationConfig, ModelError, ReasoningEffort};

pub(super) fn new_request(generation: &GenerationConfig) -> Result<sdk::ModelRequest, ModelError> {
    let max_output_tokens = NonZeroU32::new(generation.max_output_tokens).ok_or_else(|| {
        ModelError::new(
            "model call configuration invalid: generation.max_output_tokens must be greater than zero",
        )
    })?;
    let mut request = sdk::ModelRequest::new(max_output_tokens);
    // OpenAI reasoning models reject sampling controls such as temperature.
    // Keep the configured value for baseline requests, but omit it whenever
    // the caller explicitly selects a reasoning effort.
    request.generation.temperature = generation
        .reasoning_effort
        .is_none()
        .then(|| f64::from(generation.temperature));
    request.reasoning = map_reasoning_config(generation.reasoning_effort);
    Ok(request)
}

/// Maps the frozen runtime reasoning setting onto the SDK request config.
/// `None` keeps the SDK default (reasoning untouched, pre-M7 request shape);
/// an unsupported effort for the resolved target is rejected by SDK
/// capability preflight and normalized into a configuration `ModelError`.
fn map_reasoning_config(effort: Option<ReasoningEffort>) -> sdk::ReasoningConfig {
    match effort {
        None => sdk::ReasoningConfig::default(),
        Some(effort) => sdk::ReasoningConfig {
            mode: sdk::ReasoningMode::Effort(match effort {
                ReasoningEffort::Minimal => sdk::ReasoningEffort::Minimal,
                ReasoningEffort::Low => sdk::ReasoningEffort::Low,
                ReasoningEffort::Medium => sdk::ReasoningEffort::Medium,
                ReasoningEffort::High => sdk::ReasoningEffort::High,
                ReasoningEffort::VeryHigh => sdk::ReasoningEffort::Xhigh,
                ReasoningEffort::Maximum => sdk::ReasoningEffort::Max,
            }),
            report: sdk::ReasoningReport::None,
        },
    }
}
