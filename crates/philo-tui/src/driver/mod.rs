//! Runtime orchestration and host-backed effect execution.

mod events;
pub(crate) mod host_effects;
pub(crate) mod media;
mod run;
mod scrollback;

pub use run::run;
