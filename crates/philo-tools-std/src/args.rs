//! Argument-field extraction over the registry-validated JSON object,
//! without a JSON dependency. Inputs were already validated as JSON objects
//! by `ToolArguments::parse`; this scanner walks them defensively.

pub(crate) enum FieldError {
    Missing,
    NotAString,
    NotANumber,
    NotABool,
    BadEscape,
}

/// Extracts a required top-level string field.
pub(crate) fn required_string(json: &str, key: &str) -> Result<String, FieldError> {
    extract_string(json, key)
}

/// Extracts an optional top-level string field (`None` when absent).
pub(crate) fn optional_string(json: &str, key: &str) -> Result<Option<String>, FieldError> {
    match extract_string(json, key) {
        Ok(value) => Ok(Some(value)),
        Err(FieldError::Missing) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Extracts an optional top-level non-negative integer field.
pub(crate) fn optional_u64(json: &str, key: &str) -> Result<Option<u64>, FieldError> {
    let Some(raw) = raw_value(json, key) else {
        return Ok(None);
    };
    raw.parse::<u64>()
        .map(Some)
        .map_err(|_| FieldError::NotANumber)
}

/// Extracts an optional top-level boolean field (`true` / `false`).
pub(crate) fn optional_bool(json: &str, key: &str) -> Result<Option<bool>, FieldError> {
    let Some(raw) = raw_value(json, key) else {
        return Ok(None);
    };
    match raw.as_str() {
        "true" => Ok(Some(true)),
        "false" => Ok(Some(false)),
        _ => Err(FieldError::NotABool),
    }
}

fn extract_string(json: &str, key: &str) -> Result<String, FieldError> {
    let mut scanner = Scanner {
        bytes: json.as_bytes(),
        index: 0,
    };
    if !scanner.eat(b'{') {
        return Err(FieldError::Missing);
    }
    scanner.skip_ws();
    if scanner.eat(b'}') {
        return Err(FieldError::Missing);
    }
    loop {
        let Some(name) = scanner.parse_string() else {
            return Err(FieldError::Missing);
        };
        if !scanner.eat(b':') {
            return Err(FieldError::Missing);
        }
        let name = name.map_err(|()| FieldError::BadEscape)?;
        if name == key {
            scanner.skip_ws();
            if scanner.peek() != Some(b'"') {
                return Err(FieldError::NotAString);
            }
            return match scanner.parse_string() {
                Some(Ok(value)) => Ok(value),
                Some(Err(())) => Err(FieldError::BadEscape),
                None => Err(FieldError::NotAString),
            };
        }
        if !scanner.skip_value() {
            return Err(FieldError::Missing);
        }
        scanner.skip_ws();
        if scanner.eat(b'}') {
            return Err(FieldError::Missing);
        }
        if !scanner.eat(b',') {
            return Err(FieldError::Missing);
        }
    }
}

/// Returns the raw (non-string) scalar text of a top-level field.
fn raw_value(json: &str, key: &str) -> Option<String> {
    let mut scanner = Scanner {
        bytes: json.as_bytes(),
        index: 0,
    };
    if !scanner.eat(b'{') {
        return None;
    }
    scanner.skip_ws();
    if scanner.eat(b'}') {
        return None;
    }
    loop {
        let name = scanner.parse_string()?.ok()?;
        if !scanner.eat(b':') {
            return None;
        }
        if name == key {
            scanner.skip_ws();
            let start = scanner.index;
            if !scanner.skip_value() {
                return None;
            }
            let raw = std::str::from_utf8(&scanner.bytes[start..scanner.index]).ok()?;
            return Some(raw.trim().to_owned());
        }
        if !scanner.skip_value() {
            return None;
        }
        scanner.skip_ws();
        if scanner.eat(b'}') {
            return None;
        }
        if !scanner.eat(b',') {
            return None;
        }
    }
}

pub(crate) struct Scanner<'a> {
    pub bytes: &'a [u8],
    pub index: usize,
}

impl Scanner<'_> {
    fn skip_ws(&mut self) {
        while self
            .bytes
            .get(self.index)
            .is_some_and(u8::is_ascii_whitespace)
        {
            self.index += 1;
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.skip_ws();
        self.bytes.get(self.index).copied()
    }

    fn eat(&mut self, expected: u8) -> bool {
        if self.peek() == Some(expected) {
            self.index += 1;
            true
        } else {
            false
        }
    }

    /// Parses a JSON string, decoding escapes. `None` means no string starts
    /// here; `Some(Err(()))` means an invalid escape sequence.
    fn parse_string(&mut self) -> Option<Result<String, ()>> {
        if !self.eat(b'"') {
            return None;
        }
        let mut value = String::new();
        loop {
            let byte = *self.bytes.get(self.index)?;
            self.index += 1;
            match byte {
                b'"' => return Some(Ok(value)),
                b'\\' => {
                    let escape = *self.bytes.get(self.index)?;
                    self.index += 1;
                    match escape {
                        b'"' => value.push('"'),
                        b'\\' => value.push('\\'),
                        b'/' => value.push('/'),
                        b'b' => value.push('\u{0008}'),
                        b'f' => value.push('\u{000C}'),
                        b'n' => value.push('\n'),
                        b'r' => value.push('\r'),
                        b't' => value.push('\t'),
                        b'u' => match self.parse_unicode_escape() {
                            Some(ch) => value.push(ch),
                            None => return Some(Err(())),
                        },
                        _ => return Some(Err(())),
                    }
                }
                _ => {
                    let start = self.index - 1;
                    let width = utf8_width(byte);
                    let end = start + width;
                    let slice = self.bytes.get(start..end)?;
                    let text = std::str::from_utf8(slice).ok()?;
                    value.push_str(text);
                    self.index = end;
                }
            }
        }
    }

    fn parse_unicode_escape(&mut self) -> Option<char> {
        let high = self.parse_hex4()?;
        if (0xD800..=0xDBFF).contains(&high) {
            if self.bytes.get(self.index) != Some(&b'\\')
                || self.bytes.get(self.index + 1) != Some(&b'u')
            {
                return None;
            }
            self.index += 2;
            let low = self.parse_hex4()?;
            if !(0xDC00..=0xDFFF).contains(&low) {
                return None;
            }
            let combined = 0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00);
            return char::from_u32(combined);
        }
        char::from_u32(high)
    }

    fn parse_hex4(&mut self) -> Option<u32> {
        let slice = self.bytes.get(self.index..self.index + 4)?;
        let text = std::str::from_utf8(slice).ok()?;
        let value = u32::from_str_radix(text, 16).ok()?;
        self.index += 4;
        Some(value)
    }

    fn skip_value(&mut self) -> bool {
        match self.peek() {
            Some(b'"') => matches!(self.parse_string(), Some(Ok(_) | Err(()))),
            Some(b'{') => self.skip_container(b'{', b'}'),
            Some(b'[') => self.skip_container(b'[', b']'),
            Some(_) => {
                while let Some(byte) = self.bytes.get(self.index) {
                    if matches!(byte, b',' | b'}' | b']') || byte.is_ascii_whitespace() {
                        break;
                    }
                    self.index += 1;
                }
                true
            }
            None => false,
        }
    }

    fn skip_container(&mut self, open: u8, close: u8) -> bool {
        if !self.eat(open) {
            return false;
        }
        let mut depth = 1usize;
        while depth > 0 {
            let Some(byte) = self.bytes.get(self.index).copied() else {
                return false;
            };
            if byte == b'"' {
                if self.parse_string().is_none() {
                    return false;
                }
                continue;
            }
            self.index += 1;
            if byte == open {
                depth += 1;
            } else if byte == close {
                depth -= 1;
            }
        }
        true
    }
}

fn utf8_width(byte: u8) -> usize {
    match byte {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}
