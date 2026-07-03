//! Breeden–Litzenberger implied risk-neutral density (RND) extraction.
//!
//! Recovers the market-implied terminal-price distribution of the underlying
//! from an option-chain snapshot via the Breeden–Litzenberger identity
//! `f(K) = e^{rT} · ∂²C/∂K²`: extract mid-implied vols across strikes, fit a
//! smooth (parabolic, smooth-by-construction) implied-vol smile, reprice a
//! dense strike grid through the BSM kernel ([`crate::black_scholes`]), take
//! the discounted second strike-difference, floor no-arbitrage-violating
//! negative lobes at zero, and normalize to a proper probability density
//! (∫f dK = 1). Percentiles, moments, and the tail-risk readout are read off
//! the resulting density / CDF.
//!
//! Provenance: `docs/spec/02-quant-suite.md` engine E3 (`solveImpliedRND`);
//! the constants catalogue there is reproduced as named constants below.
//!
//! Deviations from legacy:
//! - **D12**: the legacy `generateDummyRND` (E3b) fabricated a skew-normal
//!   placeholder with hardcoded, self-inconsistent moments whenever the chain
//!   was unusable. This engine never fabricates: an unusable chain returns a
//!   [`QuantError`], never an invented density or moment.
//! - **D13**: no fabricated fallback analytics of any kind — absence is an
//!   error value, never a placeholder number.
//! - **Numerics**: the recovered curve is treated as a continuous PDF,
//!   normalized by trapezoidal integral (∫f dK = 1), with moments and
//!   percentiles integrated / inverted from it, rather than the legacy
//!   discrete point-mass renormalization (Σp = 1). The ATM base vol driving
//!   the grid width and IV clamps is derived internally (IV nearest spot), so
//!   the public inputs are exactly the chain, spot, tenor, and rate.
//!
//! Retained from legacy (deliberate, per the spec's "Not defects" list): the
//! degenerate flat-smile fallback when the smile normal equations are singular
//! (an honest "no skew information" state using the measured ATM vol, never an
//! invented skew), and the spot-scaled second-difference bump `dK`.

use crate::black_scholes::{BsInputs, implied_vol, price};
use crate::error::QuantError;
use slayer_core::{HysteresisBand, OptionQuote, OptionRight, Readout};

/// Minimum number of distinct-strike implied-vol points required to attempt a
/// density fit. Below this the chain is unusable and the engine errors (never
/// the legacy dummy RND — D12).
const RND_MIN_CHAIN_SIZE: usize = 5;

/// Determinant magnitude below which the smile normal equations are treated as
/// singular, triggering the flat-smile fallback.
const RND_DET_EPS: f64 = 1e-12;

/// Continuous dividend yield assumed by the RND kernel (index/ETF convention).
const RND_DIV_YIELD: f64 = 0.0;

/// Absolute floor of the interpolated implied vol (1.5 vol points).
const RND_IV_FLOOR_ABS: f64 = 0.015;
/// Relative floor of the interpolated implied vol, as a multiple of base vol.
const RND_IV_FLOOR_REL: f64 = 0.25;
/// Absolute cap of the interpolated implied vol (220 vol points).
const RND_IV_CAP_ABS: f64 = 2.20;
/// Relative cap of the interpolated implied vol, as a multiple of base vol.
const RND_IV_CAP_REL: f64 = 5.0;

/// Half-width of the strike grid, in model standard deviations of terminal
/// price either side of spot.
const RND_GRID_SIGMAS: f64 = 3.2;
/// Lower bound of the strike grid as a fraction of spot (protects the deep
/// downside wing from going non-positive).
const RND_MIN_STRIKE_FRAC: f64 = 0.40;
/// Grid mesh density: the grid holds `RND_MESH_DENSITY + 1` nodes.
const RND_MESH_DENSITY: usize = 100;

/// Absolute floor of the Breeden–Litzenberger second-difference bump `dK`, in
/// underlying points.
const RND_MIN_DK: f64 = 0.5;
/// Second-difference bump `dK` as a fraction of spot (spot-scaled so a fixed
/// dollar bump does not collapse into floating-point cancellation on
/// high-priced underlyings).
const RND_DK_SPOT_FRAC: f64 = 0.0025;

/// Gaussian smoothing bandwidth, expressed in grid steps.
const RND_SMOOTH_BW_STEPS: f64 = 2.0;
/// Gaussian smoothing kernel half-window, in node indices.
const RND_SMOOTH_HALF_WINDOW: usize = 10;

