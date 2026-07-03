//! Feed error taxonomy.

use thiserror::Error;

/// Errors produced at the feed edge.
#[derive(Debug, Error)]
pub enum FeedError {
    /// The requested universe is empty or contains symbols the provider
    /// cannot serve.
    #[error("unsupported universe: {0}")]
    UnsupportedUniverse(String),
    /// Upstream transport failure (HTTP/WebSocket).
    #[error("transport: {0}")]
    Transport(String),
    /// Upstream payload failed normalization.
    #[error("malformed upstream payload: {0}")]
    Malformed(String),
    /// Provider credentials missing or rejected.
    #[error("auth: {0}")]
    Auth(String),
}
