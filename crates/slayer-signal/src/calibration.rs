//! Probability calibration — Wilson score intervals, isotonic (PAV)
//! recalibration, and ECE / Brier calibration diagnostics.
//!
//! Given a raw win probability `p_hat` and a labeled outcome history of
//! `(pred, win)` pairs, this module answers two questions the terminal asks of
//! every forecast: *how tight is the estimate?* (Wilson interval) and *is the
//! model's stated probability honest against its own track record?* (isotonic
//! recalibration + ECE / Brier). Everything here is a pure function of its
//! arguments; there is no clock, no store, no fabricated fallback.
//!
//! # Provenance
//!
//! Legacy `calculateWilsonConfidence` / `calibrateIsotonicLoss` /
//! `calculateECE` / `calculateBrierScore` (`v11Math.ts`), spec
//! `docs/spec/01-skyvision-v11.md` E10. The named constants
//! ([`WILSON_Z_95`], [`CALIBRATION_MIN_SAMPLES`], [`CALIBRATION_BINS`],
//! [`DEFAULT_ECE`], [`DEFAULT_BRIER`]) are the E10 constants table verbatim.
//!
//! # Deviations from legacy
//!
//! - **D11** — the legacy empty-history returns (`0.05` ECE, `0.15` Brier) were
//!   emitted as if measured. Here they are the named constants [`DEFAULT_ECE`]
//!   and [`DEFAULT_BRIER`], documented as *declared priors*: the value a
//!   diagnostic takes when there is no history to measure, never presented as a
//!   live calibration statistic.
//! - **D17** — the legacy PAV curve was fit on `history.pred` but queried at a
//!   score (`systemScore.total / 100`) that nothing guaranteed shared the same
//!   scale. Here `p_hat` and every `history.pred` are explicit parameters on
//!   one documented probability scale (`[0, 1]`); the module never silently
//!   rescales one onto the other — keeping the two consistent is the caller's
//!   contract.
//!
//! # Cold start
//!
//! Below [`CALIBRATION_MIN_SAMPLES`] labeled outcomes the isotonic map is not
//! identifiable, so [`calibrate_isotonic`] returns the raw estimate `p_hat`
//! **unchanged**. This is a documented pass-through, not a fabricated
//! correction: the caller learns that nothing was adjusted rather than being
//! handed an invented adjustment.

use serde::{Deserialize, Serialize};

/// `z` multiplier for a 95% two-sided confidence interval. Spec E10
/// `WILSON_Z_95`.
pub const WILSON_Z_95: f64 = 1.96;

/// Minimum labeled outcomes before the isotonic map is fit; below this the
/// calibration is a documented pass-through. Spec E10 `CALIBRATION_MIN_SAMPLES`.
pub const CALIBRATION_MIN_SAMPLES: usize = 200;

/// Number of equal-width probability bins for the isotonic fit, ECE, and the
/// reliability partition. Spec E10 `CALIBRATION_BINS`.
pub const CALIBRATION_BINS: usize = 10;

/// Declared prior returned by [`ece`] when there is no history to measure — a
/// documented default, **not** a fabricated live statistic (D11). Spec E10
/// `DEFAULT_ECE`.
pub const DEFAULT_ECE: f64 = 0.05;

/// Declared prior returned by [`brier_score`] when there is no history to
/// measure — a documented default, **not** a fabricated live statistic (D11).
/// Spec E10 `DEFAULT_BRIER`.
pub const DEFAULT_BRIER: f64 = 0.15;

/// A Wilson score interval, both endpoints clamped to `[0, 1]`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WilsonInterval {
    /// Lower endpoint of the interval, in `[0, 1]`.
    pub lo: f64,
    /// Upper endpoint of the interval, in `[0, 1]`.
    pub hi: f64,
}

