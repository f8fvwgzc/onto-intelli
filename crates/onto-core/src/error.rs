use std::fmt;

/// Errors shared by every onto-intelli crate.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid: {0}")]
    Invalid(String),
    #[error("permission denied: {0}")]
    Denied(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("storage: {0}")]
    Storage(String),
    #[error("config: {0}")]
    Config(String),
    #[error("llm: {0}")]
    Llm(String),
    #[error("unavailable: {0}")]
    Unavailable(String),
    /// The store moved on since the write was staged (another process
    /// committed); the engine catches up and retries.
    #[error("stale: {0}")]
    Stale(String),
    /// A rate limit or quota ran out: HTTP 429 (with `retry-after`), gRPC
    /// `RESOURCE_EXHAUSTED`. The message may end in "retry in N s".
    #[error("limited: {0}")]
    Limited(String),
    #[error("{0}")]
    Other(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    /// Stable machine-readable code, used by the gRPC/HTTP/FFI layers.
    pub fn code(&self) -> &'static str {
        match self {
            Error::NotFound(_) => "not_found",
            Error::Invalid(_) => "invalid_argument",
            Error::Denied(_) => "permission_denied",
            Error::Conflict(_) => "conflict",
            Error::Storage(_) => "storage",
            Error::Config(_) => "config",
            Error::Llm(_) => "llm",
            Error::Unavailable(_) => "unavailable",
            Error::Stale(_) => "aborted",
            Error::Limited(_) => "resource_exhausted",
            Error::Other(_) => "internal",
        }
    }

    /// Seconds a client should wait before retrying a [`Error::Limited`]:
    /// the N of "retry in N s" in its message, else 60. `None` for other errors.
    pub fn retry_after_secs(&self) -> Option<u64> {
        let Error::Limited(m) = self else { return None };
        let secs = m
            .rsplit_once("retry in ")
            .and_then(|(_, rest)| rest.split_whitespace().next())
            .and_then(|n| n.parse::<u64>().ok());
        Some(secs.unwrap_or(60).max(1))
    }

    pub fn invalid(msg: impl fmt::Display) -> Self {
        Error::Invalid(msg.to_string())
    }

    pub fn not_found(msg: impl fmt::Display) -> Self {
        Error::NotFound(msg.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Invalid(e.to_string())
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Storage(e.to_string())
    }
}

impl From<toml::de::Error> for Error {
    fn from(e: toml::de::Error) -> Self {
        Error::Config(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limited_errors_carry_a_retry_time() {
        let e = Error::Limited("asks_per_minute (60) reached; retry in 3 s".into());
        assert_eq!(e.code(), "resource_exhausted");
        assert_eq!(e.retry_after_secs(), Some(3));
        assert_eq!(Error::Limited("quota".into()).retry_after_secs(), Some(60));
        assert_eq!(Error::invalid("x").retry_after_secs(), None);
    }
}
