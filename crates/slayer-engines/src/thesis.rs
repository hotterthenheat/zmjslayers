//! Thesis Stability composite and its sub-kernels.
//!
//! Reimplements the legacy SkyVision V11 thesis engines from
//! `docs/spec/01-skyvision-v11.md`:
//!
//! - **E6** — the momentum / VWAP / volume unit kernels ([`momentum01`],
//!   [`vwap01`], [`volume01`]).
//! - **E7** — `calculateSystemScoreFromCandles`, the 0..100 Thesis Stability
//!   blend and its ten 0..10 sub-readouts ([`SystemScore`]).
//!
//! The legacy engine takes a single trade direction and returns one score.
//! Here [`thesis`] evaluates **both** directions and returns a [`ThesisReadout`]
//! carrying the long score, the short score, the dominant side's full
//! sub-scores, the directional lean, and — per the binary-state doctrine — an
//! `engagement` [`Readout`] gating whether the terminal should treat the thesis
//! as in force at all.
//!
//! All indicator math is delegated to [`crate::technicals`], so the D14 (RVOL),
//! D16 (fractal ties) and D18 (Wilder seeding) fixes flow in through those
//! kernels. Everything here is pure: time enters only as the bar timestamps in
//! the candles, and the caller supplies any previous state explicitly.
//!
//! ## Deviations from legacy
//! - **Engagement gate (rebuild addition).** The legacy composite had no
//!   activation concept — it always emitted a raw number. Doctrine requires a
//!   binary state, so `engagement` resolves `max(long, short)/100` through
//!   [`ENGAGEMENT_BAND`] (hysteresis, activate `0.65` / deactivate `0.55`).
//!   These thresholds are rebuild constants, not legacy values.
//! - **VWAP source (rebuild reconstruction).** [`slayer_core::Candle`] carries
//!   no provider VWAP field, so the composite computes a session VWAP from
//!   candle volume via [`crate::technicals::session_vwap`] and falls back to the
//!   bar close where cumulative volume is zero — reproducing the legacy
//!   `bar.vwap || bar.close` semantics without fabricating a VWAP.
//! - **RVOL / seeding fixes** inherited from [`crate::technicals`]: D14, D18.

use serde::{Deserialize, Serialize};
use slayer_core::{BinaryState, Candle, HysteresisBand, Readout};

use crate::technicals::{
    ATR_FALLBACK, FRACTAL_HALF_WINDOW, RSI_NEUTRAL, RVOL_BASELINE_BARS, fractal_pivots, rvol,
    session_vwap, structure01, wilder_atr, wilder_rsi,
};

/// Long trade direction (`+1`) fed to the direction-aware kernels.
pub const DIR_LONG: f64 = 1.0;
/// Short trade direction (`−1`).
pub const DIR_SHORT: f64 = -1.0;

/// Wilder period used by the composite's RSI/ATR series (E4 legacy default).
const COMPOSITE_WILDER_PERIOD: usize = crate::technicals::WILDER_PERIOD_DEFAULT;

// --- E6 kernel constants ---------------------------------------------------

/// `momentum01` tanh scale `m0`: `momVel` is measured in ATR-units of a 10-bar
/// move, and `tanh(momVel/MOMENTUM_TANH_SCALE)` shapes it into `[-1, 1]`.
pub const MOMENTUM_TANH_SCALE: f64 = 2.0;
/// Multiplier applied to `momentum01` when momentum and RSI slope diverge.
pub const MOMENTUM_DIVERGENCE_PENALTY: f64 = 0.5;
/// `vwap01` kernel peak distance: the log-normal kernel peaks when price sits
/// this many ATRs beyond VWAP in the trade direction.
pub const VWAP_PEAK_ATR: f64 = 0.5;
/// `vwap01` kernel width in log space.
pub const VWAP_KERNEL_SIGMA: f64 = 0.6;
/// Relative volume at which `volume01` saturates to 1.0.
pub const RVOL_FULL: f64 = 2.0;

// --- E7 composite constants ------------------------------------------------

