//! Volatility metrics: realized-vol cone, honest IV rank / percentile, skew
//! analytics, and the vol risk premium.
//!
//! All estimators are descriptive measurements (signed magnitudes and
//! fractions), not stateful readouts — none answers "is a condition in
//! force?", so per the binary-state doctrine they are represented as data, not
//! [`slayer_core::Readout`]s. Annualization is the caller's concern: every
//! cone entry takes `periods_per_year` explicitly (the calendar is owned
//! upstream), reusing the estimator family in [`crate::realized_vol`] rather
//! than duplicating it.
//!
//! Provenance: `docs/spec/02-quant-suite.md` engines E4 (`calculateVolatility
//! Cone`), E5 (`computeSkewAnalytics`), and the honest IV-rank definition; the
//! IV-history requirement traces to spec 01 defect D9.
//!
//! Deviations from legacy:
//! - **D9**: `iv_rank` and `iv_percentile` are computed honestly from a
//!   supplied IV history — rank = `(cur − min)/(max − min)`, percentile =
//!   fraction of history below current — never the legacy affine-of-IV fakes.
//!   Insufficient history returns `None` (explicit abstention), never a
//!   fabricated number.
//! - **D3**: `skew_slope` uses the correct `dσ/dK` sign and the *actual*
//!   25-delta strike gap, not the legacy `(put25 − call25)/(0.1·spot)` which
//!   both flipped the sign and hardcoded the strike spacing.
//! - **D13**: no fabricated RV / percentile fallbacks. A window with
//!   insufficient history is omitted; risk-reversal / butterfly with no
//!   25-delta bracket are `None`. The legacy fixed-50 RR/BF percentiles are
//!   omitted entirely — a genuine rank needs the rolling history maintained by
//!   engine E12, and is never faked here.

use crate::error::QuantError;
use crate::rnd::quote_implied_vol;
use crate::{black_scholes, realized_vol};
use slayer_core::{Candle, OptionQuote, OptionRight};

/// Canonical realized-vol cone windows, in bars. Provenance: E4.
pub const VOL_CONE_WINDOWS: [usize; 6] = [5, 10, 20, 30, 45, 60];

/// Minimum number of rolling samples required to form a cone for a window;
/// below this the window carries no distribution and is omitted.
const VOL_CONE_MIN_SAMPLES: usize = 2;

/// Lower-quartile probability for the cone.
const CONE_P25: f64 = 0.25;
/// Median probability for the cone.
const CONE_MEDIAN: f64 = 0.50;
/// Upper-quartile probability for the cone.
const CONE_P75: f64 = 0.75;

/// Minimum IV-history length below which rank / percentile abstain (`None`).
const IV_METRICS_MIN_HISTORY: usize = 2;

/// Target absolute delta for the risk-reversal / butterfly wings. Provenance:
/// E5 `WING_TARGET_DELTA`.
const WING_TARGET_DELTA: f64 = 0.25;

/// Continuous dividend yield assumed when recomputing skew deltas.
const SKEW_DIV_YIELD: f64 = 0.0;

/// Minimum |Δ| gap between two contracts for delta interpolation to be
/// well-posed.
const DELTA_INTERP_EPS: f64 = 1e-12;

/// Realized-volatility estimator selector for the cone. Each variant dispatches
/// to the corresponding function in [`crate::realized_vol`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RvEstimator {
    /// Close-to-close sample volatility.
    CloseToClose,
    /// Parkinson high–low range estimator.
    Parkinson,
    /// Garman–Klass OHLC estimator.
    GarmanKlass,
    /// Rogers–Satchell drift-independent estimator.
    RogersSatchell,
    /// Yang–Zhang gap-aware estimator.
    YangZhang,
}

