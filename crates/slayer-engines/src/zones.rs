//! Strike gravity, dealer zones (E5), and wall zone-state (E10).
//!
//! Provenance: `docs/spec/03-dealer-structure.md` E5 (`computeStrikeGravity`,
//! `src/lib/strikeGravity.ts`) and E10 (the HOLDING / TESTING / FAILING wall
//! zone-state scoring, `src/components/DealerFlowView.tsx` lines 148–213).
//!
//! Deviations from legacy:
//! - E10 is the canonical ternary→binary collapse. HOLDING / TESTING / FAILING
//!   is replaced by a signed defended-side margin `m` (call `(wall−spot)/spot`,
//!   put `(spot−wall)/spot`), a continuous `score = m / WALL_TESTING_BAND_PCT`
//!   (breached `< 0`, TESTING recoverable as `0 ≤ score < 1`, HOLDING `≥ 1`),
//!   and a [`Readout`] resolving ACTIVE ⟺ `m ≥ 0` through a named
//!   [`HysteresisBand`]. No ternary status enum survives at the type level.
//! - D11 (asymmetric TESTING band): preserved by construction. The margin axis
//!   has a single breach point at `m = 0`, so there is no TESTING band on the
//!   breached side — exactly the legacy label semantics — but the discontinuity
//!   is gone because the state now varies continuously in `score`.
//! - D12 (walls leaked the render window and the selected exposure tab): fixed.
//!   The two wall strikes are explicit inputs computed upstream on the full
//!   chain, so the zone-state no longer depends on presentation.
//! - The `atm` / `support` / `resistance` / `straddle` sides are descriptive
//!   DATA enums (sign of `strike − spot`, or zone edges vs spot), not states —
//!   only the "is the wall in force?" question becomes a [`Readout`].
//! - Legacy `null` returns for degenerate input (empty chain, `spot ≤ 0`) become
//!   empty structures / `None` fields / INACTIVE readouts per the binary-state
//!   doctrine's absence rule; nothing panics on data.

use serde::{Deserialize, Serialize};
use slayer_core::{BinaryState, HysteresisBand, Readout};

use std::cmp::Ordering;

/// Default gravity weight on absolute GEX density.
pub const GRAVITY_W_GEX: f64 = 0.4;
/// Default gravity weight on total open interest.
pub const GRAVITY_W_OI: f64 = 0.2;
/// Default gravity weight on total volume.
pub const GRAVITY_W_VOL: f64 = 0.2;
/// Default gravity weight on proximity to spot.
pub const GRAVITY_W_PROX: f64 = 0.2;
/// Default number of top-gravity strikes retained and clustered into zones.
pub const GRAVITY_TOP_N: usize = 10;
/// Proximity decay scale, as a fraction of spot: a strike this far from spot
/// scores `e⁻¹ ≈ 0.37` on the proximity axis.
pub const GRAVITY_PROXIMITY_SCALE: f64 = 0.04;
/// Half-width of the at-the-money band, as a fraction of spot. Inside it a
/// strike's side is `Atm` rather than support/resistance.
pub const GRAVITY_ATM_BAND_PCT: f64 = 0.001;

/// Quantile of the sorted inter-strike gaps used to estimate the strike step
/// (a low quantile so missing/illiquid strikes do not over-merge walls).
const STRIKE_STEP_QUANTILE: f64 = 0.25;
/// Strike-step fallback as a fraction of spot when no positive gaps exist.
const STRIKE_STEP_FALLBACK_PCT: f64 = 0.005;
/// Zone-gap multiplier: strikes within this many strike steps cluster together.
const ZONE_GAP_STEP_MULT: f64 = 2.5;
/// Zone-gap floor as a fraction of spot.
const ZONE_GAP_MIN_PCT: f64 = 0.001;
/// Divisor floor for the cluster-concentration ratio (legacy `|| 1`).
const TOTAL_GRAVITY_FLOOR: f64 = 1.0;

