//! Terminal wire schema — the single authoritative definition of every
//! payload the gateway emits and the terminal renders.
//!
//! This is a deliberately-designed contract, not a reflection of internal
//! engine structs. The gateway's pipeline maps engine outputs into these
//! types; the TypeScript mirror in `terminal/src/wire/` reflects exactly this
//! shape. Keeping the contract here (in the dependency-free core) means the
//! terminal can be built against it without depending on the engine crates,
//! and internal engine refactors cannot silently break the wire.
//!
//! Doctrine (`docs/ARCHITECTURE.md` §2): every stateful readout is a
//! [`crate::Readout`] — [`crate::BinaryState`] plus the continuous score it
//! was resolved from. There is no third state anywhere in this schema.

use crate::{BinaryState, ExpiryDate, Readout, TsMillis};
use serde::{Deserialize, Serialize};

/// Wire protocol version. Bump on any breaking schema change; the gateway
/// advertises it on the WS hello frame and `/api/v1/health`.
pub const WIRE_VERSION: u32 = 1;

/// A labeled numeric metric with an optional unit tag, for dense readouts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Metric {
    /// Short display label.
    pub label: String,
    /// Numeric value.
    pub value: f64,
    /// Unit tag (e.g. `"$"`, `"%"`, `"contracts"`, `"σ"`). Empty when
    /// dimensionless.
    pub unit: String,
}

impl Metric {
    /// Construct a metric.
    #[must_use]
    pub fn new(label: impl Into<String>, value: f64, unit: impl Into<String>) -> Self {
        Self { label: label.into(), value, unit: unit.into() }
    }
}

/// One rung of the per-strike dealer-exposure ladder.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StrikeRow {
    /// Strike price, points.
    pub strike: f64,
    /// Net gamma exposure at this strike, $ per 1% spot move.
    pub gex: f64,
    /// Net delta exposure at this strike, $.
    pub dex: f64,
    /// Net vanna exposure at this strike, $ per 1% IV move.
    pub vex: f64,
    /// Aggregate open interest at this strike, contracts.
    pub open_interest: u64,
    /// `true` if this is the strike nearest spot (render highlight).
    pub at_spot: bool,
}

/// A dealer wall (call or put) with its binary zone-state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WallReadout {
    /// Wall strike, points. `None` when no dominant wall exists on this side.
    pub strike: Option<f64>,
    /// Signed defended-side margin `(strike−spot)/spot` (call) or
    /// `(spot−strike)/spot` (put). Positive ⇒ wall intact.
    pub margin: f64,
    /// Wall strength, 0–100 (share of gross GEX).
    pub strength: f64,
    /// ACTIVE ⟺ the wall is not breached on its defended side; score carries
    /// `margin / testing-band` so "testing" is recoverable as `0 ≤ score < 1`.
    pub state: Readout,
}

/// Dealer-positioning structure panel (GEX engine + zones).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DealerPanel {
    /// Net gamma exposure, $ per 1% spot move.
    pub net_gex: f64,
    /// Gross gamma exposure, $ per 1% spot move.
    pub gross_gex: f64,
    /// Net delta exposure, $.
    pub net_dex: f64,
    /// Net vanna exposure, $ per 1% IV move.
    pub net_vex: f64,
    /// Net charm exposure, $ per day.
    pub net_charm: f64,
    /// Dealer State Index, [-1, 1] (directionless; >0 ⇒ stabilizing long
    /// gamma).
    pub dsi: f64,
    /// Dealer positioning normalized to [0, 1].
    pub dealer01: f64,
    /// Gamma-flip spot, points. `None` when no cumulative-GEX zero-crossing
    /// exists (never fabricated).
    pub gamma_flip: Option<f64>,
    /// ACTIVE ⟺ a gamma flip was located; score carries crossing quality.
    pub gamma_flip_state: Readout,
    /// Call wall and its zone-state.
    pub call_wall: WallReadout,
    /// Put wall and its zone-state.
    pub put_wall: WallReadout,
    /// Expected move as a fraction of spot (ATM IV · √T).
    pub expected_move_pct: f64,
    /// Magnet strike (peak strike-gravity), points. `None` when no dominant
    /// magnet exists.
    pub magnet: Option<f64>,
    /// Per-strike exposure ladder, sorted ascending by strike.
    pub ladder: Vec<StrikeRow>,
    /// Quotes excluded for lack of both greeks and IV (data-quality bit).
    pub excluded_quotes: u32,
}

