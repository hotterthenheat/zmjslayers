//! Realized volatility estimator family.
//!
//! All estimators return *annualized* volatility (decimal) given a candle
//! series and the number of bar periods per year (e.g. 252 for daily bars,
//! 252 × 390 for regular-session minute bars — the caller owns the calendar).
//!
//! References: Parkinson (1980), Garman–Klass (1980), Rogers–Satchell (1991),
//! Yang–Zhang (2000). Exact coefficients below; legacy deviations are noted
//! in `docs/spec/04-vol-distribution.md`.

use crate::error::QuantError;
use slayer_core::Candle;
use std::f64::consts::LN_2;

/// Parkinson variance divisor: 4·ln2.
const PARKINSON_DIVISOR: f64 = 4.0 * LN_2;

/// Garman–Klass close-to-open coefficient: 2·ln2 − 1.
const GK_CO_COEFF: f64 = 2.0 * LN_2 - 1.0;

/// Yang–Zhang k-weight numerator α in k = α / (1.34 + (n+1)/(n-1)).
const YZ_ALPHA: f64 = 0.34;

/// Yang–Zhang k-weight denominator constant.
const YZ_BETA: f64 = 1.34;

/// Minimum candles for the single-bar range estimators.
const MIN_BARS_RANGE: usize = 1;

/// Minimum candles for return-based estimators (need one return).
const MIN_BARS_RETURNS: usize = 3;

fn validate(candles: &[Candle], min: usize) -> Result<(), QuantError> {
    if candles.len() < min {
        return Err(QuantError::InsufficientData { needed: min, got: candles.len() });
    }
    let sane = candles.iter().all(|c| {
        c.open > 0.0
            && c.close > 0.0
            && c.low > 0.0
            && c.high >= c.low
            && c.high >= c.open.max(c.close)
            && c.low <= c.open.min(c.close)
            && [c.open, c.high, c.low, c.close].iter().all(|v| v.is_finite())
    });
    if sane { Ok(()) } else { Err(QuantError::Domain("candle OHLC out of domain")) }
}

fn sample_variance(xs: &[f64]) -> f64 {
    let n = xs.len() as f64;
    let mean = xs.iter().sum::<f64>() / n;
    xs.iter().map(|x| (x - mean) * (x - mean)).sum::<f64>() / (n - 1.0)
}

/// Close-to-close estimator: annualized sample standard deviation of log
/// returns. Needs ≥ 3 candles (two returns).
pub fn close_to_close(candles: &[Candle], periods_per_year: f64) -> Result<f64, QuantError> {
    validate(candles, MIN_BARS_RETURNS)?;
    let returns: Vec<f64> =
        candles.windows(2).map(|w| (w[1].close / w[0].close).ln()).collect();
    Ok((sample_variance(&returns) * periods_per_year).sqrt())
}

/// Parkinson (1980) high–low range estimator:
/// σ² = (P/n) · Σ ln(H/L)² / (4 ln 2).
pub fn parkinson(candles: &[Candle], periods_per_year: f64) -> Result<f64, QuantError> {
    validate(candles, MIN_BARS_RANGE)?;
    let n = candles.len() as f64;
    let sum: f64 = candles
        .iter()
        .map(|c| {
            let hl = (c.high / c.low).ln();
            hl * hl
        })
        .sum();
    Ok((periods_per_year * sum / (n * PARKINSON_DIVISOR)).sqrt())
}

/// Garman–Klass (1980) OHLC estimator:
/// σ² = (P/n) · Σ [ ½ ln(H/L)² − (2 ln 2 − 1) ln(C/O)² ].
pub fn garman_klass(candles: &[Candle], periods_per_year: f64) -> Result<f64, QuantError> {
    validate(candles, MIN_BARS_RANGE)?;
    let n = candles.len() as f64;
    let sum: f64 = candles
        .iter()
        .map(|c| {
            let hl = (c.high / c.low).ln();
            let co = (c.close / c.open).ln();
            0.5 * hl * hl - GK_CO_COEFF * co * co
        })
        .sum();
    // The GK combination can go negative on pathological bars; clamp at zero
    // rather than emitting NaN from sqrt.
    Ok(((periods_per_year * sum / n).max(0.0)).sqrt())
}

/// Rogers–Satchell (1991) drift-independent estimator:
/// σ² = (P/n) · Σ [ ln(H/C)·ln(H/O) + ln(L/C)·ln(L/O) ].
pub fn rogers_satchell(candles: &[Candle], periods_per_year: f64) -> Result<f64, QuantError> {
    validate(candles, MIN_BARS_RANGE)?;
    let n = candles.len() as f64;
    let sum: f64 = candles.iter().map(rs_term).sum();
    Ok(((periods_per_year * sum / n).max(0.0)).sqrt())
}

