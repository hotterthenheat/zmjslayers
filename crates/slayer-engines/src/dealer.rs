//! Dealer dynamics — vanna, charm, gamma, migration, and OI-flow sub-engines (E2).
//!
//! Provenance: `docs/spec/03-dealer-structure.md` E2 (`computeDealerDynamics`,
//! `src/lib/dealerDynamics.ts`), steps 3–7 (the time-derivative layer). The
//! concentration / NBRS / vacuum / wall-strength layers (steps 8–11) live
//! elsewhere; the wall-strength `0–100` companion is consumed by E10 in
//! [`crate::zones`].
//!
//! Deviations from legacy:
//! - Each ternary trend/state collapses to a signed `*_score` plus a descriptive
//!   direction enum (Rising/Falling/Flat, Bullish/Bearish, Adding/Removing — DATA
//!   per the binary-state doctrine), and each engine additionally ships a
//!   [`Readout`] answering "is this flow in force?" (ACTIVE ⟺ the magnitude
//!   clears the legacy STABLE/NEUTRAL band), resolved through a named
//!   [`HysteresisBand`] so a boundary-grazing score cannot flap the state. The
//!   instantaneous direction and the hysteretic readout may briefly disagree
//!   inside the band — that divergence is the anti-flap doctrine working, not a
//!   bug.
//! - D1 (scale-dependent `$1e6` gamma-velocity floor): fixed. The STABLE band is
//!   purely relative (`GAMMA_VEL_REL_THRESH·|netGex|`) with only a [`DEALER_EPS`]
//!   divide-by-zero guard, so a small-book underlying is no longer permanently
//!   STABLE.
//! - D2 (OI-velocity wall-clock / overnight gaps): the minute delta `dt_min` is an
//!   explicit parameter — a pure engine never reads a clock — so session-boundary
//!   handling is the gateway's responsibility, not baked in here.
//! - History management (the 180-tick cap, the 20-tick charm window, `Date.now`)
//!   lives in the gateway; these engines take the derived prior aggregates as
//!   params and are pure.

use serde::{Deserialize, Serialize};
use slayer_core::{BinaryState, HysteresisBand, Readout};

/// Absolute zero-band epsilon shared by the neutral/flat tests and used as a
/// divide-by-zero guard (legacy `1e−9`).
pub const DEALER_EPS: f64 = 1e-9;
/// Vanna trend threshold: `|Δvanna|` must clear `2%` of `|netVanna|` to trend.
pub const VANNA_TREND_REL_THRESH: f64 = 0.02;
/// Migration full-scale: a CoM shift of `1%` of spot in one tick maps to `±1`.
pub const MIGRATION_FULL_SCALE_PCT: f64 = 0.01;
/// Migration STABLE band: `|migScore|` below this reads STABLE.
pub const MIGRATION_STABLE_BAND: f64 = 0.05;
/// Gamma-velocity STABLE band as a fraction of `|netGex|` (D1: purely relative).
pub const GAMMA_VEL_REL_THRESH: f64 = 0.03;
/// OI-velocity STABLE band floor, contracts/min (`≥ 1`).
pub const OI_VEL_ABS_FLOOR: f64 = 1.0;
/// OI-velocity STABLE band as a fraction of the book per minute (`0.5%`).
pub const OI_VEL_REL_THRESH: f64 = 0.005;

/// Minute-delta floor for OI velocity (legacy `1e−6`).
const DT_MIN_FLOOR: f64 = 1e-6;
/// Normalized activation threshold: a `|signal|/band` score of `1` clears the
/// legacy STABLE band.
const FLOW_ACTIVE_AT: f64 = 1.0;
/// Normalized deactivation threshold: the flow stays engaged until its magnitude
/// falls to `85%` of the STABLE band (`15%` anti-flap hysteresis).
const FLOW_DEACTIVATE_AT: f64 = 0.85;
/// Band for the relative "flow in force?" readouts (migration, gamma, OI).
const FLOW_ACTIVE_BAND: HysteresisBand = HysteresisBand::new(FLOW_ACTIVE_AT, FLOW_DEACTIVATE_AT);
/// Band for the magnitude "engaged?" readouts (vanna hedge, charm): ACTIVE once
/// the raw magnitude clears [`DEALER_EPS`].
const ENGAGED_BAND: HysteresisBand = HysteresisBand::new(DEALER_EPS, 0.0);