/// Testing-band half-width, as a fraction of spot: within `±0.5%` of the wall a
/// legacy label read TESTING. It is the unit of the wall zone-state `score`
/// (`score = m / WALL_TESTING_BAND_PCT`).
pub const WALL_TESTING_BAND_PCT: f64 = 0.005;
/// Score (in testing-band units) at or above which a wall reads not-breached:
/// `m ≥ 0 ⟺ score ≥ 0`. This is the activation edge of [`WALL_ZONE_BAND`].
const WALL_ACTIVATE_AT_SCORE: f64 = 0.0;
/// Hysteresis retained past the breach point, in testing-band units. `0.1`
/// testing-band units is `0.05%` of spot: a holding wall stays ACTIVE until
/// price crosses more than this far past the strike, per the anti-flap doctrine
/// (`docs/ARCHITECTURE.md` §2). Cold-start resolution is exactly `ACTIVE ⟺ m ≥ 0`.
const WALL_DEACTIVATE_HYSTERESIS: f64 = 0.1;
/// Hysteresis band resolving the wall zone-state score into a [`BinaryState`].
const WALL_ZONE_BAND: HysteresisBand = HysteresisBand::new(
    WALL_ACTIVATE_AT_SCORE,
    WALL_ACTIVATE_AT_SCORE - WALL_DEACTIVATE_HYSTERESIS,
);

/// Which side of the book a strike sits on relative to spot. Descriptive DATA.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum GravitySide {
    /// Below spot (`strike < spot`): a support strike.
    Support,
    /// Above spot (`strike > spot`): a resistance strike.
    Resistance,
    /// Within [`GRAVITY_ATM_BAND_PCT`] of spot: at-the-money.
    Atm,
}

/// Which side of spot a clustered zone occupies. Descriptive DATA.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ZoneSide {
    /// Entire zone below spot (`hi < spot`).
    Support,
    /// Entire zone above spot (`lo > spot`).
    Resistance,
    /// Zone straddles spot (a pin zone).
    Straddle,
}

/// Which dealer wall a [`WallZone`] describes. Descriptive DATA.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum WallKind {
    /// The call wall (defended from above: intact while `strike ≥ spot`).
    Call,
    /// The put wall (defended from below: intact while `strike ≤ spot`).
    Put,
}

/// One strike's raw inputs to the gravity map. All fields are plain numbers so
/// the gateway can wire them without this module importing the GEX engine.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GravityStrikeInput {
    /// Strike price, points.
    pub strike: f64,
    /// Signed net dealer GEX at this strike, `$` (call − put convention).
    pub net_gex: f64,
    /// Call open interest, contracts.
    pub call_oi: f64,
    /// Put open interest, contracts.
    pub put_oi: f64,
    /// Call volume, contracts.
    pub call_volume: f64,
    /// Put volume, contracts.
    pub put_volume: f64,
}

/// Normalized gravity weights actually applied, after zeroing signal-less axes
/// and renormalizing to sum to 1.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GravityWeights {
    /// Weight applied to the GEX-density axis.
    pub gex: f64,
    /// Weight applied to the open-interest axis.
    pub oi: f64,
    /// Weight applied to the volume axis.
    pub volume: f64,
    /// Weight applied to the proximity axis.
    pub proximity: f64,
}

/// Tunable gravity configuration. [`Default`] reproduces the legacy blend.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GravityConfig {
    /// Raw weight on the GEX-density axis.
    pub w_gex: f64,
    /// Raw weight on the open-interest axis.
    pub w_oi: f64,
    /// Raw weight on the volume axis.
    pub w_volume: f64,
    /// Raw weight on the proximity axis (always kept; never zeroed).
    pub w_proximity: f64,
    /// Number of top-gravity strikes to rank and cluster.
    pub top_n: usize,
    /// Proximity decay scale as a fraction of spot.
    pub proximity_scale: f64,
}

impl Default for GravityConfig {
    fn default() -> Self {
        Self {
            w_gex: GRAVITY_W_GEX,
            w_oi: GRAVITY_W_OI,
            w_volume: GRAVITY_W_VOL,
            w_proximity: GRAVITY_W_PROX,
            top_n: GRAVITY_TOP_N,
            proximity_scale: GRAVITY_PROXIMITY_SCALE,
        }
    }
}

/// One scored strike in the gravity map.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GravityStrike {
    /// Strike price, points.
    pub strike: f64,
    /// Composite magnetism in `[0, 1]` (weighted blend of the four axes).
    pub gravity_score: f64,
    /// Signed net dealer GEX at this strike, `$`.
    pub net_gex: f64,
    /// Signed distance from spot as a fraction: `(strike − spot)/spot`.
    pub distance_pct: f64,
    /// Normalized GEX-density weight in `[0, 1]`.
    pub gex_weight: f64,
    /// Normalized open-interest weight in `[0, 1]`.
    pub oi_weight: f64,
    /// Normalized volume weight in `[0, 1]`.
    pub volume_weight: f64,
    /// Proximity weight in `(0, 1]`: `exp(−|distance_pct| / proximity_scale)`.
    pub proximity_weight: f64,
    /// Descriptive side relative to spot.
    pub side: GravitySide,
}

