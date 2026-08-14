//! Pure cross-layer mapping: runtime <-> kernel <-> session <-> model.
//!
//! Every function here is a stateless projection between the vocabularies
//! of adjacent layers; nothing in this module performs IO or publishes
//! events.

pub(crate) mod entries;
pub(crate) mod failure;
pub(crate) mod messages;
pub(crate) mod parts;
pub(crate) mod tool;
