//! GBM first-passage and exercise probabilities.
//!
//! Kernel math shared by the decision brain (barrier-touch targets, spec 01
//! E24) and the 0DTE engine (spec 03 E6): the reflection-principle
//! probability that geometric Brownian motion touches a barrier before a
//! horizon, and the risk-neutral probability of finishing in the money.
//!
//! Pure, deterministic, no I/O; all inputs are parameters.

use crate::dist::norm_cdf;

/// Probability that a GBM with log-drift `nu = r − q − σ²/2` touches
/// `barrier` before `tau` years elapse, monitored continuously (reflection
/// principle). Degenerate inputs (`spot ≤ 0`, `barrier ≤ 0`, `tau ≤ 0`,
/// `sigma ≤ 0`) yield 0; a barrier exactly at spot yields 1. Pass `nu = 0`
/// for the driftless form `2·Φ(−|ln(K/S)|/σ√τ)`.
#[must_use]
pub fn gbm_touch_probability(spot: f64, barrier: f64, sigma: f64, tau: f64, nu: f64) -> f64 {
    if tau <= 0.0 || sigma <= 0.0 || spot <= 0.0 || barrier <= 0.0 {
        return 0.0;
    }
    let x = (barrier / spot).ln();
    if x == 0.0 {
        return 1.0;
    }
    let srt = sigma * tau.sqrt();
    let exp_term = (2.0 * nu * x / (sigma * sigma)).exp();
    let p = if x > 0.0 {
        norm_cdf((-x + nu * tau) / srt) + exp_term * norm_cdf((-x - nu * tau) / srt)
    } else {
        norm_cdf((x - nu * tau) / srt) + exp_term * norm_cdf((x + nu * tau) / srt)
    };
    p.clamp(0.0, 1.0)
}

/// Risk-neutral probability that the underlying finishes in the money at
/// expiry: `Φ(d2)` for a call, `Φ(−d2)` for a put. Degenerate inputs fall
/// back to the intrinsic indicator (already-ITM ⇒ 1, else 0), per spec 03
/// E6.1.
#[must_use]
pub fn prob_expire_itm(
    spot: f64,
    strike: f64,
    tau: f64,
    iv: f64,
    is_call: bool,
    rate: f64,
    div_yield: f64,
) -> f64 {
    if spot <= 0.0 || strike <= 0.0 || tau <= 0.0 || iv <= 0.0 {
        let itm = if is_call {
            spot > strike
        } else {
            spot < strike
        };
        return if itm { 1.0 } else { 0.0 };
    }
    let d2 = ((spot / strike).ln() + (rate - div_yield - 0.5 * iv * iv) * tau) / (iv * tau.sqrt());
    if is_call { norm_cdf(d2) } else { norm_cdf(-d2) }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn touch_probability_properties() {
        // At-spot barrier is a certain touch; degenerate inputs are zero.
        assert_eq!(gbm_touch_probability(100.0, 100.0, 0.2, 0.5, 0.0), 1.0);
        assert_eq!(gbm_touch_probability(100.0, 110.0, 0.0, 0.5, 0.0), 0.0);
        assert_eq!(gbm_touch_probability(100.0, 110.0, 0.2, 0.0, 0.0), 0.0);
        // Driftless closed form 2·Φ(−|x|/σ√τ).
        let x: f64 = (110.0_f64 / 100.0_f64).ln();
        let closed = 2.0 * norm_cdf(-x.abs() / (0.2 * 0.25_f64.sqrt()));
        let p = gbm_touch_probability(100.0, 110.0, 0.2, 0.25, 0.0);
        assert!((p - closed).abs() < 1e-12);
        // Touch dominates finish-beyond: P(touch K) ≥ P(S_T beyond K).
        let finish = prob_expire_itm(100.0, 110.0, 0.25, 0.2, true, 0.0, 0.0);
        assert!(p >= finish);
        // Positive drift raises the probability of touching an upper barrier.
        let up_drift = gbm_touch_probability(100.0, 110.0, 0.2, 0.25, 0.08);
        assert!(up_drift > p);
    }

    #[test]
    fn itm_probability_properties() {
        // ATM driftless ≈ 1/2 (exactly Φ(−σ√τ/2)).
        let atm = prob_expire_itm(100.0, 100.0, 0.25, 0.2, true, 0.0, 0.0);
        assert!((atm - norm_cdf(-0.05)).abs() < 1e-12);
        // Call + put ITM probabilities partition (continuous distribution).
        let call = prob_expire_itm(100.0, 105.0, 0.25, 0.2, true, 0.05, 0.0);
        let put = prob_expire_itm(100.0, 105.0, 0.25, 0.2, false, 0.05, 0.0);
        assert!((call + put - 1.0).abs() < 1e-12);
        // Degenerate: intrinsic indicator.
        assert_eq!(prob_expire_itm(100.0, 90.0, 0.0, 0.2, true, 0.05, 0.0), 1.0);
        assert_eq!(
            prob_expire_itm(100.0, 110.0, 0.0, 0.2, true, 0.05, 0.0),
            0.0
        );
    }
}
