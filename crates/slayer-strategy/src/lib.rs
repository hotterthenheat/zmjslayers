//! # slayer-strategy
//!
//! Multi-leg options strategy construction and analytics: strategy suites and
//! payoff topology, scenario shock matrices (spot × vol × time), portfolio
//! greek aggregation, and the expiry-GEX / charm-vanna decay curves. Pure math
//! over the kernel (`docs/spec/02-quant-suite.md` E6–E11).

pub mod multileg;
pub mod portfolio;
pub mod scenario;