impl RvEstimator {
    fn apply(self, candles: &[Candle], periods_per_year: f64) -> Result<f64, QuantError> {
        match self {
            Self::CloseToClose => realized_vol::close_to_close(candles, periods_per_year),
            Self::Parkinson => realized_vol::parkinson(candles, periods_per_year),
            Self::GarmanKlass => realized_vol::garman_klass(candles, periods_per_year),
            Self::RogersSatchell => realized_vol::rogers_satchell(candles, periods_per_year),
            Self::YangZhang => realized_vol::yang_zhang(candles, periods_per_year),
        }
    }
}

/// One window of the realized-vol cone: the distribution of rolling realized
/// vol over the window across all available history, plus the most recent
/// (current) value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VolConeWindow {
    /// Window length, bars.
    pub window: usize,
    /// Minimum rolling realized vol.
    pub min: f64,
    /// 25th percentile.
    pub p25: f64,
    /// Median.
    pub median: f64,
    /// 75th percentile.
    pub p75: f64,
    /// Maximum rolling realized vol.
    pub max: f64,
    /// Most-recent window's realized vol.
    pub current: f64,
    /// Number of rolling samples the distribution is built from.
    pub sample_count: usize,
}

/// A realized-vol cone across several windows.
#[derive(Debug, Clone, PartialEq)]
pub struct VolCone {
    /// Per-window cone entries; windows without enough history are omitted.
    pub windows: Vec<VolConeWindow>,
}

/// IV rank / percentile computed honestly against a supplied IV history.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IvMetrics {
    /// The current implied vol the metrics are computed for.
    pub current_iv: f64,
    /// `(current − min)/(max − min)` over history, clamped to [0, 1]. `None`
    /// when history is too short or has no spread (abstention, not a fake).
    pub rank: Option<f64>,
    /// Fraction of history strictly below `current_iv` ∈ [0, 1]. `None` when
    /// history is shorter than [`IV_METRICS_MIN_HISTORY`].
    pub percentile: Option<f64>,
    /// Number of finite, positive history observations used.
    pub history_len: usize,
}

/// 25-delta skew analytics from a chain snapshot.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SkewAnalytics {
    /// At-the-money implied vol (mean IV at the strike nearest spot).
    pub atm_iv: f64,
    /// 25-delta call implied vol, if a bracket exists.
    pub call_25d_iv: Option<f64>,
    /// 25-delta put implied vol, if a bracket exists.
    pub put_25d_iv: Option<f64>,
    /// Risk reversal `call25 − put25` (decimal vol); negative under put skew.
    pub risk_reversal_25d: Option<f64>,
    /// Butterfly `(call25 + put25)/2 − atm` (decimal vol).
    pub butterfly_25d: Option<f64>,
    /// ATM smile slope `dσ/dK` at the 25-delta wings (D3-corrected sign, actual
    /// strike gap), decimal vol per underlying point.
    pub skew_slope: Option<f64>,
}

/// Type-7 (linear-interpolated) quantile of an ascending-sorted slice.
fn quantile_sorted(sorted: &[f64], p: f64) -> f64 {
    match sorted.len() {
        0 => f64::NAN,
        1 => sorted[0],
        n => {
            let rank = p * (n - 1) as f64;
            let lo = rank.floor() as usize;
            let hi = (lo + 1).min(n - 1);
            let frac = rank - lo as f64;
            sorted[lo] + frac * (sorted[hi] - sorted[lo])
        }
    }
}

