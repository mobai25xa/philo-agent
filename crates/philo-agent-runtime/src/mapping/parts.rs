//! Multi-part user payload mapping across layers.
//!
//! Explicit per-field mapping chain: each layer owns its type, and image
//! bytes pass through byte-for-byte.

use crate::UserPart;
use philo_agent_kernel as kernel;
use philo_session as session;

pub(crate) fn kernel_user_parts(parts: &[UserPart]) -> Vec<kernel::UserPart> {
    parts
        .iter()
        .map(|part| match part {
            UserPart::Text(text) => kernel::UserPart::Text(text.clone()),
            UserPart::Image { media_type, bytes } => kernel::UserPart::Image {
                media_type: media_type.clone(),
                bytes: bytes.clone(),
            },
        })
        .collect()
}

pub(crate) fn session_user_parts(parts: &[kernel::UserPart]) -> Vec<session::SessionUserPart> {
    parts
        .iter()
        .map(|part| match part {
            kernel::UserPart::Text(text) => session::SessionUserPart::Text(text.clone()),
            kernel::UserPart::Image { media_type, bytes } => session::SessionUserPart::Image {
                media_type: media_type.clone(),
                bytes: bytes.clone(),
            },
        })
        .collect()
}

pub(super) fn runtime_parts_from_session(parts: &[session::SessionUserPart]) -> Vec<UserPart> {
    parts
        .iter()
        .map(|part| match part {
            session::SessionUserPart::Text(text) => UserPart::Text(text.clone()),
            session::SessionUserPart::Image { media_type, bytes } => UserPart::Image {
                media_type: media_type.clone(),
                bytes: bytes.clone(),
            },
        })
        .collect()
}

pub(super) fn runtime_parts_from_kernel(parts: &[kernel::UserPart]) -> Vec<UserPart> {
    parts
        .iter()
        .map(|part| match part {
            kernel::UserPart::Text(text) => UserPart::Text(text.clone()),
            kernel::UserPart::Image { media_type, bytes } => UserPart::Image {
                media_type: media_type.clone(),
                bytes: bytes.clone(),
            },
        })
        .collect()
}
