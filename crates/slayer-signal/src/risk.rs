//! Historical risk scoring: tail risk, liquidity, and model trust.
//!
//! Provenance: legacy `v11Math.ts` `calculatePercentile` / `computeTailRisk`
//! (E11), `computeLiquidityScore` (E12), and `computeModelTrust` (E13). Given
//! *real* similar-trade returns, a live quote, and a real calibration history,
//! this module resolves the three risk inputs the decision gate (E16) consumes:
//! a historical VaR/ES tail-risk profile, a 0–100 liquidity score, and a model
//! trust score with a descriptive letter grade.
//!
//! Every quantity the legacy fabricated is an explicit parameter here: the
//! gateway supplies a real `volume`, `open_interest`, prior mids, prediction
//! history, similar-sample count, and recent win rate, or documents a prior.
//! Nothing is invented (D11).
//!
//! # State doctrine
//!
//! These are *scoring* functions feeding the decision gate, not in-force
//! questions, so they return descriptive data (continuous sub-scores plus a
//! [`TrustGrade`] letter), not [`slayer_core::Readout`]s. The letter grade is
//! directional/descriptive DATA — a plain enum is correct here (see
//! `docs/ARCHITECTURE.md` §2). The binary opportunity readout is assembled one
//! layer up, in `decision.rs`.
//!
//! # Cold-start behavior (documented, never guessed)
//!
//! - Empty similar-trade `returns` → VaR/ES resolve to `0`, `worst` falls back
//!   to [`DEFAULT_WORST_LOSS`], `score` to `0`. Absence of loss history is not
//!   evidence of loss; the tail-risk score reads zero and the decision layer is
//!   expected to gate on sample strength, not read this as "safe".
//! - Fewer than two `prior_mids` → quote stability reads `1.0`: with no second
//!   quote there is nothing to contradict stability.
//! - Empty `pred_actual_deltas` → the forecast-error term uses the documented
//!   prior [`DEFAULT_FORECAST_MAE`], never a live-looking computed value.
//!
//! # Deviations from legacy
//!
//! - **D3** — the model-trust forecast-error term now compares *like units*.
//!   The legacy `predActualDeltas = returns.map(r → r − calibratedP)` subtracted
//!   a probability from a fractional return, so its MAE was dimensionless
//!   nonsense. Here `pred_actual_deltas` is defined as a per-observation
//!   *same-unit* forecast error supplied by the caller — either
//!   `predicted_win_prob − realized_win_indicator` (probability units, the
//!   recommended convention, for which [`FORECAST_ERROR_SCALE_DEFAULT`] is
//!   calibrated) or `predicted_return − realized_return` (return units). This
//!   module only takes the deltas and averages their magnitudes; correctness of
//!   the *unit* is a documented contract on the caller.
//! - **D10** — quote stability is now scale-independent. The legacy compared a
//!   `$²` variance of raw mids against an absolute ceiling, so a $500 SPX option
//!   with ordinary tick noise always scored 0 and a $0.30 option always ~1.
//!   Here stability is the variance of *relative* mids (each mid divided by the
//!   mean mid — a squared coefficient of variation), which is invariant under
//!   scaling the whole quote. `stability_max` is therefore a dimensionless
//!   relative-variance ceiling, not `$²`.
//! - **D11** — no fabricated analytics. `volume`, `open_interest`, `prior_mids`,
//!   `predictions_history`, `n_similar`, and `recent_win_rate` are all real
//!   parameters; none are the legacy hardcoded 14 500 / 18 800 / 0.741 / sine
//!   and cosine waves.
//! - **Rounding deferred** — the legacy rounded the liquidity score to an
//!   integer inside the engine. Per the spec's "Discarded" note (display
//!   rounding is standardized at the API boundary), [`LiquidityScore::total`] is
//!   the raw `0..100` blend; the wire layer rounds for display.

use serde::{Deserialize, Serialize};
use thiserror::Error;

// ---------------------------------------------------------------------------
// Percentile / tail-risk constants (E11)
// ---------------------------------------------------------------------------

/// Percentile-domain upper bound: `pct` is a percentage in `[0, 100]`.
const PCT_MAX: f64 = 100.0;
/// VaR/ES lower tail level, percent. Spec E11 `VAR_LEVEL_95`.
const VAR_LEVEL_95: f64 = 95.0;
/// VaR/ES deep tail level, percent. Spec E11 `VAR_LEVEL_99`.
const VAR_LEVEL_99: f64 = 99.0;
/// Default expected-shortfall denominator: the ES that maps to a full
/// tail-risk score of `1.0`, i.e. a 100% loss. Spec E11 `ES_MAX`.
pub const ES_MAX_DEFAULT: f64 = 1.0;
/// Worst-loss prior used only for an empty return history (a 100% loss). Spec
/// E11 `DEFAULT_WORST_LOSS`. This is a documented cold-start prior, never a
/// value presented as computed.
const DEFAULT_WORST_LOSS: f64 = 1.0;

// ---------------------------------------------------------------------------
// Liquidity constants (E12)
// ---------------------------------------------------------------------------

