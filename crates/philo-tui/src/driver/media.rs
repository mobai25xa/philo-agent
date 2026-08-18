//! Turning pending attachments into frontend submit attachments.
//!
//! The app state keeps `/image` paths verbatim, so the read happens here,
//! on the way out. A file that cannot be read stops the send instead of
//! quietly dropping what the user attached.

use philo_agent_service::FrontendAttachment;

use crate::app::attachment::PendingAttachment;
use crate::platform::image_file;

#[cfg(test)]
use crate::app::transcript::TranscriptLine;

/// The outcome of resolving one message's attachments.
pub(crate) struct Resolved {
    /// Image attachments, in the order they were queued.
    pub(crate) attachments: Vec<FrontendAttachment>,
    /// The attachments that did resolve, decoded so that a retry after a
    /// refused send never re-reads the file.
    pub(crate) kept: Vec<PendingAttachment>,
    /// One message per attachment that could not be read.
    pub(crate) errors: Vec<String>,
}

pub(crate) fn resolve(attachments: Vec<PendingAttachment>) -> Resolved {
    let mut resolved = Resolved {
        attachments: Vec::new(),
        kept: Vec::new(),
        errors: Vec::new(),
    };
    for attachment in attachments {
        match attachment {
            PendingAttachment::Path(path) => match image_file::read(&path) {
                Ok((media_type, bytes)) => {
                    resolved.attachments.push(FrontendAttachment {
                        media_type: media_type.clone(),
                        bytes: bytes.clone(),
                    });
                    resolved.kept.push(PendingAttachment::Image {
                        media_type,
                        bytes,
                        origin: path,
                    });
                }
                Err(error) => resolved.errors.push(error),
            },
            PendingAttachment::Image {
                media_type,
                bytes,
                origin,
            } => {
                resolved.attachments.push(FrontendAttachment {
                    media_type: media_type.clone(),
                    bytes: bytes.clone(),
                });
                resolved.kept.push(PendingAttachment::Image {
                    media_type,
                    bytes,
                    origin,
                });
            }
        }
    }
    resolved
}

/// What the user sees when an attachment stops the send.
#[cfg(test)]
pub(crate) fn refusal_lines(errors: &[String]) -> Vec<TranscriptLine> {
    crate::app::transcript::refusal_lines_for_restore(errors, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_png(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("philo-tui-media-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create dir");
        let path = dir.join(name);
        std::fs::write(&path, [1, 2, 3, 4]).expect("write");
        path
    }

    #[test]
    fn paths_are_read_once_and_kept_decoded_for_a_retry() {
        let path = temp_png("attach.png");
        let resolved = resolve(vec![PendingAttachment::Path(
            path.to_str().expect("utf-8 path").to_owned(),
        )]);

        assert!(resolved.errors.is_empty());
        assert_eq!(
            resolved.attachments,
            [FrontendAttachment {
                media_type: "image/png".to_owned(),
                bytes: vec![1, 2, 3, 4],
            }]
        );
        let PendingAttachment::Image { origin, .. } = &resolved.kept[0] else {
            panic!("a read path is kept as decoded bytes");
        };
        assert!(origin.ends_with("attach.png"), "the echo keeps the origin");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn an_unreadable_path_is_reported_and_the_others_survive() {
        let clipboard = PendingAttachment::Image {
            media_type: "image/png".to_owned(),
            bytes: vec![9],
            origin: "clipboard image".to_owned(),
        };
        let resolved = resolve(vec![
            PendingAttachment::Path("no-such-file.png".to_owned()),
            clipboard.clone(),
        ]);

        assert_eq!(resolved.errors.len(), 1);
        assert!(resolved.errors[0].contains("cannot read image"));
        assert_eq!(resolved.kept, [clipboard]);
        assert_eq!(
            resolved.attachments.len(),
            1,
            "the readable one is still mapped"
        );
    }
}