/// Build a realized-vol cone over the requested `windows`.
///
/// For each window, realized vol is computed on every rolling sub-window of
/// `candles` via `estimator`, and the min / 25th / median / 75th / max of that
/// sample plus the most-recent (current) value are reported. Windows with
/// fewer than [`VOL_CONE_MIN_SAMPLES`] usable rolling samples are omitted.
///
/// # Errors
/// Returns [`QuantError::Domain`] if a candle sub-window is malformed (OHLC out
/// of domain). Sub-windows too short for the chosen estimator are skipped, not
/// errored — the affected window is simply omitted if none survive.
pub fn vol_cone(
    candles: &[Candle],
    windows: &[usize],
    periods_per_year: f64,
    estimator: RvEstimator,
) -> Result<VolCone, QuantError> {
    let mut out = Vec::new();
    for &w in windows {
        if w == 0 || candles.len() < w {
            continue;
        }
        let mut rvs = Vec::with_capacity(candles.len() - w + 1);
        for start in 0..=(candles.len() - w) {
            match estimator.apply(&candles[start..start + w], periods_per_year) {
                Ok(v) if v.is_finite() => rvs.push(v),
                Ok(_) => {}
                Err(QuantError::InsufficientData { .. }) => {}
                Err(e) => return Err(e),
            }
        }
        if rvs.len() < VOL_CONE_MIN_SAMPLES {
            continue;
        }
        let current = rvs[rvs.len() - 1];
        let mut sorted = rvs.clone();
        sorted.sort_by(f64::total_cmp);
        out.push(VolConeWindow {
            window: w,
            min: sorted[0],
            p25: quantile_sorted(&sorted, CONE_P25),
            median: quantile_sorted(&sorted, CONE_MEDIAN),
            p75: quantile_sorted(&sorted, CONE_P75),
            max: sorted[sorted.len() - 1],
            current,
            sample_count: rvs.len(),
        });
    }
    Ok(VolCone { windows: out })
}

/// Compute honest IV rank and percentile of `current_iv` against `history`.
///
/// Non-finite / non-positive history observations are discarded. `rank` is
/// `None` when fewer than [`IV_METRICS_MIN_HISTORY`] observations survive or
/// the history has no spread; `percentile` is `None` only when history is too
/// short. Never fabricates (contrast legacy D9).
#[must_use]
pub fn iv_metrics(current_iv: f64, history: &[f64]) -> IvMetrics {
    let hist: Vec<f64> = history
        .iter()
        .copied()
        .filter(|v| v.is_finite() && *v > 0.0)
        .collect();
    let n = hist.len();
    if !(current_iv.is_finite() && current_iv > 0.0) || n < IV_METRICS_MIN_HISTORY {
        return IvMetrics {
            current_iv,
            rank: None,
            percentile: None,
            history_len: n,
        };
    }
    let min = hist.iter().copied().fold(f64::INFINITY, f64::min);
    let max = hist.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let rank = if max > min {
        Some(((current_iv - min) / (max - min)).clamp(0.0, 1.0))
    } else {
        None
    };
    let below = hist.iter().filter(|&&v| v < current_iv).count();
    let percentile = Some(below as f64 / n as f64);
    IvMetrics {
        current_iv,
        rank,
        percentile,
        history_len: n,
    }
}

/// Interpolate `(iv, strike)` at target absolute delta over per-contract points
/// `(strike, abs_delta, iv)`. Points must be sorted by strike (|Δ| is monotone
/// decreasing in strike for both wings). Returns `None` when no straddling
/// bracket exists — abstention, never a fabricated fallback.
fn interp_at_abs_delta(points: &[(f64, f64, f64)], target: f64) -> Option<(f64, f64)> {
    for w in points.windows(2) {
        let (s0, d0, iv0) = w[0];
        let (s1, d1, iv1) = w[1];
        let (lo, hi) = if d0 <= d1 { (d0, d1) } else { (d1, d0) };
        if (lo..=hi).contains(&target) && (d1 - d0).abs() > DELTA_INTERP_EPS {
            let frac = (target - d0) / (d1 - d0);
            return Some((iv0 + frac * (iv1 - iv0), s0 + frac * (s1 - s0)));
        }
    }
    None
}

