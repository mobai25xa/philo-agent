//! Validated, non-credential HTTP headers for one model deployment.

use std::{error::Error, fmt};

use http::{HeaderName, HeaderValue};
use philo::api::extension as ext;

/// Product identity sent when a deployment does not override `User-Agent`.
pub const DEFAULT_USER_AGENT: &str = concat!("philo-agent/", env!("CARGO_PKG_VERSION"));

/// A validated set of non-credential request headers.
///
/// Values are intentionally absent from `Debug`: callers may accidentally
/// place deployment metadata in a value even though credential headers are
/// rejected by this type.
#[derive(Clone, Default)]
pub struct ModelRequestHeaders {
    headers: ext::ProviderHeaders,
    names: Vec<HeaderName>,
}

impl ModelRequestHeaders {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            headers: ext::ProviderHeaders::new(),
            names: Vec::new(),
        }
    }

    /// Validates and sets one header. A repeated name replaces the earlier
    /// value case-insensitively, matching HTTP header-map semantics.
    pub fn set(
        &mut self,
        name: impl AsRef<str>,
        value: impl AsRef<str>,
    ) -> Result<(), ModelRequestHeaderError> {
        let name = HeaderName::from_bytes(name.as_ref().as_bytes())
            .map_err(|_| ModelRequestHeaderError::invalid_name())?;
        validate_name(&name)?;
        if value.as_ref().trim().is_empty() {
            return Err(ModelRequestHeaderError::empty_value(&name));
        }
        let value = HeaderValue::from_str(value.as_ref())
            .map_err(|_| ModelRequestHeaderError::invalid_value(&name))?;

        let mut operations: Vec<ext::HeaderOperation> = self
            .headers
            .operations()
            .iter()
            .filter(|operation| operation.name() != name)
            .cloned()
            .collect();
        operations.push(ext::HeaderOperation::Set(name.clone(), value));
        self.headers = ext::ProviderHeaders::try_from_patch(
            operations.into_iter().collect::<ext::HeaderPatch>(),
        )
        .map_err(|_| ModelRequestHeaderError::reserved(&name))?;
        self.names.retain(|configured| configured != name);
        self.names.push(name);
        Ok(())
    }

    /// Builds a validated set from name/value pairs.
    pub fn try_from_iter<N, V, I>(headers: I) -> Result<Self, ModelRequestHeaderError>
    where
        N: AsRef<str>,
        V: AsRef<str>,
        I: IntoIterator<Item = (N, V)>,
    {
        let mut configured = Self::new();
        for (name, value) in headers {
            configured.set(name, value)?;
        }
        Ok(configured)
    }

    /// Canonical lowercase names, in deterministic configuration order.
    pub fn names(&self) -> impl ExactSizeIterator<Item = &str> {
        self.names.iter().map(HeaderName::as_str)
    }

    pub(crate) fn provider_headers(&self) -> ext::ProviderHeaders {
        self.headers.clone()
    }
}

impl fmt::Debug for ModelRequestHeaders {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelRequestHeaders")
            .field("names", &self.names().collect::<Vec<_>>())
            .finish()
    }
}

/// Header validation failure that never includes the rejected value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelRequestHeaderError {
    name: Option<HeaderName>,
    kind: HeaderErrorKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HeaderErrorKind {
    InvalidName,
    InvalidValue,
    EmptyValue,
    Reserved,
    Sensitive,
}

impl ModelRequestHeaderError {
    fn invalid_name() -> Self {
        Self {
            name: None,
            kind: HeaderErrorKind::InvalidName,
        }
    }

    fn invalid_value(name: &HeaderName) -> Self {
        Self {
            name: Some(name.clone()),
            kind: HeaderErrorKind::InvalidValue,
        }
    }

    fn empty_value(name: &HeaderName) -> Self {
        Self {
            name: Some(name.clone()),
            kind: HeaderErrorKind::EmptyValue,
        }
    }

    fn reserved(name: &HeaderName) -> Self {
        Self {
            name: Some(name.clone()),
            kind: HeaderErrorKind::Reserved,
        }
    }

    fn sensitive(name: &HeaderName) -> Self {
        Self {
            name: Some(name.clone()),
            kind: HeaderErrorKind::Sensitive,
        }
    }
}

impl fmt::Display for ModelRequestHeaderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Some(name) = &self.name else {
            return formatter.write_str("request header name is invalid");
        };
        match self.kind {
            HeaderErrorKind::InvalidName => formatter.write_str("request header name is invalid"),
            HeaderErrorKind::InvalidValue => {
                write!(formatter, "request header '{name}' has an invalid value")
            }
            HeaderErrorKind::EmptyValue => {
                write!(formatter, "request header '{name}' must not be empty")
            }
            HeaderErrorKind::Reserved => {
                write!(formatter, "request header '{name}' is managed by the SDK")
            }
            HeaderErrorKind::Sensitive => write!(
                formatter,
                "request header '{name}' may carry credentials and must not be configured as a literal header"
            ),
        }
    }
}

impl Error for ModelRequestHeaderError {}

fn validate_name(name: &HeaderName) -> Result<(), ModelRequestHeaderError> {
    match name.as_str() {
        // Protocol-owned fields, including the fixed Anthropic protocol version.
        "content-type" | "accept" | "anthropic-version" => {
            Err(ModelRequestHeaderError::reserved(name))
        }
        // Compression is disabled by the standard transport and only identity
        // response bodies are accepted, so advertising another encoding is invalid.
        "accept-encoding" => Err(ModelRequestHeaderError::reserved(name)),
        // Credential and ambient-session material stays out of literal config.
        "authorization"
        | "proxy-authorization"
        | "cookie"
        | "set-cookie"
        | "api-key"
        | "x-api-key"
        | "apikey"
        | "key"
        | "token"
        | "secret"
        | "password" => Err(ModelRequestHeaderError::sensitive(name)),
        _ => Ok(()),
    }
}

pub(crate) fn default_provider_headers() -> ext::ProviderHeaders {
    ext::ProviderHeaders::try_from_patch(ext::HeaderPatch::from_iter([ext::HeaderOperation::Set(
        http::header::USER_AGENT,
        HeaderValue::from_static(DEFAULT_USER_AGENT),
    )]))
    .expect("the static User-Agent is provider-owned and valid")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_names_replace_case_insensitively_and_debug_redacts_values() {
        let mut headers = ModelRequestHeaders::new();
        headers.set("X-Route", "secret-like-value").unwrap();
        headers.set("x-route", "replacement").unwrap();

        assert_eq!(headers.names().collect::<Vec<_>>(), ["x-route"]);
        let debug = format!("{headers:?}");
        assert!(debug.contains("x-route"));
        assert!(!debug.contains("secret-like-value"));
        assert!(!debug.contains("replacement"));
    }

    #[test]
    fn invalid_reserved_and_sensitive_headers_fail_without_echoing_values() {
        for name in [
            "host",
            "content-length",
            "content-type",
            "accept",
            "anthropic-version",
            "accept-encoding",
            "authorization",
            "cookie",
            "api-key",
            "x-api-key",
        ] {
            let error = ModelRequestHeaders::try_from_iter([(name, "do-not-print")])
                .expect_err("header must be rejected");
            assert!(!error.to_string().contains("do-not-print"));
        }
        assert!(ModelRequestHeaders::try_from_iter([("bad header", "value")]).is_err());
        assert!(ModelRequestHeaders::try_from_iter([("x-route", "bad\r\nvalue")]).is_err());
    }
}