/// Thesis Stability blend weight on Structure01.
pub const THESIS_W_STRUCT: f64 = 0.25;
/// Thesis Stability blend weight on momentum01.
pub const THESIS_W_MOM: f64 = 0.25;
/// Thesis Stability blend weight on the full VWAP score.
pub const THESIS_W_VWAP: f64 = 0.20;
/// Thesis Stability blend weight on the full volume score.
pub const THESIS_W_VOLUME: f64 = 0.15;
/// Thesis Stability blend weight on the candles-only dealer proxy.
pub const THESIS_W_DEALER: f64 = 0.15;

/// Lower clamp on Thesis Stability (E7 `THESIS_MIN`).
pub const THESIS_MIN: f64 = 1.0;
/// Upper clamp on Thesis Stability, also the score scale (E7 `THESIS_MAX`).
pub const THESIS_MAX: f64 = 100.0;

/// Lookback (bars) for the RSI slope term.
const RSI_SLOPE_LOOKBACK: usize = 5;
/// Lookback (bars) for the momentum-velocity close term.
const MOM_LOOKBACK: usize = 10;
/// Minimum bars before the VWAP-reclaim flag can be evaluated.
const VWAP_RECLAIM_MIN_BARS: usize = 4;
/// Number of prior bars whose RVOL forms the volume-trend baseline.
const RVOL_TREND_LOOKBACK: usize = 5;
/// Legacy neutral `prevRVOLSum` used before the trend baseline is available;
/// divided by [`RVOL_TREND_LOOKBACK`] it yields a neutral mean RVOL of 1.0.
const RVOL_TREND_DEFAULT_SUM: f64 = 5.0;

/// `vwap01_full` weight on the log-normal kernel.
pub const VWAP_BLEND_KERNEL: f64 = 0.6;
/// `vwap01_full` weight on the VWAP slope term.
pub const VWAP_BLEND_SLOPE: f64 = 0.25;
/// `vwap01_full` weight on the reclaim flag.
pub const VWAP_BLEND_RECLAIM: f64 = 0.15;

/// `volume01_full` weight on the base RVOL score.
pub const VOLUME_BLEND_BASE: f64 = 0.6;
/// `volume01_full` weight on the RVOL trend term.
pub const VOLUME_BLEND_TREND: f64 = 0.25;
/// `volume01_full` weight on the volume-acceleration term.
pub const VOLUME_BLEND_ACCEL: f64 = 0.15;

/// ATR growth mapped to a full expansion score: a 50% rise saturates.
pub const ATR_EXPANSION_SCALE: f64 = 0.5;
/// `momentumAcceleration` sub-score scale on `momVel`.
pub const MOM_ACCEL_SCALE: f64 = 4.0;

/// Neutral total emitted for empty input (E7 `SCORE_NEUTRAL`).
pub const SCORE_NEUTRAL_TOTAL: u8 = 50;
/// Neutral sub-score emitted for empty input.
pub const SCORE_NEUTRAL_SUB: u8 = 5;

/// Multiplier mapping a `[0, 1]` sub-metric to the `0..10` readout scale.
const SUB_SCORE_SCALE: f64 = 10.0;
/// Maximum value of a `0..10` sub-score readout.
const SUB_SCORE_MAX: u8 = 10;
/// Maximum value of the `total` readout (equals [`THESIS_MAX`] as an integer).
const TOTAL_SCORE_MAX: u8 = 100;

// --- Engagement gate (rebuild) ---------------------------------------------

/// Engagement activates when `max(long, short)/100` reaches this score.
pub const ENGAGEMENT_ACTIVATE: f64 = 0.65;
/// Engagement deactivates when the score falls to this level.
pub const ENGAGEMENT_DEACTIVATE: f64 = 0.55;
/// Hysteresis band gating whether the thesis is in force. Rebuild constants:
/// the legacy engine had no such gate (see module deviations).
pub const ENGAGEMENT_BAND: HysteresisBand =
    HysteresisBand::new(ENGAGEMENT_ACTIVATE, ENGAGEMENT_DEACTIVATE);

