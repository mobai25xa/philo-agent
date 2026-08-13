use std::fmt;

/// JSON-object schema metadata exposed to a model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolSchema {
    json: String,
}

impl ToolSchema {
    /// Creates a schema from its JSON representation.
    pub fn new(json: impl Into<String>) -> Result<Self, String> {
        let json = json.into();
        if !is_json_object(&json) {
            return Err("tool schema must be a JSON object".to_owned());
        }
        Ok(Self { json })
    }

    /// Returns the schema JSON exactly as registered.
    pub fn as_str(&self) -> &str {
        &self.json
    }
}

pub trait ToolSchemaInput {
    fn into_schema(self) -> Result<ToolSchema, String>;
}
impl ToolSchemaInput for ToolSchema {
    fn into_schema(self) -> Result<ToolSchema, String> {
        Ok(self)
    }
}
impl ToolSchemaInput for String {
    fn into_schema(self) -> Result<ToolSchema, String> {
        ToolSchema::new(self)
    }
}
impl ToolSchemaInput for &str {
    fn into_schema(self) -> Result<ToolSchema, String> {
        ToolSchema::new(self)
    }
}

impl Default for ToolSchema {
    fn default() -> Self {
        Self {
            json: "{}".to_owned(),
        }
    }
}

/// Factual classification of a tool's effect on the world (M10).
///
/// Metadata for external approval decorators and UIs: it describes what the
/// tool does, never what to allow. It is mandatory at construction (a
/// missing class is a compile-time error) and is not part of the
/// model-facing name/description/schema serialization surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EffectClass {
    /// Does not modify any external state.
    ReadOnly,
    /// Modifies workspace files.
    Workspace,
    /// Arbitrary command execution.
    System,
}

/// Immutable model-facing description of a tool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolDefinition {
    name: String,
    description: String,
    parameters: ToolSchema,
    effect_class: EffectClass,
}

impl ToolDefinition {
    /// Creates a tool definition. The effect class is mandatory factual
    /// metadata; there is deliberately no default.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: impl ToolSchemaInput,
        effect_class: EffectClass,
    ) -> Result<Self, String> {
        let name = name.into();
        if name.trim().is_empty() {
            return Err("tool name must not be empty".to_owned());
        }
        Ok(Self {
            name,
            description: description.into(),
            parameters: parameters.into_schema()?,
            effect_class,
        })
    }
    /// Convenience constructor for an empty parameter schema.
    pub fn simple(
        name: impl Into<String>,
        description: impl Into<String>,
        effect_class: EffectClass,
    ) -> Self {
        Self::new(name, description, ToolSchema::default(), effect_class)
            .expect("simple tool name is valid")
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn description(&self) -> &str {
        &self.description
    }
    pub fn parameters(&self) -> &ToolSchema {
        &self.parameters
    }
    /// The tool's factual effect classification.
    pub fn effect_class(&self) -> EffectClass {
        self.effect_class
    }

    pub(crate) fn validate_arguments(&self, arguments: &str) -> Result<(), String> {
        let schema = self.parameters.as_str();
        let Some(required_start) = schema.find("\"required\"") else {
            return Ok(());
        };
        let tail = &schema[required_start..];
        let Some(open) = tail.find('[') else {
            return Ok(());
        };
        let Some(close) = tail[open + 1..].find(']') else {
            return Err("invalid required schema".to_owned());
        };
        let required = &tail[open + 1..open + 1 + close];
        let mut rest = required;
        while let Some(start) = rest.find('"') {
            let after = &rest[start + 1..];
            let Some(end) = after.find('"') else {
                return Err("invalid required schema".to_owned());
            };
            let key = &after[..end];
            if !contains_object_key(arguments, key) {
                return Err(format!("missing required argument: {key}"));
            }
            rest = &after[end + 1..];
        }
        Ok(())
    }
}

fn contains_object_key(arguments: &str, key: &str) -> bool {
    let needle = format!("\"{key}\"");
    arguments.find(&needle).is_some_and(|index| {
        arguments[index + needle.len()..]
            .trim_start()
            .starts_with(':')
    })
}

