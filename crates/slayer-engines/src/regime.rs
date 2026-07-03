//! Market-regime engine: Hurst persistence, Ornstein–Uhlenbeck half-life, a
//! softmax three-state regime classifier, and volatility
//! compression/expansion/term-structure reads.
//!
//! Provenance: `docs/spec/04-vol-distribution.md` E13–E17
//! (`src/lib/regimeEngine.ts`, plus the ATR/bandwidth vol-regime read from
//! `src/lib/displacementEngine.ts`). Close-to-close volatility for the
//! term-structure slope is taken from [`slayer_quant::realized_vol`] rather
//! than reimplemented.
//!
//! Deviations from legacy:
//! - D9 (E14): applies the Anis–Lloyd (1976) / Peters (1994) small-sample
//!   correction to the rescaled-range Hurst estimate. The observed `R/S` at
//!   each window is compared against its finite-sample expectation under
//!   independence, so short series no longer bias `H` upward.
//! - D7 (E16): the volatility-ratio baseline is measured over the returns
//!   *preceding* the recent window, removing the numerator/denominator
//!   overlap that collapsed the ratio toward 1 for short histories.
//! - D8 (E16): the dominant-state confidence is a [`Readout`] ("regime
//!   confident") resolved on the maximum posterior, not a mislabeled
//!   "transition probability".
//! - D15 (E15): an overshooting AR(1) fit (`φ = 1 + b ≤ 0`) yields [`None`]
//!   rather than the legacy discontinuous `−b` half-life fallback.
//! - Binary-state doctrine (`docs/ARCHITECTURE.md` §2): the legacy
//!   COMPRESSION/NEUTRAL/EXPANSION vol-regime label collapses to two
//!   independent [`Readout`]s (compression, expansion) driven by the
//!   Bollinger-bandwidth percentile. The three-state classifier label stays
//!   descriptive data ([`RegimeLabel`]); only the "confident" bit is a
//!   [`Readout`].

use serde::{Deserialize, Serialize};
use slayer_core::{Candle, HysteresisBand, Readout};
use slayer_quant::realized_vol::close_to_close;
use std::f64::consts::{FRAC_PI_2, LN_2};

// ── E14 Hurst (R/S) constants ────────────────────────────────────────────

/// Minimum log returns before a Hurst exponent is estimated; below this the
/// estimate is undefined and [`HURST_FALLBACK`] is returned.
const HURST_MIN_RETURNS: usize = 32;
/// Smallest R/S window (returns per chunk).
const HURST_BASE_WINDOW: usize = 8;
/// Geometric growth factor of the R/S window ladder.
const HURST_WINDOW_GROWTH: f64 = 1.6;
/// Chunk standard-deviation floor below which a chunk's R/S is discarded.
const HURST_STD_FLOOR: f64 = 1e-12;
/// Minimum `(ln n, ln R/S)` points required to fit the R/S slope.
const HURST_MIN_WINDOW_POINTS: usize = 2;
/// Hurst exponent of an uncorrelated random walk; the classifier and
/// persistence readout are centred here.
const HURST_RANDOM_WALK: f64 = 0.5;
/// Fallback Hurst exponent when the series is too short to estimate.
const HURST_FALLBACK: f64 = 0.5;
/// Hurst exponent at or above which the persistence readout activates.
const HURST_PERSIST_ACTIVATE: f64 = 0.55;
/// Hurst exponent at or below which the persistence readout deactivates.
const HURST_PERSIST_DEACTIVATE: f64 = 0.45;

/// Trend-persistence band on the Hurst exponent (score = `H`).
const HURST_PERSISTENCE_BAND: HysteresisBand =
    HysteresisBand::new(HURST_PERSIST_ACTIVATE, HURST_PERSIST_DEACTIVATE);

// ── E15 Ornstein–Uhlenbeck constants ─────────────────────────────────────

/// Minimum positive-finite prices before an OU fit is attempted.
const OU_MIN_POINTS: usize = 20;
/// Trailing simple-moving-average window used to stationarize the series.
const OU_MEAN_WINDOW: usize = 20;
/// AR(1) slope threshold: mean reversion requires `b < −OU_B_THRESHOLD`.
const OU_B_THRESHOLD: f64 = 1e-4;

// ── E16 classifier constants ─────────────────────────────────────────────

