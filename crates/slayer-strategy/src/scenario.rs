//! Scenario shock matrices: strategy-level (spot% × vol-shift with a
//! time-decay step) and single-contract desk risk slides.
//!
//! Provenance: legacy `quantSuite.ts` `computeScenarioShockMatrix` (spec 02
//! E7) and `scenarioMatrix.ts` `computeScenarioMatrix` (spec 02 E8), per
//! `docs/spec/02-quant-suite.md`. All repricing delegates to
//! [`slayer_quant::black_scholes`].
//!
//! Deviations from legacy:
//! - **D4 fixed (E7).** The legacy entry baseline was always priced at
//!   `30/365` years regardless of each leg's actual expiry, so any position
//!   not entered at exactly 30 DTE showed phantom baseline P&L. Here each
//!   leg's baseline is the model price at *its own* [`Leg::expiry_t_years`]
//!   and unshocked spot/vol, so the `(0, 0)` node with a zero decay step is
//!   identically zero P&L.
//! - **The fixed DTE axis `[30, 15, 0]` is collapsed to one `days_forward`
//!   decay step.** Once D4 is fixed, a shared absolute-DTE axis is
//!   ill-defined for heterogeneous leg expiries; each firing prices every leg
//!   at `max(0, t − days_forward/365)`. Callers wanting several horizons call
//!   once per step. The spot/vol grids remain the spec defaults
//!   ([`SHOCK_DEFAULT_SPOT_GRID`], [`SHOCK_DEFAULT_VOL_GRID`]) and are always
//!   passed explicitly — no hidden defaults.
//! - **Cells carry shocked aggregate greeks** (`delta`/`gamma`/`vega`/
//!   `theta`), which the legacy E7 nodes omitted; per-share × signed
//!   quantity, matching the spec E6 `combinedGreeks` convention. Theta is
//!   per year and vega per unit vol (kernel-natural; display conversions via
//!   [`slayer_quant::black_scholes::THETA_PER_DAY`] /
//!   [`slayer_quant::black_scholes::VEGA_PER_VOL_POINT`]).
//! - **D18 fixed by omission (E8).** The legacy percent-P&L channel divided
//!   by an entry price floored at `$0.01`, blowing up on near-free contracts
//!   — and the same floor contaminated the *absolute* P&L. Cells here carry
//!   absolute dollars computed from the true entry price; percent framing is
//!   a display concern.
//! - Legacy rounding (cents / one decimal) and the derived `best`/`worst`
//!   extremes are dropped: full precision is returned and extremes are a
//!   trivial fold over the cells (the legacy rounded-vs-raw comparison
//!   footgun disappears with them).

use serde::{Deserialize, Serialize};
use slayer_quant::QuantError;
use slayer_quant::black_scholes::{self, BsInputs};
use thiserror::Error;

use crate::multileg::{MarketInputs, Strategy, StrategyError};
use slayer_core::OptionRight;

/// Default spot-shock grid, fractions of spot. Spec E7
/// `SHOCK_DEFAULT_SPOT_GRID`.
pub const SHOCK_DEFAULT_SPOT_GRID: [f64; 5] = [-0.05, -0.025, 0.0, 0.025, 0.05];

/// Default vol-shock grid, absolute annualized decimals added to IV. Spec E7
/// `SHOCK_DEFAULT_VOL_GRID`.
pub const SHOCK_DEFAULT_VOL_GRID: [f64; 5] = [-0.05, -0.025, 0.0, 0.025, 0.05];

/// Default single-contract spot-shift grid, fractions of spot. Spec E8
/// `SCEN_DEFAULT_SPOT_GRID`.
pub const SCEN_DEFAULT_SPOT_GRID: [f64; 7] = [-0.05, -0.03, -0.015, 0.0, 0.015, 0.03, 0.05];

/// Default single-contract IV-shift grid, absolute annualized decimals. Spec
/// E8 `SCEN_DEFAULT_IV_GRID`.
pub const SCEN_DEFAULT_IV_GRID: [f64; 5] = [-0.05, -0.02, 0.0, 0.02, 0.05];

/// Default forward time-decay step, calendar days. Spec E8
/// `SCEN_DEFAULT_DAYS_FWD`.
pub const SCEN_DEFAULT_DAYS_FWD: f64 = 1.0;

