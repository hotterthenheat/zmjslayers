//! Core error taxonomy. Errors are values; nothing in the core panics on
//! malformed market data.

use thiserror::Error;

/// Errors producible by core types.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum CoreError {
    /// A ticker failed validation.
    #[error("invalid symbol: {0:?}")]
    InvalidSymbol(String),
}
