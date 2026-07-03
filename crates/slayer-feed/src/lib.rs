//! # slayer-feed
//!
//! Market data providers for Slayer Terminal. One trait
//! ([`FeedProvider`]), multiple implementations: a deterministic
//! [`SyntheticFeed`] for dev/demo/test, and HTTP adapters for live vendors.
//! Everything downstream consumes normalized [`slayer_core::MarketEvent`]s
//! and never sees a vendor payload.

mod error;
pub mod provider;
pub mod synthetic;

pub use error::FeedError;
pub use provider::FeedProvider;
pub use synthetic::{SyntheticConfig, SyntheticFeed, SyntheticSymbol};