/// The ten 0..10 sub-readouts plus the 1..100 total, for one trade direction
/// (E7 step 13). Integer readouts, as the terminal renders them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemScore {
    /// Thesis Stability, rounded to `1..=100`.
    pub total: u8,
    /// Structure quality, `struct01 · 10`.
    pub structure_quality: u8,
    /// Displacement quality — identical to `structure_quality` in the legacy
    /// mapping; preserved as a distinct field for wire fidelity.
    pub displacement_quality: u8,
    /// Volume expansion, `volume01_full · 10`.
    pub volume_expansion: u8,
    /// RSI cascade, `momentum01 · 10`.
    pub rsi_cascade: u8,
    /// VWAP alignment, `vwap01_full · 10`.
    pub vwap_alignment: u8,
    /// Liquidity sweep proxy, `dealer01 · 10`.
    pub liquidity_sweep: u8,
    /// Higher-timeframe agreement, `thesisStability / 10`.
    pub htf_agreement: u8,
    /// Volatility regime, `atr_expansion · 10`.
    pub volatility_regime: u8,
    /// Premium/discount, `(1 − vwap01_full) · 10`.
    pub premium_discount: u8,
    /// Momentum acceleration, `clamp(momVel/4, −1, 1)` remapped to `0..10`.
    pub momentum_acceleration: u8,
}

impl SystemScore {
    /// The neutral score emitted for empty candle input (E7 step 1).
    const fn neutral() -> Self {
        Self {
            total: SCORE_NEUTRAL_TOTAL,
            structure_quality: SCORE_NEUTRAL_SUB,
            displacement_quality: SCORE_NEUTRAL_SUB,
            volume_expansion: SCORE_NEUTRAL_SUB,
            rsi_cascade: SCORE_NEUTRAL_SUB,
            vwap_alignment: SCORE_NEUTRAL_SUB,
            liquidity_sweep: SCORE_NEUTRAL_SUB,
            htf_agreement: SCORE_NEUTRAL_SUB,
            volatility_regime: SCORE_NEUTRAL_SUB,
            premium_discount: SCORE_NEUTRAL_SUB,
            momentum_acceleration: SCORE_NEUTRAL_SUB,
        }
    }
}

/// The thesis readout: both directional scores, the dominant side's detail,
/// the directional lean, and the binary engagement state.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ThesisReadout {
    /// Thesis Stability for the long direction, `1..=100`.
    pub long_score: u8,
    /// Thesis Stability for the short direction, `1..=100`.
    pub short_score: u8,
    /// Sign of `long_score − short_score`: `+1` long-leaning, `−1`
    /// short-leaning, `0` balanced. Descriptive data, not a state.
    pub direction: i8,
    /// Full sub-scores for the dominant (higher-scoring) side.
    pub dominant: SystemScore,
    /// Whether the thesis is in force: `max(long, short)/100` resolved through
    /// [`ENGAGEMENT_BAND`]. The score field carries the continuous quantity.
    pub engagement: Readout,
}

/// `v` clamped to `[0, 1]`.
fn clamp01(v: f64) -> f64 {
    v.clamp(0.0, 1.0)
}

/// Remap a signed quantity (nominally `[-1, 1]`) into `[0, 1]` via `(v+1)/2`,
/// clamped. This is the legacy `clamp01((x+1)/2)` pattern in one place.
fn unit_from_signed(v: f64) -> f64 {
    clamp01((v + 1.0) / 2.0)
}

/// Sign of `x` as `+1 / −1 / 0` (both signed zeros and NaN map to `0`),
/// matching the `Math.sign` uses in the legacy kernels.
fn sign_of(x: f64) -> f64 {
    if x > 0.0 {
        1.0
    } else if x < 0.0 {
        -1.0
    } else {
        0.0
    }
}

/// JavaScript numeric `||`: returns `x` unless it is `0.0` or `NaN` (the falsy
/// numeric values), in which case it returns `fallback`. Reproduces the legacy
/// `|| 1` / `|| atrVal` divide guards exactly.
fn js_num_or(x: f64, fallback: f64) -> f64 {
    if x == 0.0 || x.is_nan() { fallback } else { x }
}

/// Round `v` to the nearest integer and clamp into `[lo, hi]`. Non-finite `v`
/// maps to `lo`.
fn round_to_u8(v: f64, lo: u8, hi: u8) -> u8 {
    if !v.is_finite() {
        return lo;
    }
    v.round().clamp(f64::from(lo), f64::from(hi)) as u8
}

/// Read a session-VWAP value, falling back to the bar close when it is absent,
/// zero, or non-finite (`bar.vwap || bar.close`).
fn vwap_or_close(vwaps: &[Option<f64>], candles: &[Candle], i: usize) -> f64 {
    match vwaps[i] {
        Some(v) if v.is_finite() && v != 0.0 => v,
        _ => candles[i].close,
    }
}