/// Spread percentage at which the spread sub-score reaches zero. Spec E12
/// `SPREAD_MAX_PCT`.
const SPREAD_MAX_PCT: f64 = 0.10;
/// Spread-percentage fallback when the mid is non-positive. Spec E12
/// `SPREAD_FALLBACK`.
const SPREAD_FALLBACK: f64 = 0.05;
/// `log10` divisor for volume/OI sub-scores: `1e4` contracts saturates to 1.0.
/// Spec E12 `LOG_LIQ_SCALE`.
const LOG_LIQ_SCALE: f64 = 4.0;
/// Count floor for the `log10` volume/OI sub-scores: treat a count below one as
/// one so the logarithm is defined. Spec E12 `max(·, 1)`.
const LIQ_COUNT_FLOOR: u64 = 1;
/// Default dimensionless relative-variance ceiling for quote stability (D10).
/// Carried over from the spec's `QUOTE_VAR_MAX = 0.05`, but reinterpreted: it is
/// now a squared coefficient of variation, not a `$²` variance. Callers should
/// tune it to their quote-noise regime because the unit changed.
pub const STABILITY_MAX_DEFAULT: f64 = 0.05;
/// Liquidity blend weight on the spread sub-score. Spec E12 `LIQ_W_SPREAD`.
const LIQ_W_SPREAD: f64 = 0.40;
/// Liquidity blend weight on the volume sub-score. Spec E12 `LIQ_W_VOLUME`.
const LIQ_W_VOLUME: f64 = 0.25;
/// Liquidity blend weight on the open-interest sub-score. Spec E12 `LIQ_W_OI`.
const LIQ_W_OI: f64 = 0.25;
/// Liquidity blend weight on the quote-stability sub-score. Spec E12
/// `LIQ_W_STABILITY`. The four liquidity weights sum to `1.0`.
const LIQ_W_STABILITY: f64 = 0.10;
/// Scale mapping the `[0, 1]` liquidity blend onto the `0..100` score.
const LIQ_SCORE_SCALE: f64 = 100.0;

// ---------------------------------------------------------------------------
// Model-trust constants (E13)
// ---------------------------------------------------------------------------

/// Forecast mean-absolute-error prior used when no forecast-error history is
/// supplied. Spec E13 `DEFAULT_FORECAST_MAE`. A documented cold-start prior.
const DEFAULT_FORECAST_MAE: f64 = 0.12;
/// Default MAE at which the forecast-error term saturates to full error. Spec
/// E13 `FORECAST_ERROR_SCALE`. Calibrated for probability-unit deltas (D3).
pub const FORECAST_ERROR_SCALE_DEFAULT: f64 = 0.15;
/// Default prediction-history variance ceiling for the stability term. Spec E13
/// `PRED_VAR_MAX`.
pub const PRED_VAR_MAX_DEFAULT: f64 = 0.04;
/// `log10(n_similar)` offset in the sample-strength term: `n = 10` scores `0`.
/// Spec E13 `SAMPLE_LOG_OFFSET`.
const SAMPLE_LOG_OFFSET: f64 = 1.0;
/// `log10(n_similar)` span in the sample-strength term: `n = 1000` scores `1`.
/// Spec E13 `SAMPLE_LOG_SPAN`.
const SAMPLE_LOG_SPAN: f64 = 2.0;
/// Sample-count floor for the `log10` sample-strength term: treat a count below
/// one as one so the logarithm is defined. Spec E13 `max(n_similar, 1)`.
const SAMPLE_COUNT_FLOOR: u64 = 1;
/// Trust weight on calibration `(1 − ece)`. Spec E13 `TRUST_W_CALIBRATION`.
const TRUST_W_CALIBRATION: f64 = 0.30;
/// Trust weight on forecast accuracy `(1 − forecast_error)`. Spec E13
/// `TRUST_W_FORECAST`.
const TRUST_W_FORECAST: f64 = 0.20;
/// Trust weight on prediction stability. Spec E13 `TRUST_W_STABILITY`.
const TRUST_W_STABILITY: f64 = 0.20;
/// Trust weight on sample strength. Spec E13 `TRUST_W_SAMPLE`.
const TRUST_W_SAMPLE: f64 = 0.15;
/// Trust weight on recent performance. Spec E13 `TRUST_W_RECENT`. The five
/// trust weights sum to `1.0`.
const TRUST_W_RECENT: f64 = 0.15;
/// Trust score at or above which the grade is `A`. Spec E13 `GRADE_A`.
const GRADE_A_MIN: f64 = 0.80;
/// Trust score at or above which the grade is `B`. Spec E13 `GRADE_B`.
const GRADE_B_MIN: f64 = 0.60;
/// Trust score at or above which the grade is `C`. Spec E13 `GRADE_C`.
const GRADE_C_MIN: f64 = 0.40;
/// Trust score at or above which the grade is `D`. Spec E13 `GRADE_D`.
const GRADE_D_MIN: f64 = 0.20;