/// Calibration diagnostics plus the recalibrated probability for one forecast.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CalibrationReport {
    /// Expected Calibration Error over [`CALIBRATION_BINS`] bins, or
    /// [`DEFAULT_ECE`] on empty history.
    pub ece: f64,
    /// Brier score (mean squared error of `pred` vs `win`), or
    /// [`DEFAULT_BRIER`] on empty history.
    pub brier: f64,
    /// The input `p_hat` mapped through the isotonic calibration curve (or
    /// returned unchanged in the cold-start regime).
    pub calibrated_p: f64,
}

/// Clamp to the unit interval. A non-finite input clamps to `0.0`: an
/// unmeasurable probability is not in force.
fn clamp01(v: f64) -> f64 {
    if v.is_nan() { 0.0 } else { v.clamp(0.0, 1.0) }
}

/// Wilson score interval for an observed proportion `p` from `n` trials at the
/// 95% level ([`WILSON_Z_95`]).
///
/// With `n == 0` the interval is `[0, 0]` (no evidence). `p` is clamped into
/// `[0, 1]` before use, so the variance term `p(1 − p)` can never go negative.
/// The interval always brackets the observed `p` and widens as `n` shrinks.
#[must_use]
pub fn wilson_interval(p: f64, n: u64) -> WilsonInterval {
    if n == 0 {
        return WilsonInterval { lo: 0.0, hi: 0.0 };
    }
    let p = clamp01(p);
    let z = WILSON_Z_95;
    let z2 = z * z;
    let nf = n as f64;
    // Shared `1 + z²/n` denominator from completing the square.
    let denom = 1.0 + z2 / nf;
    let center = (p + z2 / (2.0 * nf)) / denom;
    let half = z * (p * (1.0 - p) / nf + z2 / (4.0 * nf * nf)).sqrt() / denom;
    WilsonInterval {
        lo: clamp01(center - half),
        hi: clamp01(center + half),
    }
}

/// One populated reliability bin: the mean predicted probability, the realized
/// win rate, and the sample count. Shared by ECE and the isotonic fit.
struct Bin {
    /// Mean predicted probability of the samples that fell in this bin.
    mean_pred: f64,
    /// Realized win rate (mean outcome) of the samples in this bin.
    mean_win: f64,
    /// Number of samples in this bin.
    count: f64,
}

/// Partition `history` into [`CALIBRATION_BINS`] equal-width probability bins by
/// `pred`, dropping any non-finite `(pred, win)` pair, and return the populated
/// bins in ascending prediction order together with the finite-sample total.
///
/// Dropping non-finite entries is exclusion of malformed data, never
/// fabrication — a `NaN` prediction cannot be a labeled outcome.
fn bin_history(history: &[(f64, f64)]) -> (Vec<Bin>, f64) {
    let bins = CALIBRATION_BINS;
    let mut sum_pred = vec![0.0_f64; bins];
    let mut sum_win = vec![0.0_f64; bins];
    let mut count = vec![0.0_f64; bins];
    let mut total = 0.0_f64;
    for &(pred, win) in history {
        if !pred.is_finite() || !win.is_finite() {
            continue;
        }
        // bin = min(BINS − 1, floor(pred·BINS)); a pred outside [0,1] is
        // pinned to the edge bin rather than indexing out of range.
        let idx = (pred * bins as f64).floor().clamp(0.0, (bins - 1) as f64) as usize;
        sum_pred[idx] += pred;
        sum_win[idx] += win;
        count[idx] += 1.0;
        total += 1.0;
    }
    let populated = (0..bins)
        .filter(|&b| count[b] > 0.0)
        .map(|b| Bin {
            mean_pred: sum_pred[b] / count[b],
            mean_win: sum_win[b] / count[b],
            count: count[b],
        })
        .collect();
    (populated, total)
}

/// A pooled isotonic node: prediction abscissa, calibrated value, and the total
/// weight (sample count) it aggregates.
struct Node {
    /// Pooled mean prediction (interpolation abscissa).
    x: f64,
    /// Pooled calibrated probability (monotone non-decreasing across nodes).
    value: f64,
    /// Total sample weight pooled into this node.
    weight: f64,
}

