//! Secrets a provider needs (a BMC's password, a key) and how `bohio.cfg` names them.
//!
//! A setting a provider's schema marks sensitive never holds the secret: it holds a **reference**, and the node asks a
//! [`SecretResolver`] for the value when it builds the source. The library reads no file and no environment variable, so
//! the resolver is the embedder's (`Node::with_secrets`); the `bohio` command's reads `file:` and `credential:`
//! references. A [`Secret`] prints as `[redacted]`, and nothing here logs, quotes or returns the text of one in an error.

#![forbid(unsafe_code)]

use core::fmt;

/// A secret's text. `Debug` and `Display` never show it.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// A secret holding `text`.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    /// The text: only for the provider that was given it.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret([redacted])")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

/// Why a reference was not turned into a secret. The text names the kind of reference and what is wrong, never a value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct SecretError(pub String);

impl SecretError {
    /// A refusal for `reason`.
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Self {
        Self(reason.into())
    }
}

/// Turns a reference (`file:PATH` or `credential:NAME`) into its secret.
pub trait SecretResolver: Send + Sync {
    /// The secret `reference` names, of at most `max_bytes` bytes (`[secrets] max_bytes`).
    ///
    /// # Errors
    ///
    /// [`SecretError`] for a kind of reference this resolver does not read, a source it refuses (a file other accounts may
    /// read), one that is not there, or one larger than `max_bytes`.
    fn resolve(&self, reference: &str, max_bytes: usize) -> Result<Secret, SecretError>;
}

/// The resolver of a node given none: it refuses every reference.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoSecrets;

impl SecretResolver for NoSecrets {
    fn resolve(&self, _reference: &str, _max_bytes: usize) -> Result<Secret, SecretError> {
        Err(SecretError::new(
            "this node has no secret resolver (Node::with_secrets)",
        ))
    }
}

/// The kinds of reference, as `kind:rest`.
pub const KINDS: [&str; 2] = ["file", "credential"];

/// Splits a reference into its kind and the rest, `None` when the text is not one (a secret written out is not).
#[must_use]
pub fn split(reference: &str) -> Option<(&str, &str)> {
    let (kind, rest) = reference.split_once(':')?;
    (KINDS.contains(&kind) && !rest.is_empty()).then_some((kind, rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secret_prints_nothing_of_itself() {
        let secret = Secret::new("hunter2");
        assert!(!format!("{secret} {secret:?}").contains("hunter2"));
        assert_eq!(secret.expose(), "hunter2");
    }

    #[test]
    fn only_a_kind_and_something_after_it_is_a_reference() {
        assert_eq!(split("file:/etc/x"), Some(("file", "/etc/x")));
        assert_eq!(split("credential:bmc"), Some(("credential", "bmc")));
        assert_eq!(split("hunter2"), None);
        assert_eq!(split("file:"), None);
        assert_eq!(split("other:thing"), None);
        assert_eq!(split("keyring:svc/me"), None, "there is no keyring kind");
    }
}