/// A contiguous high-magnetism band of strikes.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GravityZone {
    /// Lowest strike in the zone, points.
    pub lo: f64,
    /// Highest strike in the zone, points.
    pub hi: f64,
    /// Signed net dealer GEX summed over the zone, `$`.
    pub net_gex: f64,
    /// Total gravity summed over the zone.
    pub gravity: f64,
    /// Descriptive side relative to spot.
    pub side: ZoneSide,
}

/// Strike-gravity / dealer-zone map (E5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StrikeGravity {
    /// Underlying spot the map was computed at, points.
    pub spot: f64,
    /// Every scored strike, sorted by gravity descending (ties: strike ascending).
    pub strikes: Vec<GravityStrike>,
    /// Highest-gravity strike overall (the primary magnet), if any.
    pub primary_magnet: Option<f64>,
    /// Highest-gravity strike strictly above spot, if any.
    pub upper_magnet: Option<f64>,
    /// Highest-gravity strike strictly below spot, if any.
    pub lower_magnet: Option<f64>,
    /// Clustered zones from the top-N ranked strikes, sorted by gravity desc.
    pub zones: Vec<GravityZone>,
    /// Highest-gravity zone built from the ranked strikes below spot.
    pub support_wall: Option<GravityZone>,
    /// Highest-gravity zone built from the ranked strikes above spot.
    pub resistance_wall: Option<GravityZone>,
    /// Gravity concentration of the top zone in `[0, 1]`.
    pub cluster_score: f64,
    /// Normalized weights actually applied.
    pub weights: GravityWeights,
}

/// Wall zone-state (E10): the ternary→binary collapse for one dealer wall.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WallZone {
    /// Which wall this describes.
    pub kind: WallKind,
    /// Wall strike, points.
    pub strike: f64,
    /// Signed defended-side margin `m`: call `(strike−spot)/spot`, put
    /// `(spot−strike)/spot`. Positive ⇒ the wall is intact on its defended side.
    pub margin: f64,
    /// Binary state (ACTIVE ⟺ wall not breached, `m ≥ 0`) plus the continuous
    /// `score = m / WALL_TESTING_BAND_PCT` it resolved from. TESTING is
    /// recoverable as `0 ≤ score < 1`, HOLDING as `score ≥ 1`, breach as `< 0`.
    pub readout: Readout,
    /// E2 wall-strength magnitude companion, `0–100`, clamped.
    pub strength_0_100: f64,
}

/// One wall's inputs to [`zone_structure`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WallInput {
    /// Wall strike, points (computed upstream on the full chain — see D12).
    pub strike: f64,
    /// E2 wall-strength magnitude companion, `0–100`.
    pub strength_0_100: f64,
    /// Prior-tick binary state, carried forward for hysteresis.
    pub previous_state: BinaryState,
}

/// Combined input to [`zone_structure`].
#[derive(Debug, Clone, Copy)]
pub struct ZoneInput<'a> {
    /// Underlying spot, points.
    pub spot: f64,
    /// Per-strike gravity inputs.
    pub strikes: &'a [GravityStrikeInput],
    /// Gravity configuration.
    pub config: GravityConfig,
    /// Call-wall inputs.
    pub call_wall: WallInput,
    /// Put-wall inputs.
    pub put_wall: WallInput,
}

/// Full dealer zone structure: the gravity map plus both wall zone-states.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ZoneStructure {
    /// Strike-gravity / dealer-zone map (E5).
    pub gravity: StrikeGravity,
    /// Call-wall zone-state (E10).
    pub call_wall: WallZone,
    /// Put-wall zone-state (E10).
    pub put_wall: WallZone,
}

/// Coerce a non-finite value to `0.0` (legacy `fin`).
fn fin(v: f64) -> f64 {
    if v.is_finite() { v } else { 0.0 }
}

