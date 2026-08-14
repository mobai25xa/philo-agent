//! User and assistant message value types.

/// One part of a multi-part user message. The runtime never interprets or
/// modifies image bytes; they map byte-for-byte down the explicit chain
/// (runtime -> kernel -> session) and into model calls.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UserPart {
    Text(String),
    Image { media_type: String, bytes: Vec<u8> },
}

/// Why constructing a [`UserMessage`] was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvalidUserMessage {
    EmptyParts,
    EmptyTextPart,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserMessage {
    parts: Vec<UserPart>,
}
impl UserMessage {
    /// Plain-text convenience constructor.
    ///
    /// # Panics
    ///
    /// Panics when `text` is empty; use [`UserMessage::from_parts`] for
    /// fallible construction.
    pub fn new(text: impl Into<String>) -> Self {
        Self::from_parts(vec![UserPart::Text(text.into())])
            .expect("plain-text user message must not be empty")
    }
    /// Multi-part constructor: parts must be non-empty and text parts must
    /// not be empty strings. Image-only messages are valid.
    pub fn from_parts(parts: Vec<UserPart>) -> Result<Self, InvalidUserMessage> {
        if parts.is_empty() {
            return Err(InvalidUserMessage::EmptyParts);
        }
        for part in &parts {
            if matches!(part, UserPart::Text(text) if text.is_empty()) {
                return Err(InvalidUserMessage::EmptyTextPart);
            }
        }
        Ok(Self { parts })
    }
    pub fn parts(&self) -> &[UserPart] {
        &self.parts
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssistantMessage {
    pub(crate) content: String,
}
impl AssistantMessage {
    pub fn content(&self) -> &str {
        &self.content
    }
}
