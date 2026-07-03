//! Multi-leg strategy construction: payoff coordinates, breakevens, exact
//! max-profit/max-loss, payoff tail topology, and mark-to-model value/greeks.
//!
//! Provenance: legacy `quantSuite.ts` `buildStrategySuite` /
//! `payoffTailTopology` / `generatePayoffCoordinates` (spec 02 E6), per
//! `docs/spec/02-quant-suite.md`. All pricing and greeks delegate to
//! [`slayer_quant::black_scholes`]; nothing is repriced locally.
//!
//! Deviations from legacy:
//! - **Exact kink lattice replaces the ±4·EM scan.** The expiry payoff is
//!   piecewise linear with kinks only at `S = 0` and the strikes, so extremes
//!   and breakevens are computed *exactly* on that lattice plus the right-tail
//!   slope. This fixes **D5** (the legacy scan floor `max(0, min(spot−4em,
//!   lowest RND strike))` never reached `S = 0`, understating short-put max
//!   loss and long-put max profit) and removes the legacy zero-initialized
//!   extremes (which could report an unattainable `0` as max profit/loss),
//!   the `avgIv = 0.2` empty-leg fallback, and the RND-tail heuristics.
//! - **D16 fixed.** Breakeven dedupe uses [`BREAKEVEN_DEDUPE_REL_TOL`]
//!   (relative, FP-noise scale) instead of a fixed `$2` window, and the
//!   legacy `$0.1` display rounding is dropped (presentation is UI-side).
//!   With exact segment interpolation the tolerance only has to absorb a
//!   crossing reported twice at a shared kink, never merge real breakevens.
//! - **RND-coupled outputs are not ported here.** POP, Kelly sizing, and the
//!   probability channel of the payoff coordinates consume the E3 density and
//!   live with the RND consumer (which is also where legacy defect D6, the
//!   dropped top-of-support POP sample, applies). [`PayoffPoint`] carries
//!   `{spot, pnl}` only — no fabricated probability.
//! - **No fabricated pricing inputs.** Per-leg vols, rate, and dividend yield
//!   are explicit parameters; a missing vol is a [`StrategyError`], never a
//!   default (D13 family).
//! - **Kernel-natural greek units** (theta per year, vega per unit vol),
//!   resolving the legacy vega-units ambiguity (D1). Display conversions are
//!   the caller's concern — see [`slayer_quant::black_scholes::THETA_PER_DAY`]
//!   and [`slayer_quant::black_scholes::VEGA_PER_VOL_POINT`].
//! - Multi-expiry caveat, preserved from legacy: payoff functions evaluate
//!   every leg at *its own* expiry intrinsic, i.e. they treat the strategy as
//!   a single-expiry structure. Calendar-spread payoff between expiries is
//!   out of scope here, as it was in the legacy builder.

use serde::{Deserialize, Serialize};
use slayer_core::OptionRight;
use slayer_quant::QuantError;
use slayer_quant::black_scholes::{self, BsInputs};
use thiserror::Error;

/// US equity option contract multiplier, shares per contract. Spec E6
/// `CONTRACT_MULTIPLIER`. A caller convenience only — [`Strategy::multiplier`]
/// is always explicit, never assumed.
pub const CONTRACT_MULTIPLIER: f64 = 100.0;

/// Payoff-chart lower bound as a fraction of spot. Spec E6
/// `PAYOFF_CHART_LO_FRAC`.
pub const PAYOFF_CHART_LO_FRAC: f64 = 0.85;

/// Payoff-chart upper bound as a fraction of spot. Spec E6
/// `PAYOFF_CHART_HI_FRAC`.
pub const PAYOFF_CHART_HI_FRAC: f64 = 1.15;

/// Payoff-chart step count (81 points inclusive). Spec E6
/// `PAYOFF_CHART_STEPS`.
pub const PAYOFF_CHART_STEPS: u32 = 80;

/// Relative tolerance for merging a zero crossing reported twice at a shared
/// kink, with a `$1` scale floor near zero. Replaces the legacy fixed `$2`
/// dedupe window whose absolute scale merged genuinely distinct breakevens on
/// low-priced underlyings (D16).
pub const BREAKEVEN_DEDUPE_REL_TOL: f64 = 1e-9;