/// Errors producible by the risk-scoring functions. Bad market data and empty
/// histories are values (cold-start behavior), not errors; only a non-finite or
/// non-positive *configuration scale* — which would make a sub-score undefined
/// — is an error.
#[derive(Debug, Error, PartialEq)]
pub enum RiskError {
    /// `es_max` was non-finite or non-positive.
    #[error("es_max must be finite and positive, got {0}")]
    InvalidEsMax(f64),
    /// `stability_max` was non-finite or non-positive.
    #[error("stability_max must be finite and positive, got {0}")]
    InvalidStabilityMax(f64),
    /// `forecast_scale` was non-finite or non-positive.
    #[error("forecast_scale must be finite and positive, got {0}")]
    InvalidForecastScale(f64),
    /// `pred_var_max` was non-finite or non-positive.
    #[error("prediction variance ceiling must be finite and positive, got {0}")]
    InvalidPredVarMax(f64),
    /// A scalar input that must be finite was not; the field is named.
    #[error("non-finite input: {0}")]
    NonFiniteInput(&'static str),
}

/// Historical tail-risk profile over similar-trade returns, all in
/// fractional-return units (a loss of `0.10` is a 10% loss).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TailRisk {
    /// 95th-percentile loss (Value-at-Risk), fraction.
    pub var95: f64,
    /// 99th-percentile loss (Value-at-Risk), fraction.
    pub var99: f64,
    /// Expected shortfall in the 95% tail: mean loss at or beyond `var95`
    /// (falls back to `var95` when no loss reaches it), fraction.
    pub es95: f64,
    /// Expected shortfall in the 99% tail: mean loss at or beyond `var99`
    /// (falls back to `var99` when no loss reaches it), fraction.
    pub es99: f64,
    /// Worst observed loss, or [`DEFAULT_WORST_LOSS`] for an empty history,
    /// fraction.
    pub worst: f64,
    /// Tail-risk score `clamp01(es95 / es_max) ∈ [0, 1]`.
    pub score: f64,
}

/// Liquidity quality of a single quote. The four sub-scores are each in
/// `[0, 1]`; `total` is their weighted blend on `0..100`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LiquidityScore {
    /// Blended liquidity score, `0..100` (unrounded; the wire layer rounds).
    pub total: f64,
    /// Spread sub-score `clamp01(1 − spread% / SPREAD_MAX_PCT) ∈ [0, 1]`.
    pub spread: f64,
    /// Volume sub-score `clamp01(log10(max(volume, 1)) / 4) ∈ [0, 1]`.
    pub volume: f64,
    /// Open-interest sub-score `clamp01(log10(max(oi, 1)) / 4) ∈ [0, 1]`.
    pub oi: f64,
    /// Quote-stability sub-score from the *relative*-mid variance (D10),
    /// `clamp01(1 − relvar / stability_max) ∈ [0, 1]`.
    pub stability: f64,
}

/// The five `[0, 1]` sub-scores that blend into the model-trust score, each
/// oriented so that higher is better.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TrustComponents {
    /// Calibration term `1 − ece`.
    pub calibration: f64,
    /// Forecast-accuracy term `1 − forecast_error` (D3: like-unit deltas).
    pub forecast_accuracy: f64,
    /// Prediction-stability term from prediction-history variance.
    pub prediction_stability: f64,
    /// Sample-strength term from `log10(n_similar)`.
    pub sample_strength: f64,
    /// Recent-performance term (recent win rate, clamped to `[0, 1]`).
    pub recent_performance: f64,
}

/// Descriptive letter grade for the model-trust score. Directional/descriptive
/// DATA, not an in-force readout (see module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrustGrade {
    /// Trust `≥ 0.80`.
    A,
    /// Trust `≥ 0.60` and `< 0.80`.
    B,
    /// Trust `≥ 0.40` and `< 0.60`.
    C,
    /// Trust `≥ 0.20` and `< 0.40`.
    D,
    /// Trust `< 0.20`.
    F,
}

/// Model-trust readout: a `[0, 1]` score, its descriptive letter grade, and the
/// component sub-scores it blends.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ModelTrust {
    /// Blended trust score `∈ [0, 1]`.
    pub score: f64,
    /// Descriptive letter grade of `score`.
    pub grade: TrustGrade,
    /// The component sub-scores behind `score`.
    pub components: TrustComponents,
}

/// Linear-interpolated percentile of `values` at `pct ∈ [0, 100]` (E11).
///
/// Sorts ascending (total order, so non-finite inputs sort deterministically to
/// the end — callers should pass finite data), then interpolates between the
/// floor and ceil order statistics at rank `(clamp(pct, 0, 100) / 100)·(n − 1)`.
/// An empty slice returns `0`.
#[must_use]
pub fn percentile(values: &[f64], pct: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted: Vec<f64> = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let max_idx = (sorted.len() - 1) as f64;
    let rank = ((pct.clamp(0.0, PCT_MAX) / PCT_MAX) * max_idx).clamp(0.0, max_idx);
    let lo = rank.floor() as usize;
    let hi = rank.ceil() as usize;
    if lo == hi {
        return sorted[lo];
    }
    let frac = rank - lo as f64;
    sorted[lo] + frac * (sorted[hi] - sorted[lo])
}