/// Compute 25-delta skew analytics from a chain snapshot.
///
/// Deltas are taken from provider greeks when present, else recomputed from the
/// contract's (possibly recovered) implied vol via BSM. `t_years` and `rate`
/// are required for that recomputation.
///
/// # Errors
/// Returns [`QuantError::Domain`] for a non-positive / non-finite spot, tenor,
/// or rate, and [`QuantError::InsufficientData`] when no contract yields a
/// usable implied vol (so even ATM vol is undefined).
pub fn skew_analytics(
    quotes: &[OptionQuote],
    spot: f64,
    t_years: f64,
    rate: f64,
) -> Result<SkewAnalytics, QuantError> {
    if !(spot.is_finite() && spot > 0.0) {
        return Err(QuantError::Domain("skew: spot must be finite and positive"));
    }
    if !(t_years.is_finite() && t_years > 0.0) {
        return Err(QuantError::Domain(
            "skew: tenor must be finite and positive",
        ));
    }
    if !rate.is_finite() {
        return Err(QuantError::Domain("skew: rate must be finite"));
    }

    let mut calls: Vec<(f64, f64, f64)> = Vec::new();
    let mut puts: Vec<(f64, f64, f64)> = Vec::new();
    // (strike, iv) of every contract with a usable IV, for ATM determination.
    let mut atm_pool: Vec<(f64, f64)> = Vec::new();

    for q in quotes {
        let Some(iv) = quote_implied_vol(q, spot, t_years, rate) else {
            continue;
        };
        let abs_delta = match q.greeks {
            Some(g) if g.delta.is_finite() => g.delta.abs(),
            _ => {
                let inputs = black_scholes::BsInputs {
                    spot,
                    strike: q.strike,
                    t_years,
                    vol: iv,
                    rate,
                    div_yield: SKEW_DIV_YIELD,
                };
                match black_scholes::greeks(&inputs, q.right) {
                    Ok(g) => g.delta.abs(),
                    Err(_) => continue,
                }
            }
        };
        atm_pool.push((q.strike, iv));
        match q.right {
            OptionRight::Call => calls.push((q.strike, abs_delta, iv)),
            OptionRight::Put => puts.push((q.strike, abs_delta, iv)),
        }
    }

    if atm_pool.is_empty() {
        return Err(QuantError::InsufficientData { needed: 1, got: 0 });
    }

    // ATM IV = mean of IVs at the strike nearest spot.
    let atm_strike = atm_pool
        .iter()
        .min_by(|a, b| (a.0 - spot).abs().total_cmp(&(b.0 - spot).abs()))
        .map_or(spot, |&(s, _)| s);
    let atm_ivs: Vec<f64> = atm_pool
        .iter()
        .filter(|&&(s, _)| s == atm_strike)
        .map(|&(_, iv)| iv)
        .collect();
    let atm_iv = atm_ivs.iter().sum::<f64>() / atm_ivs.len() as f64;

    calls.sort_by(|a, b| a.0.total_cmp(&b.0));
    puts.sort_by(|a, b| a.0.total_cmp(&b.0));

    let call25 = interp_at_abs_delta(&calls, WING_TARGET_DELTA);
    let put25 = interp_at_abs_delta(&puts, WING_TARGET_DELTA);

    let risk_reversal_25d = match (call25, put25) {
        (Some((c, _)), Some((p, _))) => Some(c - p),
        _ => None,
    };
    let butterfly_25d = match (call25, put25) {
        (Some((c, _)), Some((p, _))) => Some(0.5 * (c + p) - atm_iv),
        _ => None,
    };
    // D3-corrected: dσ/dK across the actual 25Δ strike gap (call strike above
    // put strike), so the sign is right and the spacing is real.
    let skew_slope = match (call25, put25) {
        (Some((civ, ck)), Some((piv, pk))) if (ck - pk).abs() > DELTA_INTERP_EPS => {
            Some((civ - piv) / (ck - pk))
        }
        _ => None,
    };

    Ok(SkewAnalytics {
        atm_iv,
        call_25d_iv: call25.map(|(iv, _)| iv),
        put_25d_iv: put25.map(|(iv, _)| iv),
        risk_reversal_25d,
        butterfly_25d,
        skew_slope,
    })
}

