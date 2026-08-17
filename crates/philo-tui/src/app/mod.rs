//! Pure interaction state, semantic actions, effects and projections.
//!
//! Protocol types live at the root (`action`, `effect`). `state` is the
//! aggregate and dispatch. Projections (`transcript`, `session`, `activity`,
//! `status`) never own I/O.

pub(crate) mod action;
pub(crate) mod activity;
pub(crate) mod attachment;
pub(crate) mod command;
pub(crate) mod effect;
pub(crate) mod input;
pub(crate) mod overlay;
pub(crate) mod session;
pub(crate) mod state;
pub(crate) mod status;
pub(crate) mod text;
pub(crate) mod transcript;