/// Subtrahend converting raw fourth standardized moment to excess kurtosis.
const EXCESS_KURTOSIS_OFFSET: f64 = 3.0;
/// Excess-kurtosis threshold above which the terminal distribution is flagged
/// fat-tailed. Provenance: E3 `isFatTailed`.
const FAT_TAIL_KURTOSIS_THRESHOLD: f64 = 1.2;
/// Fat-tail readout band (strict single threshold on excess kurtosis).
const FAT_TAIL_BAND: HysteresisBand = HysteresisBand::strict(FAT_TAIL_KURTOSIS_THRESHOLD);

/// Standard institutional percentile set (5 / 25 / 50 / 75 / 95%).
const PERCENTILE_LEVELS: [f64; 5] = [0.05, 0.25, 0.50, 0.75, 0.95];

/// One node of the recovered risk-neutral density curve.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RndPoint {
    /// Strike / terminal-price coordinate, underlying points.
    pub strike: f64,
    /// Probability density `f(strike)`, normalized so ∫f dK = 1 (units: per
    /// underlying point).
    pub density: f64,
    /// Cumulative distribution `P(S_T ≤ strike)` ∈ [0, 1].
    pub cdf: f64,
}

/// The 5 / 25 / 50 / 75 / 95th percentiles of the recovered distribution, in
/// underlying points.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RndPercentiles {
    /// 5th percentile.
    pub p05: f64,
    /// 25th percentile.
    pub p25: f64,
    /// Median (50th percentile).
    pub p50: f64,
    /// 75th percentile.
    pub p75: f64,
    /// 95th percentile.
    pub p95: f64,
}

/// A recovered risk-neutral density curve plus its summary statistics.
#[derive(Debug, Clone, PartialEq)]
pub struct RndCurve {
    /// Density nodes, ascending in strike (`RND_MESH_DENSITY + 1` of them).
    pub points: Vec<RndPoint>,
    /// CDF-derived percentiles.
    pub percentiles: RndPercentiles,
    /// Distribution mean (risk-neutral expected terminal price), points.
    pub mean: f64,
    /// Distribution standard deviation, points.
    pub std_dev: f64,
    /// Distribution skewness (dimensionless, third standardized moment).
    pub skewness: f64,
    /// Excess kurtosis (fourth standardized moment minus 3).
    pub excess_kurtosis: f64,
    /// Fat-tail readout: `Active` iff excess kurtosis exceeds
    /// [`FAT_TAIL_KURTOSIS_THRESHOLD`]; `score` carries the excess kurtosis.
    pub fat_tail: Readout,
    /// Probability mass strictly below spot, `P(S_T < spot)` ∈ [0, 1].
    pub prob_below_spot: f64,
    /// Probability mass at or above spot, `P(S_T ≥ spot)` ∈ [0, 1].
    pub prob_above_spot: f64,
}

/// Extract an implied vol for one quote: prefer the quoted IV, else invert the
/// bid/ask midpoint through the BSM implied-vol solver. Returns `None` when no
/// sound IV can be recovered (no quoted IV and no two-sided mid, or the mid
/// violates the no-arbitrage vol bracket). Shared with the skew engine.
pub(crate) fn quote_implied_vol(
    quote: &OptionQuote,
    spot: f64,
    t_years: f64,
    rate: f64,
) -> Option<f64> {
    if let Some(iv) = quote.iv
        && iv.is_finite()
        && iv > 0.0
    {
        return Some(iv);
    }
    let mid = quote.mid()?;
    let base = BsInputs {
        spot,
        strike: quote.strike,
        t_years,
        vol: 0.0,
        rate,
        div_yield: RND_DIV_YIELD,
    };
    implied_vol(mid, &base, quote.right).ok().filter(|v| v.is_finite() && *v > 0.0)
}

/// Determinant of a 3×3 matrix given row-major.
fn det3x3(m: &[[f64; 3]; 3]) -> f64 {
    m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
        - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0])
}