/// Total-ordering comparator over finite-ish `f64` (NaN sorts as equal).
fn cmp_f64(a: f64, b: f64) -> Ordering {
    a.partial_cmp(&b).unwrap_or(Ordering::Equal)
}

/// Empty gravity map for degenerate input.
fn empty_gravity(spot: f64) -> StrikeGravity {
    StrikeGravity {
        spot,
        strikes: Vec::new(),
        primary_magnet: None,
        upper_magnet: None,
        lower_magnet: None,
        zones: Vec::new(),
        support_wall: None,
        resistance_wall: None,
        cluster_score: 0.0,
        weights: GravityWeights {
            gex: 0.0,
            oi: 0.0,
            volume: 0.0,
            proximity: 0.0,
        },
    }
}

/// Cluster the given strikes into contiguous zones (legacy `buildZones`).
fn build_zones(mut rows: Vec<GravityStrike>, spot: f64, zone_gap: f64) -> Vec<GravityZone> {
    rows.sort_by(|a, b| cmp_f64(a.strike, b.strike));
    let mut zones = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        let mut j = i;
        while j + 1 < rows.len() && (rows[j + 1].strike - rows[j].strike) <= zone_gap {
            j += 1;
        }
        let cluster = &rows[i..=j];
        let lo = cluster
            .iter()
            .map(|s| s.strike)
            .fold(f64::INFINITY, f64::min);
        let hi = cluster
            .iter()
            .map(|s| s.strike)
            .fold(f64::NEG_INFINITY, f64::max);
        let net_gex = cluster.iter().map(|s| s.net_gex).sum();
        let gravity = cluster.iter().map(|s| s.gravity_score).sum();
        let side = if hi < spot {
            ZoneSide::Support
        } else if lo > spot {
            ZoneSide::Resistance
        } else {
            ZoneSide::Straddle
        };
        zones.push(GravityZone {
            lo,
            hi,
            net_gex,
            gravity,
            side,
        });
        i = j + 1;
    }
    zones
}

/// Estimate the strike step from the scored strikes (25th-percentile gap).
fn estimate_step(scored: &[GravityStrike], spot: f64) -> f64 {
    let mut sorted: Vec<f64> = scored.iter().map(|s| s.strike).collect();
    sorted.sort_by(|a, b| cmp_f64(*a, *b));
    let mut gaps: Vec<f64> = sorted
        .windows(2)
        .map(|w| w[1] - w[0])
        .filter(|d| *d > 0.0)
        .collect();
    if gaps.is_empty() {
        return spot * STRIKE_STEP_FALLBACK_PCT;
    }
    gaps.sort_by(|a, b| cmp_f64(*a, *b));
    let idx = ((gaps.len() as f64) * STRIKE_STEP_QUANTILE).floor() as usize;
    gaps[idx.min(gaps.len() - 1)]
}