/// Floor on the single-contract remaining DTE after the decay step, days.
/// Spec E8 `SCEN_MIN_DTE_DAYS`.
pub const SCEN_MIN_DTE_DAYS: f64 = 0.05;

/// Floor applied to a shocked vol, annualized decimal. Spec E7 `SHOCK_MIN_VOL`
/// / spec E8 `SCEN_MIN_IV` (both 0.01 in legacy).
pub const SHOCKED_VOL_FLOOR: f64 = 0.01;

/// Calendar days per year for day↔year conversion. Spec E2
/// `DAYS_PER_YEAR_CAL`.
pub const DAYS_PER_YEAR_CAL: f64 = 365.0;

/// Errors from scenario repricing. Malformed inputs are values, never panics.
#[derive(Debug, Error, PartialEq)]
pub enum ScenarioError {
    /// A scenario parameter is outside its domain.
    #[error("scenario domain error: {0}")]
    Domain(&'static str),
    /// Error propagated from strategy validation.
    #[error(transparent)]
    Strategy(#[from] StrategyError),
    /// Error propagated from the pricing kernel.
    #[error(transparent)]
    Quant(#[from] QuantError),
}

/// One node of a shock matrix: the shock coordinates, the repriced P&L, and
/// the shocked first-order greek profile.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ShockCell {
    /// Spot shock, fraction of spot (−0.05 = −5%).
    pub spot_shock: f64,
    /// Vol shock, absolute annualized decimal added to each IV.
    pub vol_shock: f64,
    /// P&L at the node, $. Strategy matrix: vs. the unshocked model baseline
    /// (D4 fix). Contract matrix: vs. the entry premium.
    pub pnl: f64,
    /// Shocked aggregate ∂V/∂S, per share × signed quantity.
    pub delta: f64,
    /// Shocked aggregate ∂²V/∂S², per share × signed quantity.
    pub gamma: f64,
    /// Shocked aggregate ∂V/∂σ, per unit vol, per share × signed quantity.
    pub vega: f64,
    /// Shocked aggregate ∂V/∂t, per year, per share × signed quantity.
    pub theta: f64,
}

/// One contract for the E8 desk risk slide. DTE is in calendar days as in the
/// legacy interface; conversion uses [`DAYS_PER_YEAR_CAL`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ContractSpec {
    /// Call or put.
    pub right: OptionRight,
    /// Strike, points.
    pub strike: f64,
    /// Days to expiry, calendar days.
    pub dte_days: f64,
    /// Implied vol, annualized decimal.
    pub iv: f64,
    /// Entry premium, $ per share (non-negative; used as the P&L baseline).
    pub entry_price: f64,
    /// Signed contract count: positive long, negative short.
    pub quantity: i32,
}

impl ContractSpec {
    /// Validate the contract's domain.
    pub fn validate(&self) -> Result<(), ScenarioError> {
        if !(self.strike > 0.0 && self.strike.is_finite()) {
            return Err(ScenarioError::Domain(
                "contract strike must be positive and finite",
            ));
        }
        if !(self.dte_days >= 0.0 && self.dte_days.is_finite()) {
            return Err(ScenarioError::Domain(
                "contract dte_days must be non-negative and finite",
            ));
        }
        if !(self.iv >= 0.0 && self.iv.is_finite()) {
            return Err(ScenarioError::Domain(
                "contract iv must be non-negative and finite",
            ));
        }
        if !(self.entry_price >= 0.0 && self.entry_price.is_finite()) {
            return Err(ScenarioError::Domain(
                "contract entry_price must be non-negative and finite",
            ));
        }
        Ok(())
    }
}

/// Validate a shock grid: every shock finite, and spot shocks strictly above
/// −100% (a −100% shock puts the underlying at zero, outside the BSM domain).
fn validate_shocks(spot_shocks: &[f64], vol_shocks: &[f64]) -> Result<(), ScenarioError> {
    if spot_shocks.iter().any(|s| !s.is_finite() || *s <= -1.0) {
        return Err(ScenarioError::Domain(
            "spot shocks must be finite and above -100%",
        ));
    }
    if vol_shocks.iter().any(|v| !v.is_finite()) {
        return Err(ScenarioError::Domain("vol shocks must be finite"));
    }
    Ok(())
}

/// Validate a forward decay step in calendar days.
fn validate_days_forward(days_forward: f64) -> Result<(), ScenarioError> {
    if days_forward >= 0.0 && days_forward.is_finite() {
        Ok(())
    } else {
        Err(ScenarioError::Domain(
            "days_forward must be non-negative and finite",
        ))
    }
}

/// E7: reprice a multi-leg strategy over `spot_shocks × vol_shocks` after a
/// `days_forward` calendar-day decay step. `vols` aligns one annualized IV
/// per leg with `strategy.legs`.
///
/// Cell P&L is against each leg's *own* unshocked model baseline (D4 fix), so
/// with `days_forward = 0` the `(0, 0)` node is identically zero. Legs decay
/// to `max(0, t − days_forward/365)`; a leg reaching zero time prices at
/// intrinsic through the kernel's degenerate branch, matching the legacy
/// `dte === 0` intrinsic node. Cells are emitted vol-major (all spot shocks
/// for the first vol shock, then the next), matching the E8 rows-are-vol
/// layout.
pub fn strategy_shock_matrix(
    strategy: &Strategy,
    vols: &[f64],
    market: &MarketInputs,
    spot_shocks: &[f64],
    vol_shocks: &[f64],
    days_forward: f64,
) -> Result<Vec<ShockCell>, ScenarioError> {
    strategy.validate()?;
    if vols.len() != strategy.legs.len() {
        return Err(StrategyError::VolCountMismatch {
            legs: strategy.legs.len(),
            vols: vols.len(),
        }
        .into());
    }
    validate_shocks(spot_shocks, vol_shocks)?;
    validate_days_forward(days_forward)?;

    // Baseline: each leg at its own entry time, unshocked spot and vol (D4).
    let mut baseline = Vec::with_capacity(strategy.legs.len());
    for (leg, &vol) in strategy.legs.iter().zip(vols) {
        let inputs = BsInputs {
            spot: market.spot,
            strike: leg.strike,
            t_years: leg.expiry_t_years,
            vol,
            rate: market.rate,
            div_yield: market.div_yield,
        };
        baseline.push(black_scholes::price(&inputs, leg.right)?);
    }

    let decay_years = days_forward / DAYS_PER_YEAR_CAL;
    let mut cells = Vec::with_capacity(vol_shocks.len() * spot_shocks.len());
    for &vol_shock in vol_shocks {
        for &spot_shock in spot_shocks {
            let shocked_spot = market.spot * (1.0 + spot_shock);
            let mut cell = ShockCell {
                spot_shock,
                vol_shock,
                pnl: 0.0,
                delta: 0.0,
                gamma: 0.0,
                vega: 0.0,
                theta: 0.0,
            };
            for ((leg, &vol), &base_price) in strategy.legs.iter().zip(vols).zip(&baseline) {
                let inputs = BsInputs {
                    spot: shocked_spot,
                    strike: leg.strike,
                    t_years: (leg.expiry_t_years - decay_years).max(0.0),
                    vol: (vol + vol_shock).max(SHOCKED_VOL_FLOOR),
                    rate: market.rate,
                    div_yield: market.div_yield,
                };
                let price = black_scholes::price(&inputs, leg.right)?;
                let g = black_scholes::greeks(&inputs, leg.right)?;
                let qty = f64::from(leg.quantity);
                cell.pnl += (price - base_price) * qty * strategy.multiplier;
                cell.delta += g.delta * qty;
                cell.gamma += g.gamma * qty;
                cell.vega += g.vega * qty;
                cell.theta += g.theta * qty;
            }
            cells.push(cell);
        }
    }
    Ok(cells)
}

/// E8: the single-contract desk risk slide — reprice one contract over
/// `spot_shifts × iv_shifts` at a `days_forward` decay horizon, P&L against
/// the entry premium.
///
/// Remaining time floors at [`SCEN_MIN_DTE_DAYS`] (legacy behavior), the
/// shifted IV at [`SHOCKED_VOL_FLOOR`]. Cells are emitted vol-major (rows =
/// IV shifts), matching the legacy grid layout; extremes are a fold over the
/// cells.
pub fn contract_scenario_matrix(
    contract: &ContractSpec,
    market: &MarketInputs,
    spot_shifts: &[f64],
    iv_shifts: &[f64],
    days_forward: f64,
    multiplier: f64,
) -> Result<Vec<ShockCell>, ScenarioError> {
    contract.validate()?;
    validate_shocks(spot_shifts, iv_shifts)?;
    validate_days_forward(days_forward)?;
    if !(multiplier > 0.0 && multiplier.is_finite()) {
        return Err(ScenarioError::Domain(
            "multiplier must be positive and finite",
        ));
    }

    let t_years = (contract.dte_days - days_forward).max(SCEN_MIN_DTE_DAYS) / DAYS_PER_YEAR_CAL;
    let qty = f64::from(contract.quantity);
    let mut cells = Vec::with_capacity(iv_shifts.len() * spot_shifts.len());
    for &vol_shock in iv_shifts {
        for &spot_shock in spot_shifts {
            let inputs = BsInputs {
                spot: market.spot * (1.0 + spot_shock),
                strike: contract.strike,
                t_years,
                vol: (contract.iv + vol_shock).max(SHOCKED_VOL_FLOOR),
                rate: market.rate,
                div_yield: market.div_yield,
            };
            let price = black_scholes::price(&inputs, contract.right)?;
            let g = black_scholes::greeks(&inputs, contract.right)?;
            cells.push(ShockCell {
                spot_shock,
                vol_shock,
                pnl: (price - contract.entry_price) * qty * multiplier,
                delta: g.delta * qty,
                gamma: g.gamma * qty,
                vega: g.vega * qty,
                theta: g.theta * qty,
            });
        }
    }
    Ok(cells)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::multileg::{CONTRACT_MULTIPLIER, Leg};

    const MARKET: MarketInputs = MarketInputs {
        spot: 100.0,
        rate: 0.05,
        div_yield: 0.0,
    };

    fn long_call() -> Strategy {
        Strategy {
            legs: vec![Leg {
                right: OptionRight::Call,
                strike: 100.0,
                expiry_t_years: 60.0 / 365.0,
                quantity: 1,
                premium: 5.0,
            }],
            multiplier: CONTRACT_MULTIPLIER,
        }
    }

    #[test]
    fn long_call_pnl_monotone_in_spot_shock() {
        let cells = strategy_shock_matrix(
            &long_call(),
            &[0.2],
            &MARKET,
            &SHOCK_DEFAULT_SPOT_GRID,
            &[0.0],
            0.0,
        )
        .unwrap();
        assert_eq!(cells.len(), SHOCK_DEFAULT_SPOT_GRID.len());
        for pair in cells.windows(2) {
            assert!(pair[1].pnl > pair[0].pnl, "call P&L must rise with spot");
            assert!(pair[1].delta > pair[0].delta, "call delta rises with spot");
        }
    }

    #[test]
    fn unshocked_node_is_zero_at_any_expiry() {
        // D4 regression: the leg is 60 DTE, not 30; with no shock and no
        // decay step the baseline must cancel exactly.
        let cells =
            strategy_shock_matrix(&long_call(), &[0.2], &MARKET, &[0.0], &[0.0], 0.0).unwrap();
        assert_eq!(cells.len(), 1);
        assert!(cells[0].pnl.abs() < 1e-12);
    }

    #[test]
    fn decay_step_bleeds_time_value() {
        let cells =
            strategy_shock_matrix(&long_call(), &[0.2], &MARKET, &[0.0], &[0.0], 5.0).unwrap();
        assert!(cells[0].pnl < 0.0, "long ATM call loses value over 5 days");
    }

    #[test]
    fn vol_shock_sign_matches_vega() {
        let cells = strategy_shock_matrix(
            &long_call(),
            &[0.2],
            &MARKET,
            &[0.0],
            &[-0.05, 0.0, 0.05],
            0.0,
        )
        .unwrap();
        assert!(cells[0].pnl < 0.0, "long call loses on a vol crush");
        assert!(cells[2].pnl > 0.0, "long call gains on a vol spike");
        assert!(cells.iter().all(|c| c.vega > 0.0));
    }

    #[test]
    fn full_decay_prices_intrinsic() {
        // Decay past expiry: the leg reprices at t = 0 intrinsic.
        let s = long_call();
        let cells = strategy_shock_matrix(&s, &[0.2], &MARKET, &[0.05], &[0.0], 365.0).unwrap();
        let base = black_scholes::price(
            &BsInputs {
                spot: 100.0,
                strike: 100.0,
                t_years: 60.0 / 365.0,
                vol: 0.2,
                rate: 0.05,
                div_yield: 0.0,
            },
            OptionRight::Call,
        )
        .unwrap();
        let expected = (105.0 - 100.0 - base) * 100.0; // intrinsic − baseline
        assert!((cells[0].pnl - expected).abs() < 1e-9);
        assert!(cells[0].gamma.abs() < 1e-12, "expired leg has no gamma");
    }

    #[test]
    fn contract_matrix_matches_kernel_and_flips_with_sign() {
        let contract = ContractSpec {
            right: OptionRight::Call,
            strike: 100.0,
            dte_days: 30.0,
            iv: 0.2,
            entry_price: 2.5,
            quantity: 1,
        };
        let cells = contract_scenario_matrix(
            &contract,
            &MARKET,
            &SCEN_DEFAULT_SPOT_GRID,
            &SCEN_DEFAULT_IV_GRID,
            SCEN_DEFAULT_DAYS_FWD,
            CONTRACT_MULTIPLIER,
        )
        .unwrap();
        assert_eq!(
            cells.len(),
            SCEN_DEFAULT_SPOT_GRID.len() * SCEN_DEFAULT_IV_GRID.len()
        );

        // Manual repricing of the unshocked cell one day forward.
        let unshocked = cells
            .iter()
            .find(|c| c.spot_shock == 0.0 && c.vol_shock == 0.0)
            .unwrap();
        let px = black_scholes::price(
            &BsInputs {
                spot: 100.0,
                strike: 100.0,
                t_years: 29.0 / 365.0,
                vol: 0.2,
                rate: 0.05,
                div_yield: 0.0,
            },
            OptionRight::Call,
        )
        .unwrap();
        assert!((unshocked.pnl - (px - 2.5) * 100.0).abs() < 1e-12);

        // Short position: every cell exactly negated.
        let short = ContractSpec {
            quantity: -1,
            ..contract
        };
        let short_cells = contract_scenario_matrix(
            &short,
            &MARKET,
            &SCEN_DEFAULT_SPOT_GRID,
            &SCEN_DEFAULT_IV_GRID,
            SCEN_DEFAULT_DAYS_FWD,
            CONTRACT_MULTIPLIER,
        )
        .unwrap();
        for (l, s) in cells.iter().zip(&short_cells) {
            assert!((l.pnl + s.pnl).abs() < 1e-12);
            assert!((l.delta + s.delta).abs() < 1e-15);
        }
    }

    #[test]
    fn dte_floor_prevents_negative_time() {
        // 0.5 days left, one day forward: floors at SCEN_MIN_DTE_DAYS.
        let contract = ContractSpec {
            right: OptionRight::Put,
            strike: 100.0,
            dte_days: 0.5,
            iv: 0.3,
            entry_price: 1.0,
            quantity: 1,
        };
        let cells =
            contract_scenario_matrix(&contract, &MARKET, &[0.0], &[0.0], 1.0, 100.0).unwrap();
        assert!(cells[0].pnl.is_finite());
    }

    #[test]
    fn errors_are_values() {
        let s = long_call();
        // Vol slice misaligned.
        assert!(matches!(
            strategy_shock_matrix(&s, &[], &MARKET, &[0.0], &[0.0], 0.0),
            Err(ScenarioError::Strategy(
                StrategyError::VolCountMismatch { .. }
            ))
        ));
        // Spot shock at −100%.
        assert!(matches!(
            strategy_shock_matrix(&s, &[0.2], &MARKET, &[-1.0], &[0.0], 0.0),
            Err(ScenarioError::Domain(_))
        ));
        // Negative decay step.
        assert!(matches!(
            strategy_shock_matrix(&s, &[0.2], &MARKET, &[0.0], &[0.0], -1.0),
            Err(ScenarioError::Domain(_))
        ));
        // Bad contract.
        let bad = ContractSpec {
            right: OptionRight::Call,
            strike: 0.0,
            dte_days: 30.0,
            iv: 0.2,
            entry_price: 1.0,
            quantity: 1,
        };
        assert!(matches!(
            contract_scenario_matrix(&bad, &MARKET, &[0.0], &[0.0], 1.0, 100.0),
            Err(ScenarioError::Domain(_))
        ));
    }

    #[test]
    fn cells_serde_round_trip() {
        let cells = strategy_shock_matrix(
            &long_call(),
            &[0.2],
            &MARKET,
            &SHOCK_DEFAULT_SPOT_GRID,
            &SHOCK_DEFAULT_VOL_GRID,
            1.0,
        )
        .unwrap();
        let json = serde_json::to_string(&cells).unwrap();
        let back: Vec<ShockCell> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cells);
    }
}