/// Ordinary-least-squares parabolic smile `iv(x) = a + b·x + c·x²`, with
/// `x = ln(K/spot)`, solved by Cramer's rule. Returns the flat smile
/// `(iv_base, 0, 0)` when the normal-equation matrix is singular (deliberate
/// degenerate fallback — an honest "no skew information" state, never an
/// invented skew).
fn fit_smile(points: &[(f64, f64)], spot: f64, iv_base: f64) -> (f64, f64, f64) {
    let (mut s_x, mut s_x2, mut s_x3, mut s_x4) = (0.0, 0.0, 0.0, 0.0);
    let (mut s_y, mut s_xy, mut s_x2y) = (0.0, 0.0, 0.0);
    let n = points.len() as f64;
    for &(strike, iv) in points {
        let x = (strike / spot).ln();
        let x2 = x * x;
        s_x += x;
        s_x2 += x2;
        s_x3 += x2 * x;
        s_x4 += x2 * x2;
        s_y += iv;
        s_xy += x * iv;
        s_x2y += x2 * iv;
    }
    let m = [[n, s_x, s_x2], [s_x, s_x2, s_x3], [s_x2, s_x3, s_x4]];
    let det = det3x3(&m);
    if det.abs() <= RND_DET_EPS {
        return (iv_base, 0.0, 0.0);
    }
    let rhs = [s_y, s_xy, s_x2y];
    let col = |j: usize| -> f64 {
        let mut mj = m;
        for (row, &r) in mj.iter_mut().zip(rhs.iter()) {
            row[j] = r;
        }
        det3x3(&mj) / det
    };
    (col(0), col(1), col(2))
}

/// Clamp an interpolated implied vol into the sane band around `iv_base`.
fn clamp_iv(iv: f64, iv_base: f64) -> f64 {
    let lo = RND_IV_FLOOR_ABS.max(RND_IV_FLOOR_REL * iv_base);
    let hi = RND_IV_CAP_ABS.max(RND_IV_CAP_REL * iv_base);
    iv.clamp(lo, hi)
}

/// Trapezoidal integral of `ys` sampled on a uniform grid of spacing `step`.
fn trapz(ys: &[f64], step: f64) -> f64 {
    ys.windows(2).map(|w| 0.5 * (w[0] + w[1]) * step).sum()
}

/// Linear-interpolate the strike at which the (monotone non-decreasing) CDF
/// crosses probability `p`.
fn invert_cdf(strikes: &[f64], cdf: &[f64], p: f64) -> f64 {
    if p <= cdf[0] {
        return strikes[0];
    }
    let last = cdf.len() - 1;
    if p >= cdf[last] {
        return strikes[last];
    }
    for i in 0..last {
        if cdf[i] <= p && p <= cdf[i + 1] {
            let span = cdf[i + 1] - cdf[i];
            if span <= 0.0 {
                return strikes[i];
            }
            let frac = (p - cdf[i]) / span;
            return strikes[i] + frac * (strikes[i + 1] - strikes[i]);
        }
    }
    strikes[last]
}

/// Linear-interpolate the CDF value at strike coordinate `x` on the uniform
/// grid `strikes` (spacing `step`, first node `strikes[0]`).
fn cdf_at(strikes: &[f64], cdf: &[f64], step: f64, x: f64) -> f64 {
    let last = strikes.len() - 1;
    if x <= strikes[0] {
        return cdf[0];
    }
    if x >= strikes[last] {
        return cdf[last];
    }
    let rel = (x - strikes[0]) / step;
    let i = (rel.floor() as usize).min(last - 1);
    let frac = rel - i as f64;
    cdf[i] + frac * (cdf[i + 1] - cdf[i])
}

