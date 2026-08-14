//! Filesystem intake for `/image <path>`.
//!
//! Bytes pass through untouched and the media type comes from the
//! extension — the same rule the single-shot `--image` flag follows.

use std::path::Path;

/// Maps a file extension to a concrete image media type.
fn media_type_for_extension(extension: &str) -> Option<&'static str> {
    match extension.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

/// Reads one image file into its media type and bytes.
pub(crate) fn read(path: &str) -> Result<(String, Vec<u8>), String> {
    let path = Path::new(path);
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .ok_or_else(|| {
            format!(
                "cannot infer an image media type: '{}' has no usable extension",
                path.display()
            )
        })?;
    let media_type = media_type_for_extension(extension).ok_or_else(|| {
        format!(
            "unsupported image extension '.{extension}' for '{}': expected png, jpg, jpeg, \
             gif, or webp",
            path.display()
        )
    })?;
    let bytes = std::fs::read(path)
        .map_err(|error| format!("cannot read image '{}': {}", path.display(), error.kind()))?;
    Ok((media_type.to_owned(), bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_extensions_map_case_insensitively() {
        assert_eq!(media_type_for_extension("PNG"), Some("image/png"));
        assert_eq!(media_type_for_extension("jpeg"), Some("image/jpeg"));
        assert_eq!(media_type_for_extension("bmp"), None);
    }

    #[test]
    fn bytes_pass_through_verbatim() {
        let dir = std::env::temp_dir().join(format!("philo-tui-image-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create dir");
        let path = dir.join("probe.png");
        std::fs::write(&path, [0x89, 0x50, 0x4E, 0x47, 0x00, 0xFF]).expect("write");

        let (media_type, bytes) = read(path.to_str().expect("utf-8 path")).expect("reads");
        assert_eq!(media_type, "image/png");
        assert_eq!(bytes, [0x89, 0x50, 0x4E, 0x47, 0x00, 0xFF]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_files_and_unknown_extensions_report_why() {
        assert!(
            read("no-such-file.png")
                .expect_err("missing")
                .contains("cannot read image")
        );
        assert!(
            read("document.pdf")
                .expect_err("unsupported")
                .contains("unsupported image extension")
        );
        assert!(
            read("no-extension")
                .expect_err("no extension")
                .contains("no usable extension")
        );
    }
}
