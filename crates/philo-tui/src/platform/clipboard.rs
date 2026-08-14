//! System clipboard access for `Ctrl+V` (arboard).
//!
//! System adapter only: what an image or a text payload means for the
//! pending message is the driver's call. Clipboard failures are never
//! fatal — the caller degrades to a hint.

/// What the clipboard held when the user asked for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ClipboardContent {
    Image {
        media_type: String,
        bytes: Vec<u8>,
    },
    Text(String),
    /// Nothing usable (empty, or a format we cannot attach).
    Empty,
}

/// Reads the clipboard, preferring an image over text.
///
/// The clipboard hands out raw pixels, so images are encoded to PNG here:
/// the model contract takes a real image format with its media type.
pub(crate) fn read() -> Result<ClipboardContent, String> {
    let mut clipboard = arboard::Clipboard::new().map_err(describe)?;
    match clipboard.get_image() {
        Ok(image) => {
            let width = image.width;
            let height = image.height;
            let bytes = encode_png(width, height, &image.bytes)?;
            Ok(ClipboardContent::Image {
                media_type: "image/png".to_owned(),
                bytes,
            })
        }
        Err(arboard::Error::ContentNotAvailable | arboard::Error::ConversionFailure) => {
            match clipboard.get_text() {
                Ok(text) if !text.is_empty() => Ok(ClipboardContent::Text(text)),
                Ok(_) | Err(arboard::Error::ContentNotAvailable) => Ok(ClipboardContent::Empty),
                Err(error) => Err(describe(error)),
            }
        }
        Err(error) => Err(describe(error)),
    }
}

/// Encodes RGBA8 pixels as PNG.
fn encode_png(width: usize, height: usize, rgba: &[u8]) -> Result<Vec<u8>, String> {
    let (Ok(width), Ok(height)) = (u32::try_from(width), u32::try_from(height)) else {
        return Err("clipboard image is too large to encode".to_owned());
    };
    if width == 0 || height == 0 {
        return Err("clipboard image is empty".to_owned());
    }
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4));
    if expected != Some(rgba.len()) {
        return Err("clipboard image pixels do not match its size".to_owned());
    }

    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|error| format!("cannot encode the clipboard image: {error}"))?;
        writer
            .write_image_data(rgba)
            .map_err(|error| format!("cannot encode the clipboard image: {error}"))?;
        writer
            .finish()
            .map_err(|error| format!("cannot encode the clipboard image: {error}"))?;
    }
    Ok(out)
}

fn describe(error: arboard::Error) -> String {
    match error {
        arboard::Error::ContentNotAvailable => "the clipboard is empty".to_owned(),
        arboard::Error::ClipboardNotSupported => {
            "this terminal has no reachable clipboard".to_owned()
        }
        arboard::Error::ClipboardOccupied => "the clipboard is busy".to_owned(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgba_pixels_encode_to_a_png_signature() {
        let pixels = vec![0xFFu8; 2 * 2 * 4];
        let png = encode_png(2, 2, &pixels).expect("encodes");
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        assert!(png.len() > 8);
    }

    #[test]
    fn mismatched_or_empty_images_are_reported_not_encoded() {
        assert!(encode_png(2, 2, &[0; 4]).is_err(), "short buffer");
        assert!(encode_png(0, 4, &[]).is_err(), "zero width");
    }
}