fn rs_term(c: &Candle) -> f64 {
    (c.high / c.close).ln() * (c.high / c.open).ln()
        + (c.low / c.close).ln() * (c.low / c.open).ln()
}

/// Yang–Zhang (2000) estimator — drift-independent, handles overnight gaps:
/// σ²_yz = σ²_overnight + k·σ²_open-to-close + (1−k)·σ²_rs,
/// k = 0.34 / (1.34 + (n+1)/(n−1)) with n the number of gap periods.
pub fn yang_zhang(candles: &[Candle], periods_per_year: f64) -> Result<f64, QuantError> {
    validate(candles, MIN_BARS_RETURNS)?;
    let n = (candles.len() - 1) as f64;

    let overnight: Vec<f64> =
        candles.windows(2).map(|w| (w[1].open / w[0].close).ln()).collect();
    let open_close: Vec<f64> = candles[1..].iter().map(|c| (c.close / c.open).ln()).collect();
    let rs_mean = candles[1..].iter().map(rs_term).sum::<f64>() / n;

    let k = YZ_ALPHA / (YZ_BETA + (n + 1.0) / (n - 1.0));
    let var = sample_variance(&overnight) + k * sample_variance(&open_close)
        + (1.0 - k) * rs_mean;
    Ok(((periods_per_year * var).max(0.0)).sqrt())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand_chacha::ChaCha8Rng;
    use rand_distr::{Distribution, StandardNormal};
    use slayer_core::TsMillis;

    /// Simulate GBM daily candles with `INTRA_STEPS` intra-bar monitoring
    /// points. Range estimators are biased slightly low under discrete
    /// monitoring, hence the loose tolerances below.
    fn gbm_candles(true_vol: f64, n_bars: usize, seed: u64) -> Vec<Candle> {
        const INTRA_STEPS: usize = 256;
        const PERIODS: f64 = 252.0;
        let mut rng = ChaCha8Rng::seed_from_u64(seed);
        let dt = 1.0 / (PERIODS * INTRA_STEPS as f64);
        let step_vol = true_vol * dt.sqrt();
        let mut price = 100.0_f64;
        let mut out = Vec::with_capacity(n_bars);
        for bar in 0..n_bars {
            let open = price;
            let (mut high, mut low) = (open, open);
            for _ in 0..INTRA_STEPS {
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
    fn all_estimators_recover_gbm_vol() {
        let true_vol = 0.20;
        let candles = gbm_candles(true_vol, 2000, 7);
        let p = 252.0;
        let estimates = [
            close_to_close(&candles, p).unwrap(),
            parkinson(&candles, p).unwrap(),
            garman_klass(&candles, p).unwrap(),
            rogers_satchell(&candles, p).unwrap(),
            yang_zhang(&candles, p).unwrap(),
        ];
        // Range estimators carry a small negative bias under discrete
        // monitoring (H/L understate the continuous extremes even at 256
        // intra-bar steps), hence the asymmetric-but-loose tolerance.
        for (i, est) in estimates.iter().enumerate() {
            let rel = (est - true_vol).abs() / true_vol;
            assert!(rel < 0.08, "estimator {i}: {est} vs {true_vol} (rel {rel:.3})");
        }
    }

    #[test]
    fn flat_series_has_zero_vol() {
        let flat: Vec<Candle> = (0..10)
            .map(|i| Candle {
                ts: TsMillis(i),
                open: 50.0,
                high: 50.0,
                low: 50.0,
                close: 50.0,
                volume: 0.0,
            })
            .collect();
        for f in [close_to_close, parkinson, garman_klass, rogers_satchell, yang_zhang] {
            assert_eq!(f(&flat, 252.0).unwrap(), 0.0);
        }
    }

    #[test]
    fn short_series_rejected() {
        let candles = gbm_candles(0.2, 2, 1);
        assert!(matches!(
            close_to_close(&candles, 252.0),
            Err(QuantError::InsufficientData { needed: 3, got: 2 })
        ));
        assert!(parkinson(&candles, 252.0).is_ok());
    }

    #[test]
    fn malformed_candles_rejected() {
        let mut candles = gbm_candles(0.2, 5, 2);
        candles[2].high = candles[2].low - 1.0;
        for f in [close_to_close, parkinson, garman_klass, rogers_satchell, yang_zhang] {
            assert_eq!(f(&candles, 252.0), Err(QuantError::Domain("candle OHLC out of domain")));
        }
    }
}
