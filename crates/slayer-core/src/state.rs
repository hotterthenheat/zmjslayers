//! Binary state doctrine.
//!
//! Every engine, structural zone, and UI readout in Slayer Terminal resolves
//! to exactly one of two states: [`BinaryState::Active`] or
//! [`BinaryState::Inactive`]. There is no third variant anywhere in the
//! system — the type makes ambiguous states unrepresentable.
//!
//! The information a ternary label ("holding / testing / failing") would have
//! carried is preserved as a continuous [`Readout::score`] alongside the
//! binary state. State resolution from score uses [`HysteresisBand`] so a
//! score oscillating at the boundary cannot flap the state.

use serde::{Deserialize, Serialize};

/// The only two states any engine, zone, or readout may occupy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BinaryState {
    /// The condition the readout measures is in force.
    Active,
    /// The condition the readout measures is not in force.
    #[default]
    Inactive,
}

impl BinaryState {
    /// `true` iff the state is [`BinaryState::Active`].
    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(self, Self::Active)
    }
}

/// Two-threshold band resolving a continuous score into a [`BinaryState`].
///
/// A readout activates when its score reaches `activate_at` and deactivates
/// only when it falls to `deactivate_at` (which must not exceed
/// `activate_at`). Between the two thresholds the previous state holds, so
/// boundary noise cannot flap the output. Resolution is strictly binary and
/// deterministic given the score history.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct HysteresisBand {
    /// Score at or above which the state resolves to `Active`.
    pub activate_at: f64,
    /// Score at or below which the state resolves to `Inactive`.
    pub deactivate_at: f64,
}

impl HysteresisBand {
    /// Build a band. Panics at construction (not at resolve time) if the
    /// thresholds are inverted or non-finite — a misconfigured band is a
    /// programming error, never a runtime condition.
    #[must_use]
    pub const fn new(activate_at: f64, deactivate_at: f64) -> Self {
        assert!(
            activate_at >= deactivate_at,
            "hysteresis band inverted: activate_at must be >= deactivate_at"
        );
        assert!(
            activate_at.is_finite() && deactivate_at.is_finite(),
            "hysteresis thresholds must be finite"
        );
        Self { activate_at, deactivate_at }
    }

    /// Degenerate band with a single threshold (no hysteresis).
    #[must_use]
    pub const fn strict(threshold: f64) -> Self {
        Self::new(threshold, threshold)
    }

    /// Resolve `score` against the band given the `previous` state.
    ///
    /// Non-finite scores resolve to `Inactive`: a readout whose score cannot
    /// be computed is not in force, by definition.
    #[must_use]
    pub fn resolve(&self, score: f64, previous: BinaryState) -> BinaryState {
        if !score.is_finite() {
            return BinaryState::Inactive;
        }
        if score >= self.activate_at {
            BinaryState::Active
        } else if score <= self.deactivate_at {
            BinaryState::Inactive
        } else {
            previous
        }
    }
}

/// A stateful readout: the binary state plus the continuous score it was
/// resolved from. The terminal renders `state` as the signal and `score` as a
/// numeric readout; interpretation happens in the trader's head, not in a
/// mushy enum.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Readout {
    /// Resolved binary state.
    pub state: BinaryState,
    /// The continuous quantity the state was resolved from. Units and range
    /// are documented per engine in `docs/spec/`.
    pub score: f64,
}

impl Readout {
    /// Resolve a score through a band with no prior state (cold start:
    /// previous defaults to `Inactive`).
    #[must_use]
    pub fn resolve(band: &HysteresisBand, score: f64) -> Self {
        Self { state: band.resolve(score, BinaryState::Inactive), score }
    }

    /// Resolve a score through a band carrying the previous state forward.
    #[must_use]
    pub fn resolve_from(band: &HysteresisBand, score: f64, previous: BinaryState) -> Self {
        Self { state: band.resolve(score, previous), score }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const BAND: HysteresisBand = HysteresisBand::new(0.7, 0.4);

    #[test]
    fn activates_at_upper_threshold() {
        assert_eq!(BAND.resolve(0.7, BinaryState::Inactive), BinaryState::Active);
        assert_eq!(BAND.resolve(0.95, BinaryState::Inactive), BinaryState::Active);
    }

    #[test]
    fn deactivates_at_lower_threshold() {
        assert_eq!(BAND.resolve(0.4, BinaryState::Active), BinaryState::Inactive);
        assert_eq!(BAND.resolve(0.1, BinaryState::Active), BinaryState::Inactive);
    }

    #[test]
    fn holds_previous_state_inside_band() {
        assert_eq!(BAND.resolve(0.55, BinaryState::Active), BinaryState::Active);
        assert_eq!(BAND.resolve(0.55, BinaryState::Inactive), BinaryState::Inactive);
    }

    #[test]
    fn non_finite_scores_resolve_inactive() {
        assert_eq!(BAND.resolve(f64::NAN, BinaryState::Active), BinaryState::Inactive);
        assert_eq!(BAND.resolve(f64::INFINITY, BinaryState::Active), BinaryState::Inactive);
        assert_eq!(BAND.resolve(f64::NEG_INFINITY, BinaryState::Active), BinaryState::Inactive);
    }

    #[test]
    fn wire_format_is_screaming_snake() {
        assert_eq!(serde_json::to_string(&BinaryState::Active).unwrap(), "\"ACTIVE\"");
        assert_eq!(serde_json::to_string(&BinaryState::Inactive).unwrap(), "\"INACTIVE\"");
    }
}
