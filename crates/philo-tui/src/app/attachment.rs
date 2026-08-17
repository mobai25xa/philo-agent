//! Attachments waiting for the next message.
//!
//! Pure state: `/image` registers a path verbatim and `Ctrl+V` hands over
//! bytes the driver already decoded. Filesystem and clipboard access belong
//! to the platform layer, resolution to the driver.

/// One image queued for the next user message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PendingAttachment {
    /// A path registered by `/image`, read when the message is sent.
    Path(String),
    /// An image already decoded (clipboard, or a path read on a previous
    /// send attempt).
    Image {
        media_type: String,
        bytes: Vec<u8>,
        /// Where it came from, for the echo.
        origin: String,
    },
}

impl PendingAttachment {
    pub(crate) fn label(&self) -> String {
        match self {
            Self::Path(path) => path.clone(),
            Self::Image {
                media_type,
                bytes,
                origin,
            } => format!("{origin} ({media_type}, {})", human_bytes(bytes.len())),
        }
    }
}

/// The queue shown above the input and drained on submit.
#[derive(Debug, Default)]
pub(crate) struct Attachments {
    items: Vec<PendingAttachment>,
}

impl Attachments {
    pub(crate) fn push(&mut self, attachment: PendingAttachment) {
        self.items.push(attachment);
    }

    pub(crate) fn extend(&mut self, attachments: Vec<PendingAttachment>) {
        self.items.extend(attachments);
    }

    /// Hands the queue to the send path; the queue starts empty again.
    pub(crate) fn take(&mut self) -> Vec<PendingAttachment> {
        std::mem::take(&mut self.items)
    }

    pub(crate) fn len(&self) -> usize {
        self.items.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub(crate) fn labels(&self) -> Vec<String> {
        self.items.iter().map(PendingAttachment::label).collect()
    }

    /// The row shown above the input while attachments wait.
    pub(crate) fn summary(&self) -> Option<String> {
        if self.items.is_empty() {
            return None;
        }
        Some(self.labels().join("  ·  "))
    }
}

/// Byte counts as the echo shows them.
pub(crate) fn human_bytes(len: usize) -> String {
    #[allow(clippy::cast_precision_loss)]
    let value = len as f64;
    if len < 1024 {
        format!("{len} B")
    } else if len < 1024 * 1024 {
        format!("{:.1} KB", value / 1024.0)
    } else {
        format!("{:.1} MB", value / (1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clipboard_image(bytes: usize) -> PendingAttachment {
        PendingAttachment::Image {
            media_type: "image/png".to_owned(),
            bytes: vec![0; bytes],
            origin: "clipboard image".to_owned(),
        }
    }

    #[test]
    fn labels_name_paths_and_describe_decoded_images() {
        let mut attachments = Attachments::default();
        attachments.push(PendingAttachment::Path("shots/a.png".to_owned()));
        attachments.push(clipboard_image(2048));
        assert_eq!(
            attachments.labels(),
            [
                "shots/a.png".to_owned(),
                "clipboard image (image/png, 2.0 KB)".to_owned(),
            ]
        );
        assert_eq!(
            attachments.summary(),
            Some("shots/a.png  ·  clipboard image (image/png, 2.0 KB)".to_owned())
        );
    }

    #[test]
    fn taking_the_queue_empties_it() {
        let mut attachments = Attachments::default();
        attachments.push(clipboard_image(4));
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments.take().len(), 1);
        assert!(attachments.is_empty());
        assert_eq!(attachments.summary(), None);
    }

    #[test]
    fn byte_counts_scale_by_unit() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(3 * 1024 * 1024), "3.0 MB");
    }
}