/// Score every strike as a dealer magnet and cluster the top strikes into
/// support / resistance / straddle zones (E5, `computeStrikeGravity`).
#[must_use]
pub fn strike_gravity(
    strikes: &[GravityStrikeInput],
    spot: f64,
    config: &GravityConfig,
) -> StrikeGravity {
    if strikes.is_empty() || spot <= 0.0 || spot.is_nan() {
        return empty_gravity(spot);
    }

    // Kept rows: finite exposures, positive strike.
    struct Kept {
        strike: f64,
        net_gex: f64,
        abs_gex: f64,
        oi: f64,
        volume: f64,
    }
    let kept: Vec<Kept> = strikes
        .iter()
        .filter(|r| r.strike > 0.0)
        .map(|r| {
            let net_gex = fin(r.net_gex);
            Kept {
                strike: r.strike,
                net_gex,
                abs_gex: net_gex.abs(),
                oi: fin(r.call_oi) + fin(r.put_oi),
                volume: fin(r.call_volume) + fin(r.put_volume),
            }
        })
        .collect();
    if kept.is_empty() {
        return empty_gravity(spot);
    }

    let max_abs_gex = kept.iter().map(|k| k.abs_gex).fold(0.0_f64, f64::max);
    let max_oi = kept.iter().map(|k| k.oi).fold(0.0_f64, f64::max);
    let max_vol = kept.iter().map(|k| k.volume).fold(0.0_f64, f64::max);
    let has_gex = max_abs_gex > 0.0;
    let has_oi = max_oi > 0.0;
    let has_vol = max_vol > 0.0;

    // Zero out signal-less axes, keep proximity, renormalize to sum 1.
    let mut w_gex = if has_gex { config.w_gex } else { 0.0 };
    let mut w_oi = if has_oi { config.w_oi } else { 0.0 };
    let mut w_vol = if has_vol { config.w_volume } else { 0.0 };
    let mut w_prox = config.w_proximity;
    let w_sum = w_gex + w_oi + w_vol + w_prox;
    let w_sum = if w_sum > 0.0 { w_sum } else { 1.0 };
    w_gex /= w_sum;
    w_oi /= w_sum;
    w_vol /= w_sum;
    w_prox /= w_sum;
    let prox_scale = if config.proximity_scale > 0.0 {
        config.proximity_scale
    } else {
        GRAVITY_PROXIMITY_SCALE
    };

    let mut scored: Vec<GravityStrike> = kept
        .iter()
        .map(|k| {
            let gex_weight = if has_gex {
                k.abs_gex / max_abs_gex
            } else {
                0.0
            };
            let oi_weight = if has_oi { k.oi / max_oi } else { 0.0 };
            let volume_weight = if has_vol { k.volume / max_vol } else { 0.0 };
            let distance_pct = (k.strike - spot) / spot;
            let proximity_weight = (-distance_pct.abs() / prox_scale).exp();
            let gravity_score = w_gex * gex_weight
                + w_oi * oi_weight
                + w_vol * volume_weight
                + w_prox * proximity_weight;
            let side = if distance_pct.abs() < GRAVITY_ATM_BAND_PCT {
                GravitySide::Atm
            } else if distance_pct > 0.0 {
                GravitySide::Resistance
            } else {
                GravitySide::Support
            };
            GravityStrike {
                strike: k.strike,
                gravity_score,
                net_gex: k.net_gex,
                distance_pct,
                gex_weight,
                oi_weight,
                volume_weight,
                proximity_weight,
                side,
            }
        })
        .collect();

    scored.sort_by(|a, b| {
        cmp_f64(b.gravity_score, a.gravity_score).then_with(|| cmp_f64(a.strike, b.strike))
    });

    let primary_magnet = scored.first().map(|s| s.strike);
    let upper_magnet = scored.iter().find(|s| s.strike > spot).map(|s| s.strike);
    let lower_magnet = scored.iter().find(|s| s.strike < spot).map(|s| s.strike);

    let ranked: Vec<GravityStrike> = scored.iter().take(config.top_n).copied().collect();
    let step = estimate_step(&scored, spot);
    let zone_gap = (step * ZONE_GAP_STEP_MULT).max(spot * ZONE_GAP_MIN_PCT);

    let mut zones = build_zones(ranked.clone(), spot, zone_gap);
    zones.sort_by(|a, b| cmp_f64(b.gravity, a.gravity));

    let support_rows: Vec<GravityStrike> =
        ranked.iter().filter(|s| s.strike < spot).copied().collect();
    let resistance_rows: Vec<GravityStrike> =
        ranked.iter().filter(|s| s.strike > spot).copied().collect();
    let support_wall = build_zones(support_rows, spot, zone_gap)
        .into_iter()
        .max_by(|a, b| cmp_f64(a.gravity, b.gravity));
    let resistance_wall = build_zones(resistance_rows, spot, zone_gap)
        .into_iter()
        .max_by(|a, b| cmp_f64(a.gravity, b.gravity));

    let total_ranked: f64 = ranked.iter().map(|s| s.gravity_score).sum();
    let denom = if total_ranked > 0.0 {
        total_ranked
    } else {
        TOTAL_GRAVITY_FLOOR
    };
    let cluster_score = zones.first().map_or(0.0, |z| (z.gravity / denom).min(1.0));

    StrikeGravity {
        spot,
        strikes: scored,
        primary_magnet,
        upper_magnet,
        lower_magnet,
        zones,
        support_wall,
        resistance_wall,
        cluster_score,
        weights: GravityWeights {
            gex: w_gex,
            oi: w_oi,
            volume: w_vol,
            proximity: w_prox,
        },
    }
}