/// Vol risk premium: current implied vol minus current realized vol (decimal
/// vol points; positive = options rich to realized). Provenance: E4
/// `varianceRiskPremium`.
#[must_use]
pub fn vol_risk_premium(current_iv: f64, current_rv: f64) -> f64 {
    current_iv - current_rv
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::black_scholes::{BsInputs, price};
    use rand::SeedableRng;
    use rand_chacha::ChaCha8Rng;
    use rand_distr::{Distribution, StandardNormal};
    use slayer_core::{ExpiryDate, TsMillis};

    const EXPIRY: ExpiryDate = ExpiryDate {
        year: 2026,
        month: 9,
        day: 18,
    };
    const PERIODS: f64 = 252.0;

    fn gbm_candles(true_vol: f64, n_bars: usize, seed: u64) -> Vec<Candle> {
        const INTRA: usize = 128;
        let mut rng = ChaCha8Rng::seed_from_u64(seed);
        let dt = 1.0 / (PERIODS * INTRA as f64);
        let step_vol = true_vol * dt.sqrt();
        let mut price = 100.0_f64;
        let mut out = Vec::with_capacity(n_bars);
        for bar in 0..n_bars {
            let open = price;
            let (mut high, mut low) = (open, open);
            for _ in 0..INTRA {
                let z: f64 = StandardNormal.sample(&mut rng);
                price *= (step_vol * z - 0.5 * step_vol * step_vol).exp();
                high = high.max(price);
                low = low.min(price);
            }
            out.push(Candle {
                ts: TsMillis(bar as u64),
                open,
                high,
                low,
                close: price,
                volume: 0.0,
            });
        }
        out
    }

    #[test]
    fn cone_is_ordered_and_recovers_vol() {
        let candles = gbm_candles(0.20, 400, 11);
        let cone = vol_cone(&candles, &VOL_CONE_WINDOWS, PERIODS, RvEstimator::YangZhang).unwrap();
        assert_eq!(cone.windows.len(), VOL_CONE_WINDOWS.len());
        for w in &cone.windows {
            assert!(w.min <= w.p25 + 1e-12);
            assert!(w.p25 <= w.median + 1e-12);
            assert!(w.median <= w.p75 + 1e-12);
            assert!(w.p75 <= w.max + 1e-12);
            assert!(w.current >= w.min - 1e-12 && w.current <= w.max + 1e-12);
            assert!(w.sample_count >= VOL_CONE_MIN_SAMPLES);
            assert!(
                (w.median - 0.20).abs() < 0.06,
                "window {} median {}",
                w.window,
                w.median
            );
        }
    }

    #[test]
    fn cone_omits_windows_without_history() {
        let candles = gbm_candles(0.2, 8, 3);
        let cone = vol_cone(&candles, &VOL_CONE_WINDOWS, PERIODS, RvEstimator::Parkinson).unwrap();
        // Only the 5-bar window has ≥2 rolling samples from 8 candles.
        assert_eq!(cone.windows.len(), 1);
        assert_eq!(cone.windows[0].window, 5);
    }

    #[test]
    fn cone_propagates_malformed_candles() {
        let mut candles = gbm_candles(0.2, 40, 4);
        candles[10].high = candles[10].low - 1.0;
        assert!(matches!(
            vol_cone(&candles, &[5], PERIODS, RvEstimator::GarmanKlass),
            Err(QuantError::Domain(_))
        ));
    }

    #[test]
    fn iv_rank_and_percentile_are_honest() {
        let history = [0.10, 0.20, 0.30, 0.40, 0.50];
        let m = iv_metrics(0.30, &history);
        assert!((m.rank.unwrap() - 0.5).abs() < 1e-9); // (0.30-0.10)/(0.50-0.10)
        assert!((m.percentile.unwrap() - 0.4).abs() < 1e-9); // 2 of 5 strictly below
        assert_eq!(m.history_len, 5);

        let hi = iv_metrics(0.60, &history);
        assert_eq!(hi.rank, Some(1.0)); // clamped
        assert_eq!(hi.percentile, Some(1.0));
    }

    #[test]
    fn iv_metrics_abstain_without_history() {
        let empty = iv_metrics(0.25, &[]);
        assert_eq!(empty.rank, None);
        assert_eq!(empty.percentile, None);

        let single = iv_metrics(0.25, &[0.25]);
        assert_eq!(single.rank, None);
        assert_eq!(single.percentile, None);

        let flat = iv_metrics(0.30, &[0.30, 0.30, 0.30]);
        assert_eq!(flat.rank, None); // no spread
        assert_eq!(flat.percentile, Some(0.0));
    }

    /// Build a chain with a parametric smile `iv(x) = atm − slope·x + curv·x²`,
    /// `x = ln(K/spot)`, over calls and puts. Positive `slope` ⇒ put skew.
    fn smile_chain(spot: f64, atm: f64, slope: f64, curv: f64, t: f64, r: f64) -> Vec<OptionQuote> {
        let mut out = Vec::new();
        for i in 0..41 {
            let frac = 0.80 + 0.40 * i as f64 / 40.0;
            let k = spot * frac;
            let x = (k / spot).ln();
            let iv = (atm - slope * x + curv * x * x).max(0.02);
            for right in [OptionRight::Call, OptionRight::Put] {
                let p = price(
                    &BsInputs {
                        spot,
                        strike: k,
                        t_years: t,
                        vol: iv,
                        rate: r,
                        div_yield: 0.0,
                    },
                    right,
                )
                .unwrap();
                out.push(OptionQuote {
                    strike: k,
                    right,
                    expiry: EXPIRY,
                    bid: Some(p),
                    ask: Some(p),
                    volume: 0,
                    open_interest: 0,
                    iv: Some(iv),
                    greeks: None,
                });
            }
        }
        out
    }

    #[test]
    fn put_skew_has_negative_rr_and_slope() {
        let chain = smile_chain(100.0, 0.20, 0.6, 0.8, 0.25, 0.03);
        let s = skew_analytics(&chain, 100.0, 0.25, 0.03).unwrap();
        assert!(s.call_25d_iv.is_some() && s.put_25d_iv.is_some());
        assert!(
            s.risk_reversal_25d.unwrap() < 0.0,
            "rr {:?}",
            s.risk_reversal_25d
        );
        assert!(s.butterfly_25d.unwrap() > 0.0, "bf {:?}", s.butterfly_25d);
        assert!(s.skew_slope.unwrap() < 0.0, "slope {:?}", s.skew_slope);
        assert!((s.atm_iv - 0.20).abs() < 0.02);
    }

    #[test]
    fn flat_smile_has_zero_skew() {
        let chain = smile_chain(100.0, 0.20, 0.0, 0.0, 0.25, 0.03);
        let s = skew_analytics(&chain, 100.0, 0.25, 0.03).unwrap();
        assert!(s.risk_reversal_25d.unwrap().abs() < 1e-6);
        assert!(s.butterfly_25d.unwrap().abs() < 1e-6);
        assert!(s.skew_slope.unwrap().abs() < 1e-6);
    }

    #[test]
    fn skew_rejects_empty_and_bad_domain() {
        assert!(matches!(
            skew_analytics(&[], 100.0, 0.25, 0.03),
            Err(QuantError::InsufficientData { .. })
        ));
        let chain = smile_chain(100.0, 0.20, 0.0, 0.0, 0.25, 0.03);
        assert!(skew_analytics(&chain, 0.0, 0.25, 0.03).is_err());
        assert!(skew_analytics(&chain, 100.0, -1.0, 0.03).is_err());
    }

    #[test]
    fn vrp_is_iv_minus_rv() {
        assert!((vol_risk_premium(0.25, 0.18) - 0.07).abs() < 1e-15);
        assert!(vol_risk_premium(0.15, 0.20) < 0.0);
    }
}
