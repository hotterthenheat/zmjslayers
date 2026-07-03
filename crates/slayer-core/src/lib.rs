//! # slayer-core
//!
//! Shared types, the binary-state doctrine, and the wire schema for Slayer
//! Terminal. This crate is the contract every other crate builds against:
//! it depends on nothing but `serde`/`thiserror` and performs no I/O.
//!
//! See `docs/ARCHITECTURE.md` for the system doctrine, in particular §2:
//! every engine, structural zone, and UI readout resolves to exactly
//! [`BinaryState::Active`] or [`BinaryState::Inactive`] — ambiguous states
//! are unrepresentable at the type level.

mod error;
mod market;
mod state;
pub mod wire;

pub use error::CoreError;
pub use market::{
    BarInterval, Candle, ExpiryDate, Greeks, MarketEvent, OptionChain, OptionQuote, OptionRight,
    SpotQuote, Symbol, TsMillis,
};
pub use state::{BinaryState, HysteresisBand, Readout};
