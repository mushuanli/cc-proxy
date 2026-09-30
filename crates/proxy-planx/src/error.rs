//! Error types for the planx credential mechanism.

/// Errors from resolving or refreshing subscription credentials.
#[derive(Debug, thiserror::Error)]
pub enum PlanxError {
    /// Transport error. Backed by `reqwest` normally and by `wreq` when the
    /// `impersonate` feature swaps in the BoringSSL engine.
    #[error("HTTP request failed: {0}")]
    Http(#[from] crate::transport::engine::Error),

    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("configuration error: {0}")]
    Config(String),

    #[error("account has no refresh_token")]
    MissingRefreshToken,

    #[error("OAuth refresh failed with HTTP {status}: {body}")]
    Refresh { status: u16, body: String },

    #[error("OAuth refresh response carried no access_token")]
    RefreshNoAccessToken,

    #[error("account '{0}' is not configured")]
    UnknownAccount(String),

    #[error("{0}")]
    Other(String),
}

impl PlanxError {
    pub fn config(message: impl Into<String>) -> Self {
        PlanxError::Config(message.into())
    }

    pub fn other(message: impl Into<String>) -> Self {
        PlanxError::Other(message.into())
    }
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, PlanxError>;

/// Truncate a response body for logging / error messages.
pub(crate) fn truncate(raw: &str, max: usize) -> String {
    if raw.chars().count() <= max {
        return raw.to_string();
    }
    let mut out: String = raw.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_keeps_short_strings() {
        assert_eq!(truncate("abc", 10), "abc");
    }

    #[test]
    fn truncate_clips_and_marks() {
        let long = "x".repeat(50);
        let out = truncate(&long, 10);
        assert_eq!(out.chars().count(), 11);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn truncate_is_char_safe() {
        let out = truncate("账号凭据错误", 2);
        assert_eq!(out, "账号…");
    }
}