/// Map `p_hat` through an isotonic (pool-adjacent-violators) calibration curve
/// fit on `history`.
///
/// Cold start: with fewer than [`CALIBRATION_MIN_SAMPLES`] entries the curve is
/// not identifiable and `p_hat` is returned unchanged (documented — not a
/// fabricated correction). Otherwise the history is binned into
/// [`CALIBRATION_BINS`] reliability bins, the bin win-rates are pooled into a
/// monotone non-decreasing step by PAV, and `p_hat` is linearly interpolated
/// between the pooled nodes (flat outside their range). The output is therefore
/// monotone non-decreasing in `p_hat`.
///
/// A non-finite `p_hat`, or a history whose finite entries populate no bin, is
/// also returned unchanged.
#[must_use]
pub fn calibrate_isotonic(p_hat: f64, history: &[(f64, f64)]) -> f64 {
    if history.len() < CALIBRATION_MIN_SAMPLES {
        return p_hat;
    }
    let (bins, _total) = bin_history(history);
    if bins.is_empty() || !p_hat.is_finite() {
        return p_hat;
    }

    let mut nodes: Vec<Node> = bins
        .iter()
        .map(|b| Node {
            x: b.mean_pred,
            value: b.mean_win,
            weight: b.count,
        })
        .collect();

    // Pool-adjacent-violators: whenever an adjacent pair violates monotonicity,
    // replace it with its weight-average and restart the scan. Terminates with
    // node values monotone non-decreasing; abscissae stay ascending because a
    // pooled `x` lies between the two it merges.
    let mut scanning = true;
    while scanning {
        scanning = false;
        for i in 0..nodes.len().saturating_sub(1) {
            if nodes[i].value > nodes[i + 1].value {
                let w = nodes[i].weight + nodes[i + 1].weight;
                let x = (nodes[i].x * nodes[i].weight + nodes[i + 1].x * nodes[i + 1].weight) / w;
                let value = (nodes[i].value * nodes[i].weight
                    + nodes[i + 1].value * nodes[i + 1].weight)
                    / w;
                nodes[i] = Node {
                    x,
                    value,
                    weight: w,
                };
                nodes.remove(i + 1);
                scanning = true;
                break;
            }
        }
    }

    let last = nodes.len() - 1;
    if p_hat <= nodes[0].x {
        return nodes[0].value;
    }
    if p_hat >= nodes[last].x {
        return nodes[last].value;
    }
    // Locate the bracketing segment: nodes[i].x < p_hat <= nodes[i+1].x.
    let mut i = 0;
    while i + 1 < nodes.len() && nodes[i + 1].x < p_hat {
        i += 1;
    }
    let span = nodes[i + 1].x - nodes[i].x;
    let t = if span > 0.0 {
        (p_hat - nodes[i].x) / span
    } else {
        0.0
    };
    nodes[i].value + t * (nodes[i + 1].value - nodes[i].value)
}

/// Expected Calibration Error over [`CALIBRATION_BINS`] bins:
/// `Σ_b (count_b / N)·|mean_win_b − mean_pred_b|`.
///
/// Empty history (or history with no finite entries) returns the declared prior
/// [`DEFAULT_ECE`].
#[must_use]
pub fn ece(history: &[(f64, f64)]) -> f64 {
    if history.is_empty() {
        return DEFAULT_ECE;
    }
    let (bins, total) = bin_history(history);
    if bins.is_empty() {
        return DEFAULT_ECE;
    }
    bins.iter()
        .map(|b| (b.count / total) * (b.mean_win - b.mean_pred).abs())
        .sum()
}