/// E6 momentum kernel: `clamp01(tanh(momVel/m0) · divPen)` where the divergence
/// penalty fires when momentum and RSI slope point opposite ways (and the slope
/// is non-zero). Negative momentum clamps to `0`.
#[must_use]
pub fn momentum01(mom_vel: f64, rsi_slope: f64) -> f64 {
    let divergent = sign_of(mom_vel) != sign_of(rsi_slope) && rsi_slope != 0.0;
    let div_pen = if divergent {
        MOMENTUM_DIVERGENCE_PENALTY
    } else {
        1.0
    };
    clamp01((mom_vel / MOMENTUM_TANH_SCALE).tanh() * div_pen)
}

/// E6 VWAP kernel: a log-normal bump in the signed ATR-distance of price beyond
/// VWAP. Returns `0` when `atr ≤ 0` (or NaN) or when price is not beyond VWAP in
/// the trade direction; peaks at `1.0` at [`VWAP_PEAK_ATR`] ATRs.
#[must_use]
pub fn vwap01(close: f64, vwap: f64, atr: f64, dir: f64) -> f64 {
    if atr <= 0.0 || atr.is_nan() {
        return 0.0;
    }
    let d = dir * (close - vwap) / atr;
    if d <= 0.0 {
        return 0.0;
    }
    let z = (d / VWAP_PEAK_ATR).ln();
    (-(z * z) / (2.0 * VWAP_KERNEL_SIGMA * VWAP_KERNEL_SIGMA)).exp()
}

/// E6 volume kernel: `clamp01((rvol − 1)/(RVOL_FULL − 1))`. RVOL of 1 scores 0,
/// RVOL at or above [`RVOL_FULL`] scores 1.
#[must_use]
pub fn volume01(relative_volume: f64) -> f64 {
    clamp01(
        (relative_volume - crate::technicals::RVOL_NEUTRAL)
            / (RVOL_FULL - crate::technicals::RVOL_NEUTRAL),
    )
}