/// Directional-thesis panel (technicals + thesis composite).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThesisPanel {
    /// Long-side thesis stability, 0–100.
    pub long_score: f64,
    /// Short-side thesis stability, 0–100.
    pub short_score: f64,
    /// Dominant-side direction: +1 long, −1 short, 0 balanced.
    pub direction: i8,
    /// ACTIVE ⟺ the dominant thesis is engaged past its hysteresis gate.
    pub engagement: Readout,
    /// Named sub-scores for the dominant side (structure, momentum, VWAP,
    /// volume, dealer), each 0–10.
    pub sub_scores: Vec<Metric>,
}

/// Volatility-regime panel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegimePanel {
    /// Hurst exponent (small-sample corrected). 0.5 ⇒ random walk.
    pub hurst: f64,
    /// Ornstein–Uhlenbeck half-life in bars, when mean-reverting.
    pub half_life_bars: Option<f64>,
    /// Regime label (descriptive: e.g. `TREND_EXPANSION`).
    pub label: String,
    /// Regime-state probabilities, summing to 1 (labeled).
    pub probabilities: Vec<Metric>,
    /// ACTIVE ⟺ the argmax regime is confident past its threshold.
    pub confidence: Readout,
    /// ACTIVE ⟺ volatility is compressed below its floor percentile.
    pub compression: Readout,
    /// ACTIVE ⟺ volatility is expanding above its ceiling percentile.
    pub expansion: Readout,
    /// Realized-vol term-structure slope (short − long), signed.
    pub term_structure_slope: f64,
}

/// Volatility surface / distribution panel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VolPanel {
    /// Current annualized realized volatility (decimal).
    pub realized_vol: f64,
    /// Current annualized implied volatility (decimal), ATM.
    pub implied_vol: f64,
    /// Variance risk premium, IV − RV (decimal).
    pub variance_risk_premium: f64,
    /// IV rank vs its own history, [0, 1].
    pub iv_rank: f64,
    /// IV percentile vs its own history, [0, 1].
    pub iv_percentile: f64,
    /// Risk-neutral density percentiles (5/25/50/75/95), price points.
    pub rnd_percentiles: Vec<Metric>,
}

/// Dealer-flow dynamics panel (vanna / charm / migration / gamma / OI).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FlowPanel {
    /// Vanna hedge-flow signed intensity, [-1, 1] (+ supportive).
    pub vanna_flow: f64,
    /// Charm bias signed intensity, [-1, 1] (+ bullish).
    pub charm_bias: f64,
    /// OI migration score, [-1, 1] (+ upward center-of-mass drift).
    pub migration: f64,
    /// ACTIVE ⟺ dealer hedging flow is engaged (gamma dynamics not stable).
    pub gamma_dynamics: Readout,
    /// ACTIVE ⟺ open-interest flow is building/unwinding (not stable).
    pub oi_flow: Readout,
}

/// A single engine's board entry: its binary state and continuous score.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EngineStatus {
    /// Engine identifier (stable key, e.g. `GEX`, `THESIS`, `REGIME`).
    pub engine: String,
    /// Binary state.
    pub state: BinaryState,
    /// The continuous score the state resolved from.
    pub score: f64,
}

/// The complete per-symbol terminal snapshot.
///
/// Always transported inside a [`WireFrame::Snapshot`], which supplies the
/// `"type":"SNAPSHOT"` discriminator via its internal tag — this struct
/// therefore carries no tag of its own (a second one would duplicate the JSON
/// key). The fields flatten directly beneath the frame tag on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TerminalSnapshot {
    /// Wire protocol version (mirrors [`WIRE_VERSION`]).
    pub wire_version: u32,
    /// Underlying ticker.
    pub symbol: String,
    /// Spot at snapshot time, points.
    pub spot: f64,
    /// Snapshot time (event time, not wall clock).
    pub ts: TsMillis,
    /// Nearest option expiry represented in the chain.
    pub expiry: Option<ExpiryDate>,
    /// `true` when this snapshot derives from the synthetic feed (never
    /// presented as live).
    pub synthetic: bool,
    /// Dealer-positioning structure.
    pub dealer: DealerPanel,
    /// Directional thesis.
    pub thesis: ThesisPanel,
    /// Volatility regime.
    pub regime: RegimePanel,
    /// Volatility surface / distribution.
    pub vol: VolPanel,
    /// Dealer-flow dynamics.
    pub flow: FlowPanel,
    /// The engine board: every engine's binary state + score, for the
    /// status strip.
    pub engines: Vec<EngineStatus>,
}