/// Minimum log returns before the softmax classifier runs; below this the
/// flat-prior classification is returned.
const REGIME_MIN_RETURNS: usize = 10;
/// Recent-volatility window (returns) feeding vol, kurtosis and vol-ratio.
const REGIME_RECENT_WINDOW: usize = 30;
/// Energy slope applied to `H − 0.5` (trend) and `0.5 − H` (revert).
const REGIME_HURST_GAIN: f64 = 6.0;
/// Trend energy gain on `volRatio − 1`.
const REGIME_TREND_VOL_GAIN: f64 = 1.2;
/// Revert energy gain on `1 − volRatio`.
const REGIME_REVERT_VOL_GAIN: f64 = 1.5;
/// Constant prior energy added to the mean-reversion state.
const REGIME_REVERT_PRIOR: f64 = 0.4;
/// Kurtosis above which tail-risk energy accrues (Gaussian ≈ 3).
const REGIME_KURT_THRESHOLD: f64 = 4.0;
/// Tail energy gain on `kurt − REGIME_KURT_THRESHOLD`.
const REGIME_TAIL_KURT_GAIN: f64 = 0.5;
/// Volatility-ratio threshold above which tail-risk energy accrues.
const REGIME_TAIL_VOL_THRESHOLD: f64 = 1.6;
/// Tail energy gain on `volRatio − REGIME_TAIL_VOL_THRESHOLD`.
const REGIME_TAIL_VOL_GAIN: f64 = 2.0;
/// Neutral volatility ratio (recent vol equals its baseline).
const VOL_RATIO_NEUTRAL: f64 = 1.0;
/// Kurtosis assumed when recent volatility is zero (Gaussian reference).
const GAUSSIAN_KURTOSIS: f64 = 3.0;
/// Flat-prior posterior for the trend-expansion state (insufficient data).
const REGIME_FLAT_TREND: f64 = 0.33;
/// Flat-prior posterior for the mean-reversion state (insufficient data).
const REGIME_FLAT_REVERT: f64 = 0.34;
/// Flat-prior posterior for the tail-risk state (insufficient data).
const REGIME_FLAT_TAIL: f64 = 0.33;
/// Maximum posterior at or above which the classification is "confident".
const REGIME_CONFIDENT_ACTIVATE: f64 = 0.55;
/// Maximum posterior at or below which the classification is "unconfident".
const REGIME_CONFIDENT_DEACTIVATE: f64 = 0.45;

/// Confidence band on the maximum posterior (score = `max posterior`).
const REGIME_CONFIDENT_BAND: HysteresisBand =
    HysteresisBand::new(REGIME_CONFIDENT_ACTIVATE, REGIME_CONFIDENT_DEACTIVATE);

// ── E13/E17 bandwidth & term-structure constants ─────────────────────────

/// Bollinger-band period (bars); capped by the available candle count.
const BB_PERIOD: usize = 20;
/// Bandwidth multiplier: a `±2σ` band spans `BB_WIDTH_MULT · σ` about the mean.
const BB_WIDTH_MULT: f64 = 4.0;
/// Percentile lookback over the bandwidth series (bars).
const VOL_ENGINE_LOOKBACK: usize = 100;
/// Minimum candles before a bandwidth percentile is computed.
const VOL_ENGINE_MIN_CANDLES: usize = 5;
/// Neutral percentile returned when there is insufficient data.
const NEUTRAL_PERCENTILE: f64 = 0.5;
/// Bandwidth percentile at or above which expansion activates.
const BANDWIDTH_EXPANSION_PCTILE: f64 = 0.70;
/// Bandwidth percentile at or below which compression activates.
const BANDWIDTH_COMPRESSION_PCTILE: f64 = 0.30;
/// Hysteresis margin (in percentile units) on the compression/expansion bands.
const BANDWIDTH_PCTILE_HYSTERESIS: f64 = 0.10;

/// Expansion band on the bandwidth percentile (score = percentile).
const EXPANSION_BAND: HysteresisBand = HysteresisBand::new(
    BANDWIDTH_EXPANSION_PCTILE,
    BANDWIDTH_EXPANSION_PCTILE - BANDWIDTH_PCTILE_HYSTERESIS,
);
/// Compression band on the *inverted* bandwidth percentile
/// (score = `1 − percentile`, so a low percentile activates compression).
const COMPRESSION_BAND: HysteresisBand = HysteresisBand::new(
    1.0 - BANDWIDTH_COMPRESSION_PCTILE,
    1.0 - BANDWIDTH_COMPRESSION_PCTILE - BANDWIDTH_PCTILE_HYSTERESIS,
);