/// Errors from strategy construction and analytics. Malformed inputs are
/// values, never panics.
#[derive(Debug, Error, PartialEq)]
pub enum StrategyError {
    /// The strategy has no legs; every analytic here is undefined on an
    /// empty structure (the legacy filled the gap with a 0.2 IV fallback).
    #[error("strategy has no legs")]
    Empty,
    /// A leg or strategy field is outside its domain.
    #[error("strategy domain error: {0}")]
    Domain(&'static str),
    /// The per-leg vol slice does not align with the legs.
    #[error("vol count mismatch: {legs} legs, {vols} vols")]
    VolCountMismatch {
        /// Number of legs in the strategy.
        legs: usize,
        /// Number of vols supplied.
        vols: usize,
    },
    /// Error propagated from the pricing kernel.
    #[error(transparent)]
    Quant(#[from] QuantError),
}

/// One option leg. Quantity is signed (positive long, negative short);
/// premium is the entry price per share.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Leg {
    /// Call or put.
    pub right: OptionRight,
    /// Strike, points.
    pub strike: f64,
    /// Time to expiry, years (ACT/365 fraction chosen by the caller).
    pub expiry_t_years: f64,
    /// Signed contract count: positive long, negative short.
    pub quantity: i32,
    /// Entry premium, $ per share (non-negative).
    pub premium: f64,
}

impl Leg {
    /// Validate the leg's domain: positive finite strike, non-negative finite
    /// time and premium.
    pub fn validate(&self) -> Result<(), StrategyError> {
        if !(self.strike > 0.0 && self.strike.is_finite()) {
            return Err(StrategyError::Domain(
                "leg strike must be positive and finite",
            ));
        }
        if !(self.expiry_t_years >= 0.0 && self.expiry_t_years.is_finite()) {
            return Err(StrategyError::Domain(
                "leg expiry_t_years must be non-negative and finite",
            ));
        }
        if !(self.premium >= 0.0 && self.premium.is_finite()) {
            return Err(StrategyError::Domain(
                "leg premium must be non-negative and finite",
            ));
        }
        Ok(())
    }

    /// Expiry intrinsic value, $ per share, at terminal spot `spot`.
    #[must_use]
    pub fn intrinsic(&self, spot: f64) -> f64 {
        match self.right {
            OptionRight::Call => (spot - self.strike).max(0.0),
            OptionRight::Put => (self.strike - spot).max(0.0),
        }
    }
}

/// A multi-leg option strategy: the legs plus the contract multiplier
/// (shares per contract, e.g. [`CONTRACT_MULTIPLIER`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Strategy {
    /// The legs.
    pub legs: Vec<Leg>,
    /// Contract multiplier, shares per contract (positive).
    pub multiplier: f64,
}

impl Strategy {
    /// Validate the strategy: at least one leg, positive finite multiplier,
    /// every leg in domain.
    pub fn validate(&self) -> Result<(), StrategyError> {
        if self.legs.is_empty() {
            return Err(StrategyError::Empty);
        }
        if !(self.multiplier > 0.0 && self.multiplier.is_finite()) {
            return Err(StrategyError::Domain(
                "multiplier must be positive and finite",
            ));
        }
        for leg in &self.legs {
            leg.validate()?;
        }
        Ok(())
    }

    /// Net premium, $: `Σ premium · quantity · multiplier`. Positive is a net
    /// debit paid, negative a net credit received (spec E6 sign convention).
    #[must_use]
    pub fn net_premium(&self) -> f64 {
        self.legs
            .iter()
            .map(|l| l.premium * f64::from(l.quantity) * self.multiplier)
            .sum()
    }

    /// Expiry P&L, $, at terminal spot `spot`:
    /// `Σ (intrinsic − premium) · quantity · multiplier` (spec E6 step 2).
    #[must_use]
    pub fn payoff_at(&self, spot: f64) -> f64 {
        self.legs
            .iter()
            .map(|l| (l.intrinsic(spot) - l.premium) * f64::from(l.quantity) * self.multiplier)
            .sum()
    }