/// Historical VaR/ES tail risk over similar-trade `returns` (E11).
///
/// `returns` are fractional (a `−0.10` return is a 10% loss); losses are their
/// negation. VaR95/VaR99 are [`percentile`]s of the losses; ES95/ES99 are the
/// mean loss at or beyond the corresponding VaR, falling back to the VaR itself
/// when no loss reaches it (which only happens for an empty history). Non-finite
/// returns are dropped. See the module docs for the empty-history cold start.
///
/// `es_max` is the ES that maps to a full tail-risk score of `1.0`; pass
/// [`ES_MAX_DEFAULT`] for the spec default.
///
/// # Errors
///
/// [`RiskError::InvalidEsMax`] when `es_max` is non-finite or non-positive.
pub fn tail_risk(returns: &[f64], es_max: f64) -> Result<TailRisk, RiskError> {
    if !es_max.is_finite() || es_max <= 0.0 {
        return Err(RiskError::InvalidEsMax(es_max));
    }
    let losses: Vec<f64> = returns
        .iter()
        .copied()
        .filter(|r| r.is_finite())
        .map(|r| -r)
        .collect();

    let var95 = percentile(&losses, VAR_LEVEL_95);
    let var99 = percentile(&losses, VAR_LEVEL_99);
    let es95 = expected_shortfall(&losses, var95);
    let es99 = expected_shortfall(&losses, var99);
    let worst = if losses.is_empty() {
        DEFAULT_WORST_LOSS
    } else {
        losses.iter().copied().fold(f64::NEG_INFINITY, f64::max)
    };
    let score = clamp01(es95 / es_max);

    Ok(TailRisk {
        var95,
        var99,
        es95,
        es99,
        worst,
        score,
    })
}

/// Liquidity score of a single quote (E12).
///
/// Blends a spread sub-score, `log10` volume and open-interest sub-scores, and a
/// scale-independent quote-stability sub-score (D10: variance of *relative*
/// mids). `volume` and `open_interest` are contract counts; `prior_mids` are the
/// recent mid history in price units; `stability_max` is the dimensionless
/// relative-variance ceiling (pass [`STABILITY_MAX_DEFAULT`] for the spec
/// default). See the module docs for the fewer-than-two-mids cold start.
///
/// # Errors
///
/// [`RiskError::NonFiniteInput`] when `bid` or `ask` is non-finite, and
/// [`RiskError::InvalidStabilityMax`] when `stability_max` is non-finite or
/// non-positive.
pub fn liquidity_score(
    bid: f64,
    ask: f64,
    volume: u64,
    open_interest: u64,
    prior_mids: &[f64],
    stability_max: f64,
) -> Result<LiquidityScore, RiskError> {
    if !bid.is_finite() {
        return Err(RiskError::NonFiniteInput("bid"));
    }
    if !ask.is_finite() {
        return Err(RiskError::NonFiniteInput("ask"));
    }
    if !stability_max.is_finite() || stability_max <= 0.0 {
        return Err(RiskError::InvalidStabilityMax(stability_max));
    }

    let mid = (bid + ask) / 2.0;
    let spread_pct = if mid > 0.0 {
        (ask - bid) / mid
    } else {
        SPREAD_FALLBACK
    };
    let spread = clamp01(1.0 - spread_pct / SPREAD_MAX_PCT);
    let volume_sub = clamp01((volume.max(LIQ_COUNT_FLOOR) as f64).log10() / LOG_LIQ_SCALE);
    let oi = clamp01((open_interest.max(LIQ_COUNT_FLOOR) as f64).log10() / LOG_LIQ_SCALE);
    let stability = quote_stability(prior_mids, stability_max);

    let total = LIQ_SCORE_SCALE
        * (LIQ_W_SPREAD * spread
            + LIQ_W_VOLUME * volume_sub
            + LIQ_W_OI * oi
            + LIQ_W_STABILITY * stability);

    Ok(LiquidityScore {
        total,
        spread,
        volume: volume_sub,
        oi,
        stability,
    })
}

/// Model-trust score, letter grade, and components (E13).
///
/// Blends calibration `(1 − ece)`, forecast accuracy `(1 − forecast_error)`,
/// prediction stability, sample strength, and recent performance.
///
/// - `ece` is the expected calibration error `∈ [0, 1]` (clamped defensively).
/// - `pred_actual_deltas` are per-observation *like-unit* forecast errors
///   (D3): `predicted − actual` in a single consistent unit, recommended as
///   `predicted_win_prob − realized_win_indicator`. Non-finite entries are
///   dropped; an empty set uses the [`DEFAULT_FORECAST_MAE`] prior.
/// - `predictions_history` are recent predicted probabilities; their sample
///   variance drives stability (fewer than two → variance `0` → stability `1`).
/// - `n_similar` is the similar-sample count feeding sample strength.
/// - `recent_win_rate` is the recent realized win rate `∈ [0, 1]` (clamped).
/// - `forecast_scale` / `pred_var_max` are the saturation ceilings; pass
///   [`FORECAST_ERROR_SCALE_DEFAULT`] / [`PRED_VAR_MAX_DEFAULT`] for spec
///   defaults.
///
/// # Errors
///
/// [`RiskError::NonFiniteInput`] when `ece` or `recent_win_rate` is non-finite;
/// [`RiskError::InvalidForecastScale`] / [`RiskError::InvalidPredVarMax`] when
/// the corresponding ceiling is non-finite or non-positive.
pub fn model_trust(
    ece: f64,
    pred_actual_deltas: &[f64],
    predictions_history: &[f64],
    n_similar: u64,
    recent_win_rate: f64,
    forecast_scale: f64,
    pred_var_max: f64,
) -> Result<ModelTrust, RiskError> {
    if !ece.is_finite() {
        return Err(RiskError::NonFiniteInput("ece"));
    }
    if !recent_win_rate.is_finite() {
        return Err(RiskError::NonFiniteInput("recent_win_rate"));
    }
    if !forecast_scale.is_finite() || forecast_scale <= 0.0 {
        return Err(RiskError::InvalidForecastScale(forecast_scale));
    }
    if !pred_var_max.is_finite() || pred_var_max <= 0.0 {
        return Err(RiskError::InvalidPredVarMax(pred_var_max));
    }

    let mean_abs_error = finite_mean_abs(pred_actual_deltas).unwrap_or(DEFAULT_FORECAST_MAE);
    let forecast_error = clamp01(mean_abs_error / forecast_scale);

    let pred_var = sample_variance(predictions_history);
    let prediction_stability = clamp01(1.0 - pred_var / pred_var_max);

    let n = n_similar.max(SAMPLE_COUNT_FLOOR) as f64;
    let sample_strength = clamp01((n.log10() - SAMPLE_LOG_OFFSET) / SAMPLE_LOG_SPAN);

    let calibration = 1.0 - clamp01(ece);
    let forecast_accuracy = 1.0 - forecast_error;
    let recent_performance = clamp01(recent_win_rate);

    let score = clamp01(
        TRUST_W_CALIBRATION * calibration
            + TRUST_W_FORECAST * forecast_accuracy
            + TRUST_W_STABILITY * prediction_stability
            + TRUST_W_SAMPLE * sample_strength
            + TRUST_W_RECENT * recent_performance,
    );

    Ok(ModelTrust {
        score,
        grade: grade_for(score),
        components: TrustComponents {
            calibration,
            forecast_accuracy,
            prediction_stability,
            sample_strength,
            recent_performance,
        },
    })
}

