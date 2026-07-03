//! # slayer-engines
//!
//! Deterministic signal engines for Slayer Terminal. Every engine is a pure
//! function from normalized market state to a serializable readout; every
//! stateful readout resolves to `BinaryState::Active`/`Inactive` plus the
//! continuous score it was resolved from (`docs/ARCHITECTURE.md` §2).
//!
//! Provenance: `docs/spec/` — each module documents which legacy engines it
//! reimplements and which catalogued defects it deliberately fixes.

pub mod gex;
pub mod technicals;
pub mod thesis;