    /// Net signed call quantity (spec E6 step 4 `netCallQty`).
    fn net_call_qty(&self) -> i64 {
        self.legs
            .iter()
            .filter(|l| l.right == OptionRight::Call)
            .map(|l| i64::from(l.quantity))
            .sum()
    }

    /// Net signed put quantity (spec E6 step 4 `netPutQty`).
    fn net_put_qty(&self) -> i64 {
        self.legs
            .iter()
            .filter(|l| l.right == OptionRight::Put)
            .map(|l| i64::from(l.quantity))
            .sum()
    }
}

/// Sign of `dP&L/dS` on a payoff tail. Descriptive data (payoff geometry),
/// not a readout — see `docs/ARCHITECTURE.md` §2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TailSlope {
    /// P&L increases with spot on the tail.
    Rising,
    /// P&L decreases with spot on the tail.
    Falling,
    /// P&L is constant on the tail.
    Flat,
}

/// Payoff tail topology (spec E6 step 4). The continuous quantities behind
/// the labels — the signed net call/put quantities — ship alongside, per the
/// spec's state-semantics table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PayoffTopology {
    /// Slope sign of P&L as `S → 0⁺` (below every strike): `−net_put_qty`.
    pub left: TailSlope,
    /// Slope sign of P&L as `S → ∞` (above every strike): `net_call_qty`.
    pub right: TailSlope,
    /// Σ signed call quantity (right-tail slope in contracts per point).
    pub net_call_qty: i64,
    /// Σ signed put quantity.
    pub net_put_qty: i64,
    /// `true` iff the right tail rises without bound (`net_call_qty > 0`).
    /// The left tail is always bounded — spot floors at zero.
    pub profit_unbounded: bool,
    /// `true` iff the right tail falls without bound (`net_call_qty < 0`).
    pub loss_unbounded: bool,
}

/// A payoff extreme over `S ∈ [0, ∞)`: finite (attained at a kink of the
/// piecewise-linear payoff) or unbounded (legacy `'unlimited'`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PayoffExtreme {
    /// Finite extreme, $.
    Bounded(f64),
    /// The tail runs without bound.
    Unbounded,
}

impl PayoffExtreme {
    /// `true` when the extreme is unbounded.
    #[must_use]
    pub const fn is_unbounded(&self) -> bool {
        matches!(self, Self::Unbounded)
    }
}

/// The strategy's expiry payoff profile (spec E6): net premium, exact
/// breakevens, exact extremes, tail topology.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PayoffProfile {
    /// Net premium, $ (positive = debit paid, negative = credit received).
    pub net_premium: f64,
    /// Breakeven spots, ascending — exact zero crossings of the piecewise
    /// linear expiry P&L over `S ∈ [0, ∞)`.
    pub breakevens: Vec<f64>,
    /// Maximum P&L over `S ∈ [0, ∞)`, $.
    pub max_profit: PayoffExtreme,
    /// Minimum P&L (most negative) over `S ∈ [0, ∞)`, $.
    pub max_loss: PayoffExtreme,
    /// Payoff tail topology.
    pub topology: PayoffTopology,
}

/// One payoff-chart point (spec E6 `generatePayoffCoordinates`, minus the
/// RND probability channel — see the module deviations).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PayoffPoint {
    /// Terminal underlying price, points.
    pub spot: f64,
    /// Expiry P&L at that price, $.
    pub pnl: f64,
}

/// Market context for mark-to-model valuation. Every field is explicit —
/// there is no default rate or dividend yield.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MarketInputs {
    /// Underlying spot, points.
    pub spot: f64,
    /// Continuously-compounded risk-free rate, decimal.
    pub rate: f64,
    /// Continuous dividend yield, decimal.
    pub div_yield: f64,
}

/// Aggregate greek bundle, kernel-natural units (theta per year, vega and
/// vanna per unit vol, charm per year). The scaling convention (per-share ×
/// quantity vs. × multiplier) is set by the producing function — see its doc.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AggregateGreeks {
    /// Σ ∂V/∂S.
    pub delta: f64,
    /// Σ ∂²V/∂S².
    pub gamma: f64,
    /// Σ ∂V/∂t, per year.
    pub theta: f64,
    /// Σ ∂V/∂σ, per unit vol.
    pub vega: f64,
    /// Σ ∂²V/∂S∂σ, per unit vol.
    pub vanna: f64,
    /// Σ ∂Δ/∂t, per year.
    pub charm: f64,
}