/// Extract the Breeden–Litzenberger risk-neutral density from an option-chain
/// snapshot.
///
/// `quotes` is the raw chain (calls and puts, any subset of strikes), `spot`
/// the underlying, `t_years` the tenor in years (ACT/365 by the caller), and
/// `rate` the continuously-compounded risk-free rate. The ATM base vol used
/// for grid sizing and IV clamps is derived internally as the recovered IV
/// nearest spot.
///
/// # Errors
/// Returns [`QuantError::Domain`] for a non-positive / non-finite spot, tenor,
/// or rate, or a degenerate grid; [`QuantError::InsufficientData`] when fewer
/// than [`RND_MIN_CHAIN_SIZE`] distinct-strike implied vols can be recovered.
/// Never fabricates a density (contrast the legacy dummy RND, D12).
pub fn implied_rnd(
    quotes: &[OptionQuote],
    spot: f64,
    t_years: f64,
    rate: f64,
) -> Result<RndCurve, QuantError> {
    if !(spot.is_finite() && spot > 0.0) {
        return Err(QuantError::Domain("RND: spot must be finite and positive"));
    }
    if !(t_years.is_finite() && t_years > 0.0) {
        return Err(QuantError::Domain("RND: tenor must be finite and positive"));
    }
    if !rate.is_finite() {
        return Err(QuantError::Domain("RND: rate must be finite"));
    }

    // Recover per-quote implied vols, keyed by strike with calls overwriting
    // puts at a shared strike (spec E3 step 2).
    let mut raw: Vec<(f64, bool, f64)> = quotes
        .iter()
        .filter_map(|q| {
            quote_implied_vol(q, spot, t_years, rate)
                .map(|iv| (q.strike, matches!(q.right, OptionRight::Call), iv))
        })
        .filter(|&(strike, _, _)| strike.is_finite() && strike > 0.0)
        .collect();
    raw.sort_by(|a, b| a.0.total_cmp(&b.0));

    let mut smile: Vec<(f64, f64)> = Vec::with_capacity(raw.len());
    for (strike, is_call, iv) in raw {
        match smile.last_mut() {
            Some(last) if last.0 == strike => {
                if is_call {
                    last.1 = iv;
                }
            }
            _ => smile.push((strike, iv)),
        }
    }

    if smile.len() < RND_MIN_CHAIN_SIZE {
        return Err(QuantError::InsufficientData {
            needed: RND_MIN_CHAIN_SIZE,
            got: smile.len(),
        });
    }

    // Base vol = recovered IV at the strike nearest spot.
    let iv_base = smile
        .iter()
        .min_by(|a, b| (a.0 - spot).abs().total_cmp(&(b.0 - spot).abs()))
        .map_or(0.0, |&(_, iv)| iv);
    if !(iv_base.is_finite() && iv_base > 0.0) {
        return Err(QuantError::Domain("RND: degenerate base volatility"));
    }

    let (a, b, c) = fit_smile(&smile, spot, iv_base);
    let interp_iv = |k: f64| -> f64 {
        let x = (k / spot).ln();
        clamp_iv(a + b * x + c * x * x, iv_base)
    };

    // Dense strike grid centered on spot.
    let std_model = spot * iv_base * t_years.sqrt();
    let min_strike = (RND_MIN_STRIKE_FRAC * spot).max(spot - RND_GRID_SIGMAS * std_model);
    let max_strike = spot + RND_GRID_SIGMAS * std_model;
    if max_strike <= min_strike {
        return Err(QuantError::Domain("RND: degenerate strike grid"));
    }
    let step_k = (max_strike - min_strike) / RND_MESH_DENSITY as f64;
    let d_k = RND_MIN_DK.max(RND_DK_SPOT_FRAC * spot);
    if min_strike - d_k <= 0.0 {
        return Err(QuantError::Domain("RND: grid too narrow for second-difference stencil"));
    }

    let call_price = |k: f64| -> Result<f64, QuantError> {
        price(
            &BsInputs { spot, strike: k, t_years, vol: interp_iv(k), rate, div_yield: RND_DIV_YIELD },
            OptionRight::Call,
        )
    };

    let discount = (rate * t_years).exp();
    let n_nodes = RND_MESH_DENSITY + 1;
    let mut strikes = Vec::with_capacity(n_nodes);
    let mut raw_density = Vec::with_capacity(n_nodes);
    for i in 0..n_nodes {
        let k = min_strike + i as f64 * step_k;
        // Breeden–Litzenberger second strike-difference, no-arbitrage floored.
        let second = call_price(k + d_k)? - 2.0 * call_price(k)? + call_price(k - d_k)?;
        strikes.push(k);
        raw_density.push((discount * second / (d_k * d_k)).max(0.0));
    }

    // Gaussian smoothing over a fixed index half-window; weights depend only on
    // the index offset since the grid is uniform.
    let bandwidth_idx = RND_SMOOTH_BW_STEPS;
    let mut density: Vec<f64> = (0..n_nodes)
        .map(|i| {
            let lo = i.saturating_sub(RND_SMOOTH_HALF_WINDOW);
            let hi = (i + RND_SMOOTH_HALF_WINDOW).min(n_nodes - 1);
            let (mut num, mut den) = (0.0, 0.0);
            for (j, &p) in raw_density.iter().enumerate().take(hi + 1).skip(lo) {
                let u = (i as f64 - j as f64) / bandwidth_idx;
                let w = (-0.5 * u * u).exp();
                num += w * p;
                den += w;
            }
            if den > 0.0 { num / den } else { 0.0 }
        })
        .collect();

    // Normalize to a unit-integral PDF.
    let total = trapz(&density, step_k);
    if !(total.is_finite() && total > 0.0) {
        return Err(QuantError::Domain("RND: degenerate (non-positive) density integral"));
    }
    for d in &mut density {
        *d /= total;
    }

    // Cumulative trapezoidal CDF.
    let mut cdf = Vec::with_capacity(n_nodes);
    let mut acc = 0.0;
    cdf.push(0.0);
    for i in 1..n_nodes {
        acc += 0.5 * (density[i] + density[i - 1]) * step_k;
        cdf.push(acc);
    }

    // Moments by trapezoidal integration of the PDF.
    let k_f: Vec<f64> = strikes.iter().zip(&density).map(|(k, f)| k * f).collect();
    let mean = trapz(&k_f, step_k);
    let var_terms: Vec<f64> =
        strikes.iter().zip(&density).map(|(k, f)| (k - mean) * (k - mean) * f).collect();
    let variance = trapz(&var_terms, step_k).max(0.0);
    let std_dev = variance.sqrt();
    let (skewness, excess_kurtosis) = if std_dev > 0.0 {
        let sk: Vec<f64> = strikes
            .iter()
            .zip(&density)
            .map(|(k, f)| ((k - mean) / std_dev).powi(3) * f)
            .collect();
        let ku: Vec<f64> = strikes
            .iter()
            .zip(&density)
            .map(|(k, f)| ((k - mean) / std_dev).powi(4) * f)
            .collect();
        (trapz(&sk, step_k), trapz(&ku, step_k) - EXCESS_KURTOSIS_OFFSET)
    } else {
        (0.0, 0.0)
    };

    let levels = PERCENTILE_LEVELS.map(|p| invert_cdf(&strikes, &cdf, p));
    let percentiles = RndPercentiles {
        p05: levels[0],
        p25: levels[1],
        p50: levels[2],
        p75: levels[3],
        p95: levels[4],
    };

    let prob_below_spot = cdf_at(&strikes, &cdf, step_k, spot).clamp(0.0, 1.0);
    let prob_above_spot = (1.0 - prob_below_spot).clamp(0.0, 1.0);

    let points = strikes
        .iter()
        .zip(&density)
        .zip(&cdf)
        .map(|((&strike, &density), &cdf)| RndPoint { strike, density, cdf })
        .collect();

    Ok(RndCurve {
        points,
        percentiles,
        mean,
        std_dev,
        skewness,
        excess_kurtosis,
        fat_tail: Readout::resolve(&FAT_TAIL_BAND, excess_kurtosis),
        prob_below_spot,
        prob_above_spot,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::black_scholes::{BsInputs, price};
    use crate::dist::norm_cdf_inv;
    use slayer_core::ExpiryDate;

    const EXPIRY: ExpiryDate = ExpiryDate { year: 2026, month: 8, day: 21 };

    /// Build a call chain whose mid prices are exact flat-vol BSM prices, so
    /// the recovered RND must be the closed-form lognormal. `iv` is left
    /// `None` to exercise the full IV-recovery path.
    fn lognormal_call_chain(
        spot: f64,
        sigma: f64,
        t: f64,
        r: f64,
        strikes: &[f64],
    ) -> Vec<OptionQuote> {
        strikes
            .iter()
            .map(|&k| {
                let p = price(
                    &BsInputs { spot, strike: k, t_years: t, vol: sigma, rate: r, div_yield: 0.0 },
                    OptionRight::Call,
                )
                .unwrap();
                OptionQuote {
                    strike: k,
                    right: OptionRight::Call,
                    expiry: EXPIRY,
                    bid: Some(p),
                    ask: Some(p),
                    volume: 0,
                    open_interest: 0,
                    iv: None,
                    greeks: None,
                }
            })
            .collect()
    }

    fn strikes(spot: f64, lo_frac: f64, hi_frac: f64, n: usize) -> Vec<f64> {
        (0..n)
            .map(|i| {
                let frac = lo_frac + (hi_frac - lo_frac) * i as f64 / (n - 1) as f64;
                spot * frac
            })
            .collect()
    }

    #[test]
    fn density_integrates_to_one() {
        let ks = strikes(100.0, 0.85, 1.15, 31);
        let chain = lognormal_call_chain(100.0, 0.20, 30.0 / 365.0, 0.03, &ks);
        let rnd = implied_rnd(&chain, 100.0, 30.0 / 365.0, 0.03).unwrap();
        let dens: Vec<f64> = rnd.points.iter().map(|p| p.density).collect();
        let step = rnd.points[1].strike - rnd.points[0].strike;
        let integral = super::trapz(&dens, step);
        assert!((integral - 1.0).abs() < 1e-9, "integral = {integral}");
    }

    #[test]
    fn cdf_is_monotone_and_bounded() {
        let ks = strikes(100.0, 0.85, 1.15, 31);
        let chain = lognormal_call_chain(100.0, 0.25, 45.0 / 365.0, 0.04, &ks);
        let rnd = implied_rnd(&chain, 100.0, 45.0 / 365.0, 0.04).unwrap();
        let mut prev = -1.0;
        for pt in &rnd.points {
            assert!(pt.cdf >= prev - 1e-12, "cdf not monotone at K={}", pt.strike);
            assert!((0.0..=1.0 + 1e-9).contains(&pt.cdf));
            assert!(pt.density >= 0.0, "negative density at K={}", pt.strike);
            prev = pt.cdf;
        }
        assert!((rnd.points.last().unwrap().cdf - 1.0).abs() < 1e-9);
    }

    #[test]
    fn lognormal_roundtrip_recovers_closed_form_percentiles() {
        let spot = 100.0;
        let sigma = 0.20;
        let t = 30.0 / 365.0;
        let r = 0.03;
        let ks = strikes(spot, 0.85, 1.15, 31);
        let chain = lognormal_call_chain(spot, sigma, t, r, &ks);
        let rnd = implied_rnd(&chain, spot, t, r).unwrap();

        // Closed-form risk-neutral lognormal quantile of S_T.
        let closed = |p: f64| -> f64 {
            let z = norm_cdf_inv(p);
            spot * ((r - 0.5 * sigma * sigma) * t + sigma * t.sqrt() * z).exp()
        };
        let got = [
            rnd.percentiles.p05,
            rnd.percentiles.p25,
            rnd.percentiles.p50,
            rnd.percentiles.p75,
            rnd.percentiles.p95,
        ];
        for (p, &g) in PERCENTILE_LEVELS.iter().zip(got.iter()) {
            let want = closed(*p);
            assert!(
                (g - want).abs() < 0.01 * spot,
                "p{}: got {g}, want {want}",
                (p * 100.0) as i32
            );
        }
        // Mean should match the risk-neutral forward S·e^{rT}.
        let fwd = spot * (r * t).exp();
        assert!((rnd.mean - fwd).abs() < 0.02 * spot, "mean {} vs fwd {fwd}", rnd.mean);
    }

    #[test]
    fn near_lognormal_is_not_fat_tailed() {
        let ks = strikes(100.0, 0.85, 1.15, 31);
        let chain = lognormal_call_chain(100.0, 0.18, 30.0 / 365.0, 0.03, &ks);
        let rnd = implied_rnd(&chain, 100.0, 30.0 / 365.0, 0.03).unwrap();
        // A short-dated lognormal has near-zero excess kurtosis.
        assert!(rnd.excess_kurtosis.abs() < FAT_TAIL_KURTOSIS_THRESHOLD);
        assert!(!rnd.fat_tail.state.is_active());
        assert_eq!(rnd.fat_tail.score, rnd.excess_kurtosis);
    }

    #[test]
    fn prob_below_and_above_partition_unity() {
        let ks = strikes(100.0, 0.85, 1.15, 31);
        let chain = lognormal_call_chain(100.0, 0.22, 60.0 / 365.0, 0.05, &ks);
        let rnd = implied_rnd(&chain, 100.0, 60.0 / 365.0, 0.05).unwrap();
        assert!((rnd.prob_below_spot + rnd.prob_above_spot - 1.0).abs() < 1e-12);
        assert!(rnd.prob_below_spot > 0.0 && rnd.prob_below_spot < 1.0);
    }

    #[test]
    fn thin_chain_errors_never_fabricates() {
        let ks = strikes(100.0, 0.95, 1.05, 3);
        let chain = lognormal_call_chain(100.0, 0.20, 30.0 / 365.0, 0.03, &ks);
        assert!(matches!(
            implied_rnd(&chain, 100.0, 30.0 / 365.0, 0.03),
            Err(QuantError::InsufficientData { needed: RND_MIN_CHAIN_SIZE, got: 3 })
        ));
    }

    #[test]
    fn rejects_bad_domain() {
        let ks = strikes(100.0, 0.85, 1.15, 31);
        let chain = lognormal_call_chain(100.0, 0.20, 30.0 / 365.0, 0.03, &ks);
        assert!(implied_rnd(&chain, -1.0, 0.1, 0.03).is_err());
        assert!(implied_rnd(&chain, 100.0, 0.0, 0.03).is_err());
        assert!(implied_rnd(&chain, 100.0, 0.1, f64::NAN).is_err());
    }
}
