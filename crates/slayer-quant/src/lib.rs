//! # slayer-quant
//!
//! Pure quantitative math kernel for Slayer Terminal: distributions,
//! Black–Scholes–Merton analytics, realized volatility estimators,
//! risk-neutral density extraction, and Monte Carlo. No I/O, no clocks,
//! no global state; all randomness takes an explicit seed.

pub mod black_scholes;
pub mod dist;
mod error;
pub mod first_passage;
pub mod monte_carlo;
pub mod realized_vol;
pub mod rnd;
pub mod vol_metrics;

pub use error::QuantError;