/// Near-term close-to-close realized-vol window (bars).
const RV_NEAR_WINDOW: usize = 10;
/// Far-term close-to-close realized-vol window (bars).
const RV_FAR_WINDOW: usize = 40;
/// Periods-per-year passed to the realized-vol estimator; the annualization
/// factor cancels in the near/far *ratio*, so any positive constant works.
const RV_UNIT_PERIODS: f64 = 1.0;

// ── Output types ─────────────────────────────────────────────────────────

/// The three descriptive regime labels. This is *data*, not a binary state:
/// the argmax of the softmax posteriors. Only the accompanying
/// [`RegimeClassification::confident`] readout carries an `ACTIVE`/`INACTIVE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RegimeLabel {
    /// Persistent, expanding market (high Hurst and/or elevated volatility).
    TrendExpansion,
    /// Mean-reverting market (low Hurst, subdued volatility).
    MeanReversion,
    /// Fat-tailed, high-kurtosis / volatility-shock market.
    TailRisk,
}

/// Softmax posteriors over the three regimes. The three fields sum to 1
/// (up to floating-point rounding).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RegimeProbabilities {
    /// Posterior probability of [`RegimeLabel::TrendExpansion`].
    pub trend_expansion: f64,
    /// Posterior probability of [`RegimeLabel::MeanReversion`].
    pub mean_reversion: f64,
    /// Posterior probability of [`RegimeLabel::TailRisk`].
    pub tail_risk: f64,
}

/// Output of the three-state softmax regime classifier (E16).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RegimeClassification {
    /// Posteriors over the three states (sum to 1).
    pub probabilities: RegimeProbabilities,
    /// Argmax descriptive label (ties resolve to trend, then revert, then tail).
    pub label: RegimeLabel,
    /// "Regime confident" readout resolved on the maximum posterior via
    /// [`REGIME_CONFIDENT_BAND`]; score is that maximum posterior in `[0, 1]`.
    pub confident: Readout,
}

/// Aggregate market-regime state over a candle history.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RegimeState {
    /// Anis–Lloyd/Peters-corrected Hurst exponent in `[0, 1]`
    /// (`> 0.5` persistent/trending, `< 0.5` mean-reverting).
    pub hurst: f64,
    /// Trend-persistence readout (score = [`RegimeState::hurst`]).
    pub persistence: Readout,
    /// Ornstein–Uhlenbeck half-life in *bars*, or [`None`] when the series is
    /// not mean-reverting (`φ = 1 + b ∉ (0, 1)`) or too short.
    pub half_life_bars: Option<f64>,
    /// Three-state softmax classification.
    pub classification: RegimeClassification,
    /// Volatility-compression readout: `ACTIVE` when the Bollinger-bandwidth
    /// percentile is low; score is `1 − percentile` in `[0, 1]`.
    pub compression: Readout,
    /// Volatility-expansion readout: `ACTIVE` when the Bollinger-bandwidth
    /// percentile is high; score is the percentile in `[0, 1]`.
    pub expansion: Readout,
    /// Realized-vol term-structure slope, `rvNear / rvFar − 1` (dimensionless,
    /// signed): `> 0` inverted (near vol elevated), `< 0` upward-sloping.
    pub term_structure_slope: f64,
}

// ── Public engine functions ──────────────────────────────────────────────

/// Full regime analysis over a candle history. Pure and deterministic: the
/// same slice always yields the same [`RegimeState`].
#[must_use]
pub fn analyze_regime(candles: &[Candle]) -> RegimeState {
    let closes: Vec<f64> = candles.iter().map(|c| c.close).collect();
    let hurst = hurst_exponent(&closes);
    let persistence = Readout::resolve(&HURST_PERSISTENCE_BAND, hurst);
    let half_life_bars = ou_half_life(&closes);
    let classification = classify_regime(candles);
    let pct = bandwidth_percentile(&closes);
    let compression = Readout::resolve(&COMPRESSION_BAND, 1.0 - pct);
    let expansion = Readout::resolve(&EXPANSION_BAND, pct);
    let term_structure_slope = rv_term_structure_slope(candles);
    RegimeState {
        hurst,
        persistence,
        half_life_bars,
        classification,
        compression,
        expansion,
        term_structure_slope,
    }
}

