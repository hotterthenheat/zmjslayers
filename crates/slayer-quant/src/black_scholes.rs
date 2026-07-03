//! Black–Scholes–Merton pricing, greeks, and implied volatility.
//!
//! Kernel units are *natural*: time in years, rates and vols as annualized
//! decimals, theta per year, vega per unit vol. Display conversions
//! (theta/day, vega per vol point) are the caller's concern — see
//! [`THETA_PER_DAY`] and [`VEGA_PER_VOL_POINT`].

use crate::dist::{norm_cdf, norm_pdf};
use crate::error::QuantError;
use slayer_core::OptionRight;

/// Divide annual theta by this to quote per calendar day.
pub const THETA_PER_DAY: f64 = 365.0;

/// Divide unit-vol vega by this to quote per 1 vol point (1%).
pub const VEGA_PER_VOL_POINT: f64 = 100.0;

/// Contract inputs to the BSM model. Construction validates domain: spot,
/// strike positive; time and vol non-negative and finite.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BsInputs {
    /// Underlying spot, points.
    pub spot: f64,
    /// Strike, points.
    pub strike: f64,
    /// Time to expiry, years (ACT/365 fraction chosen by caller).
    pub t_years: f64,
    /// Annualized volatility, decimal.
    pub vol: f64,
    /// Continuously-compounded risk-free rate, decimal.
    pub rate: f64,
    /// Continuous dividend yield, decimal.
    pub div_yield: f64,
}

impl BsInputs {
    /// Validate the pricing domain.
    pub fn validate(&self) -> Result<(), QuantError> {
        let ok = self.spot > 0.0
            && self.strike > 0.0
            && self.t_years >= 0.0
            && self.vol >= 0.0
            && [
                self.spot,
                self.strike,
                self.t_years,
                self.vol,
                self.rate,
                self.div_yield,
            ]
            .iter()
            .all(|v| v.is_finite());
        if ok {
            Ok(())
        } else {
            Err(QuantError::Domain("BsInputs out of domain"))
        }
    }

    fn d1_d2(&self) -> (f64, f64) {
        let sqrt_t = self.t_years.sqrt();
        let denom = self.vol * sqrt_t;
        let d1 = ((self.spot / self.strike).ln()
            + (self.rate - self.div_yield + 0.5 * self.vol * self.vol) * self.t_years)
            / denom;
        (d1, d1 - denom)
    }

    /// Deterministic payoff at expiry (or vol = 0), discounted.
    fn deterministic_value(&self, right: OptionRight) -> f64 {
        let fwd = self.spot * ((self.rate - self.div_yield) * self.t_years).exp();
        let df = (-self.rate * self.t_years).exp();
        let intrinsic = match right {
            OptionRight::Call => (fwd - self.strike).max(0.0),
            OptionRight::Put => (self.strike - fwd).max(0.0),
        };
        df * intrinsic
    }
}

/// Full greek set for one contract, natural units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BsGreeks {
    /// ∂V/∂S.
    pub delta: f64,
    /// ∂²V/∂S².
    pub gamma: f64,
    /// ∂V/∂t, per year.
    pub theta: f64,
    /// ∂V/∂σ, per unit vol.
    pub vega: f64,
    /// ∂²V/∂S∂σ, per unit vol (dealer-flow engines consume this).
    pub vanna: f64,
    /// ∂Δ/∂t, per year (charm; dealer-flow engines consume this).
    pub charm: f64,
}

/// BSM price of a European option.
pub fn price(inputs: &BsInputs, right: OptionRight) -> Result<f64, QuantError> {
    inputs.validate()?;
    if inputs.t_years == 0.0 || inputs.vol == 0.0 {
        return Ok(inputs.deterministic_value(right));
    }
    let (d1, d2) = inputs.d1_d2();
    let disc_r = (-inputs.rate * inputs.t_years).exp();
    let disc_q = (-inputs.div_yield * inputs.t_years).exp();
    let v = match right {
        OptionRight::Call => {
            inputs.spot * disc_q * norm_cdf(d1) - inputs.strike * disc_r * norm_cdf(d2)
        }
        OptionRight::Put => {
            inputs.strike * disc_r * norm_cdf(-d2) - inputs.spot * disc_q * norm_cdf(-d1)
        }
    };
    Ok(v)
}

