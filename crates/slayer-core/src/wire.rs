//! Terminal wire schema.
//!
//! The single definition of every payload the gateway emits and the terminal
//! renders. The TypeScript mirror lives in `terminal/src/wire/` and is kept
//! honest by the golden-vector fixtures in `schema/golden/`.
//!
//! NOTE: readout shapes land together with the engine implementations; this
//! module intentionally starts minimal and grows with `slayer-engines`.

use serde::{Deserialize, Serialize};

/// Wire protocol version. Bump on any breaking schema change.
pub const WIRE_VERSION: u32 = 1;

/// Placeholder module marker so the module tree compiles before the engine
/// build phase lands the full snapshot schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaMarker;