/// Anis–Lloyd/Peters-corrected Hurst exponent of a price `series` via
/// rescaled-range analysis of its log returns. Returns [`HURST_FALLBACK`]
/// (0.5) when the series is too short or the R/S slope is undefined; the
/// result is clamped to `[0, 1]`.
#[must_use]
pub fn hurst_exponent(series: &[f64]) -> f64 {
    let returns = log_returns(series);
    let n = returns.len();
    if n < HURST_MIN_RETURNS {
        return HURST_FALLBACK;
    }
    let max_window = n / 2;
    let mut xs: Vec<f64> = Vec::new();
    let mut ys: Vec<f64> = Vec::new();
    let mut window = HURST_BASE_WINDOW;
    while window <= max_window {
        if let Some(observed) = window_rescaled_range(&returns, window) {
            let expected = expected_rescaled_range(window);
            if observed > 0.0 && expected > 0.0 {
                // ln(obs) − ln(E) ≈ (H − 0.5)·ln(n): the small-sample bias in
                // E[R/S] is subtracted before the slope fit (Anis–Lloyd 1976,
                // Peters 1994; fixes legacy D9).
                xs.push((window as f64).ln());
                ys.push(observed.ln() - expected.ln());
            }
        }
        let next = (window as f64 * HURST_WINDOW_GROWTH).floor() as usize;
        window = if next > window { next } else { window + 1 };
    }
    if xs.len() < HURST_MIN_WINDOW_POINTS {
        return HURST_FALLBACK;
    }
    match ols_slope(&xs, &ys) {
        Some(slope) => (HURST_RANDOM_WALK + slope).clamp(0.0, 1.0),
        None => HURST_FALLBACK,
    }
}

/// Ornstein–Uhlenbeck half-life in *bars* from an AR(1) fit on the series
/// stationarized by its trailing simple moving average.
///
/// Returns [`None`] when the series is shorter than [`OU_MIN_POINTS`], when
/// the fit is not mean-reverting (`b ≥ −OU_B_THRESHOLD`), or when the implied
/// AR(1) coefficient `φ = 1 + b` falls outside `(0, 1)` (an overshooting fit
/// has no well-defined smooth half-life — fixes legacy D15).
#[must_use]
pub fn ou_half_life(series: &[f64]) -> Option<f64> {
    let px: Vec<f64> = series.iter().copied().filter(|v| *v > 0.0 && v.is_finite()).collect();
    let len = px.len();
    if len < OU_MIN_POINTS {
        return None;
    }
    let w = OU_MEAN_WINDOW.clamp(2, len - 1);
    // Causal (trailing) rolling mean.
    let mut roll_mean: Vec<f64> = Vec::with_capacity(len);
    for t in 0..len {
        let start = (t + 1).saturating_sub(w);
        roll_mean.push(mean(&px[start..=t]));
    }
    // Regress the price change on the previous bar's deviation from its mean.
    let mut dev: Vec<f64> = Vec::with_capacity(len - 1);
    let mut dxs: Vec<f64> = Vec::with_capacity(len - 1);
    for t in 1..len {
        dev.push(px[t - 1] - roll_mean[t - 1]);
        dxs.push(px[t] - px[t - 1]);
    }
    let b = ols_slope(&dev, &dxs)?;
    if b >= -OU_B_THRESHOLD {
        return None;
    }
    let phi = 1.0 + b;
    if phi <= 0.0 || phi >= 1.0 {
        return None;
    }
    let half_life = -LN_2 / phi.ln();
    (half_life.is_finite() && half_life > 0.0).then_some(half_life)
}

/// Three-state softmax regime classification (E16) from realized-vol level,
/// kurtosis and the Hurst exponent. Below [`REGIME_MIN_RETURNS`] returns the
/// flat-prior classification.
#[must_use]
pub fn classify_regime(candles: &[Candle]) -> RegimeClassification {
    let closes: Vec<f64> = candles.iter().map(|c| c.close).collect();
    let returns = log_returns(&closes);
    let hurst = hurst_exponent(&closes);
    if returns.len() < REGIME_MIN_RETURNS {
        return flat_classification();
    }

    let recent_start = returns.len().saturating_sub(REGIME_RECENT_WINDOW);
    let recent = &returns[recent_start..];
    let vol = sample_std(recent);
    let m = mean(recent);
    let kurt = if vol > 0.0 {
        let sum: f64 = recent
            .iter()
            .map(|r| {
                let z = (r - m) / vol;
                z * z * z * z
            })
            .sum();
        sum / recent.len() as f64
    } else {
        GAUSSIAN_KURTOSIS
    };

    // D7 fix: baseline volatility is measured over the returns preceding the
    // recent window, never overlapping the numerator.
    let baseline = {
        let base = &returns[..recent_start];
        let s = sample_std(base);
        if base.len() >= 2 && s > 0.0 {
            s
        } else {
            let full = sample_std(&returns);
            if full > 0.0 { full } else { VOL_RATIO_NEUTRAL }
        }
    };
    let vol_ratio = if baseline > 0.0 { vol / baseline } else { VOL_RATIO_NEUTRAL };

    let trend_e = ((hurst - HURST_RANDOM_WALK) * REGIME_HURST_GAIN).max(0.0)
        + ((vol_ratio - VOL_RATIO_NEUTRAL) * REGIME_TREND_VOL_GAIN).max(0.0);
    let revert_e = ((HURST_RANDOM_WALK - hurst) * REGIME_HURST_GAIN).max(0.0)
        + ((VOL_RATIO_NEUTRAL - vol_ratio) * REGIME_REVERT_VOL_GAIN).max(0.0)
        + REGIME_REVERT_PRIOR;
    let tail_e = ((kurt - REGIME_KURT_THRESHOLD) * REGIME_TAIL_KURT_GAIN).max(0.0)
        + ((vol_ratio - REGIME_TAIL_VOL_THRESHOLD) * REGIME_TAIL_VOL_GAIN).max(0.0);

    let probs = softmax([trend_e, revert_e, tail_e]);
    let (label, idx) = argmax3(probs);
    let confident = Readout::resolve(&REGIME_CONFIDENT_BAND, probs[idx]);
    RegimeClassification {
        probabilities: RegimeProbabilities {
            trend_expansion: probs[0],
            mean_reversion: probs[1],
            tail_risk: probs[2],
        },
        label,
        confident,
    }
}