/// Evaluate the E7 composite for one direction (`dir` sign: `+1` long / `−1`
/// short). `atr_fallback` is the ATR to substitute if the ATR series resolves
/// to zero (legacy `atrVal`).
fn system_score(candles: &[Candle], dir: f64, atr_fallback: f64) -> SystemScore {
    let n = candles.len();
    if n == 0 {
        return SystemScore::neutral();
    }
    let last = candles[n - 1];

    let rsis = wilder_rsi(candles, COMPOSITE_WILDER_PERIOD);
    let atrs = wilder_atr(candles, COMPOSITE_WILDER_PERIOD);
    let vwaps = session_vwap(candles);

    let current_rsi = rsis[n - 1].unwrap_or(RSI_NEUTRAL);
    let current_atr = js_num_or(atrs[n - 1].unwrap_or(ATR_FALLBACK), atr_fallback);
    let current_vwap = vwap_or_close(&vwaps, candles, n - 1);
    let atr_div = js_num_or(current_atr, 1.0);

    // Step 3–4: slope and velocity terms.
    let rsi_5 = if n > RSI_SLOPE_LOOKBACK {
        rsis[n - 1 - RSI_SLOPE_LOOKBACK].unwrap_or(RSI_NEUTRAL)
    } else {
        RSI_NEUTRAL
    };
    let close_10 = if n > MOM_LOOKBACK {
        candles[n - 1 - MOM_LOOKBACK].close
    } else {
        candles[0].close
    };
    let rsi_slope = dir * (current_rsi - rsi_5);
    let mom_vel = dir * (last.close - close_10) / atr_div;

    // Step 6–7: structure and momentum.
    let pivots = fractal_pivots(candles, FRACTAL_HALF_WINDOW);
    let struct01 = structure01(&pivots, current_atr, dir);
    let mom01 = momentum01(mom_vel, rsi_slope);

    // Step 8: VWAP block.
    let vwap01_kernel = vwap01(last.close, current_vwap, current_atr, dir);
    let vwap_ref = if n > RSI_SLOPE_LOOKBACK {
        vwap_or_close(&vwaps, candles, n - 1 - RSI_SLOPE_LOOKBACK)
    } else {
        current_vwap
    };
    let vwap_slope = dir * (current_vwap - vwap_ref) / atr_div;
    let crossed_before = n >= VWAP_RECLAIM_MIN_BARS && {
        let c2 = candles[n - 2].close;
        let v2 = vwap_or_close(&vwaps, candles, n - 2);
        if dir > 0.0 { c2 <= v2 } else { c2 >= v2 }
    };
    let crossed_back_now = if dir > 0.0 {
        last.close > current_vwap
    } else {
        last.close < current_vwap
    };
    let reclaim = if crossed_back_now && crossed_before {
        1.0
    } else {
        0.0
    };
    let vwap01_full = clamp01(
        VWAP_BLEND_KERNEL * vwap01_kernel
            + VWAP_BLEND_SLOPE * unit_from_signed(vwap_slope)
            + VWAP_BLEND_RECLAIM * reclaim,
    );

    // Step 9: volume block.
    let rvol_now = rvol(candles, n - 1, RVOL_BASELINE_BARS);
    let vol01_base = volume01(rvol_now);
    let mean_rvol_5 = {
        let sum = if n > RVOL_TREND_LOOKBACK {
            ((n - 1 - RVOL_TREND_LOOKBACK)..=(n - 2))
                .map(|idx| rvol(candles, idx, RVOL_BASELINE_BARS))
                .sum::<f64>()
        } else {
            RVOL_TREND_DEFAULT_SUM
        };
        sum / RVOL_TREND_LOOKBACK as f64
    };
    let rvol_trend = sign_of(rvol_now - mean_rvol_5);
    let prev_vol = if n >= 2 { candles[n - 2].volume } else { 1.0 };
    let accel_denom = if prev_vol <= 0.0 { 1.0 } else { prev_vol };
    let vol_accel = ((last.volume - prev_vol) / accel_denom).clamp(-1.0, 1.0);
    let volume01_full = clamp01(
        VOLUME_BLEND_BASE * vol01_base
            + VOLUME_BLEND_TREND * unit_from_signed(rvol_trend)
            + VOLUME_BLEND_ACCEL * unit_from_signed(vol_accel),
    );

    // Step 10–11: ATR expansion and dealer proxy.
    let atr_10 = if n > MOM_LOOKBACK {
        atrs[n - 1 - MOM_LOOKBACK].unwrap_or(ATR_FALLBACK)
    } else {
        atrs.first().copied().flatten().unwrap_or(ATR_FALLBACK)
    };
    let atr_expansion = clamp01((current_atr / js_num_or(atr_10, 1.0) - 1.0) / ATR_EXPANSION_SCALE);
    let dealer01 = unit_from_signed(dir * (last.close - current_vwap) / atr_div);

    // Step 12: Thesis Stability.
    let stability = (THESIS_MAX
        * (THESIS_W_STRUCT * struct01
            + THESIS_W_MOM * mom01
            + THESIS_W_VWAP * vwap01_full
            + THESIS_W_VOLUME * volume01_full
            + THESIS_W_DEALER * dealer01))
        .clamp(THESIS_MIN, THESIS_MAX);

    // Step 13: sub-score mapping.
    let struct_sub = round_to_u8(struct01 * SUB_SCORE_SCALE, 0, SUB_SCORE_MAX);
    let mom_accel = round_to_u8(
        unit_from_signed((mom_vel / MOM_ACCEL_SCALE).clamp(-1.0, 1.0)) * SUB_SCORE_SCALE,
        0,
        SUB_SCORE_MAX,
    );
    SystemScore {
        total: round_to_u8(stability, 0, TOTAL_SCORE_MAX),
        structure_quality: struct_sub,
        displacement_quality: struct_sub,
        volume_expansion: round_to_u8(volume01_full * SUB_SCORE_SCALE, 0, SUB_SCORE_MAX),
        rsi_cascade: round_to_u8(mom01 * SUB_SCORE_SCALE, 0, SUB_SCORE_MAX),
        vwap_alignment: round_to_u8(vwap01_full * SUB_SCORE_SCALE, 0, SUB_SCORE_MAX),
        liquidity_sweep: round_to_u8(dealer01 * SUB_SCORE_SCALE, 0, SUB_SCORE_MAX),
        htf_agreement: round_to_u8(stability / SUB_SCORE_SCALE, 0, SUB_SCORE_MAX),
        volatility_regime: round_to_u8(atr_expansion * SUB_SCORE_SCALE, 0, SUB_SCORE_MAX),
        premium_discount: round_to_u8((1.0 - vwap01_full) * SUB_SCORE_SCALE, 0, SUB_SCORE_MAX),
        momentum_acceleration: mom_accel,
    }
}