/// Direction of a time-derivative (vanna velocity). Descriptive DATA.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TrendDirection {
    /// Signed score `≥ 1`: rising.
    Rising,
    /// Signed score `≤ −1`: falling.
    Falling,
    /// Within the trend band: flat.
    Flat,
}

/// Vanna hedge-flow direction (sign of net vanna). Descriptive DATA.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FlowDirection {
    /// Positive net vanna: falling IV makes dealers buy — supportive.
    Supportive,
    /// Negative net vanna: falling IV makes dealers sell — pressuring.
    Pressuring,
    /// `|netVanna| < DEALER_EPS`: neutral.
    Neutral,
}

/// Charm bias direction (sign of net charm). Descriptive DATA.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BiasDirection {
    /// Positive net charm.
    Bullish,
    /// Negative net charm.
    Bearish,
    /// `|netCharm| < DEALER_EPS`.
    Neutral,
}

/// Gamma center-of-mass migration direction. Descriptive DATA.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MigrationDirection {
    /// CoM migrating up (`migScore ≥ MIGRATION_STABLE_BAND`).
    Bullish,
    /// CoM migrating down (`migScore ≤ −MIGRATION_STABLE_BAND`).
    Bearish,
    /// Within the STABLE band.
    Stable,
}

/// Gamma hedging state (sign of net-GEX velocity). Descriptive DATA.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum GammaHedgeDirection {
    /// Net GEX rising: dealers adding hedges.
    AddingHedges,
    /// Net GEX falling: dealers removing hedges.
    RemovingHedges,
    /// Within the STABLE band.
    Stable,
}

/// OI-flow state (sign of OI velocity). Descriptive DATA.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OiFlowDirection {
    /// Open interest building.
    Building,
    /// Open interest unwinding.
    Unwinding,
    /// Within the STABLE band.
    Stable,
}

/// Vanna hedge-flow sub-engine output (E2 step 3).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct VannaFlow {
    /// Net dealer vanna, `$` (signed cq for hedge-flow direction).
    pub net: f64,
    /// One-tick vanna velocity `netVanna − prev`, `$` (`0` with no prior tick).
    pub velocity: f64,
    /// Signed trend score: `velocity / max(DEALER_EPS, VANNA_TREND_REL_THRESH·|net|)`.
    pub trend_score: f64,
    /// Descriptive trend direction (`Flat` iff `|trend_score| < 1`).
    pub trend: TrendDirection,
    /// Descriptive hedge-flow direction (sign of `net`).
    pub direction: FlowDirection,
    /// "Is the vanna hedge flow in force?" ACTIVE ⟺ `|net| ≥ DEALER_EPS`;
    /// `readout.score = |net|` (`$`).
    pub engaged: Readout,
}

/// Charm-decay sub-engine output (E2 step 4).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CharmBias {
    /// Net dealer charm, `$/day` (signed cq).
    pub net_per_day: f64,
    /// Decay intensity in `[0, 1]`: `min(1, |netCharm| / priorMax)`; `0` until the
    /// gateway has accrued a prior-window maximum.
    pub intensity: f64,
    /// Descriptive bias direction (sign of `net_per_day`).
    pub bias: BiasDirection,
    /// "Is the charm bias in force?" ACTIVE ⟺ `|netCharm| ≥ DEALER_EPS`;
    /// `readout.score = |netCharm|` (`$/day`).
    pub engaged: Readout,
}

/// Gamma center-of-mass migration sub-engine output (E2 step 5).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Migration {
    /// Current gamma center of mass, points.
    pub com_current: f64,
    /// Prior-tick center of mass (equal to `com_current` with no prior tick).
    pub com_previous: f64,
    /// One-tick shift `com_current − com_previous`, points.
    pub shift: f64,
    /// `migScore = clamp(shift / (spot·MIGRATION_FULL_SCALE_PCT), −1, 1)` (signed cq).
    pub score: f64,
    /// Descriptive migration direction (`Stable` iff `|score| < MIGRATION_STABLE_BAND`).
    pub direction: MigrationDirection,
    /// "Is the gamma CoM migrating?" ACTIVE ⟺ `|score| ≥ MIGRATION_STABLE_BAND`;
    /// `readout.score = |score| / MIGRATION_STABLE_BAND`.
    pub migrating: Readout,
}