impl AggregateGreeks {
    /// The additive identity.
    pub const ZERO: Self = Self {
        delta: 0.0,
        gamma: 0.0,
        theta: 0.0,
        vega: 0.0,
        vanna: 0.0,
        charm: 0.0,
    };
}

/// Mark-to-model value and aggregate greeks of the strategy (spec E6).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StrategyValue {
    /// Position mark, $: `Σ model price · quantity · multiplier`.
    pub value: f64,
    /// `value − net_premium`, $ (mark-to-model P&L against entry).
    pub unrealized_pnl: f64,
    /// Aggregate greeks, per-share × signed quantity — spec E6
    /// `combinedGreeks` convention (deliberately **not** × multiplier).
    pub greeks: AggregateGreeks,
}

/// Tail slope label from a signed net quantity.
const fn slope_of(net_qty: i64) -> TailSlope {
    if net_qty > 0 {
        TailSlope::Rising
    } else if net_qty < 0 {
        TailSlope::Falling
    } else {
        TailSlope::Flat
    }
}

/// The expiry payoff profile: net premium, exact breakevens (linear
/// interpolation on sign changes over the kink lattice `{0} ∪ strikes`, plus
/// the analytic right-tail crossing), exact extremes, and tail topology.
///
/// Exactness argument: the expiry P&L is piecewise linear with kinks only at
/// the strikes, linear on `[0, K_min]` and on `[K_max, ∞)`. Its extremes over
/// `S ∈ [0, ∞)` are therefore attained at kinks (or unbounded on the right
/// tail), and linear interpolation between kinks recovers zero crossings
/// exactly.
pub fn payoff_profile(strategy: &Strategy) -> Result<PayoffProfile, StrategyError> {
    strategy.validate()?;

    // Kink lattice: S = 0 plus the sorted, deduplicated strikes.
    let mut kinks: Vec<f64> = strategy.legs.iter().map(|l| l.strike).collect();
    kinks.sort_by(f64::total_cmp);
    kinks.dedup();
    kinks.insert(0, 0.0);
    let pnls: Vec<f64> = kinks.iter().map(|&s| strategy.payoff_at(s)).collect();

    let net_call_qty = strategy.net_call_qty();
    let net_put_qty = strategy.net_put_qty();
    let topology = PayoffTopology {
        left: slope_of(net_put_qty.saturating_neg()),
        right: slope_of(net_call_qty),
        net_call_qty,
        net_put_qty,
        profit_unbounded: net_call_qty > 0,
        loss_unbounded: net_call_qty < 0,
    };

    // Breakevens: exact zeros at kinks, exact interpolation on sign changes
    // between kinks, analytic crossing on the right tail.
    let mut breakevens: Vec<f64> = Vec::new();
    let mut prev: Option<(f64, f64)> = None;
    for (&kink, &pnl) in kinks.iter().zip(&pnls) {
        if pnl == 0.0 {
            breakevens.push(kink);
        } else if let Some((prev_kink, prev_pnl)) = prev
            && prev_pnl * pnl < 0.0
        {
            let w = prev_pnl.abs() / (prev_pnl.abs() + pnl.abs());
            breakevens.push(prev_kink + (kink - prev_kink) * w);
        }
        prev = Some((kink, pnl));
    }
    // Right tail: P&L is linear with slope net_call_qty · multiplier $/point.
    #[allow(clippy::cast_precision_loss)] // contract counts are far below 2^52
    let right_slope = net_call_qty as f64 * strategy.multiplier;
    if let (Some(&last_kink), Some(&last_pnl)) = (kinks.last(), pnls.last())
        && last_pnl != 0.0
        && last_pnl * right_slope < 0.0
    {
        breakevens.push(last_kink - last_pnl / right_slope);
    }
    breakevens.sort_by(f64::total_cmp);
    breakevens.dedup_by(|a, b| {
        (*a - *b).abs() <= BREAKEVEN_DEDUPE_REL_TOL * a.abs().max(b.abs()).max(1.0)
    });

    let max_kink = pnls.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let min_kink = pnls.iter().copied().fold(f64::INFINITY, f64::min);
    let max_profit = if topology.profit_unbounded {
        PayoffExtreme::Unbounded
    } else {
        PayoffExtreme::Bounded(max_kink)
    };
    let max_loss = if topology.loss_unbounded {
        PayoffExtreme::Unbounded
    } else {
        PayoffExtreme::Bounded(min_kink)
    };

    Ok(PayoffProfile {
        net_premium: strategy.net_premium(),
        breakevens,
        max_profit,
        max_loss,
        topology,
    })
}

