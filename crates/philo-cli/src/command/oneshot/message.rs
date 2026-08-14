//! Single-shot message intake: image loading, media-type inference, and
//! ordered multipart construction.

use std::path::Path;

use philo_agent_runtime::{UserMessage, UserPart};

use crate::args::Cli;
use crate::error::UsageError;

/// Builds the multipart message: image parts first, text last.
pub(super) fn build(cli: &Cli, message: &str) -> Result<UserMessage, UsageError> {
    let mut parts = Vec::with_capacity(cli.image.len() + 1);
    for path in &cli.image {
        parts.push(load_image_part(path)?);
    }
    parts.push(UserPart::Text(message.to_owned()));
    UserMessage::from_parts(parts)
        .map_err(|error| UsageError::new(format!("invalid message: {error:?}")))
}

/// Reads one image verbatim and infers its concrete media type.
fn load_image_part(path: &Path) -> Result<UserPart, UsageError> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .ok_or_else(|| {
            UsageError::new(format!(
                "cannot infer an image media type: '{}' has no usable extension",
                path.display()
            ))
        })?;
    let media_type = media_type_for_extension(extension).ok_or_else(|| {
        UsageError::new(format!(
            "unsupported image extension '.{extension}' for '{}': expected png, jpg, jpeg, \
             gif, or webp",
            path.display()
        ))
    })?;
    let bytes = std::fs::read(path).map_err(|error| {
        UsageError::new(format!(
            "cannot read image '{}': {}",
            path.display(),
            error.kind()
        ))
    })?;
    Ok(UserPart::Image {
        media_type: media_type.to_owned(),
        bytes,
    })
}

fn media_type_for_extension(extension: &str) -> Option<&'static str> {
    match extension.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_extensions_map_case_insensitively() {
        assert_eq!(media_type_for_extension("PNG"), Some("image/png"));
        assert_eq!(media_type_for_extension("jpeg"), Some("image/jpeg"));
        assert_eq!(media_type_for_extension("jpg"), Some("image/jpeg"));
        assert_eq!(media_type_for_extension("webp"), Some("image/webp"));
        assert_eq!(media_type_for_extension("bmp"), None);
    }

    #[test]
    fn image_bytes_pass_through_verbatim() {
        let dir = std::env::temp_dir().join(format!("philo-cli-image-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create dir");
        let path = dir.join("probe.png");
        std::fs::write(&path, [0x89, 0x50, 0x4E, 0x47, 0x00, 0xFF]).expect("write");

        let part = load_image_part(&path).expect("loads");
        let UserPart::Image { media_type, bytes } = part else {
            panic!("expected an image part");
        };
        assert_eq!(media_type, "image/png");
        assert_eq!(bytes, [0x89, 0x50, 0x4E, 0x47, 0x00, 0xFF]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_and_unknown_extension_are_usage_errors() {
        assert!(load_image_part(Path::new("no-such-file.png")).is_err());
        assert!(load_image_part(Path::new("document.pdf")).is_err());
        assert!(load_image_part(Path::new("no-extension")).is_err());
    }
}