/// Frames the gateway pushes over the WebSocket. Tagged by `type`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum WireFrame {
    /// Connection handshake.
    Hello {
        /// Protocol version.
        wire_version: u32,
        /// Active feed provider name.
        feed: String,
    },
    /// A per-symbol snapshot.
    Snapshot(Box<TerminalSnapshot>),
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_tag_is_stable() {
        let snap = TerminalSnapshot {
            wire_version: WIRE_VERSION,
            symbol: "SPX".into(),
            spot: 6800.0,
            ts: TsMillis(1),
            expiry: None,
            synthetic: true,
            dealer: DealerPanel {
                net_gex: 0.0,
                gross_gex: 0.0,
                net_dex: 0.0,
                net_vex: 0.0,
                net_charm: 0.0,
                dsi: 0.0,
                dealer01: 0.5,
                gamma_flip: None,
                gamma_flip_state: Readout { state: BinaryState::Inactive, score: 0.0 },
                call_wall: WallReadout {
                    strike: None,
                    margin: 0.0,
                    strength: 0.0,
                    state: Readout { state: BinaryState::Inactive, score: 0.0 },
                },
                put_wall: WallReadout {
                    strike: None,
                    margin: 0.0,
                    strength: 0.0,
                    state: Readout { state: BinaryState::Inactive, score: 0.0 },
                },
                expected_move_pct: 0.0,
                magnet: None,
                ladder: vec![],
                excluded_quotes: 0,
            },
            thesis: ThesisPanel {
                long_score: 50.0,
                short_score: 50.0,
                direction: 0,
                engagement: Readout { state: BinaryState::Inactive, score: 0.5 },
                sub_scores: vec![],
            },
            regime: RegimePanel {
                hurst: 0.5,
                half_life_bars: None,
                label: "MEAN_REVERSION".into(),
                probabilities: vec![],
                confidence: Readout { state: BinaryState::Inactive, score: 0.33 },
                compression: Readout { state: BinaryState::Inactive, score: 0.0 },
                expansion: Readout { state: BinaryState::Inactive, score: 0.0 },
                term_structure_slope: 0.0,
            },
            vol: VolPanel {
                realized_vol: 0.14,
                implied_vol: 0.15,
                variance_risk_premium: 0.01,
                iv_rank: 0.5,
                iv_percentile: 0.5,
                rnd_percentiles: vec![],
            },
            flow: FlowPanel {
                vanna_flow: 0.0,
                charm_bias: 0.0,
                migration: 0.0,
                gamma_dynamics: Readout { state: BinaryState::Inactive, score: 0.0 },
                oi_flow: Readout { state: BinaryState::Inactive, score: 0.0 },
            },
            engines: vec![EngineStatus {
                engine: "GEX".into(),
                state: BinaryState::Active,
                score: 0.8,
            }],
        };
        // Bare snapshot round-trips (no tag of its own).
        let json = serde_json::to_string(&snap).unwrap();
        assert!(json.contains(r#""state":"ACTIVE""#));
        let round: TerminalSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(round, snap);

        // The frame envelope supplies exactly one "type":"SNAPSHOT" tag.
        let frame = WireFrame::Snapshot(Box::new(snap));
        let fjson = serde_json::to_string(&frame).unwrap();
        assert_eq!(fjson.matches(r#""type":"SNAPSHOT""#).count(), 1);
        assert!(!fjson.contains(r#""type":"SNAPSHOT","type""#));
        let fround: WireFrame = serde_json::from_str(&fjson).unwrap();
        assert_eq!(fround, frame);
    }

    #[test]
    fn wireframe_variants_tag_correctly() {
        let hello = WireFrame::Hello { wire_version: WIRE_VERSION, feed: "synthetic".into() };
        assert!(serde_json::to_string(&hello).unwrap().contains(r#""type":"HELLO""#));
    }
}
