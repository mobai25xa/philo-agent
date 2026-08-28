//! Durable generation choice recorded at each settled turn boundary.
//!
//! `philo-session` keeps zero dependencies, so this type mirrors the
//! runtime's generation identity by shape rather than by reference. The
//! `model_id` is the stable wire name (`{provider}/{model}`); the
//! `reasoning_effort` is a lowercase label. Both are purely structural and
//! lossless, and the mapping to a live `RuntimeGeneration` happens in the
//! service layer at rebuild time.

/// Per-turn generation choice recorded with
/// [`crate::SessionEntryKind::OperationSettled`].
///
/// Only the latest settled turn's choice is surfaced through
/// [`crate::SessionContextView`]; earlier turns' choices remain durable
/// but are not projected. The wire name is the persistent identity; the
/// runtime `display_name` is a transient mapping and is not persisted.
///
/// `provider` carries the owning provider id so the frontend can
/// disambiguate models that share a `display_name` across providers. It
/// is `None` only for legacy entries written before the field existed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionGenerationChoice {
    /// Owning provider id (display), e.g. `"openai"`. Persists alongside the
    /// wire name so the frontend corner never routes to the wrong provider.
    pub provider: Option<String>,
    /// Stable install identity of the model (`{provider}/{model}` wire
    /// name). Used to rebuild the generation on cross-process recovery.
    pub model_id: String,
    /// Frozen reasoning effort label (lowercase, e.g. `"high"`). `None`
    /// means the turn used a non-reasoning model or the provider default.
    pub reasoning_effort: Option<String>,
}
