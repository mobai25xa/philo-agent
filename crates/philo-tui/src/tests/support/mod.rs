//! Test-only frontend fixtures.

mod fixtures;
mod frontend;

pub(crate) use fixtures::{empty_session_view, image_session_view, session_view};
pub(crate) use frontend::frontend_update;