// ── Internal helpers ─────────────────────────────────────────────────────

fn flat_classification() -> RegimeClassification {
    RegimeClassification {
        probabilities: RegimeProbabilities {
            trend_expansion: REGIME_FLAT_TREND,
            mean_reversion: REGIME_FLAT_REVERT,
            tail_risk: REGIME_FLAT_TAIL,
        },
        label: RegimeLabel::MeanReversion,
        confident: Readout::resolve(&REGIME_CONFIDENT_BAND, REGIME_FLAT_REVERT),
    }
}

/// Numerically stable softmax over three energies.
fn softmax(energies: [f64; 3]) -> [f64; 3] {
    let max = energies.into_iter().fold(f64::NEG_INFINITY, f64::max);
    let exps = [
        (energies[0] - max).exp(),
        (energies[1] - max).exp(),
        (energies[2] - max).exp(),
    ];
    let sum = exps[0] + exps[1] + exps[2];
    if sum > 0.0 && sum.is_finite() {
        [exps[0] / sum, exps[1] / sum, exps[2] / sum]
    } else {
        [REGIME_FLAT_TREND, REGIME_FLAT_REVERT, REGIME_FLAT_TAIL]
    }
}

/// Argmax over three posteriors; ties resolve to the first in
/// trend → revert → tail order. Returns the label and its index.
fn argmax3(p: [f64; 3]) -> (RegimeLabel, usize) {
    let mut best = 0;
    if p[1] > p[best] {
        best = 1;
    }
    if p[2] > p[best] {
        best = 2;
    }
    let label = match best {
        0 => RegimeLabel::TrendExpansion,
        1 => RegimeLabel::MeanReversion,
        _ => RegimeLabel::TailRisk,
    };
    (label, best)
}

/// Mean R/S over the non-overlapping `window`-length chunks of `returns`.
/// [`None`] when no chunk is admissible.
fn window_rescaled_range(returns: &[f64], window: usize) -> Option<f64> {
    let mut acc = 0.0;
    let mut count = 0usize;
    for chunk in returns.chunks_exact(window) {
        let m = mean(chunk);
        let mut cum = 0.0;
        let mut lo = f64::INFINITY;
        let mut hi = f64::NEG_INFINITY;
        for &x in chunk {
            cum += x - m;
            lo = lo.min(cum);
            hi = hi.max(cum);
        }
        let range = hi - lo;
        let s = sample_std(chunk);
        if s > HURST_STD_FLOOR && range > 0.0 {
            acc += range / s;
            count += 1;
        }
    }
    (count > 0).then(|| acc / count as f64)
}

/// Anis–Lloyd (1976) expected value of `R/S` for an independent series of
/// length `n`, with Peters' `(n − 0.5)/n` front-factor correction.
fn expected_rescaled_range(n: usize) -> f64 {
    let nf = n as f64;
    let front = (nf - 0.5) / nf;
    let norm = 1.0 / (nf * FRAC_PI_2).sqrt();
    let mut sum = 0.0;
    for i in 1..n {
        let if_ = i as f64;
        sum += ((nf - if_) / if_).sqrt();
    }
    front * norm * sum
}

