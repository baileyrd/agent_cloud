//! Validated agent identifier.

use std::fmt;

/// Maximum identifier length (fits DNS labels and container names).
pub const MAX_LEN: usize = 32;

/// Reasons an identifier is rejected.
#[derive(Debug, PartialEq, Eq)]
pub enum IdError {
    /// Empty or longer than [`MAX_LEN`].
    Length(usize),
    /// Contains a character outside `[a-z0-9-]`, or starts/ends with `-`.
    Charset,
}

impl fmt::Display for IdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Length(n) => write!(f, "id length {n} not in 1..={MAX_LEN}"),
            Self::Charset => f.write_str("id must match [a-z0-9][a-z0-9-]*[a-z0-9]"),
        }
    }
}

impl std::error::Error for IdError {}

/// An agent identifier, safe to use in paths, container names and SQL keys.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AgentId(String);

impl AgentId {
    /// Parse and validate external input.
    pub fn parse(s: &str) -> Result<Self, IdError> {
        check(s).map(|()| Self(s.to_owned()))
    }

    /// The validated string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The rule every identifier follows: 1 to [`MAX_LEN`] bytes of `[a-z0-9-]`, not starting or
/// ending with `-`.
pub fn check(s: &str) -> Result<(), IdError> {
    if s.is_empty() || s.len() > MAX_LEN {
        return Err(IdError::Length(s.len()));
    }
    let ok_char = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-';
    if !s.chars().all(ok_char) || s.starts_with('-') || s.ends_with('-') {
        return Err(IdError::Charset);
    }
    Ok(())
}

impl fmt::Display for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid() {
        assert_eq!(
            AgentId::parse("agent-1").map(|i| i.to_string()),
            Ok("agent-1".into())
        );
    }

    #[test]
    fn rejects_bad_length() {
        assert_eq!(AgentId::parse(""), Err(IdError::Length(0)));
        assert_eq!(AgentId::parse(&"a".repeat(33)), Err(IdError::Length(33)));
    }

    #[test]
    fn rejects_bad_charset() {
        for s in ["Agent", "a_b", "-a", "a-", "../x", "a b"] {
            assert_eq!(AgentId::parse(s), Err(IdError::Charset), "{s}");
        }
    }
}
