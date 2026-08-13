use philo::api::stable::PhiloError;
use philo_agent_runtime::ModelError;

/// Normalizes any SDK failure into the runtime `ModelError`, carrying a
/// redacted kind/stage summary. Every `PhiloErrorKind` takes this single
/// path; no new runtime failure path is introduced.
pub(crate) fn model_error(error: &PhiloError) -> ModelError {
    ModelError::new(format!(
        "philo model call failed: kind={:?} stage={:?} code={}",
        error.kind(),
        error.context().stage(),
        error.code()
    ))
}