/// Full BSM greeks. Degenerate inputs (expired / zero vol) return the greeks
/// of the deterministic payoff: zero except delta at ±discount.
pub fn greeks(inputs: &BsInputs, right: OptionRight) -> Result<BsGreeks, QuantError> {
    inputs.validate()?;
    if inputs.t_years == 0.0 || inputs.vol == 0.0 {
        let fwd = inputs.spot * ((inputs.rate - inputs.div_yield) * inputs.t_years).exp();
        let disc_q = (-inputs.div_yield * inputs.t_years).exp();
        let itm = match right {
            OptionRight::Call => fwd > inputs.strike,
            OptionRight::Put => fwd < inputs.strike,
        };
        let sign = match right {
            OptionRight::Call => 1.0,
            OptionRight::Put => -1.0,
        };
        let delta = if itm { sign * disc_q } else { 0.0 };
        return Ok(BsGreeks {
            delta,
            gamma: 0.0,
            theta: 0.0,
            vega: 0.0,
            vanna: 0.0,
            charm: 0.0,
        });
    }

    let (d1, d2) = inputs.d1_d2();
    let sqrt_t = inputs.t_years.sqrt();
    let disc_r = (-inputs.rate * inputs.t_years).exp();
    let disc_q = (-inputs.div_yield * inputs.t_years).exp();
    let pdf_d1 = norm_pdf(d1);

    let delta = match right {
        OptionRight::Call => disc_q * norm_cdf(d1),
        OptionRight::Put => -disc_q * norm_cdf(-d1),
    };
    let gamma = disc_q * pdf_d1 / (inputs.spot * inputs.vol * sqrt_t);
    let vega = inputs.spot * disc_q * pdf_d1 * sqrt_t;

    let common_theta = -inputs.spot * disc_q * pdf_d1 * inputs.vol / (2.0 * sqrt_t);
    let theta = match right {
        OptionRight::Call => {
            common_theta - inputs.rate * inputs.strike * disc_r * norm_cdf(d2)
                + inputs.div_yield * inputs.spot * disc_q * norm_cdf(d1)
        }
        OptionRight::Put => {
            common_theta + inputs.rate * inputs.strike * disc_r * norm_cdf(-d2)
                - inputs.div_yield * inputs.spot * disc_q * norm_cdf(-d1)
        }
    };

    // Vanna: ∂²V/∂S∂σ = -e^{-qT} φ(d1) d2 / σ
    let vanna = -disc_q * pdf_d1 * d2 / inputs.vol;

    // Charm (call): -e^{-qT}[ φ(d1)(2(r-q)T - d2 σ √T)/(2T σ √T) - q Φ(d1) ]
    // Put charm adds +q e^{-qT} (Φ(d1) shifts to -Φ(-d1)).
    let drift_term = pdf_d1
        * (2.0 * (inputs.rate - inputs.div_yield) * inputs.t_years - d2 * inputs.vol * sqrt_t)
        / (2.0 * inputs.t_years * inputs.vol * sqrt_t);
    let charm = match right {
        OptionRight::Call => disc_q * (inputs.div_yield * norm_cdf(d1) - drift_term),
        OptionRight::Put => disc_q * (-inputs.div_yield * norm_cdf(-d1) - drift_term),
    };

    Ok(BsGreeks {
        delta,
        gamma,
        theta,
        vega,
        vanna,
        charm,
    })
}

/// Max Newton iterations before falling back to bisection.
const IV_NEWTON_MAX_ITER: u32 = 32;
/// Absolute price tolerance for implied-vol convergence, points.
const IV_PRICE_TOL: f64 = 1e-10;
/// Bisection search bounds for annualized vol, decimals.
const IV_LO: f64 = 1e-4;
/// Upper bisection bound: 500% vol. Anything beyond is not a market.
const IV_HI: f64 = 5.0;
/// Max bisection iterations (brackets to ~4e-19 width — converges on tol first).
const IV_BISECT_MAX_ITER: u32 = 128;
/// Vega floor below which a Newton step is meaningless.
const IV_VEGA_FLOOR: f64 = 1e-12;