/// Gamma velocity/acceleration sub-engine output (E2 step 6).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GammaDynamics {
    /// One-tick net-GEX velocity `netGex − prev`, `$` (`0` with no prior tick).
    pub velocity: f64,
    /// Net-GEX acceleration `velocity − priorVelocity`, `$`.
    pub acceleration: f64,
    /// Signed velocity score: `velocity / max(GAMMA_VEL_REL_THRESH·|netGex|, DEALER_EPS)`
    /// (D1: purely relative band).
    pub velocity_score: f64,
    /// Descriptive hedging state (`Stable` iff `|velocity_score| < 1`).
    pub direction: GammaHedgeDirection,
    /// "Are dealers actively adjusting hedges?" ACTIVE ⟺ `|velocity_score| ≥ 1`.
    pub hedging_active: Readout,
}

/// OI-flow sub-engine output (E2 step 7).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct OiFlow {
    /// Total open interest across the book, contracts.
    pub total_oi: f64,
    /// OI velocity `(totalOi − prev) / dtMin`, contracts/min (signed cq).
    pub velocity: f64,
    /// Signed velocity score: `velocity / max(OI_VEL_ABS_FLOOR, OI_VEL_REL_THRESH·totalOi)`.
    pub velocity_score: f64,
    /// Descriptive flow state (`Stable` iff `|velocity_score| < 1`).
    pub direction: OiFlowDirection,
    /// "Is open interest actively flowing?" ACTIVE ⟺ `|velocity_score| ≥ 1`.
    pub flowing: Readout,
}

/// Prior-tick binary states carried forward for hysteresis. [`Default`] is all
/// INACTIVE (cold start).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DealerPrevStates {
    /// Prior vanna-engaged state.
    pub vanna: BinaryState,
    /// Prior charm-engaged state.
    pub charm: BinaryState,
    /// Prior migration state.
    pub migration: BinaryState,
    /// Prior gamma hedging-active state.
    pub gamma: BinaryState,
    /// Prior OI-flowing state.
    pub oi_flow: BinaryState,
}

/// Combined input to [`dealer_dynamics`]. All exposures are pre-aggregated net
/// dealer Greeks wired by the gateway; prior values are `None` on the first tick.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DealerDynamicsInput {
    /// Underlying spot, points.
    pub spot: f64,
    /// Current net dealer GEX, `$`.
    pub net_gex: f64,
    /// Current net dealer vanna, `$`.
    pub net_vanna: f64,
    /// Current net dealer charm, `$/day`.
    pub net_charm: f64,
    /// Current gamma center of mass, points.
    pub gex_com: f64,
    /// Current total open interest, contracts.
    pub total_oi: f64,
    /// Prior-tick net GEX.
    pub prev_net_gex: Option<f64>,
    /// Net GEX two ticks ago (for acceleration).
    pub prev2_net_gex: Option<f64>,
    /// Prior-tick net vanna.
    pub prev_net_vanna: Option<f64>,
    /// Prior-tick gamma center of mass.
    pub prev_gex_com: Option<f64>,
    /// Prior-tick total open interest.
    pub prev_total_oi: Option<f64>,
    /// Maximum `|netCharm|` over the gateway's charm lookback window; `0` until
    /// enough snapshots have accrued.
    pub prior_max_abs_charm: f64,
    /// Minutes elapsed since the prior tick (drives OI velocity).
    pub dt_min: f64,
    /// Prior-tick binary states for hysteresis.
    pub prev_states: DealerPrevStates,
}

/// The five time-derivative dealer sub-engines resolved together.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DealerDynamics {
    /// Vanna hedge-flow.
    pub vanna: VannaFlow,
    /// Charm decay.
    pub charm: CharmBias,
    /// Gamma center-of-mass migration.
    pub migration: Migration,
    /// Gamma velocity/acceleration.
    pub gamma: GammaDynamics,
    /// OI flow.
    pub oi_flow: OiFlow,
}

/// Coerce a non-finite value to `0.0` (legacy `fin`).
fn fin(v: f64) -> f64 {
    if v.is_finite() { v } else { 0.0 }
}

/// Vanna hedge-flow sub-engine (E2 step 3).
#[must_use]
pub fn vanna_flow(net_vanna: f64, prev_net_vanna: Option<f64>, previous: BinaryState) -> VannaFlow {
    let net = fin(net_vanna);
    let velocity = prev_net_vanna.map_or(0.0, |p| net - fin(p));
    let trend_thresh = (VANNA_TREND_REL_THRESH * net.abs()).max(DEALER_EPS);
    let trend_score = velocity / trend_thresh;
    let trend = if trend_score.abs() < 1.0 {
        TrendDirection::Flat
    } else if trend_score > 0.0 {
        TrendDirection::Rising
    } else {
        TrendDirection::Falling
    };
    let direction = if net.abs() < DEALER_EPS {
        FlowDirection::Neutral
    } else if net > 0.0 {
        FlowDirection::Supportive
    } else {
        FlowDirection::Pressuring
    };
    let engaged = Readout::resolve_from(&ENGAGED_BAND, net.abs(), previous);
    VannaFlow { net, velocity, trend_score, trend, direction, engaged }
}

