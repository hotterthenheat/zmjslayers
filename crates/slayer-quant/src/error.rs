//! Kernel error taxonomy.

use thiserror::Error;

/// Errors producible by the math kernel. Malformed inputs are values, never
/// panics.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum QuantError {
    /// Inputs outside the mathematical domain of the routine.
    #[error("domain error: {0}")]
    Domain(&'static str),
    /// No solution exists within the search bracket (e.g. arbitrage-violating
    /// price handed to the implied-vol solver).
    #[error("no solution: {0}")]
    NoSolution(&'static str),
    /// A series was shorter than the estimator's minimum window.
    #[error("insufficient data: needed {needed}, got {got}")]
    InsufficientData {
        /// Minimum observations required.
        needed: usize,
        /// Observations supplied.
        got: usize,
    },
}
