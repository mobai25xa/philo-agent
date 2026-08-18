//! Test-only frontend fixtures.

mod fixtures;
mod frontend;

pub(crate) use fixtures::{
    busy_snapshot, empty_session_view, idle_snapshot, image_session_view, session_view,
};
pub(crate) use frontend::frontend_update;