/// Charm-decay sub-engine (E2 step 4).
#[must_use]
pub fn charm_bias(net_charm: f64, prior_max_abs_charm: f64, previous: BinaryState) -> CharmBias {
    let net_per_day = fin(net_charm);
    let prior_max = fin(prior_max_abs_charm);
    let intensity = if prior_max > 0.0 { (net_per_day.abs() / prior_max).min(1.0) } else { 0.0 };
    let bias = if net_per_day.abs() < DEALER_EPS {
        BiasDirection::Neutral
    } else if net_per_day > 0.0 {
        BiasDirection::Bullish
    } else {
        BiasDirection::Bearish
    };
    let engaged = Readout::resolve_from(&ENGAGED_BAND, net_per_day.abs(), previous);
    CharmBias { net_per_day, intensity, bias, engaged }
}

/// Gamma center-of-mass migration sub-engine (E2 step 5).
#[must_use]
pub fn oi_migration(
    gex_com: f64,
    prev_gex_com: Option<f64>,
    spot: f64,
    previous: BinaryState,
) -> Migration {
    let com_current = fin(gex_com);
    let com_previous = prev_gex_com.map_or(com_current, fin);
    let shift = com_current - com_previous;
    let score = if spot > 0.0 {
        (shift / (spot * MIGRATION_FULL_SCALE_PCT)).clamp(-1.0, 1.0)
    } else {
        0.0
    };
    let direction = if score.abs() < MIGRATION_STABLE_BAND {
        MigrationDirection::Stable
    } else if score > 0.0 {
        MigrationDirection::Bullish
    } else {
        MigrationDirection::Bearish
    };
    let migrating = Readout::resolve_from(&FLOW_ACTIVE_BAND, score.abs() / MIGRATION_STABLE_BAND, previous);
    Migration { com_current, com_previous, shift, score, direction, migrating }
}

/// Gamma velocity/acceleration sub-engine (E2 step 6). D1: the STABLE band is
/// purely relative, so the engine responds at any book scale.
#[must_use]
pub fn gamma_dynamics(
    net_gex: f64,
    prev_net_gex: Option<f64>,
    prev2_net_gex: Option<f64>,
    previous: BinaryState,
) -> GammaDynamics {
    let net = fin(net_gex);
    let velocity = prev_net_gex.map_or(0.0, |p| net - fin(p));
    let prior_velocity = match (prev_net_gex, prev2_net_gex) {
        (Some(p), Some(p2)) => fin(p) - fin(p2),
        _ => 0.0,
    };
    let acceleration = velocity - prior_velocity;
    let thresh = (GAMMA_VEL_REL_THRESH * net.abs()).max(DEALER_EPS);
    let velocity_score = velocity / thresh;
    let direction = if velocity_score.abs() < 1.0 {
        GammaHedgeDirection::Stable
    } else if velocity_score > 0.0 {
        GammaHedgeDirection::AddingHedges
    } else {
        GammaHedgeDirection::RemovingHedges
    };
    let hedging_active = Readout::resolve_from(&FLOW_ACTIVE_BAND, velocity_score.abs(), previous);
    GammaDynamics { velocity, acceleration, velocity_score, direction, hedging_active }
}

/// OI-flow sub-engine (E2 step 7). `dt_min` is an explicit parameter (D2).
#[must_use]
pub fn oi_flow(
    total_oi: f64,
    prev_total_oi: Option<f64>,
    dt_min: f64,
    previous: BinaryState,
) -> OiFlow {
    let total = fin(total_oi).max(0.0);
    let dt = if dt_min.is_finite() && dt_min > DT_MIN_FLOOR { dt_min } else { DT_MIN_FLOOR };
    let velocity = prev_total_oi.map_or(0.0, |p| (total - fin(p)) / dt);
    let thresh = (OI_VEL_REL_THRESH * total).max(OI_VEL_ABS_FLOOR);
    let velocity_score = velocity / thresh;
    let direction = if velocity_score.abs() < 1.0 {
        OiFlowDirection::Stable
    } else if velocity_score > 0.0 {
        OiFlowDirection::Building
    } else {
        OiFlowDirection::Unwinding
    };
    let flowing = Readout::resolve_from(&FLOW_ACTIVE_BAND, velocity_score.abs(), previous);
    OiFlow { total_oi: total, velocity, velocity_score, direction, flowing }
}

