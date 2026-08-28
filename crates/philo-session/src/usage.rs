//! Durable token-usage snapshot recorded at each settled turn boundary.
//!
//! `philo-session` keeps zero dependencies, so this type mirrors the
//! runtime's `TokenUsage` and the service's `FrontendTokenUsage` by shape
//! rather than by reference. All three share the same five optional `u64`
//! fields; the mapping is structural and lossless.

/// Per-turn token accounting recorded with [`crate::SessionEntryKind::OperationSettled`].
///
/// All fields are optional; providers report what they know. Only the latest
/// settled turn's usage is surfaced through [`crate::SessionContextView`];
/// earlier turns' usage remains durable but is not projected.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SessionTokenUsage {
    /// Input tokens billed for this turn's model call(s).
    pub input_tokens: Option<u64>,
    /// Output tokens billed for this turn's model call(s).
    pub output_tokens: Option<u64>,
    /// Cache-read tokens, when reported.
    pub cache_read_tokens: Option<u64>,
    /// Cache-write tokens, when reported.
    pub cache_write_tokens: Option<u64>,
    /// Reasoning tokens, when reported.
    pub reasoning_tokens: Option<u64>,
}

impl SessionTokenUsage {
    /// Derives input plus output tokens when both are known.
    pub fn total_tokens(&self) -> Option<u64> {
        self.input_tokens?.checked_add(self.output_tokens?)
    }
}
