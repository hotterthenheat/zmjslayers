//! Geometric-Brownian-motion Monte Carlo path engine.
//!
//! Simulates risk-neutral GBM price paths from an explicit seed and reports a
//! percentile cone at every step (5 / 25 / 50 / 75 / 95%), terminal
//! distribution statistics, and a small set of retained sample paths for
//! rendering. The simulation is *pure*: identical `(config, seed)` inputs
//! yield bit-identical output — the gateway derives the seed from the snapshot
//! clock so replays reproduce exactly (see `docs/ARCHITECTURE.md` §3).
//!
//! Discretization is the exact-in-law log-Euler GBM scheme
//! `S_{t+1} = S_t · exp((μ − ½σ²)·dt + σ·√dt·z)`, `z ~ N(0, 1)`, so the sum of
//! `n_steps` increments is distributed exactly as `N(0, T)` and the estimator
//! is unbiased for the terminal mean `S·e^{μT}`.
//!
//! Provenance: `docs/spec/02-quant-suite.md` engine E15 (`simulateMonteCarlo`),
//! GBM branch.
//!
//! Deviations from legacy:
//! - **Scope**: only the GBM model is implemented here. The Merton-jump and
//!   Heston branches are out of scope for this engine, so the legacy Heston
//!   full-truncation bias (**D14**) is unreachable by construction.
//! - **RNG**: the legacy bespoke `mulberry32` stream is replaced by seeded
//!   ChaCha8 ([`rand_chacha`]), per the architecture's seed doctrine.
//!   Determinism is preserved through the explicit `u64` seed (the spec lists
//!   seeded deterministic RNG as deliberate-keep).
//! - **Variance reduction**: optional antithetic variates (not present in the
//!   legacy engine) halve the effective sampling error at small path counts;
//!   they are deterministic and off by default.
//! - **Quantiles**: percentiles use linear-interpolated order statistics
//!   (type-7) rather than the legacy floor nearest-rank, for a smoother cone;
//!   determinism is unaffected.

use crate::error::QuantError;
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;
use rand_distr::{Distribution, StandardNormal};

/// Minimum number of time steps (clamped, never an error).
const MC_MIN_STEPS: usize = 1;
/// Maximum number of time steps.
const MC_MAX_STEPS: usize = 1000;
/// Minimum number of paths.
const MC_MIN_PATHS: usize = 1;
/// Maximum number of paths.
const MC_MAX_PATHS: usize = 200_000;
/// Default number of retained sample paths for rendering.
pub const MC_DEFAULT_SAMPLE_PATHS: usize = 80;
/// Standard institutional percentile set (5 / 25 / 50 / 75 / 95%).
const PERCENTILE_LEVELS: [f64; 5] = [0.05, 0.25, 0.50, 0.75, 0.95];

/// GBM Monte Carlo configuration. Continuous inputs are validated; `n_steps`,
/// `n_paths`, and `sample_paths` are clamped into their supported ranges
/// rather than rejected.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GbmConfig {
    /// Initial spot, underlying points.
    pub spot: f64,
    /// Annualized risk-neutral drift μ (typically the risk-free rate).
    pub drift: f64,
    /// Annualized volatility σ, decimal.
    pub vol: f64,
    /// Simulation horizon, years.
    pub horizon_years: f64,
    /// Number of time steps (clamped to `[1, 1000]`).
    pub n_steps: usize,
    /// Number of simulated paths (clamped to `[1, 200000]`).
    pub n_paths: usize,
    /// PRNG seed. Same seed ⇒ identical output.
    pub seed: u64,
    /// Number of full paths retained for rendering (clamped to `[0, n_paths]`).
    pub sample_paths: usize,
    /// Enable antithetic variates for variance reduction.
    pub antithetic: bool,
}

impl GbmConfig {
    /// Build a config with the default retained-sample-path count and no
    /// antithetic variates.
    #[must_use]
    pub fn new(
        spot: f64,
        drift: f64,
        vol: f64,
        horizon_years: f64,
        n_steps: usize,
        n_paths: usize,
        seed: u64,
    ) -> Self {
        Self {
            spot,
            drift,
            vol,
            horizon_years,
            n_steps,
            n_paths,
            seed,
            sample_paths: MC_DEFAULT_SAMPLE_PATHS,
            antithetic: false,
        }
    }
}

/// Percentile cone at one time step.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GbmConeStep {
    /// Step index, `0..=n_steps` (`0` is the deterministic spot at t = 0).
    pub step: usize,
    /// Elapsed time at this step, years.
    pub t_years: f64,
    /// 5th percentile of the price distribution across paths, points.
    pub p05: f64,
    /// 25th percentile, points.
    pub p25: f64,
    /// Median, points.
    pub p50: f64,
    /// 75th percentile, points.
    pub p75: f64,
    /// 95th percentile, points.
    pub p95: f64,
}