/// Resolve all five time-derivative dealer sub-engines from one snapshot.
#[must_use]
pub fn dealer_dynamics(input: &DealerDynamicsInput) -> DealerDynamics {
    DealerDynamics {
        vanna: vanna_flow(input.net_vanna, input.prev_net_vanna, input.prev_states.vanna),
        charm: charm_bias(input.net_charm, input.prior_max_abs_charm, input.prev_states.charm),
        migration: oi_migration(
            input.gex_com,
            input.prev_gex_com,
            input.spot,
            input.prev_states.migration,
        ),
        gamma: gamma_dynamics(
            input.net_gex,
            input.prev_net_gex,
            input.prev2_net_gex,
            input.prev_states.gamma,
        ),
        oi_flow: oi_flow(input.total_oi, input.prev_total_oi, input.dt_min, input.prev_states.oi_flow),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn vanna_rising_supportive_and_engaged() {
        let v = vanna_flow(1_000.0, Some(900.0), BinaryState::Inactive);
        assert!((v.velocity - 100.0).abs() < 1e-9);
        // trend_thresh = 0.02·1000 = 20 ⇒ score 5 ⇒ Rising.
        assert!((v.trend_score - 5.0).abs() < 1e-9);
        assert_eq!(v.trend, TrendDirection::Rising);
        assert_eq!(v.direction, FlowDirection::Supportive);
        assert_eq!(v.engaged.state, BinaryState::Active);
    }

    #[test]
    fn vanna_negative_is_pressuring() {
        let v = vanna_flow(-1_000.0, Some(-1_010.0), BinaryState::Inactive);
        assert_eq!(v.direction, FlowDirection::Pressuring);
        // |velocity| = 10 < 20 ⇒ Flat.
        assert_eq!(v.trend, TrendDirection::Flat);
        assert_eq!(v.engaged.state, BinaryState::Active);
    }

    #[test]
    fn vanna_zero_is_neutral_and_disengaged() {
        let v = vanna_flow(0.0, None, BinaryState::Inactive);
        assert_eq!(v.direction, FlowDirection::Neutral);
        assert_eq!(v.trend, TrendDirection::Flat);
        assert_eq!(v.engaged.state, BinaryState::Inactive);
    }

    #[test]
    fn charm_intensity_and_bias() {
        let c = charm_bias(50.0, 100.0, BinaryState::Inactive);
        assert!((c.intensity - 0.5).abs() < 1e-9);
        assert_eq!(c.bias, BiasDirection::Bullish);
        assert_eq!(c.engaged.state, BinaryState::Active);
        let bear = charm_bias(-50.0, 100.0, BinaryState::Inactive);
        assert_eq!(bear.bias, BiasDirection::Bearish);
        // No prior-window max ⇒ intensity 0 (charm still engaged by magnitude).
        let cold = charm_bias(50.0, 0.0, BinaryState::Inactive);
        assert!(cold.intensity.abs() < 1e-12);
        assert_eq!(cold.engaged.state, BinaryState::Active);
    }

    #[test]
    fn migration_score_sign_and_stable_band() {
        // Shift +1 pt on spot 100 ⇒ migScore = 1/(100·0.01) = 1 ⇒ Bullish, active.
        let up = oi_migration(101.0, Some(100.0), 100.0, BinaryState::Inactive);
        assert!((up.score - 1.0).abs() < 1e-9);
        assert_eq!(up.direction, MigrationDirection::Bullish);
        assert_eq!(up.migrating.state, BinaryState::Active);
        // Tiny shift ⇒ inside STABLE band.
        let flat = oi_migration(100.02, Some(100.0), 100.0, BinaryState::Inactive);
        assert!(flat.score.abs() < MIGRATION_STABLE_BAND);
        assert_eq!(flat.direction, MigrationDirection::Stable);
        assert_eq!(flat.migrating.state, BinaryState::Inactive);
        let down = oi_migration(99.0, Some(100.0), 100.0, BinaryState::Inactive);
        assert_eq!(down.direction, MigrationDirection::Bearish);
    }

    #[test]
    fn migration_first_tick_is_stable() {
        let m = oi_migration(105.0, None, 100.0, BinaryState::Inactive);
        assert!(m.shift.abs() < 1e-12);
        assert_eq!(m.direction, MigrationDirection::Stable);
    }

    #[test]
    fn gamma_velocity_sign_and_acceleration() {
        let g = gamma_dynamics(1.0e10, Some(9.0e9), Some(8.0e9), BinaryState::Inactive);
        assert!((g.velocity - 1.0e9).abs() < 1.0);
        // prior velocity 1e9 ⇒ acceleration ~0.
        assert!(g.acceleration.abs() < 1.0);
        assert_eq!(g.direction, GammaHedgeDirection::AddingHedges);
        assert_eq!(g.hedging_active.state, BinaryState::Active);
    }

    #[test]
    fn gamma_d1_fix_responds_on_a_small_book() {
        // netGex 1e5, velocity 1e4: relative thresh 3e3 ⇒ active. The legacy $1e6
        // absolute floor would have pinned this permanently STABLE.
        let g = gamma_dynamics(1.0e5, Some(9.0e4), None, BinaryState::Inactive);
        assert!(g.velocity_score.abs() > 1.0);
        assert_eq!(g.direction, GammaHedgeDirection::AddingHedges);
        assert_eq!(g.hedging_active.state, BinaryState::Active);
    }

    #[test]
    fn gamma_hysteresis_holds_through_the_band() {
        // net 1000, thresh 30, velocity 27 ⇒ |score| 0.9, inside (0.85, 1.0).
        let held = gamma_dynamics(1_000.0, Some(973.0), None, BinaryState::Active);
        assert!((held.velocity_score.abs() - 0.9).abs() < 1e-9);
        // Instantaneous direction is Stable while the hysteretic readout holds Active.
        assert_eq!(held.direction, GammaHedgeDirection::Stable);
        assert_eq!(held.hedging_active.state, BinaryState::Active);
        // Cold start at the same score resolves INACTIVE.
        let cold = gamma_dynamics(1_000.0, Some(973.0), None, BinaryState::Inactive);
        assert_eq!(cold.hedging_active.state, BinaryState::Inactive);
    }

    #[test]
    fn oi_flow_building_and_unwinding() {
        let build = oi_flow(10_000.0, Some(9_000.0), 1.0, BinaryState::Inactive);
        assert!((build.velocity - 1_000.0).abs() < 1e-9);
        // thresh = max(1, 0.005·10000) = 50 ⇒ score 20 ⇒ Building.
        assert_eq!(build.direction, OiFlowDirection::Building);
        assert_eq!(build.flowing.state, BinaryState::Active);
        let unwind = oi_flow(9_000.0, Some(10_000.0), 1.0, BinaryState::Inactive);
        assert_eq!(unwind.direction, OiFlowDirection::Unwinding);
        // Small change stays STABLE.
        let stable = oi_flow(10_010.0, Some(10_000.0), 1.0, BinaryState::Inactive);
        assert_eq!(stable.direction, OiFlowDirection::Stable);
        assert_eq!(stable.flowing.state, BinaryState::Inactive);
    }

    #[test]
    fn oi_flow_first_tick_is_stable() {
        let f = oi_flow(10_000.0, None, 0.0, BinaryState::Inactive);
        assert!(f.velocity.abs() < 1e-12);
        assert_eq!(f.direction, OiFlowDirection::Stable);
    }

    #[test]
    fn dealer_dynamics_round_trips_through_serde() {
        let input = DealerDynamicsInput {
            spot: 100.0,
            net_gex: 1.0e10,
            net_vanna: 5.0e8,
            net_charm: -2.0e7,
            gex_com: 101.0,
            total_oi: 10_000.0,
            prev_net_gex: Some(9.0e9),
            prev2_net_gex: Some(8.0e9),
            prev_net_vanna: Some(4.0e8),
            prev_gex_com: Some(100.0),
            prev_total_oi: Some(9_000.0),
            prior_max_abs_charm: 5.0e7,
            dt_min: 1.0,
            prev_states: DealerPrevStates::default(),
        };
        let d = dealer_dynamics(&input);
        assert_eq!(d.gamma.direction, GammaHedgeDirection::AddingHedges);
        assert_eq!(d.migration.direction, MigrationDirection::Bullish);
        assert_eq!(d.charm.bias, BiasDirection::Bearish);
        let json = serde_json::to_string(&d).unwrap();
        let back: DealerDynamics = serde_json::from_str(&json).unwrap();
        assert_eq!(back.oi_flow.direction, OiFlowDirection::Building);
        assert!(json.contains("\"ADDING_HEDGES\""));
    }
}
