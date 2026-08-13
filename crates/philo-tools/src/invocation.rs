use crate::definition::is_json_object;

/// Validated JSON object passed to a handler.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolArguments {
    raw: String,
}

impl ToolArguments {
    /// Parses and validates that input is a JSON object.
    pub fn parse(raw: impl Into<String>) -> Result<Self, String> {
        let raw = raw.into();
        if !is_json_object(&raw) {
            return Err("arguments must be a JSON object".to_owned());
        }
        Ok(Self { raw })
    }
    pub fn as_str(&self) -> &str {
        &self.raw
    }
}

/// A model-originated call, retaining raw arguments for durable auditability.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolInvocation {
    call_id: String,
    name: String,
    raw_arguments: String,
}

impl ToolInvocation {
    pub fn new(
        call_id: impl Into<String>,
        name: impl Into<String>,
        raw_arguments: impl Into<String>,
    ) -> Self {
        Self {
            call_id: call_id.into(),
            name: name.into(),
            raw_arguments: raw_arguments.into(),
        }
    }
    pub fn call_id(&self) -> &str {
        &self.call_id
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn raw_arguments(&self) -> &str {
        &self.raw_arguments
    }
}