/// Resolve the thesis with an explicit previous engagement state (for
/// hysteresis across ticks). Pure: no clock, no hidden state.
fn resolve_thesis(candles: &[Candle], atr_fallback: f64, previous: BinaryState) -> ThesisReadout {
    let long = system_score(candles, DIR_LONG, atr_fallback);
    let short = system_score(candles, DIR_SHORT, atr_fallback);

    let (direction, dominant) = if long.total > short.total {
        (1i8, long)
    } else if long.total < short.total {
        (-1i8, short)
    } else {
        (0i8, long)
    };

    let engagement_score = f64::from(long.total.max(short.total)) / THESIS_MAX;
    let engagement = Readout::resolve_from(&ENGAGEMENT_BAND, engagement_score, previous);

    ThesisReadout {
        long_score: long.total,
        short_score: short.total,
        direction,
        dominant,
        engagement,
    }
}

/// Evaluate the E7 thesis in both directions (cold start: engagement resolves
/// with no prior state). `atr_fallback` substitutes for a zero ATR series.
#[must_use]
pub fn thesis(candles: &[Candle], atr_fallback: f64) -> ThesisReadout {
    resolve_thesis(candles, atr_fallback, BinaryState::Inactive)
}

/// Like [`thesis`] but carries the previous engagement state forward through
/// [`ENGAGEMENT_BAND`] so boundary noise cannot flap the state across ticks.
#[must_use]
pub fn thesis_from(candles: &[Candle], atr_fallback: f64, previous: BinaryState) -> ThesisReadout {
    resolve_thesis(candles, atr_fallback, previous)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use slayer_core::TsMillis;

    const EPS: f64 = 1e-9;

    fn candle(h: f64, l: f64, c: f64, v: f64) -> Candle {
        Candle {
            ts: TsMillis(0),
            open: c,
            high: h,
            low: l,
            close: c,
            volume: v,
        }
    }

    #[test]
    fn momentum01_penalizes_divergence_and_clamps_negative() {
        // Same sign, no penalty.
        let aligned = momentum01(1.0, 1.0);
        assert!((aligned - (0.5_f64).tanh()).abs() < EPS);
        // Opposite sign with non-zero slope, half penalty.
        let divergent = momentum01(1.0, -1.0);
        assert!((divergent - (0.5_f64).tanh() * MOMENTUM_DIVERGENCE_PENALTY).abs() < EPS);
        // Zero slope disables the penalty even though signs "differ".
        let zero_slope = momentum01(1.0, 0.0);
        assert!((zero_slope - (0.5_f64).tanh()).abs() < EPS);
        // Negative momentum clamps to zero.
        assert!(momentum01(-3.0, -3.0).abs() < EPS);
    }

    #[test]
    fn vwap01_peaks_at_the_kernel_distance() {
        // d = dir*(close-vwap)/atr = 0.5 = VWAP_PEAK_ATR => kernel = 1.0.
        let v = vwap01(100.5, 100.0, 1.0, DIR_LONG);
        assert!((v - 1.0).abs() < EPS);
        // Wrong side of VWAP => 0.
        assert!(vwap01(99.5, 100.0, 1.0, DIR_LONG).abs() < EPS);
        // Non-positive ATR => 0.
        assert!(vwap01(100.5, 100.0, 0.0, DIR_LONG).abs() < EPS);
        assert!(vwap01(100.5, 100.0, f64::NAN, DIR_LONG).abs() < EPS);
    }

    #[test]
    fn volume01_maps_rvol_range() {
        assert!(volume01(1.0).abs() < EPS);
        assert!((volume01(1.5) - 0.5).abs() < EPS);
        assert!((volume01(2.0) - 1.0).abs() < EPS);
        assert!((volume01(5.0) - 1.0).abs() < EPS); // saturates
    }

    #[test]
    fn empty_candles_return_neutral_and_disengaged() {
        let t = thesis(&[], 1.0);
        assert_eq!(t.long_score, SCORE_NEUTRAL_TOTAL);
        assert_eq!(t.short_score, SCORE_NEUTRAL_TOTAL);
        assert_eq!(t.direction, 0);
        assert_eq!(t.dominant.momentum_acceleration, SCORE_NEUTRAL_SUB);
        // score = 50/100 = 0.5 < deactivate 0.55 => INACTIVE.
        assert_eq!(t.engagement.state, BinaryState::Inactive);
        assert!((t.engagement.score - 0.5).abs() < EPS);
    }

    #[test]
    fn clean_uptrend_leans_long() {
        // Rising closes with rising volume: long must dominate short.
        let candles: Vec<Candle> = (0..40)
            .map(|i| {
                let c = 100.0 + i as f64;
                candle(c + 0.5, c - 0.5, c, 1000.0 + i as f64 * 10.0)
            })
            .collect();
        let t = thesis(&candles, 1.0);
        assert!(
            t.long_score > t.short_score,
            "long {} short {}",
            t.long_score,
            t.short_score
        );
        assert_eq!(t.direction, 1);
        // The dominant side is the long side; its total equals long_score.
        assert_eq!(t.dominant.total, t.long_score);
    }

    #[test]
    fn clean_downtrend_leans_short() {
        let candles: Vec<Candle> = (0..40)
            .map(|i| {
                let c = 200.0 - i as f64;
                candle(c + 0.5, c - 0.5, c, 1000.0 + i as f64 * 10.0)
            })
            .collect();
        let t = thesis(&candles, 1.0);
        assert!(t.short_score > t.long_score);
        assert_eq!(t.direction, -1);
        assert_eq!(t.dominant.total, t.short_score);
    }

    #[test]
    fn engagement_score_ties_to_the_hysteresis_band() {
        let candles: Vec<Candle> = (0..40)
            .map(|i| {
                let c = 100.0 + i as f64;
                candle(c + 0.5, c - 0.5, c, 1000.0)
            })
            .collect();
        let t = thesis(&candles, 1.0);
        let expected = f64::from(t.long_score.max(t.short_score)) / THESIS_MAX;
        assert!((t.engagement.score - expected).abs() < EPS);
        assert_eq!(
            t.engagement.state,
            ENGAGEMENT_BAND.resolve(expected, BinaryState::Inactive)
        );
    }

    #[test]
    fn previous_state_is_carried_through_the_band() {
        // A score strictly inside the band holds whatever state came before.
        let mid = 0.5 * (ENGAGEMENT_ACTIVATE + ENGAGEMENT_DEACTIVATE);
        assert_eq!(
            ENGAGEMENT_BAND.resolve(mid, BinaryState::Active),
            BinaryState::Active
        );
        assert_eq!(
            ENGAGEMENT_BAND.resolve(mid, BinaryState::Inactive),
            BinaryState::Inactive
        );
    }

    #[test]
    fn thesis_readout_round_trips_over_the_wire() {
        let candles: Vec<Candle> = (0..30)
            .map(|i| {
                let c = 100.0 + i as f64;
                candle(c + 0.5, c - 0.5, c, 1000.0)
            })
            .collect();
        let t = thesis(&candles, 1.0);
        let json = serde_json::to_string(&t).unwrap();
        let back: ThesisReadout = serde_json::from_str(&json).unwrap();
        assert_eq!(t, back);
        // Binary state renders in the doctrine's wire vocabulary.
        assert!(json.contains("ACTIVE") || json.contains("INACTIVE"));
    }

    #[test]
    fn sub_scores_stay_in_range() {
        let candles: Vec<Candle> = (0..50)
            .map(|i| {
                let c = 100.0 + (i as f64 * 0.3).sin() * 5.0;
                candle(c + 1.0, c - 1.0, c, 1000.0 + i as f64)
            })
            .collect();
        let t = thesis(&candles, 1.0);
        for s in [
            t.dominant.structure_quality,
            t.dominant.displacement_quality,
            t.dominant.volume_expansion,
            t.dominant.rsi_cascade,
            t.dominant.vwap_alignment,
            t.dominant.liquidity_sweep,
            t.dominant.htf_agreement,
            t.dominant.volatility_regime,
            t.dominant.premium_discount,
            t.dominant.momentum_acceleration,
        ] {
            assert!(s <= SUB_SCORE_MAX);
        }
        assert!(t.dominant.total >= 1 && t.dominant.total <= TOTAL_SCORE_MAX);
    }
}