/// Implied volatility from a market price. Newton–Raphson seeded with the
/// Brenner–Subrahmanyam approximation, with a guaranteed bisection fallback
/// on [`IV_LO`], [`IV_HI`].
pub fn implied_vol(
    market_price: f64,
    inputs_at_zero_vol: &BsInputs,
    right: OptionRight,
) -> Result<f64, QuantError> {
    let base = *inputs_at_zero_vol;
    base.validate()?;
    if base.t_years <= 0.0 {
        return Err(QuantError::Domain("implied vol undefined at expiry"));
    }
    if !market_price.is_finite() || market_price < 0.0 {
        return Err(QuantError::Domain(
            "market price must be finite and non-negative",
        ));
    }
    let price_at =
        |vol: f64| -> Result<f64, QuantError> { price(&BsInputs { vol, ..base }, right) };
    // No-arbitrage bracket check.
    let lo_p = price_at(IV_LO)?;
    let hi_p = price_at(IV_HI)?;
    if market_price < lo_p - IV_PRICE_TOL || market_price > hi_p + IV_PRICE_TOL {
        return Err(QuantError::NoSolution(
            "price outside no-arbitrage vol bracket",
        ));
    }

    // Brenner–Subrahmanyam seed: σ ≈ √(2π/T) · P / S, clamped into bracket.
    let seed = ((std::f64::consts::TAU / base.t_years).sqrt() * market_price / base.spot)
        .clamp(IV_LO, IV_HI);

    let mut vol = seed;
    for _ in 0..IV_NEWTON_MAX_ITER {
        let p = price_at(vol)?;
        let diff = p - market_price;
        if diff.abs() < IV_PRICE_TOL {
            return Ok(vol);
        }
        let g = greeks(&BsInputs { vol, ..base }, right)?;
        if g.vega < IV_VEGA_FLOOR {
            break;
        }
        let next = vol - diff / g.vega;
        if !(IV_LO..=IV_HI).contains(&next) {
            break;
        }
        vol = next;
    }

    // Bisection fallback: price is monotone increasing in vol.
    let (mut lo, mut hi) = (IV_LO, IV_HI);
    for _ in 0..IV_BISECT_MAX_ITER {
        let mid = 0.5 * (lo + hi);
        let p = price_at(mid)?;
        if (p - market_price).abs() < IV_PRICE_TOL {
            return Ok(mid);
        }
        if p < market_price {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Ok(0.5 * (lo + hi))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const ATM: BsInputs = BsInputs {
        spot: 100.0,
        strike: 100.0,
        t_years: 1.0,
        vol: 0.2,
        rate: 0.05,
        div_yield: 0.0,
    };

    #[test]
    fn call_and_put_match_scipy_oracle() {
        let c = price(&ATM, OptionRight::Call).unwrap();
        let p = price(&ATM, OptionRight::Put).unwrap();
        assert!((c - 10.450_583_572_185_565).abs() < 1e-12);
        assert!((p - 5.573_526_022_256_971).abs() < 1e-12);
    }

    #[test]
    fn put_call_parity_holds_off_atm() {
        let inputs = BsInputs {
            strike: 87.5,
            vol: 0.34,
            div_yield: 0.012,
            ..ATM
        };
        let c = price(&inputs, OptionRight::Call).unwrap();
        let p = price(&inputs, OptionRight::Put).unwrap();
        let parity = inputs.spot * (-inputs.div_yield * inputs.t_years).exp()
            - inputs.strike * (-inputs.rate * inputs.t_years).exp();
        assert!((c - p - parity).abs() < 1e-12);
    }

    #[test]
    fn greeks_match_scipy_oracle() {
        let g = greeks(&ATM, OptionRight::Call).unwrap();
        assert!((g.delta - 0.636_830_651_175_619_1).abs() < 1e-12);
        assert!((g.gamma - 0.018_762_017_345_846_895).abs() < 1e-12);
        assert!((g.vega - 37.524_034_691_693_79).abs() < 1e-11);
        assert!((g.theta - (-6.414_027_546_438_197)).abs() < 1e-11);
    }

    #[test]
    fn greeks_match_finite_differences() {
        let bump_s = 1e-4;
        let bump_v = 1e-6;
        let bump_t = 1e-7;
        for right in [OptionRight::Call, OptionRight::Put] {
            let g = greeks(&ATM, right).unwrap();
            let up = price(
                &BsInputs {
                    spot: ATM.spot + bump_s,
                    ..ATM
                },
                right,
            )
            .unwrap();
            let dn = price(
                &BsInputs {
                    spot: ATM.spot - bump_s,
                    ..ATM
                },
                right,
            )
            .unwrap();
            let mid = price(&ATM, right).unwrap();
            assert!((g.delta - (up - dn) / (2.0 * bump_s)).abs() < 1e-6);
            assert!((g.gamma - (up - 2.0 * mid + dn) / (bump_s * bump_s)).abs() < 1e-5);

            let vu = price(
                &BsInputs {
                    vol: ATM.vol + bump_v,
                    ..ATM
                },
                right,
            )
            .unwrap();
            let vd = price(
                &BsInputs {
                    vol: ATM.vol - bump_v,
                    ..ATM
                },
                right,
            )
            .unwrap();
            assert!((g.vega - (vu - vd) / (2.0 * bump_v)).abs() < 1e-5);

            // Theta: ∂V/∂t with t = time-to-expiry ⇒ price(T - dt) ≈ price(T) + θ·dt
            let tu = price(
                &BsInputs {
                    t_years: ATM.t_years - bump_t,
                    ..ATM
                },
                right,
            )
            .unwrap();
            assert!((g.theta - (tu - mid) / bump_t).abs() < 1e-4);

            // Vanna: ∂delta/∂σ
            let gu = greeks(
                &BsInputs {
                    vol: ATM.vol + bump_v,
                    ..ATM
                },
                right,
            )
            .unwrap();
            let gd = greeks(
                &BsInputs {
                    vol: ATM.vol - bump_v,
                    ..ATM
                },
                right,
            )
            .unwrap();
            assert!((g.vanna - (gu.delta - gd.delta) / (2.0 * bump_v)).abs() < 1e-5);

            // Charm: ∂delta/∂t with t = time-to-expiry
            let gt = greeks(
                &BsInputs {
                    t_years: ATM.t_years - bump_t,
                    ..ATM
                },
                right,
            )
            .unwrap();
            assert!(
                (g.charm - (gt.delta - g.delta) / bump_t).abs() < 1e-4,
                "charm {right:?}"
            );
        }
    }

    #[test]
    fn implied_vol_roundtrips_across_surface() {
        for strike in [60.0, 85.0, 100.0, 120.0, 160.0] {
            for vol in [0.08, 0.2, 0.55, 1.4] {
                for t in [0.02, 0.25, 1.0, 2.0] {
                    for right in [OptionRight::Call, OptionRight::Put] {
                        let inputs = BsInputs {
                            strike,
                            vol,
                            t_years: t,
                            ..ATM
                        };
                        let p = price(&inputs, right).unwrap();
                        let intrinsic = price(&BsInputs { vol: 0.0, ..inputs }, right).unwrap();
                        if p - intrinsic < 1e-6 {
                            // Negligible extrinsic value ⇒ vega ≈ 0 ⇒ IV is
                            // mathematically unidentifiable from price.
                            continue;
                        }
                        let iv = implied_vol(p, &BsInputs { vol: 0.0, ..inputs }, right).unwrap();
                        assert!(
                            (iv - vol).abs() < 1e-6,
                            "K={strike} vol={vol} t={t} {right:?}: got {iv}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn implied_vol_rejects_arbitrage_violations() {
        let zero_vol = BsInputs { vol: 0.0, ..ATM };
        assert!(implied_vol(1000.0, &zero_vol, OptionRight::Call).is_err());
        assert!(implied_vol(-1.0, &zero_vol, OptionRight::Call).is_err());
    }

    #[test]
    fn expired_options_price_at_intrinsic() {
        let expired = BsInputs {
            t_years: 0.0,
            ..ATM
        };
        let itm = BsInputs {
            strike: 90.0,
            ..expired
        };
        assert_eq!(price(&itm, OptionRight::Call).unwrap(), 10.0);
        assert_eq!(price(&itm, OptionRight::Put).unwrap(), 0.0);
    }
}