/// Mean loss at or beyond `var`, or `var` itself when no loss reaches it.
fn expected_shortfall(losses: &[f64], var: f64) -> f64 {
    let (sum, count) = losses
        .iter()
        .copied()
        .filter(|&l| l >= var)
        .fold((0.0, 0u64), |(s, c), l| (s + l, c + 1));
    if count == 0 { var } else { sum / count as f64 }
}

/// Scale-independent quote stability (D10): `clamp01(1 − relvar / stability_max)`
/// where `relvar` is the sample variance of each mid divided by the mean mid.
/// Fewer than two finite mids → `1.0` (nothing contradicts stability); a
/// non-positive mean mid → `0.0` (stability cannot be assessed).
fn quote_stability(mids: &[f64], stability_max: f64) -> f64 {
    let finite: Vec<f64> = mids.iter().copied().filter(|m| m.is_finite()).collect();
    if finite.len() < 2 {
        return 1.0;
    }
    let mean = finite.iter().sum::<f64>() / finite.len() as f64;
    if mean <= 0.0 {
        return 0.0;
    }
    let relative: Vec<f64> = finite.iter().map(|m| m / mean).collect();
    clamp01(1.0 - sample_variance(&relative) / stability_max)
}

/// Sample variance (`n − 1` denominator) of the finite values, or `0` when
/// fewer than two are present.
fn sample_variance(xs: &[f64]) -> f64 {
    let finite: Vec<f64> = xs.iter().copied().filter(|x| x.is_finite()).collect();
    let n = finite.len();
    if n < 2 {
        return 0.0;
    }
    let mean = finite.iter().sum::<f64>() / n as f64;
    let ss: f64 = finite.iter().map(|x| (x - mean) * (x - mean)).sum();
    ss / (n - 1) as f64
}

/// Mean of the magnitudes of the finite values, or `None` when none are finite.
fn finite_mean_abs(xs: &[f64]) -> Option<f64> {
    let (sum, count) = xs
        .iter()
        .copied()
        .filter(|x| x.is_finite())
        .fold((0.0, 0u64), |(s, c), x| (s + x.abs(), c + 1));
    if count == 0 {
        None
    } else {
        Some(sum / count as f64)
    }
}

/// Map a trust score to its descriptive letter grade.
fn grade_for(trust: f64) -> TrustGrade {
    if trust >= GRADE_A_MIN {
        TrustGrade::A
    } else if trust >= GRADE_B_MIN {
        TrustGrade::B
    } else if trust >= GRADE_C_MIN {
        TrustGrade::C
    } else if trust >= GRADE_D_MIN {
        TrustGrade::D
    } else {
        TrustGrade::F
    }
}

