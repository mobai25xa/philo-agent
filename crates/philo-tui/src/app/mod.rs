//! Pure interaction state, semantic actions, effects and projections.
//!
//! Protocol types live at the root (`action`, `effect`). `state` is the
//! aggregate and dispatch. Projections (`transcript`, `session`,
//! `run_state`, `status`) never own I/O.

pub(crate) mod action;
pub(crate) mod attachment;
pub(crate) mod cells;
pub(crate) mod command;
pub(crate) mod effect;
pub(crate) mod input;
pub(crate) mod listings;
pub(crate) mod overlay;
pub(crate) mod pacer;
pub(crate) mod prose;
pub(crate) mod run_state;
pub(crate) mod select;
pub(crate) mod session;
pub(crate) mod state;
pub(crate) mod status;
pub(crate) mod submit;
pub(crate) mod text;
pub(crate) mod tool_card;
pub(crate) mod transcript;
