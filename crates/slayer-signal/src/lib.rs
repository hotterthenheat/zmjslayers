//! # slayer-signal
//!
//! The SkyVision decision brain: probability calibration, historical tail
//! risk, liquidity and model-trust scoring, opportunity quality, and the
//! hard binary decision gate. Pure and deterministic — every input is a
//! parameter, the decision resolves to `ACTIVE`/`INACTIVE` plus a continuous
//! quality score (`docs/spec/01-skyvision-v11.md` E10–E16).

pub mod calibration;
pub mod decision;
pub mod risk;