/// Resolve one wall's zone-state (E10). ACTIVE ⟺ the wall is not breached on
/// its defended side (`m ≥ 0`); `readout.score = m / WALL_TESTING_BAND_PCT`.
#[must_use]
pub fn wall_zone(
    kind: WallKind,
    spot: f64,
    strike: f64,
    strength_0_100: f64,
    previous: BinaryState,
) -> WallZone {
    let margin = if spot > 0.0 {
        match kind {
            WallKind::Call => (strike - spot) / spot,
            WallKind::Put => (spot - strike) / spot,
        }
    } else {
        f64::NAN
    };
    let score = margin / WALL_TESTING_BAND_PCT;
    let readout = Readout::resolve_from(&WALL_ZONE_BAND, score, previous);
    WallZone {
        kind,
        strike,
        margin,
        readout,
        strength_0_100: fin(strength_0_100).clamp(0.0, 100.0),
    }
}

/// Assemble the full dealer zone structure (E5 gravity + both E10 wall states).
#[must_use]
pub fn zone_structure(input: &ZoneInput) -> ZoneStructure {
    let gravity = strike_gravity(input.strikes, input.spot, &input.config);
    let call_wall = wall_zone(
        WallKind::Call,
        input.spot,
        input.call_wall.strike,
        input.call_wall.strength_0_100,
        input.call_wall.previous_state,
    );
    let put_wall = wall_zone(
        WallKind::Put,
        input.spot,
        input.put_wall.strike,
        input.put_wall.strength_0_100,
        input.put_wall.previous_state,
    );
    ZoneStructure {
        gravity,
        call_wall,
        put_wall,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn strike(strike: f64, net_gex: f64, oi: f64, vol: f64) -> GravityStrikeInput {
        GravityStrikeInput {
            strike,
            net_gex,
            call_oi: oi,
            put_oi: 0.0,
            call_volume: vol,
            put_volume: 0.0,
        }
    }

    #[test]
    fn call_wall_above_spot_is_active_and_holding() {
        let w = wall_zone(WallKind::Call, 100.0, 105.0, 60.0, BinaryState::Inactive);
        assert!((w.margin - 0.05).abs() < 1e-12);
        assert!((w.readout.score - 10.0).abs() < 1e-9);
        assert_eq!(w.readout.state, BinaryState::Active);
    }

    #[test]
    fn call_wall_at_spot_is_active_score_zero() {
        let w = wall_zone(WallKind::Call, 100.0, 100.0, 50.0, BinaryState::Inactive);
        assert!(w.margin.abs() < 1e-12);
        assert!(w.readout.score.abs() < 1e-12);
        // m ≥ 0 ⟹ ACTIVE.
        assert_eq!(w.readout.state, BinaryState::Active);
    }

    #[test]
    fn call_wall_below_spot_is_inactive() {
        let w = wall_zone(WallKind::Call, 100.0, 95.0, 40.0, BinaryState::Inactive);
        assert!((w.margin - (-0.05)).abs() < 1e-12);
        assert!(w.readout.score < 0.0);
        assert_eq!(w.readout.state, BinaryState::Inactive);
    }

    #[test]
    fn testing_band_is_recoverable_as_score_between_zero_and_one() {
        // 0.3% above spot ⇒ inside the ±0.5% testing band.
        let w = wall_zone(WallKind::Call, 100.0, 100.3, 55.0, BinaryState::Inactive);
        assert!(w.readout.score >= 0.0 && w.readout.score < 1.0);
        assert_eq!(w.readout.state, BinaryState::Active);
    }

    #[test]
    fn put_wall_defended_from_below() {
        let held = wall_zone(WallKind::Put, 100.0, 95.0, 70.0, BinaryState::Inactive);
        assert!((held.margin - 0.05).abs() < 1e-12);
        assert_eq!(held.readout.state, BinaryState::Active);
        let breached = wall_zone(WallKind::Put, 100.0, 105.0, 70.0, BinaryState::Inactive);
        assert!(breached.margin < 0.0);
        assert_eq!(breached.readout.state, BinaryState::Inactive);
    }

    #[test]
    fn wall_hysteresis_holds_active_through_a_graze() {
        // score −0.08 (within the −0.1 hysteresis band): previously ACTIVE holds.
        let grazed = wall_zone(WallKind::Call, 100.0, 99.96, 50.0, BinaryState::Active);
        assert!(grazed.readout.score < 0.0 && grazed.readout.score > -0.1);
        assert_eq!(grazed.readout.state, BinaryState::Active);
        // Beyond the band (score ≤ −0.1) it deactivates regardless.
        let broken = wall_zone(WallKind::Call, 100.0, 99.9, 50.0, BinaryState::Active);
        assert!(broken.readout.score <= -0.1);
        assert_eq!(broken.readout.state, BinaryState::Inactive);
    }

    #[test]
    fn strength_is_clamped_and_finite() {
        let w = wall_zone(WallKind::Call, 100.0, 105.0, 250.0, BinaryState::Inactive);
        assert!((w.strength_0_100 - 100.0).abs() < 1e-12);
        let n = wall_zone(
            WallKind::Call,
            100.0,
            105.0,
            f64::NAN,
            BinaryState::Inactive,
        );
        assert!((n.strength_0_100).abs() < 1e-12);
    }

    #[test]
    fn magnet_id_on_synthetic_oi_hump() {
        // OI-only chain (no gex/vol) with a hump at 105 heavy enough to pull the
        // magnet off the ATM strike.
        let chain = vec![
            strike(90.0, 0.0, 100.0, 0.0),
            strike(95.0, 0.0, 100.0, 0.0),
            strike(100.0, 0.0, 100.0, 0.0),
            strike(105.0, 0.0, 10_000.0, 0.0),
            strike(110.0, 0.0, 100.0, 0.0),
        ];
        let g = strike_gravity(&chain, 100.0, &GravityConfig::default());
        assert_eq!(g.primary_magnet, Some(105.0));
        // Only OI and proximity carry weight; each renormalizes to 0.5.
        assert!((g.weights.oi - 0.5).abs() < 1e-12);
        assert!((g.weights.proximity - 0.5).abs() < 1e-12);
        assert!((g.weights.gex).abs() < 1e-12);
        assert!((g.weights.volume).abs() < 1e-12);
        // Neighbours resolve to the right sides of spot.
        assert_eq!(g.upper_magnet, Some(105.0));
        assert_eq!(g.lower_magnet, Some(95.0));
        assert!(g.cluster_score > 0.0 && g.cluster_score <= 1.0);
    }

    #[test]
    fn side_classification_by_sign_of_distance() {
        let chain = vec![
            strike(95.0, 0.0, 100.0, 0.0),
            strike(100.0, 0.0, 100.0, 0.0),
            strike(105.0, 0.0, 100.0, 0.0),
        ];
        let g = strike_gravity(&chain, 100.0, &GravityConfig::default());
        let side_of = |k: f64| {
            g.strikes
                .iter()
                .find(|s| (s.strike - k).abs() < 1e-9)
                .unwrap()
                .side
        };
        assert_eq!(side_of(95.0), GravitySide::Support);
        assert_eq!(side_of(100.0), GravitySide::Atm);
        assert_eq!(side_of(105.0), GravitySide::Resistance);
    }

    #[test]
    fn empty_or_degenerate_input_degrades_to_empty() {
        let g = strike_gravity(&[], 100.0, &GravityConfig::default());
        assert!(g.strikes.is_empty() && g.primary_magnet.is_none());
        let chain = vec![strike(100.0, 1.0, 1.0, 1.0)];
        let bad = strike_gravity(&chain, 0.0, &GravityConfig::default());
        assert!(bad.strikes.is_empty());
    }

    #[test]
    fn zone_structure_round_trips_through_serde() {
        let chain = vec![
            strike(95.0, -1.0e9, 500.0, 200.0),
            strike(100.0, 2.0e9, 800.0, 400.0),
            strike(105.0, 1.5e9, 600.0, 300.0),
        ];
        let input = ZoneInput {
            spot: 100.0,
            strikes: &chain,
            config: GravityConfig::default(),
            call_wall: WallInput {
                strike: 105.0,
                strength_0_100: 72.0,
                previous_state: BinaryState::Inactive,
            },
            put_wall: WallInput {
                strike: 95.0,
                strength_0_100: 64.0,
                previous_state: BinaryState::Inactive,
            },
        };
        let zs = zone_structure(&input);
        assert_eq!(zs.call_wall.readout.state, BinaryState::Active);
        assert_eq!(zs.put_wall.readout.state, BinaryState::Active);
        let json = serde_json::to_string(&zs).unwrap();
        let back: ZoneStructure = serde_json::from_str(&json).unwrap();
        assert_eq!(back.call_wall.kind, WallKind::Call);
        assert!(json.contains("\"ACTIVE\""));
    }
}