/// Terminal (t = horizon) distribution statistics.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GbmTerminalStats {
    /// Sample mean of terminal price, points.
    pub mean: f64,
    /// Sample standard deviation of terminal price, points.
    pub std_dev: f64,
    /// Fraction of paths finishing strictly above spot ∈ [0, 1].
    pub prob_above_spot: f64,
    /// Fraction of paths finishing strictly below spot ∈ [0, 1].
    pub prob_below_spot: f64,
    /// Closed-form GBM mean `spot·e^{μT}` — the reference the sample mean is
    /// validated against.
    pub analytic_mean: f64,
}

/// Result of a GBM Monte Carlo simulation.
#[derive(Debug, Clone, PartialEq)]
pub struct GbmSimulation {
    /// Percentile cone, one entry per step (`n_steps + 1` entries).
    pub cone: Vec<GbmConeStep>,
    /// Terminal distribution statistics.
    pub terminal: GbmTerminalStats,
    /// Retained sample paths, each of length `n_steps + 1` (prices per step).
    pub sample_paths: Vec<Vec<f64>>,
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

/// Compute the 5-point percentile cone entry for a set of path values.
fn cone_step(step: usize, t_years: f64, values: &[f64]) -> GbmConeStep {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let q = PERCENTILE_LEVELS.map(|p| quantile_sorted(&sorted, p));
    GbmConeStep { step, t_years, p05: q[0], p25: q[1], p50: q[2], p75: q[3], p95: q[4] }
}

/// Simulate GBM price paths and summarize them.
///
/// # Errors
/// Returns [`QuantError::Domain`] when spot is non-positive, or any continuous
/// input (spot, drift, vol, horizon) is non-finite, or the horizon is
/// non-positive. Step / path / sample counts are clamped, never rejected.
pub fn simulate_gbm(cfg: &GbmConfig) -> Result<GbmSimulation, QuantError> {
    if !(cfg.spot.is_finite() && cfg.spot > 0.0) {
        return Err(QuantError::Domain("MC: spot must be finite and positive"));
    }
    if !(cfg.drift.is_finite() && cfg.vol.is_finite() && cfg.vol >= 0.0) {
        return Err(QuantError::Domain("MC: drift/vol must be finite, vol non-negative"));
    }
    if !(cfg.horizon_years.is_finite() && cfg.horizon_years > 0.0) {
        return Err(QuantError::Domain("MC: horizon must be finite and positive"));
    }

    let n_steps = cfg.n_steps.clamp(MC_MIN_STEPS, MC_MAX_STEPS);
    let n_paths = cfg.n_paths.clamp(MC_MIN_PATHS, MC_MAX_PATHS);
    let keep = cfg.sample_paths.min(n_paths);

    let dt = cfg.horizon_years / n_steps as f64;
    let sqrt_dt = dt.sqrt();
    let drift_term = (cfg.drift - 0.5 * cfg.vol * cfg.vol) * dt;
    let vol_term = cfg.vol * sqrt_dt;

    let mut rng = ChaCha8Rng::seed_from_u64(cfg.seed);
    let mut current = vec![cfg.spot; n_paths];
    let mut sample_paths: Vec<Vec<f64>> = (0..keep)
        .map(|_| {
            let mut v = Vec::with_capacity(n_steps + 1);
            v.push(cfg.spot);
            v
        })
        .collect();

    let mut cone = Vec::with_capacity(n_steps + 1);
    cone.push(cone_step(0, 0.0, &current));

    for step in 1..=n_steps {
        if cfg.antithetic {
            let mut i = 0;
            while i + 1 < n_paths {
                let z: f64 = StandardNormal.sample(&mut rng);
                current[i] *= (drift_term + vol_term * z).exp();
                current[i + 1] *= (drift_term - vol_term * z).exp();
                i += 2;
            }
            if i < n_paths {
                let z: f64 = StandardNormal.sample(&mut rng);
                current[i] *= (drift_term + vol_term * z).exp();
            }
        } else {
            for s in &mut current {
                let z: f64 = StandardNormal.sample(&mut rng);
                *s *= (drift_term + vol_term * z).exp();
            }
        }

        for (path, &price) in sample_paths.iter_mut().zip(&current) {
            path.push(price);
        }
        cone.push(cone_step(step, step as f64 * dt, &current));
    }

    let n = n_paths as f64;
    let mean = current.iter().sum::<f64>() / n;
    let var = if n_paths > 1 {
        current.iter().map(|s| (s - mean) * (s - mean)).sum::<f64>() / (n - 1.0)
    } else {
        0.0
    };
    let above = current.iter().filter(|&&s| s > cfg.spot).count() as f64 / n;
    let below = current.iter().filter(|&&s| s < cfg.spot).count() as f64 / n;

    Ok(GbmSimulation {
        cone,
        terminal: GbmTerminalStats {
            mean,
            std_dev: var.sqrt(),
            prob_above_spot: above,
            prob_below_spot: below,
            analytic_mean: cfg.spot * (cfg.drift * cfg.horizon_years).exp(),
        },
        sample_paths,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn base(n_paths: usize, antithetic: bool) -> GbmConfig {
        GbmConfig {
            spot: 100.0,
            drift: 0.05,
            vol: 0.20,
            horizon_years: 1.0,
            n_steps: 50,
            n_paths,
            seed: 424_242,
            sample_paths: 16,
            antithetic,
        }
    }

    #[test]
    fn terminal_mean_matches_analytic() {
        let sim = simulate_gbm(&base(40_000, true)).unwrap();
        let rel = (sim.terminal.mean - sim.terminal.analytic_mean).abs()
            / sim.terminal.analytic_mean;
        assert!(rel < 0.01, "mean {} vs analytic {} (rel {rel:.4})",
            sim.terminal.mean, sim.terminal.analytic_mean);
        assert!((sim.terminal.analytic_mean - 100.0 * 0.05_f64.exp()).abs() < 1e-9);
    }

    #[test]
    fn percentile_bands_are_monotone() {
        let sim = simulate_gbm(&base(20_000, false)).unwrap();
        assert_eq!(sim.cone.len(), 51);
        for c in &sim.cone {
            assert!(c.p05 <= c.p25 + 1e-9);
            assert!(c.p25 <= c.p50 + 1e-9);
            assert!(c.p50 <= c.p75 + 1e-9);
            assert!(c.p75 <= c.p95 + 1e-9);
        }
        // The cone fans out: terminal spread exceeds the t=0 (degenerate) one.
        let first = &sim.cone[0];
        let last = &sim.cone[50];
        assert!((first.p95 - first.p05).abs() < 1e-9);
        assert!(last.p95 - last.p05 > 1.0);
    }

    #[test]
    fn deterministic_for_fixed_seed() {
        let cfg = base(5_000, true);
        let a = simulate_gbm(&cfg).unwrap();
        let b = simulate_gbm(&cfg).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn different_seed_changes_paths() {
        let a = simulate_gbm(&base(2_000, false)).unwrap();
        let mut cfg = base(2_000, false);
        cfg.seed = 999;
        let b = simulate_gbm(&cfg).unwrap();
        assert_ne!(a.sample_paths, b.sample_paths);
    }

    #[test]
    fn zero_vol_collapses_to_deterministic_growth() {
        let mut cfg = base(1_000, false);
        cfg.vol = 0.0;
        let sim = simulate_gbm(&cfg).unwrap();
        let expected = 100.0 * (0.05_f64 * 1.0).exp();
        assert!(sim.terminal.std_dev < 1e-9);
        for c in &sim.cone {
            assert!((c.p05 - c.p95).abs() < 1e-9);
        }
        assert!((sim.terminal.mean - expected).abs() < 1e-9);
    }

    #[test]
    fn counts_and_samples_are_clamped() {
        let cfg = GbmConfig {
            spot: 50.0,
            drift: 0.0,
            vol: 0.3,
            horizon_years: 0.5,
            n_steps: 0,          // clamped up to 1
            n_paths: 0,          // clamped up to 1
            seed: 7,
            sample_paths: 999,   // clamped down to n_paths
            antithetic: false,
        };
        let sim = simulate_gbm(&cfg).unwrap();
        assert_eq!(sim.cone.len(), 2); // steps 0 and 1
        assert_eq!(sim.sample_paths.len(), 1);
        assert_eq!(sim.sample_paths[0].len(), 2);
    }

    #[test]
    fn rejects_bad_domain() {
        let mut cfg = base(100, false);
        cfg.spot = 0.0;
        assert!(simulate_gbm(&cfg).is_err());
        let mut cfg = base(100, false);
        cfg.horizon_years = 0.0;
        assert!(simulate_gbm(&cfg).is_err());
        let mut cfg = base(100, false);
        cfg.vol = -0.1;
        assert!(simulate_gbm(&cfg).is_err());
    }
}