/// Ordinary-least-squares slope of `ys` on `xs`. [`None`] when there are
/// fewer than two points or the regressor has zero variance.
fn ols_slope(xs: &[f64], ys: &[f64]) -> Option<f64> {
    if xs.len() < 2 || xs.len() != ys.len() {
        return None;
    }
    let xbar = mean(xs);
    let ybar = mean(ys);
    let mut sxx = 0.0;
    let mut sxy = 0.0;
    for (x, y) in xs.iter().zip(ys) {
        let dx = x - xbar;
        sxx += dx * dx;
        sxy += dx * (y - ybar);
    }
    (sxx > 0.0).then_some(sxy / sxx)
}

/// Bollinger-bandwidth percentile (E13) over the close series in `[0, 1]`.
/// Returns [`NEUTRAL_PERCENTILE`] when there is insufficient data.
fn bandwidth_percentile(closes: &[f64]) -> f64 {
    if closes.len() < VOL_ENGINE_MIN_CANDLES {
        return NEUTRAL_PERCENTILE;
    }
    let series = bollinger_bandwidth_series(closes);
    if series.len() < 2 {
        return NEUTRAL_PERCENTILE;
    }
    percentile_rank(&series, VOL_ENGINE_LOOKBACK)
}

/// Bollinger bandwidth `BB_WIDTH_MULT · σ / mean` (population `σ`) over each
/// trailing [`BB_PERIOD`]-length window of closes.
fn bollinger_bandwidth_series(closes: &[f64]) -> Vec<f64> {
    let period = BB_PERIOD.min(closes.len());
    if period < 2 {
        return Vec::new();
    }
    closes
        .windows(period)
        .map(|w| {
            let m = mean(w);
            let var = w.iter().map(|x| (x - m) * (x - m)).sum::<f64>() / period as f64;
            let sd = var.sqrt();
            if m > 0.0 { BB_WIDTH_MULT * sd / m } else { 0.0 }
        })
        .collect()
}

/// Fraction of the last `window` values strictly below the final value, in
/// `[0, 1]`. An all-equal window ranks 0; a window of one value ranks
/// [`NEUTRAL_PERCENTILE`].
fn percentile_rank(series: &[f64], window: usize) -> f64 {
    let start = series.len().saturating_sub(window);
    let slice = &series[start..];
    if slice.len() <= 1 {
        return NEUTRAL_PERCENTILE;
    }
    let current = slice[slice.len() - 1];
    let below = slice.iter().filter(|&&v| v < current).count();
    below as f64 / (slice.len() - 1) as f64
}

/// Realized-vol term-structure slope `rvNear / rvFar − 1`, or 0 when either
/// window has insufficient data or zero far-term vol.
fn rv_term_structure_slope(candles: &[Candle]) -> f64 {
    match (tail_c2c(candles, RV_NEAR_WINDOW), tail_c2c(candles, RV_FAR_WINDOW)) {
        (Some(near), Some(far)) if far > 0.0 => near / far - 1.0,
        _ => 0.0,
    }
}

/// Close-to-close realized vol over the last `window + 1` candles, or
/// [`None`] when there are too few candles or the estimator rejects them.
fn tail_c2c(candles: &[Candle], window: usize) -> Option<f64> {
    let need = window + 1;
    if candles.len() < need {
        return None;
    }
    let slice = &candles[candles.len() - need..];
    close_to_close(slice, RV_UNIT_PERIODS).ok()
}

/// Log returns of consecutive strictly-positive, finite prices.
fn log_returns(prices: &[f64]) -> Vec<f64> {
    prices
        .windows(2)
        .filter(|w| w[0] > 0.0 && w[1] > 0.0 && w[0].is_finite() && w[1].is_finite())
        .map(|w| (w[1] / w[0]).ln())
        .collect()
}

/// Arithmetic mean, 0 for an empty slice.
fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    xs.iter().sum::<f64>() / xs.len() as f64
}

