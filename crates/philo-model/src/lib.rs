//! Real `ModelPort` adapter over the root `philo` LLM SDK.
//!
//! `philo-model` maps the runtime's immutable `ModelCallSnapshot` onto an SDK
//! `ModelRequest`, normalizes the SDK streaming events back into the runtime
//! `ModelEvent` vocabulary, and normalizes every SDK failure into `ModelError`.
//!
//! Assembly is explicit: provider, protocol, and model are runtime
//! configuration expressed as an SDK `CallTarget`; credentials come from an
//! environment variable; retry and timeout policies are assembly-time SDK
//! configuration. Test transports are injected through the SDK `Transport`
//! extension point and never appear in this crate's production code.

mod adapter;
mod assemble;
mod error;
mod headers;
mod replay;
mod request;
mod stream;

pub use adapter::PhiloModelAdapter;
pub use assemble::{AdapterBuildError, ModelProtocol, PhiloModelBuilder};
pub use headers::{DEFAULT_USER_AGENT, ModelRequestHeaderError, ModelRequestHeaders};

// Assembly-time SDK vocabulary re-exported for adapter callers.
pub use philo::api::extension::Transport;
pub use philo::api::stable::{CallTarget, PhiloClient, RetryMode, RetryPolicy, TimeoutPolicy};
