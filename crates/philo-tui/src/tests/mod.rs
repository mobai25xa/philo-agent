//! Crate-internal test support and cross-layer fixtures.

mod attachments;
mod m12_flow;
pub(crate) mod support;

macro_rules! assert_tui_snapshot {
    ($name:literal, $value:expr) => {
        insta::with_settings!({
            snapshot_path => "../tests/snapshots",
            prepend_module_to_snapshot => false,
        }, {
            insta::assert_snapshot!($name, $value);
        });
    };
}

pub(crate) use assert_tui_snapshot;
