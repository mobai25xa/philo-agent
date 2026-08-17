//! Frontend identity types. TUI never imports Runtime/Session identifiers.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

/// Monotonic service-state revision carried on every update and snapshot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FrontendRevision(u64);

impl FrontendRevision {
    /// Revision before any service state change.
    pub const ZERO: Self = Self(0);

    /// Creates a revision from its numeric representation.
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the numeric representation.
    pub const fn get(self) -> u64 {
        self.0
    }

    pub(crate) fn bump(&mut self) -> Self {
        self.0 = self.0.saturating_add(1);
        *self
    }
}

impl fmt::Display for FrontendRevision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Identity of one frontend command. Late results of a smaller id are discarded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FrontendRequestId(u64);

impl FrontendRequestId {
    /// Sentinel used when a convenience helper could not enqueue work.
    pub const INVALID: Self = Self(0);

    /// Creates a request id from its numeric representation.
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the numeric representation.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// True when this id was never accepted onto a service lane.
    pub const fn is_valid(self) -> bool {
        self.0 != 0
    }
}

impl fmt::Display for FrontendRequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Service/runtime epoch. TUI discards updates from a previous epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FrontendEpoch(u64);

impl FrontendEpoch {
    /// First epoch of a freshly started service.
    pub const INITIAL: Self = Self(1);

    /// Creates an epoch from its numeric representation.
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the numeric representation.
    pub const fn get(self) -> u64 {
        self.0
    }

    pub(crate) fn bump(&mut self) -> Self {
        self.0 = self.0.saturating_add(1);
        *self
    }
}

impl Default for FrontendEpoch {
    fn default() -> Self {
        Self::INITIAL
    }
}

impl fmt::Display for FrontendEpoch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Identifies one attached frontend instance (TUI process or restart).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FrontendInstanceId(String);

impl FrontendInstanceId {
    /// Creates an instance id.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Returns the raw id.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for FrontendInstanceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Process-local allocator for [`FrontendRequestId`].
#[derive(Debug)]
pub(crate) struct RequestIdSource {
    next: AtomicU64,
}

impl RequestIdSource {
    pub(crate) fn new() -> Self {
        Self {
            next: AtomicU64::new(1),
        }
    }

    pub(crate) fn next(&self) -> FrontendRequestId {
        FrontendRequestId(self.next.fetch_add(1, Ordering::Relaxed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_ids_are_monotonic_and_nonzero() {
        let source = RequestIdSource::new();
        let first = source.next();
        let second = source.next();
        assert!(first.is_valid());
        assert!(second > first);
    }

    #[test]
    fn revision_and_epoch_bump() {
        let mut revision = FrontendRevision::ZERO;
        assert_eq!(revision.bump().get(), 1);
        let mut epoch = FrontendEpoch::INITIAL;
        assert_eq!(epoch.bump().get(), 2);
    }
}