/// Sample (Bessel, `n − 1`) standard deviation, 0 for fewer than two values.
fn sample_std(xs: &[f64]) -> f64 {
    let n = xs.len();
    if n < 2 {
        return 0.0;
    }
    let m = mean(xs);
    let var = xs.iter().map(|x| (x - m) * (x - m)).sum::<f64>() / (n as f64 - 1.0);
    var.sqrt()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use slayer_core::{BinaryState, TsMillis};

    /// Deterministic splitmix64-backed standard-normal generator so the
    /// synthetic series are reproducible without an RNG dependency.
    struct Rng(u64);

    impl Rng {
        fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
        fn uniform(&mut self) -> f64 {
            (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
        }
        fn normal(&mut self) -> f64 {
            let u1 = self.uniform().max(1e-12);
            let u2 = self.uniform();
            (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
        }
    }

    /// Persistent (positively autocorrelated) returns → high Hurst / trend.
    fn persistent_closes(n: usize, seed: u64) -> Vec<f64> {
        let mut rng = Rng(seed);
        let mut r = 0.0_f64;
        let mut price = 100.0_f64;
        let mut out = Vec::with_capacity(n);
        out.push(price);
        for _ in 1..n {
            r = 0.85 * r + 0.15 * rng.normal() * 0.01;
            price *= r.exp();
            out.push(price);
        }
        out
    }

    /// Ornstein–Uhlenbeck log-price → mean-reverting, low Hurst.
    fn ou_closes(n: usize, seed: u64) -> Vec<f64> {
        let mut rng = Rng(seed);
        let kappa = 0.3_f64;
        let mu = 100.0_f64.ln();
        let sigma = 0.02_f64;
        let mut x = mu;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            x += kappa * (mu - x) + sigma * rng.normal();
            out.push(x.exp());
        }
        out
    }

    /// Uncorrelated random walk → Hurst ≈ 0.5.
    fn random_walk_closes(n: usize, seed: u64) -> Vec<f64> {
        let mut rng = Rng(seed);
        let mut price = 100.0_f64;
        let mut out = Vec::with_capacity(n);
        out.push(price);
        for _ in 1..n {
            price *= (0.01 * rng.normal()).exp();
            out.push(price);
        }
        out
    }

    fn candles_from_closes(closes: &[f64]) -> Vec<Candle> {
        let mut out = Vec::with_capacity(closes.len());
        let mut prev = closes[0];
        for (i, &c) in closes.iter().enumerate() {
            let open = if i == 0 { c } else { prev };
            out.push(Candle {
                ts: TsMillis(i as u64 * 60_000),
                open,
                high: open.max(c) * 1.001,
                low: open.min(c) * 0.999,
                close: c,
                volume: 1_000.0,
            });
            prev = c;
        }
        out
    }

    #[test]
    fn trending_series_is_persistent_and_trend() {
        let closes = persistent_closes(600, 7);
        let h = hurst_exponent(&closes);
        assert!(h > 0.5, "expected persistent H > 0.5, got {h}");

        let candles = candles_from_closes(&closes);
        let cls = classify_regime(&candles);
        assert_eq!(cls.label, RegimeLabel::TrendExpansion, "posteriors {:?}", cls.probabilities);

        let state = analyze_regime(&candles);
        assert_eq!(state.persistence.state, BinaryState::Active);
    }

    #[test]
    fn ou_series_is_mean_reverting() {
        let closes = ou_closes(600, 11);
        let h = hurst_exponent(&closes);
        assert!(h < 0.5, "expected mean-reverting H < 0.5, got {h}");

        let hl = ou_half_life(&closes).expect("OU series must have a finite half-life");
        assert!(hl.is_finite() && hl > 0.0, "half-life {hl}");

        let candles = candles_from_closes(&closes);
        let cls = classify_regime(&candles);
        assert_eq!(cls.label, RegimeLabel::MeanReversion, "posteriors {:?}", cls.probabilities);
    }

    #[test]
    fn random_walk_has_no_half_life() {
        let closes = random_walk_closes(600, 3);
        assert!(ou_half_life(&closes).is_none());
        let h = hurst_exponent(&closes);
        assert!((h - 0.5).abs() < 0.15, "random walk H should be near 0.5, got {h}");
    }

    #[test]
    fn probabilities_sum_to_one() {
        // Normal path.
        let candles = candles_from_closes(&persistent_closes(400, 1));
        let cls = classify_regime(&candles);
        let p = cls.probabilities;
        assert!((p.trend_expansion + p.mean_reversion + p.tail_risk - 1.0).abs() < 1e-9);

        // Flat-prior path (too few returns).
        let short = candles_from_closes(&persistent_closes(6, 2));
        let flat = classify_regime(&short);
        let q = flat.probabilities;
        assert!((q.trend_expansion + q.mean_reversion + q.tail_risk - 1.0).abs() < 1e-9);
        assert_eq!(flat.label, RegimeLabel::MeanReversion);
        assert_eq!(flat.confident.state, BinaryState::Inactive);
    }

    #[test]
    fn analyze_regime_is_deterministic() {
        let candles = candles_from_closes(&persistent_closes(500, 42));
        // `black_box` on the input models the real system, where the candle
        // feed is runtime data. Without it this test crate would const-fold the
        // whole deterministic input and *one* of the two calls at compile time
        // (host libm), diverging by a ULP in the `exp`/`ln`-derived fields from
        // the other call's runtime evaluation (target libm). With an opaque
        // input both are genuine runtime evaluations of the one pure map, so
        // they are bit-for-bit identical.
        let a = analyze_regime(std::hint::black_box(&candles));
        let b = analyze_regime(std::hint::black_box(&candles));
        assert_eq!(a, b);
    }

    #[test]
    fn regime_state_survives_the_wire() {
        let a = analyze_regime(&candles_from_closes(&persistent_closes(500, 42)));
        let back: RegimeState = serde_json::from_str(&serde_json::to_string(&a).unwrap()).unwrap();
        // Discrete fields are recovered exactly; the floats to within a ULP —
        // serde_json's default deserializer is not correctly-rounded (the
        // `float_roundtrip` feature would tighten this), so an exact `==` on
        // computed f64s is not a property the wire guarantees.
        assert_eq!(back.classification.label, a.classification.label);
        assert_eq!(back.persistence.state, a.persistence.state);
        assert_eq!(back.compression.state, a.compression.state);
        assert_eq!(back.expansion.state, a.expansion.state);
        assert_eq!(back.half_life_bars.is_some(), a.half_life_bars.is_some());
        assert!((back.hurst - a.hurst).abs() < 1e-9);
        assert!((back.term_structure_slope - a.term_structure_slope).abs() < 1e-9);
        let (pb, pa) = (back.classification.probabilities, a.classification.probabilities);
        assert!((pb.trend_expansion - pa.trend_expansion).abs() < 1e-9);
        assert!((pb.mean_reversion - pa.mean_reversion).abs() < 1e-9);
        assert!((pb.tail_risk - pa.tail_risk).abs() < 1e-9);
    }

    #[test]
    fn insufficient_data_yields_inactive_defaults() {
        let candles = candles_from_closes(&[100.0, 101.0, 100.5, 101.2]);
        let state = analyze_regime(&candles);
        assert!((state.hurst - 0.5).abs() < 1e-12);
        assert_eq!(state.persistence.state, BinaryState::Inactive);
        assert!(state.half_life_bars.is_none());
        assert_eq!(state.classification.label, RegimeLabel::MeanReversion);
        assert_eq!(state.compression.state, BinaryState::Inactive);
        assert_eq!(state.expansion.state, BinaryState::Inactive);
    }

    #[test]
    fn compression_activates_when_bandwidth_collapses() {
        // Volatile history that goes flat: the trailing bandwidth is the
        // lowest in its window ⇒ low percentile ⇒ compression ACTIVE.
        let mut closes = Vec::new();
        let mut rng = Rng(9);
        let mut price = 100.0;
        for _ in 0..120 {
            price *= (0.03 * rng.normal()).exp();
            closes.push(price);
        }
        for _ in 0..40 {
            closes.push(price); // dead flat tail
        }
        let state = analyze_regime(&candles_from_closes(&closes));
        assert_eq!(state.compression.state, BinaryState::Active);
        assert_eq!(state.expansion.state, BinaryState::Inactive);
        assert!(state.compression.score > state.expansion.score);
    }

    #[test]
    fn expansion_activates_when_bandwidth_blows_out() {
        // Quiet history then a volatility blow-out: trailing bandwidth is the
        // highest in its window ⇒ high percentile ⇒ expansion ACTIVE.
        let mut closes = Vec::new();
        let mut rng = Rng(15);
        let mut price = 100.0;
        for _ in 0..120 {
            price *= (0.002 * rng.normal()).exp();
            closes.push(price);
        }
        for _ in 0..40 {
            price *= (0.06 * rng.normal()).exp();
            closes.push(price);
        }
        let state = analyze_regime(&candles_from_closes(&closes));
        assert_eq!(state.expansion.state, BinaryState::Active);
        assert_eq!(state.compression.state, BinaryState::Inactive);
    }

    #[test]
    fn hurst_short_series_falls_back() {
        assert!((hurst_exponent(&[100.0, 101.0, 99.0]) - 0.5).abs() < 1e-12);
    }

    #[test]
    fn regime_label_wire_format_is_screaming_snake() {
        assert_eq!(
            serde_json::to_string(&RegimeLabel::TrendExpansion).unwrap(),
            "\"TREND_EXPANSION\""
        );
        assert_eq!(serde_json::to_string(&RegimeLabel::TailRisk).unwrap(), "\"TAIL_RISK\"");
    }
}