/// Clamp to `[0, 1]`. All call sites pass finite values (config scales are
/// validated, arrays are filtered to finite entries), so `NaN` never reaches it.
fn clamp01(v: f64) -> f64 {
    v.clamp(0.0, 1.0)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Absolute tolerance for exact arithmetic assertions.
    const EPS: f64 = 1e-9;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() <= EPS
    }

    // --- percentile (E11) ---------------------------------------------------

    #[test]
    fn percentile_of_empty_is_zero() {
        assert!(close(percentile(&[], 95.0), 0.0));
    }

    #[test]
    fn percentile_interpolates_linearly_between_order_statistics() {
        // losses 0.01..0.10, n = 10. rank95 = 0.95·9 = 8.55 -> 0.09 + 0.55·0.01.
        let xs: Vec<f64> = (1..=10).map(|i| f64::from(i) / 100.0).collect();
        assert!(close(percentile(&xs, 95.0), 0.0955));
        // rank99 = 0.99·9 = 8.91 -> 0.09 + 0.91·0.01.
        assert!(close(percentile(&xs, 99.0), 0.0991));
        // Endpoints are exact order statistics.
        assert!(close(percentile(&xs, 0.0), 0.01));
        assert!(close(percentile(&xs, 100.0), 0.10));
        // Median of an even set interpolates the two central points.
        assert!(close(percentile(&xs, 50.0), 0.055));
    }

    #[test]
    fn percentile_sorts_unordered_input() {
        let xs = [0.10, 0.01, 0.05, 0.02];
        assert!(close(percentile(&xs, 0.0), 0.01));
        assert!(close(percentile(&xs, 100.0), 0.10));
    }

    #[test]
    fn percentile_clamps_out_of_range_pct() {
        let xs = [0.01, 0.02, 0.03];
        assert!(close(percentile(&xs, -50.0), 0.01));
        assert!(close(percentile(&xs, 250.0), 0.03));
    }

    // --- tail risk (E11) ----------------------------------------------------

    #[test]
    fn tail_risk_matches_known_distribution() {
        // losses 0.01..1.00 (n = 100); returns are their negation.
        let returns: Vec<f64> = (1..=100).map(|i| -f64::from(i) / 100.0).collect();
        let tr = tail_risk(&returns, ES_MAX_DEFAULT).unwrap();
        // var95: rank 94.05 -> 0.95 + 0.05·0.01 = 0.9505.
        assert!(close(tr.var95, 0.9505));
        // es95: mean of losses >= var95 = mean(0.96,0.97,0.98,0.99,1.00) = 0.98.
        assert!(close(tr.es95, 0.98));
        // var99: rank 98.01 -> 0.99 + 0.01·0.01 = 0.9901.
        assert!(close(tr.var99, 0.9901));
        // es99: only 1.00 clears var99.
        assert!(close(tr.es99, 1.0));
        assert!(close(tr.worst, 1.0));
        assert!(close(tr.score, 0.98));
    }

    #[test]
    fn expected_shortfall_never_below_var() {
        let returns: Vec<f64> = (1..=50).map(|i| -f64::from(i) / 100.0).collect();
        let tr = tail_risk(&returns, ES_MAX_DEFAULT).unwrap();
        assert!(tr.es95 >= tr.var95 - EPS);
        assert!(tr.es99 >= tr.var99 - EPS);
        assert!(tr.worst >= tr.es99 - EPS);
    }

    #[test]
    fn tail_risk_empty_history_is_documented_cold_start() {
        let tr = tail_risk(&[], ES_MAX_DEFAULT).unwrap();
        assert!(close(tr.var95, 0.0));
        assert!(close(tr.var99, 0.0));
        assert!(close(tr.es95, 0.0));
        assert!(close(tr.es99, 0.0));
        assert!(close(tr.worst, DEFAULT_WORST_LOSS));
        assert!(close(tr.score, 0.0));
    }

    #[test]
    fn tail_risk_drops_non_finite_returns() {
        let clean = tail_risk(&[-0.02, -0.05, -0.09], ES_MAX_DEFAULT).unwrap();
        let dirty = tail_risk(
            &[-0.02, f64::NAN, -0.05, f64::INFINITY, -0.09],
            ES_MAX_DEFAULT,
        )
        .unwrap();
        assert!(close(clean.var95, dirty.var95));
        assert!(close(clean.es95, dirty.es95));
        assert!(close(clean.worst, dirty.worst));
    }

    #[test]
    fn tail_risk_score_scales_with_es_max() {
        // Constant 10% loss -> es95 = 0.10; a 20% es_max halves the score.
        let returns = [-0.10; 8];
        let tr = tail_risk(&returns, 0.20).unwrap();
        assert!(close(tr.es95, 0.10));
        assert!(close(tr.score, 0.5));
    }

    #[test]
    fn tail_risk_rejects_bad_es_max() {
        assert_eq!(
            tail_risk(&[-0.1], 0.0).unwrap_err(),
            RiskError::InvalidEsMax(0.0)
        );
        assert_eq!(
            tail_risk(&[-0.1], -1.0).unwrap_err(),
            RiskError::InvalidEsMax(-1.0)
        );
        assert!(matches!(
            tail_risk(&[-0.1], f64::NAN).unwrap_err(),
            RiskError::InvalidEsMax(_)
        ));
    }

    // --- liquidity (E12) ----------------------------------------------------

    #[test]
    fn liquidity_total_is_monotone_decreasing_in_spread() {
        let tight = liquidity_score(0.99, 1.01, 5_000, 5_000, &[], STABILITY_MAX_DEFAULT).unwrap();
        let wide = liquidity_score(0.90, 1.10, 5_000, 5_000, &[], STABILITY_MAX_DEFAULT).unwrap();
        // Same mid (1.00), same volume/oi/stability -> only spread differs.
        assert!(close(tight.spread, 0.8)); // 1 - 0.02/0.10
        assert!(close(wide.spread, 0.0)); // 1 - 0.20/0.10 clamps to 0
        assert!(tight.total > wide.total);
    }

    #[test]
    fn liquidity_sub_scores_match_spec_formulas() {
        // volume 10_000 -> log10 = 4 -> /4 = 1.0; oi 100 -> log10 = 2 -> 0.5.
        let ls = liquidity_score(1.00, 1.02, 10_000, 100, &[], STABILITY_MAX_DEFAULT).unwrap();
        assert!(close(ls.volume, 1.0));
        assert!(close(ls.oi, 0.5));
        // spread% = 0.02 / 1.01; spreadScore = 1 - that / 0.10.
        let mid = 1.01_f64;
        let expected_spread = (1.0 - (0.02 / mid) / SPREAD_MAX_PCT).clamp(0.0, 1.0);
        assert!(close(ls.spread, expected_spread));
    }

    #[test]
    fn liquidity_non_positive_mid_uses_spread_fallback() {
        // bid + ask = 0 -> mid = 0 -> spread% = SPREAD_FALLBACK = 0.05.
        let ls = liquidity_score(0.0, 0.0, 1, 1, &[], STABILITY_MAX_DEFAULT).unwrap();
        assert!(close(ls.spread, 1.0 - SPREAD_FALLBACK / SPREAD_MAX_PCT));
    }

    #[test]
    fn quote_stability_is_scale_independent() {
        // A $500 option and a $0.30 option with the SAME relative tick noise
        // must score identical stability (D10 fix).
        let big = [500.0, 502.0, 498.0, 501.0, 499.0];
        let k = 0.30 / 500.0;
        let small: Vec<f64> = big.iter().map(|m| m * k).collect();
        let a = liquidity_score(499.0, 501.0, 1_000, 1_000, &big, STABILITY_MAX_DEFAULT).unwrap();
        let b = liquidity_score(
            499.0 * k,
            501.0 * k,
            1_000,
            1_000,
            &small,
            STABILITY_MAX_DEFAULT,
        )
        .unwrap();
        assert!(close(a.stability, b.stability));
        // Low relative noise -> stability near 1.
        assert!(a.stability > 0.99);
    }

    #[test]
    fn quote_stability_cold_start_and_degenerate() {
        // Fewer than two mids -> full stability.
        let one = liquidity_score(1.0, 1.02, 10, 10, &[3.0], STABILITY_MAX_DEFAULT).unwrap();
        assert!(close(one.stability, 1.0));
        let none = liquidity_score(1.0, 1.02, 10, 10, &[], STABILITY_MAX_DEFAULT).unwrap();
        assert!(close(none.stability, 1.0));
        // Non-positive mean mid -> stability cannot be assessed -> 0.
        let degen = liquidity_score(1.0, 1.02, 10, 10, &[0.0, 0.0], STABILITY_MAX_DEFAULT).unwrap();
        assert!(close(degen.stability, 0.0));
    }

    #[test]
    fn liquidity_total_is_within_zero_hundred() {
        let ls = liquidity_score(0.99, 1.01, 12_000, 20_000, &[1.0, 1.001, 0.999], 0.05).unwrap();
        assert!(ls.total >= 0.0 && ls.total <= 100.0);
        for s in [ls.spread, ls.volume, ls.oi, ls.stability] {
            assert!((0.0..=1.0).contains(&s));
        }
    }

    #[test]
    fn liquidity_rejects_bad_inputs() {
        assert_eq!(
            liquidity_score(f64::NAN, 1.0, 1, 1, &[], 0.05).unwrap_err(),
            RiskError::NonFiniteInput("bid")
        );
        assert_eq!(
            liquidity_score(1.0, f64::INFINITY, 1, 1, &[], 0.05).unwrap_err(),
            RiskError::NonFiniteInput("ask")
        );
        assert_eq!(
            liquidity_score(1.0, 1.0, 1, 1, &[], 0.0).unwrap_err(),
            RiskError::InvalidStabilityMax(0.0)
        );
    }

    // --- model trust (E13) --------------------------------------------------

    #[test]
    fn model_trust_perfect_inputs_grade_a() {
        let mt = model_trust(
            0.0,
            &[0.0, 0.0],
            &[0.5, 0.5],
            1_000,
            1.0,
            FORECAST_ERROR_SCALE_DEFAULT,
            PRED_VAR_MAX_DEFAULT,
        )
        .unwrap();
        assert!(close(mt.score, 1.0));
        assert_eq!(mt.grade, TrustGrade::A);
        assert!(close(mt.components.calibration, 1.0));
        assert!(close(mt.components.forecast_accuracy, 1.0));
        assert!(close(mt.components.prediction_stability, 1.0));
        assert!(close(mt.components.sample_strength, 1.0));
        assert!(close(mt.components.recent_performance, 1.0));
    }

    #[test]
    fn model_trust_worst_inputs_grade_f() {
        let mt = model_trust(
            1.0,
            &[1.0, 1.0],
            &[0.0, 1.0],
            1,
            0.0,
            FORECAST_ERROR_SCALE_DEFAULT,
            PRED_VAR_MAX_DEFAULT,
        )
        .unwrap();
        assert!(close(mt.score, 0.0));
        assert_eq!(mt.grade, TrustGrade::F);
    }

    #[test]
    fn model_trust_is_always_in_unit_interval() {
        // A spread of plausible inputs stays within [0, 1].
        for &ece in &[0.0, 0.1, 0.3, 0.7, 1.0] {
            for &wr in &[0.0, 0.5, 0.9, 1.0] {
                for &n in &[1_u64, 50, 250, 5_000] {
                    let mt = model_trust(
                        ece,
                        &[0.05, -0.03, 0.04],
                        &[0.55, 0.6, 0.58, 0.62],
                        n,
                        wr,
                        FORECAST_ERROR_SCALE_DEFAULT,
                        PRED_VAR_MAX_DEFAULT,
                    )
                    .unwrap();
                    assert!((0.0..=1.0).contains(&mt.score));
                }
            }
        }
    }

    #[test]
    fn model_trust_blend_matches_components() {
        let mt = model_trust(
            0.1,
            &[0.03, -0.06],
            &[0.5, 0.6, 0.55],
            100,
            0.7,
            FORECAST_ERROR_SCALE_DEFAULT,
            PRED_VAR_MAX_DEFAULT,
        )
        .unwrap();
        let c = mt.components;
        let expected = TRUST_W_CALIBRATION * c.calibration
            + TRUST_W_FORECAST * c.forecast_accuracy
            + TRUST_W_STABILITY * c.prediction_stability
            + TRUST_W_SAMPLE * c.sample_strength
            + TRUST_W_RECENT * c.recent_performance;
        assert!(close(mt.score, expected));
        // n = 100 -> log10 = 2 -> (2 - 1)/2 = 0.5 sample strength.
        assert!(close(c.sample_strength, 0.5));
        // ece 0.1 -> calibration 0.9.
        assert!(close(c.calibration, 0.9));
    }

    #[test]
    fn model_trust_empty_deltas_uses_documented_prior() {
        // Empty deltas -> DEFAULT_FORECAST_MAE prior, not a live value.
        let mt = model_trust(
            0.0,
            &[],
            &[0.5, 0.5],
            1_000,
            1.0,
            FORECAST_ERROR_SCALE_DEFAULT,
            PRED_VAR_MAX_DEFAULT,
        )
        .unwrap();
        let expected_fa = 1.0 - (DEFAULT_FORECAST_MAE / FORECAST_ERROR_SCALE_DEFAULT);
        assert!(close(mt.components.forecast_accuracy, expected_fa));
    }

    #[test]
    fn model_trust_forecast_error_is_like_unit_mae() {
        // D3: MAE of the supplied like-unit deltas, saturating at forecast_scale.
        // deltas mean-abs = 0.075; /0.15 = 0.5 -> forecast_accuracy 0.5.
        let mt = model_trust(
            0.0,
            &[0.05, -0.10],
            &[0.5, 0.5],
            1_000,
            1.0,
            FORECAST_ERROR_SCALE_DEFAULT,
            PRED_VAR_MAX_DEFAULT,
        )
        .unwrap();
        assert!(close(mt.components.forecast_accuracy, 0.5));
    }

    #[test]
    fn model_trust_rejects_bad_config() {
        assert_eq!(
            model_trust(f64::NAN, &[], &[], 1, 0.5, 0.15, 0.04).unwrap_err(),
            RiskError::NonFiniteInput("ece")
        );
        assert_eq!(
            model_trust(0.1, &[], &[], 1, f64::NAN, 0.15, 0.04).unwrap_err(),
            RiskError::NonFiniteInput("recent_win_rate")
        );
        assert_eq!(
            model_trust(0.1, &[], &[], 1, 0.5, 0.0, 0.04).unwrap_err(),
            RiskError::InvalidForecastScale(0.0)
        );
        assert_eq!(
            model_trust(0.1, &[], &[], 1, 0.5, 0.15, -1.0).unwrap_err(),
            RiskError::InvalidPredVarMax(-1.0)
        );
    }

    #[test]
    fn grade_boundaries_are_inclusive_lower() {
        assert_eq!(grade_for(0.80), TrustGrade::A);
        assert_eq!(grade_for(0.799_999), TrustGrade::B);
        assert_eq!(grade_for(0.60), TrustGrade::B);
        assert_eq!(grade_for(0.40), TrustGrade::C);
        assert_eq!(grade_for(0.20), TrustGrade::D);
        assert_eq!(grade_for(0.199_999), TrustGrade::F);
        assert_eq!(grade_for(0.0), TrustGrade::F);
    }

    // --- serde --------------------------------------------------------------

    #[test]
    fn output_structs_round_trip_through_json() {
        let tr = tail_risk(&[-0.03, -0.07, -0.11, 0.02], ES_MAX_DEFAULT).unwrap();
        let tr_back: TailRisk = serde_json::from_str(&serde_json::to_string(&tr).unwrap()).unwrap();
        assert!(close(tr.var95, tr_back.var95));
        assert!(close(tr.score, tr_back.score));

        let ls = liquidity_score(0.99, 1.01, 8_000, 12_000, &[1.0, 1.01], 0.05).unwrap();
        let ls_back: LiquidityScore =
            serde_json::from_str(&serde_json::to_string(&ls).unwrap()).unwrap();
        assert!(close(ls.total, ls_back.total));
        assert!(close(ls.stability, ls_back.stability));

        let mt = model_trust(0.1, &[0.02], &[0.5, 0.6], 300, 0.68, 0.15, 0.04).unwrap();
        let mt_back: ModelTrust =
            serde_json::from_str(&serde_json::to_string(&mt).unwrap()).unwrap();
        assert!(close(mt.score, mt_back.score));
        assert_eq!(mt.grade, mt_back.grade);
    }

    #[test]
    fn trust_grade_serializes_as_letter() {
        assert_eq!(serde_json::to_string(&TrustGrade::A).unwrap(), "\"A\"");
        assert_eq!(serde_json::to_string(&TrustGrade::F).unwrap(), "\"F\"");
    }
}