/// Payoff coordinates over the chart grid: 81 evenly spaced points spanning
/// [`PAYOFF_CHART_LO_FRAC`]`·spot` to [`PAYOFF_CHART_HI_FRAC`]`·spot`
/// (spec E6 `generatePayoffCoordinates`), full precision — no display
/// rounding.
pub fn payoff_coordinates(
    strategy: &Strategy,
    spot: f64,
) -> Result<Vec<PayoffPoint>, StrategyError> {
    strategy.validate()?;
    if !(spot > 0.0 && spot.is_finite()) {
        return Err(StrategyError::Domain("spot must be positive and finite"));
    }
    let lo = spot * PAYOFF_CHART_LO_FRAC;
    let hi = spot * PAYOFF_CHART_HI_FRAC;
    let step = (hi - lo) / f64::from(PAYOFF_CHART_STEPS);
    Ok((0..=PAYOFF_CHART_STEPS)
        .map(|i| {
            let s = lo + step * f64::from(i);
            PayoffPoint {
                spot: s,
                pnl: strategy.payoff_at(s),
            }
        })
        .collect())
}

/// Mark-to-model value and aggregate greeks via the kernel. `vols` supplies
/// one annualized implied vol per leg, aligned with `strategy.legs` — a
/// missing vol is an error, never a default (see the module deviations).
///
/// Greeks accumulate per-share × signed quantity (spec E6 `combinedGreeks`);
/// `value` and `unrealized_pnl` are position-level dollars (× multiplier).
pub fn strategy_value(
    strategy: &Strategy,
    vols: &[f64],
    market: &MarketInputs,
) -> Result<StrategyValue, StrategyError> {
    strategy.validate()?;
    if vols.len() != strategy.legs.len() {
        return Err(StrategyError::VolCountMismatch {
            legs: strategy.legs.len(),
            vols: vols.len(),
        });
    }
    let mut value = 0.0;
    let mut greeks = AggregateGreeks::ZERO;
    for (leg, &vol) in strategy.legs.iter().zip(vols) {
        let inputs = BsInputs {
            spot: market.spot,
            strike: leg.strike,
            t_years: leg.expiry_t_years,
            vol,
            rate: market.rate,
            div_yield: market.div_yield,
        };
        let price = black_scholes::price(&inputs, leg.right)?;
        let g = black_scholes::greeks(&inputs, leg.right)?;
        let qty = f64::from(leg.quantity);
        value += price * qty * strategy.multiplier;
        greeks.delta += g.delta * qty;
        greeks.gamma += g.gamma * qty;
        greeks.theta += g.theta * qty;
        greeks.vega += g.vega * qty;
        greeks.vanna += g.vanna * qty;
        greeks.charm += g.charm * qty;
    }
    Ok(StrategyValue {
        value,
        unrealized_pnl: value - strategy.net_premium(),
        greeks,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn leg(right: OptionRight, strike: f64, quantity: i32, premium: f64) -> Leg {
        Leg {
            right,
            strike,
            expiry_t_years: 30.0 / 365.0,
            quantity,
            premium,
        }
    }

    fn strategy(legs: Vec<Leg>) -> Strategy {
        Strategy {
            legs,
            multiplier: CONTRACT_MULTIPLIER,
        }
    }

    #[test]
    fn long_call_payoff_and_breakeven() {
        let s = strategy(vec![leg(OptionRight::Call, 100.0, 1, 5.0)]);
        // Payoff = (max(S − K, 0) − premium) · qty · mult.
        assert_eq!(s.payoff_at(110.0), 500.0);
        assert_eq!(s.payoff_at(90.0), -500.0);
        assert_eq!(s.net_premium(), 500.0);

        let p = payoff_profile(&s).unwrap();
        assert_eq!(p.breakevens, vec![105.0]); // exactly K + premium
        assert_eq!(p.max_profit, PayoffExtreme::Unbounded);
        assert_eq!(p.max_loss, PayoffExtreme::Bounded(-500.0));
        assert_eq!(p.topology.right, TailSlope::Rising);
        assert_eq!(p.topology.left, TailSlope::Flat);
        assert!(p.topology.profit_unbounded);
        assert!(!p.topology.loss_unbounded);
    }

    #[test]
    fn vertical_spread_capped_both_sides() {
        // Bull call spread: +1 100C @ 5, −1 110C @ 2 → $3 debit.
        let s = strategy(vec![
            leg(OptionRight::Call, 100.0, 1, 5.0),
            leg(OptionRight::Call, 110.0, -1, 2.0),
        ]);
        let p = payoff_profile(&s).unwrap();
        assert_eq!(p.net_premium, 300.0);
        assert_eq!(p.max_loss, PayoffExtreme::Bounded(-300.0));
        assert_eq!(p.max_profit, PayoffExtreme::Bounded(700.0));
        assert_eq!(p.breakevens, vec![103.0]);
        assert_eq!(p.topology.left, TailSlope::Flat);
        assert_eq!(p.topology.right, TailSlope::Flat);
        assert!(!p.topology.profit_unbounded && !p.topology.loss_unbounded);
    }

    #[test]
    fn straddle_two_breakevens() {
        // Long straddle: +1 100C @ 4, +1 100P @ 3.
        let s = strategy(vec![
            leg(OptionRight::Call, 100.0, 1, 4.0),
            leg(OptionRight::Put, 100.0, 1, 3.0),
        ]);
        let p = payoff_profile(&s).unwrap();
        assert_eq!(p.breakevens, vec![93.0, 107.0]); // K ± total premium
        assert_eq!(p.max_loss, PayoffExtreme::Bounded(-700.0));
        assert_eq!(p.max_profit, PayoffExtreme::Unbounded);
        assert_eq!(p.topology.left, TailSlope::Falling);
        assert_eq!(p.topology.right, TailSlope::Rising);
    }

    #[test]
    fn short_put_max_loss_reaches_zero_spot() {
        // D5 regression: −1 100P @ 3 worst case is S = 0, not a 4·EM scan
        // floor. Loss there = −(100 − 3)·100.
        let s = strategy(vec![leg(OptionRight::Put, 100.0, -1, 3.0)]);
        let p = payoff_profile(&s).unwrap();
        assert_eq!(p.max_loss, PayoffExtreme::Bounded(-9700.0));
        assert_eq!(p.max_profit, PayoffExtreme::Bounded(300.0));
        assert_eq!(p.breakevens, vec![97.0]);
        assert_eq!(p.net_premium, -300.0); // net credit
        assert_eq!(p.topology.left, TailSlope::Rising);
        assert_eq!(p.topology.right, TailSlope::Flat);
        assert!(!p.max_loss.is_unbounded());
    }

    #[test]
    fn short_call_loss_unbounded() {
        let s = strategy(vec![leg(OptionRight::Call, 100.0, -2, 5.0)]);
        let p = payoff_profile(&s).unwrap();
        assert_eq!(p.max_loss, PayoffExtreme::Unbounded);
        assert_eq!(p.max_profit, PayoffExtreme::Bounded(1000.0));
        assert!(p.topology.loss_unbounded);
        assert_eq!(p.topology.net_call_qty, -2);
        assert_eq!(p.topology.right, TailSlope::Falling);
    }

    #[test]
    fn payoff_coordinates_span_chart_grid() {
        let s = strategy(vec![leg(OptionRight::Call, 100.0, 1, 5.0)]);
        let pts = payoff_coordinates(&s, 100.0).unwrap();
        assert_eq!(pts.len(), 81);
        assert!((pts[0].spot - 85.0).abs() < 1e-12);
        assert!((pts[80].spot - 115.0).abs() < 1e-12);
        for pt in &pts {
            assert_eq!(pt.pnl, s.payoff_at(pt.spot));
        }
    }

    #[test]
    fn strategy_value_matches_kernel() {
        let s = strategy(vec![leg(OptionRight::Call, 100.0, 1, 5.0)]);
        let market = MarketInputs {
            spot: 100.0,
            rate: 0.05,
            div_yield: 0.0,
        };
        let v = strategy_value(&s, &[0.2], &market).unwrap();
        let inputs = BsInputs {
            spot: 100.0,
            strike: 100.0,
            t_years: 30.0 / 365.0,
            vol: 0.2,
            rate: 0.05,
            div_yield: 0.0,
        };
        let px = black_scholes::price(&inputs, OptionRight::Call).unwrap();
        let g = black_scholes::greeks(&inputs, OptionRight::Call).unwrap();
        assert!((v.value - px * 100.0).abs() < 1e-12);
        assert!((v.unrealized_pnl - (px * 100.0 - 500.0)).abs() < 1e-12);
        assert!((v.greeks.delta - g.delta).abs() < 1e-15);
        assert!((v.greeks.vanna - g.vanna).abs() < 1e-15);
        assert!((v.greeks.charm - g.charm).abs() < 1e-15);
    }

    #[test]
    fn strategy_value_is_sign_aware() {
        let long = strategy(vec![leg(OptionRight::Put, 95.0, 3, 2.0)]);
        let short = strategy(vec![leg(OptionRight::Put, 95.0, -3, 2.0)]);
        let market = MarketInputs {
            spot: 100.0,
            rate: 0.05,
            div_yield: 0.01,
        };
        let vl = strategy_value(&long, &[0.25], &market).unwrap();
        let vs = strategy_value(&short, &[0.25], &market).unwrap();
        assert!((vl.value + vs.value).abs() < 1e-12);
        assert!((vl.greeks.delta + vs.greeks.delta).abs() < 1e-15);
        assert!((vl.greeks.theta + vs.greeks.theta).abs() < 1e-12);
    }

    #[test]
    fn errors_are_values() {
        assert_eq!(
            payoff_profile(&Strategy {
                legs: vec![],
                multiplier: 100.0
            }),
            Err(StrategyError::Empty)
        );
        let s = strategy(vec![leg(OptionRight::Call, 100.0, 1, 5.0)]);
        let market = MarketInputs {
            spot: 100.0,
            rate: 0.05,
            div_yield: 0.0,
        };
        assert_eq!(
            strategy_value(&s, &[], &market),
            Err(StrategyError::VolCountMismatch { legs: 1, vols: 0 })
        );
        let bad = strategy(vec![leg(OptionRight::Call, -1.0, 1, 5.0)]);
        assert!(matches!(
            payoff_profile(&bad),
            Err(StrategyError::Domain(_))
        ));
        assert!(matches!(
            payoff_coordinates(&s, 0.0),
            Err(StrategyError::Domain(_))
        ));
        // Kernel domain errors propagate as values.
        assert!(matches!(
            strategy_value(&s, &[f64::NAN], &market),
            Err(StrategyError::Quant(_))
        ));
    }

    #[test]
    fn outputs_serde_round_trip() {
        let s = strategy(vec![
            leg(OptionRight::Call, 100.0, 1, 4.0),
            leg(OptionRight::Put, 100.0, 1, 3.0),
        ]);
        let p = payoff_profile(&s).unwrap();
        let json = serde_json::to_string(&p).unwrap();
        let back: PayoffProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(back, p);

        let market = MarketInputs {
            spot: 100.0,
            rate: 0.05,
            div_yield: 0.0,
        };
        let v = strategy_value(&s, &[0.2, 0.2], &market).unwrap();
        let json = serde_json::to_string(&v).unwrap();
        let back: StrategyValue = serde_json::from_str(&json).unwrap();
        assert_eq!(back, v);
    }
}