/// Brier score: `mean((pred − win)²)` over the finite entries of `history`.
///
/// Empty history (or history with no finite entries) returns the declared prior
/// [`DEFAULT_BRIER`].
#[must_use]
pub fn brier_score(history: &[(f64, f64)]) -> f64 {
    if history.is_empty() {
        return DEFAULT_BRIER;
    }
    let mut sum_sq = 0.0_f64;
    let mut n = 0_u64;
    for &(pred, win) in history {
        if !pred.is_finite() || !win.is_finite() {
            continue;
        }
        let d = pred - win;
        sum_sq += d * d;
        n += 1;
    }
    if n == 0 {
        DEFAULT_BRIER
    } else {
        sum_sq / n as f64
    }
}

/// Build the full [`CalibrationReport`] for one forecast: ECE, Brier, and the
/// isotonically recalibrated probability for `p_hat`.
#[must_use]
pub fn calibration_report(p_hat: f64, history: &[(f64, f64)]) -> CalibrationReport {
    CalibrationReport {
        ece: ece(history),
        brier: brier_score(history),
        calibrated_p: calibrate_isotonic(p_hat, history),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Perfectly-calibrated history: each of the 10 bins holds `per_bin`
    /// samples all predicting the bin centre, with a win rate exactly equal to
    /// that centre. The isotonic map is the identity and ECE is zero.
    fn perfect_history() -> Vec<(f64, f64)> {
        let per_bin = 100u32;
        let mut h = Vec::new();
        for k in 0..CALIBRATION_BINS {
            let center = (k as f64 + 0.5) / CALIBRATION_BINS as f64; // 0.05..0.95
            let wins = (center * f64::from(per_bin)).round() as u32;
            for j in 0..per_bin {
                h.push((center, if j < wins { 1.0 } else { 0.0 }));
            }
        }
        h
    }

    /// History whose raw bin win-rates are deliberately non-monotone, to
    /// exercise PAV pooling. 500 samples total (above the cold-start floor).
    fn noisy_history() -> Vec<(f64, f64)> {
        let rates = [0.02, 0.30, 0.10, 0.40, 0.35, 0.55, 0.50, 0.70, 0.85, 0.92];
        let per_bin = 50u32;
        let mut h = Vec::new();
        for (k, &rate) in rates.iter().enumerate() {
            let center = (k as f64 + 0.5) / CALIBRATION_BINS as f64;
            let wins = (rate * f64::from(per_bin)).round() as u32;
            for j in 0..per_bin {
                h.push((center, if j < wins { 1.0 } else { 0.0 }));
            }
        }
        h
    }

    #[test]
    fn wilson_zero_n_is_degenerate() {
        let w = wilson_interval(0.5, 0);
        assert!(w.lo.abs() < 1e-15 && w.hi.abs() < 1e-15);
    }

    #[test]
    fn wilson_brackets_the_observed_proportion() {
        for &(p, n) in &[
            (0.6_f64, 50u64),
            (0.2, 10),
            (0.9, 200),
            (0.5, 5),
            (0.05, 40),
        ] {
            let w = wilson_interval(p, n);
            assert!(
                w.lo <= p + 1e-12 && p <= w.hi + 1e-12,
                "p={p} n={n} interval=[{},{}]",
                w.lo,
                w.hi
            );
            assert!(w.lo >= 0.0 && w.hi <= 1.0 && w.lo <= w.hi);
        }
    }

    #[test]
    fn wilson_widens_as_n_shrinks() {
        let wide = wilson_interval(0.5, 10);
        let narrow = wilson_interval(0.5, 1000);
        assert!((wide.hi - wide.lo) > (narrow.hi - narrow.lo));
    }

    #[test]
    fn wilson_is_symmetric_about_a_half() {
        // For p = 0.5 the centre is exactly 0.5, so lo + hi == 1.
        let w = wilson_interval(0.5, 100);
        assert!((w.lo + w.hi - 1.0).abs() < 1e-12);
    }

    #[test]
    fn wilson_clamps_out_of_range_p() {
        let w = wilson_interval(1.5, 100);
        assert!(w.lo >= 0.0 && w.hi <= 1.0 && w.lo <= w.hi);
    }

    #[test]
    fn cold_start_returns_p_hat_unchanged() {
        let h = vec![(0.6, 1.0); CALIBRATION_MIN_SAMPLES - 1];
        assert!((calibrate_isotonic(0.42, &h) - 0.42).abs() < 1e-15);
    }

    #[test]
    fn perfect_calibration_is_the_identity_map() {
        let h = perfect_history();
        for &p in &[0.1_f64, 0.25, 0.5, 0.73, 0.9] {
            let c = calibrate_isotonic(p, &h);
            assert!((c - p).abs() < 1e-9, "p={p} calibrated={c}");
        }
    }

    #[test]
    fn perfect_calibration_has_zero_ece() {
        assert!(ece(&perfect_history()).abs() < 1e-9);
    }

    #[test]
    fn pav_output_is_monotone_non_decreasing_in_p_hat() {
        let h = noisy_history();
        let mut prev = f64::NEG_INFINITY;
        let mut p = 0.0_f64;
        while p <= 1.0 {
            let c = calibrate_isotonic(p, &h);
            assert!(c >= prev - 1e-12, "not monotone at p={p}: {c} < {prev}");
            prev = c;
            p += 0.01;
        }
    }

    #[test]
    fn ece_flags_a_confidently_wrong_model() {
        // Always predicts 0.9, never wins: bin-9 gap is |0.0 − 0.9| at weight 1.
        let h = vec![(0.9, 0.0); 300];
        assert!((ece(&h) - 0.9).abs() < 1e-9);
    }

    #[test]
    fn ece_empty_history_returns_the_declared_prior() {
        assert!((ece(&[]) - DEFAULT_ECE).abs() < 1e-15);
    }

    #[test]
    fn brier_empty_history_returns_the_declared_prior() {
        assert!((brier_score(&[]) - DEFAULT_BRIER).abs() < 1e-15);
    }

    #[test]
    fn brier_is_zero_for_deterministic_correct_calls() {
        let h = vec![(0.0, 0.0), (1.0, 1.0)];
        assert!(brier_score(&h).abs() < 1e-15);
    }

    #[test]
    fn brier_squares_the_residual() {
        let h = vec![(0.3, 1.0)];
        assert!((brier_score(&h) - 0.49).abs() < 1e-12);
    }

    #[test]
    fn non_finite_history_entries_are_dropped_not_propagated() {
        let mut h = perfect_history();
        h.push((f64::NAN, 1.0));
        h.push((0.5, f64::INFINITY));
        assert!(calibrate_isotonic(0.5, &h).is_finite());
        assert!(ece(&h).is_finite());
        assert!(brier_score(&h).is_finite());
    }

    #[test]
    fn report_bundles_diagnostics_and_calibrated_probability() {
        let r = calibration_report(0.4, &perfect_history());
        assert!(r.ece.abs() < 1e-9);
        assert!((r.calibrated_p - 0.4).abs() < 1e-9);
        // Binary outcomes leave irreducible Brier variance even when calibrated.
        assert!(r.brier > 0.0);
    }

    #[test]
    fn structs_serde_round_trip() {
        let w = wilson_interval(0.6, 100);
        let wb: WilsonInterval = serde_json::from_str(&serde_json::to_string(&w).unwrap()).unwrap();
        assert!((w.lo - wb.lo).abs() < 1e-12 && (w.hi - wb.hi).abs() < 1e-12);

        let r = calibration_report(0.5, &perfect_history());
        let rb: CalibrationReport =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert!((r.ece - rb.ece).abs() < 1e-9);
        assert!((r.brier - rb.brier).abs() < 1e-12);
        assert!((r.calibrated_p - rb.calibrated_p).abs() < 1e-9);
    }
}