impl fmt::Display for ToolDefinition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name)
    }
}

pub(crate) fn is_json_object(input: &str) -> bool {
    let trimmed = input.trim();
    if !(trimmed.starts_with('{') && trimmed.ends_with('}')) {
        return false;
    }
    let mut parser = JsonParser {
        input: trimmed.as_bytes(),
        index: 0,
    };
    let valid = parser.object();
    parser.ws();
    valid && parser.index == parser.input.len()
}

struct JsonParser<'a> {
    input: &'a [u8],
    index: usize,
}
impl<'a> JsonParser<'a> {
    fn ws(&mut self) {
        while self
            .input
            .get(self.index)
            .is_some_and(|byte| byte.is_ascii_whitespace())
        {
            self.index += 1;
        }
    }
    fn byte(&self) -> Option<u8> {
        self.input.get(self.index).copied()
    }
    fn eat(&mut self, expected: u8) -> bool {
        self.ws();
        if self.byte() == Some(expected) {
            self.index += 1;
            true
        } else {
            false
        }
    }
    fn object(&mut self) -> bool {
        if !self.eat(b'{') {
            return false;
        }
        self.ws();
        if self.eat(b'}') {
            return true;
        }
        loop {
            if !self.string() || !self.eat(b':') || !self.value() {
                return false;
            }
            self.ws();
            if self.eat(b'}') {
                return true;
            }
            if !self.eat(b',') {
                return false;
            }
        }
    }
    fn array(&mut self) -> bool {
        if !self.eat(b'[') {
            return false;
        }
        self.ws();
        if self.eat(b']') {
            return true;
        }
        loop {
            if !self.value() {
                return false;
            }
            self.ws();
            if self.eat(b']') {
                return true;
            }
            if !self.eat(b',') {
                return false;
            }
        }
    }
    fn string(&mut self) -> bool {
        if !self.eat(b'"') {
            return false;
        }
        let mut escaped = false;
        while let Some(byte) = self.byte() {
            self.index += 1;
            if escaped {
                escaped = false;
                continue;
            }
            if byte == b'\\' {
                escaped = true;
                continue;
            }
            if byte == b'"' {
                return true;
            }
            if byte < 0x20 {
                return false;
            }
        }
        false
    }
    fn value(&mut self) -> bool {
        self.ws();
        match self.byte() {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => self.string(),
            Some(b't') => self.literal(b"true"),
            Some(b'f') => self.literal(b"false"),
            Some(b'n') => self.literal(b"null"),
            Some(byte) if byte == b'-' || byte.is_ascii_digit() => self.number(),
            _ => false,
        }
    }
    fn literal(&mut self, literal: &[u8]) -> bool {
        if self.index + literal.len() <= self.input.len()
            && &self.input[self.index..self.index + literal.len()] == literal
        {
            self.index += literal.len();
            true
        } else {
            false
        }
    }
    fn number(&mut self) -> bool {
        let start = self.index;
        if self.byte() == Some(b'-') {
            self.index += 1;
        }
        let mut digits = 0;
        while self.byte().is_some_and(|byte| byte.is_ascii_digit()) {
            self.index += 1;
            digits += 1;
        }
        if digits == 0 {
            self.index = start;
            return false;
        }
        if self.byte() == Some(b'.') {
            self.index += 1;
            let before = self.index;
            while self.byte().is_some_and(|byte| byte.is_ascii_digit()) {
                self.index += 1;
            }
            if before == self.index {
                return false;
            }
        }
        if self.byte().is_some_and(|byte| byte == b'e' || byte == b'E') {
            self.index += 1;
            if self.byte().is_some_and(|byte| byte == b'+' || byte == b'-') {
                self.index += 1;
            }
            let before = self.index;
            while self.byte().is_some_and(|byte| byte.is_ascii_digit()) {
                self.index += 1;
            }
            if before == self.index {
                return false;
            }
        }
        true
    }
}
