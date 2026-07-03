//! Portfolio greeks aggregation, the per-expiry GEX curve, and the
//! charm/vanna decay clock.
//!
//! Provenance: legacy `quantSuite.ts` `aggregatePortfolioGreeks` (spec 02
//! E9), `aggregateExpiryGexCurve` (E10), and `generateCharmVannaClock`
//! (E11), per `docs/spec/02-quant-suite.md`. All greeks come from
//! [`slayer_quant::black_scholes`].
//!
//! Deviations from legacy:
//! - **D2 fixed (E9).** The legacy cost basis summed `entryPrice·|qty|·mult`
//!   (absolute quantity) against a signed market value, so every short
//!   position reported `−(current + entry)` instead of `(entry − current)`
//!   P&L. [`PortfolioSummary::cost_basis`] is signed
//!   (`entry · qty · multiplier`), making `total_profit` correct for shorts.
//! - **Fabricated defaults dropped (E9, D13 family).** The legacy
//!   `strike || spot`, `(dte || 30)/365`, and `iv || 0.18` fallbacks are
//!   unrepresentable: [`Position::Option`] requires strike, time, and IV, and
//!   the contract multiplier is an explicit field, never a hardcoded 100.
//! - **Kernel-natural greek units** (theta per year, vega/vanna per unit
//!   vol, charm per year), consistent with the dealer engines; the legacy
//!   `/365` daily theta/charm are display conversions
//!   ([`slayer_quant::black_scholes::THETA_PER_DAY`]). Resolves the D1 vega
//!   ambiguity by construction. Option greeks scale by `qty · multiplier`
//!   (book-level, legacy E9 convention); stock contributes `qty` to delta.
//! - **E10 rebuilt with real expiries.** The legacy `ChainContract` had no
//!   per-contract expiry, so the curve degenerated to a single `'Front'` node
//!   (after an earlier fake `exp(−0.4·idx)` term structure was removed as
//!   fabrication). [`GexContract`] carries an explicit [`ExpiryDate`], so the
//!   curve has one honest node per expiry present in the input — buckets are
//!   never synthesized.
//! - **D21 fixed (E10).** `dominant_strike` is the strike with the largest
//!   per-strike *aggregated* |net GEX| (calls + puts summed at the strike),
//!   not the single contract with max |gex|; a strike with offsetting call
//!   and put exposure no longer outranks a genuinely dominant one. It is
//!   `None` when every strike nets to zero — never a fabricated pick.
//! - **E11 rebuilt from the kernel.** The legacy clock was a cosmetic
//!   hardcoded polynomial (`0.8 + 0.1·(h−9.5)` / `1.0 + 0.8·(h−13.5)^2.5`)
//!   with zero market inputs and hardcoded "EST" labels (D20). Here the
//!   clock samples *actual* BSM charm and vanna over a caller-supplied
//!   hours-to-expiry grid ([`session_clock_grid`] reproduces the legacy
//!   half-hour session cadence). Wall-clock/timezone mapping is the
//!   gateway's concern — time is a parameter (D20 fixed by
//!   parameterization), and the binary `isPeakDecayWindow` flag is dropped
//!   in favor of the continuous quantities themselves.

use serde::{Deserialize, Serialize};
use slayer_core::{ExpiryDate, OptionRight};
use slayer_quant::QuantError;
use slayer_quant::black_scholes::{self, BsInputs};
use thiserror::Error;

use crate::multileg::{AggregateGreeks, MarketInputs};

/// GEX scaling to $ per 1% spot move. Spec E10 `GEX_PCT_MOVE`.
pub const GEX_PCT_MOVE: f64 = 0.01;

/// Trading days per year for the intraday trading-clock year fraction
/// (spec 03 E6 `TRADING_DAYS` convention, as used by the 0DTE engine).
pub const TRADING_DAYS: f64 = 252.0;

/// Regular-session length, hours. Spec E11 (`SESSION_CLOSE_HOUR` −
/// `SESSION_OPEN_HOUR` = 16.0 − 9.5).
pub const SESSION_HOURS: f64 = 6.5;

/// Decay-clock sampling step, hours (the legacy clock's half-hour labels).
pub const DECAY_CLOCK_STEP_HOURS: f64 = 0.5;

/// Points in the default session grid: 14 half-hour samples spanning
/// [`SESSION_HOURS`] down to zero inclusive (legacy 09:30–16:00 labels).
pub const DECAY_CLOCK_POINTS: u32 = 14;

/// Errors from portfolio aggregation. Malformed inputs are values, never
/// panics.
#[derive(Debug, Error, PartialEq)]
pub enum PortfolioError {
    /// An input is outside its domain.
    #[error("portfolio domain error: {0}")]
    Domain(&'static str),
    /// Error propagated from the pricing kernel.
    #[error(transparent)]
    Quant(#[from] QuantError),
}

/// One book position. Stock and option lines carry exactly the fields they
/// need — the legacy optional strike/IV/DTE with fabricated defaults are
/// unrepresentable (see the module deviations).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Position {
    /// Shares of the underlying.
    Stock {
        /// Signed share count: positive long, negative short.
        quantity: f64,
        /// Entry price, $ per share.
        entry_price: f64,
        /// Current market price, $ per share.
        current_price: f64,
    },
    /// An option line.
    Option {
        /// Call or put.
        right: OptionRight,
        /// Strike, points.
        strike: f64,
        /// Time to expiry, years.
        t_years: f64,
        /// Implied vol, annualized decimal.
        iv: f64,
        /// Signed contract count: positive long, negative short.
        quantity: i32,
        /// Entry premium, $ per share.
        entry_price: f64,
        /// Current market premium, $ per share.
        current_price: f64,
        /// Contract multiplier, shares per contract.
        multiplier: f64,
    },
}

/// Book-level aggregate (spec E9): mark-to-market value, signed cost basis,
/// P&L, and net greeks.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PortfolioSummary {
    /// Mark-to-market value, $ (signed; shorts contribute negatively).
    pub market_value: f64,
    /// Signed cost basis, $: `Σ entry · qty · multiplier` (D2 fix).
    pub cost_basis: f64,
    /// `market_value − cost_basis`, $.
    pub total_profit: f64,
    /// Net book greeks, kernel-natural units. Options scale by
    /// `qty · multiplier`; stock adds `qty` shares to delta only.
    pub greeks: AggregateGreeks,
}

/// E9: aggregate net greeks and P&L over a book of stock and option
/// positions. An empty book aggregates to zeros — a fact, not a fallback.
pub fn aggregate_portfolio(
    positions: &[Position],
    market: &MarketInputs,
) -> Result<PortfolioSummary, PortfolioError> {
    let mut market_value = 0.0;
    let mut cost_basis = 0.0;
    let mut greeks = AggregateGreeks::ZERO;

    for position in positions {
        match *position {
            Position::Stock {
                quantity,
                entry_price,
                current_price,
            } => {
                if !(quantity.is_finite()
                    && entry_price >= 0.0
                    && entry_price.is_finite()
                    && current_price >= 0.0
                    && current_price.is_finite())
                {
                    return Err(PortfolioError::Domain(
                        "stock position fields must be finite and prices non-negative",
                    ));
                }
                market_value += current_price * quantity;
                cost_basis += entry_price * quantity;
                greeks.delta += quantity;
            }
            Position::Option {
                right,
                strike,
                t_years,
                iv,
                quantity,
                entry_price,
                current_price,
                multiplier,
            } => {
                if !(entry_price >= 0.0
                    && entry_price.is_finite()
                    && current_price >= 0.0
                    && current_price.is_finite())
                {
                    return Err(PortfolioError::Domain(
                        "option entry/current prices must be non-negative and finite",
                    ));
                }
                if !(multiplier > 0.0 && multiplier.is_finite()) {
                    return Err(PortfolioError::Domain(
                        "option multiplier must be positive and finite",
                    ));
                }
                let inputs = BsInputs {
                    spot: market.spot,
                    strike,
                    t_years,
                    vol: iv,
                    rate: market.rate,
                    div_yield: market.div_yield,
                };
                let g = black_scholes::greeks(&inputs, right)?;
                let scale = f64::from(quantity) * multiplier;
                market_value += current_price * scale;
                cost_basis += entry_price * scale;
                greeks.delta += g.delta * scale;
                greeks.gamma += g.gamma * scale;
                greeks.theta += g.theta * scale;
                greeks.vega += g.vega * scale;
                greeks.vanna += g.vanna * scale;
                greeks.charm += g.charm * scale;
            }
        }
    }

    Ok(PortfolioSummary {
        market_value,
        cost_basis,
        total_profit: market_value - cost_basis,
        greeks,
    })
}

/// One chain contract for the expiry-GEX curve (spec E10). Gamma is the
/// per-share BSM gamma as supplied by the chain — this module recomputes
/// nothing upstream and invents nothing missing.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GexContract {
    /// Expiry the contract belongs to.
    pub expiry: ExpiryDate,
    /// Strike, points.
    pub strike: f64,
    /// Call or put.
    pub right: OptionRight,
    /// BSM gamma, per share per point.
    pub gamma: f64,
    /// Open interest, contracts.
    pub open_interest: f64,
}

/// Net dealer gamma exposure for one expiry bucket (spec E10).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ExpiryGexNode {
    /// The expiry bucket.
    pub expiry: ExpiryDate,
    /// `call_gex + put_gex`, $ per 1% spot move.
    pub total_gex: f64,
    /// Σ call-side GEX (dealer-long convention, sign +1), $ per 1% move.
    pub call_gex: f64,
    /// Σ put-side GEX (dealer-short convention, sign −1), $ per 1% move.
    pub put_gex: f64,
    /// Strike with the largest per-strike aggregated |net GEX| (D21 fix);
    /// `None` when every strike in the bucket nets to zero exposure.
    pub dominant_strike: Option<f64>,
}

/// E10: net GEX per expiry bucket. Per-contract exposure is
/// `gamma · OI · multiplier · spot² · `[`GEX_PCT_MOVE`]` · sign` with sign +1
/// for calls and −1 for puts (dealers long calls / short puts). Nodes are
/// sorted ascending by expiry; an empty input yields an empty curve.
pub fn expiry_gex_curve(
    contracts: &[GexContract],
    spot: f64,
    multiplier: f64,
) -> Result<Vec<ExpiryGexNode>, PortfolioError> {
    if !(spot > 0.0 && spot.is_finite()) {
        return Err(PortfolioError::Domain("spot must be positive and finite"));
    }
    if !(multiplier > 0.0 && multiplier.is_finite()) {
        return Err(PortfolioError::Domain(
            "multiplier must be positive and finite",
        ));
    }

    // Per-contract signed GEX, sorted by (expiry, strike) so buckets and
    // per-strike runs fall out of one deterministic pass.
    let mut rows: Vec<(ExpiryDate, f64, OptionRight, f64)> = Vec::with_capacity(contracts.len());
    for c in contracts {
        if !(c.strike > 0.0 && c.strike.is_finite()) {
            return Err(PortfolioError::Domain(
                "contract strike must be positive and finite",
            ));
        }
        if !c.gamma.is_finite() {
            return Err(PortfolioError::Domain("contract gamma must be finite"));
        }
        if !(c.open_interest >= 0.0 && c.open_interest.is_finite()) {
            return Err(PortfolioError::Domain(
                "contract open interest must be non-negative and finite",
            ));
        }
        let sign = match c.right {
            OptionRight::Call => 1.0,
            OptionRight::Put => -1.0,
        };
        let gex = c.gamma * c.open_interest * multiplier * spot * spot * GEX_PCT_MOVE * sign;
        rows.push((c.expiry, c.strike, c.right, gex));
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| f64::total_cmp(&a.1, &b.1)));

    let mut curve: Vec<ExpiryGexNode> = Vec::new();
    let mut node: Option<ExpiryGexNode> = None;
    // Per-strike aggregation state within the current bucket.
    let mut run: Option<(f64, f64)> = None; // (strike, Σ gex at strike)
    let mut best: Option<(f64, f64)> = None; // (strike, |Σ gex|), strict max

    // Fold the finished strike run into the bucket's dominant-strike best.
    fn close_run(run: Option<(f64, f64)>, best: &mut Option<(f64, f64)>) {
        if let Some((strike, sum)) = run
            && sum.abs() > 0.0
            && best.is_none_or(|(_, b)| sum.abs() > b)
        {
            *best = Some((strike, sum.abs()));
        }
    }

    for (expiry, strike, right, gex) in rows {
        let flush = node.as_ref().is_some_and(|n| n.expiry != expiry);
        if flush {
            close_run(run.take(), &mut best);
            if let Some(mut n) = node.take() {
                n.dominant_strike = best.take().map(|(k, _)| k);
                curve.push(n);
            }
        }
        let n = node.get_or_insert(ExpiryGexNode {
            expiry,
            total_gex: 0.0,
            call_gex: 0.0,
            put_gex: 0.0,
            dominant_strike: None,
        });
        n.total_gex += gex;
        match right {
            OptionRight::Call => n.call_gex += gex,
            OptionRight::Put => n.put_gex += gex,
        }
        match run {
            Some((k, sum)) if k == strike => run = Some((k, sum + gex)),
            other => {
                close_run(other, &mut best);
                run = Some((strike, gex));
            }
        }
    }
    close_run(run, &mut best);
    if let Some(mut n) = node {
        n.dominant_strike = best.map(|(k, _)| k);
        curve.push(n);
    }
    Ok(curve)
}

/// The contract the decay clock is sampled for (spec E11). Every pricing
/// input is explicit.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ClockInputs {
    /// Underlying spot, points.
    pub spot: f64,
    /// Strike, points.
    pub strike: f64,
    /// Implied vol, annualized decimal.
    pub vol: f64,
    /// Continuously-compounded risk-free rate, decimal.
    pub rate: f64,
    /// Continuous dividend yield, decimal.
    pub div_yield: f64,
    /// Call or put.
    pub right: OptionRight,
}

/// One decay-clock sample: kernel charm and vanna at a given trading-hours
/// distance from expiry.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DecayClockPoint {
    /// Trading hours to expiry.
    pub hours_to_expiry: f64,
    /// Year fraction the greeks were computed at.
    pub t_years: f64,
    /// ∂Δ/∂t, per year (kernel-natural; divide by
    /// [`slayer_quant::black_scholes::THETA_PER_DAY`] for per-day).
    pub charm: f64,
    /// ∂²V/∂S∂σ, per unit vol.
    pub vanna: f64,
}

/// Trading-clock year fraction: `(hours / `[`SESSION_HOURS`]`) /`
/// [`TRADING_DAYS`]. Zero hours maps to zero years — an expired contract's
/// greeks come from the kernel's exact degenerate branch, never a floor.
#[must_use]
pub fn trading_hours_to_years(hours: f64) -> f64 {
    (hours / SESSION_HOURS) / TRADING_DAYS
}

/// The default session grid: [`DECAY_CLOCK_POINTS`] half-hour samples from
/// [`SESSION_HOURS`] down to zero, reproducing the legacy 09:30–16:00
/// half-hour cadence as hours-to-expiry (D20: no wall-clock, no timezone).
#[must_use]
pub fn session_clock_grid() -> Vec<f64> {
    (0..DECAY_CLOCK_POINTS)
        .map(|i| SESSION_HOURS - DECAY_CLOCK_STEP_HOURS * f64::from(i))
        .collect()
}

/// E11: sample kernel charm and vanna across an hours-to-expiry grid.
/// Hours must be non-negative and finite; the grid is the caller's choice
/// ([`session_clock_grid`] gives the legacy session cadence).
pub fn charm_vanna_clock(
    inputs: &ClockInputs,
    hours_to_expiry: &[f64],
) -> Result<Vec<DecayClockPoint>, PortfolioError> {
    let mut points = Vec::with_capacity(hours_to_expiry.len());
    for &hours in hours_to_expiry {
        if !(hours >= 0.0 && hours.is_finite()) {
            return Err(PortfolioError::Domain(
                "hours to expiry must be non-negative and finite",
            ));
        }
        let t_years = trading_hours_to_years(hours);
        let bs = BsInputs {
            spot: inputs.spot,
            strike: inputs.strike,
            t_years,
            vol: inputs.vol,
            rate: inputs.rate,
            div_yield: inputs.div_yield,
        };
        let g = black_scholes::greeks(&bs, inputs.right)?;
        points.push(DecayClockPoint {
            hours_to_expiry: hours,
            t_years,
            charm: g.charm,
            vanna: g.vanna,
        });
    }
    Ok(points)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const MARKET: MarketInputs = MarketInputs {
        spot: 100.0,
        rate: 0.05,
        div_yield: 0.0,
    };

    fn call_position(quantity: i32) -> Position {
        Position::Option {
            right: OptionRight::Call,
            strike: 100.0,
            t_years: 30.0 / 365.0,
            iv: 0.2,
            quantity,
            entry_price: 5.0,
            current_price: 3.0,
            multiplier: 100.0,
        }
    }

    fn put_position() -> Position {
        Position::Option {
            right: OptionRight::Put,
            strike: 95.0,
            t_years: 60.0 / 365.0,
            iv: 0.25,
            quantity: 2,
            entry_price: 1.5,
            current_price: 2.0,
            multiplier: 100.0,
        }
    }

    fn expiry(day: u8) -> ExpiryDate {
        ExpiryDate {
            year: 2026,
            month: 7,
            day,
        }
    }

    #[test]
    fn portfolio_greeks_are_additive() {
        let a = [call_position(1)];
        let b = [
            put_position(),
            Position::Stock {
                quantity: 50.0,
                entry_price: 98.0,
                current_price: 100.0,
            },
        ];
        let both: Vec<Position> = a.iter().chain(&b).copied().collect();
        let ra = aggregate_portfolio(&a, &MARKET).unwrap();
        let rb = aggregate_portfolio(&b, &MARKET).unwrap();
        let rboth = aggregate_portfolio(&both, &MARKET).unwrap();
        assert!((rboth.greeks.delta - (ra.greeks.delta + rb.greeks.delta)).abs() < 1e-9);
        assert!((rboth.greeks.gamma - (ra.greeks.gamma + rb.greeks.gamma)).abs() < 1e-9);
        assert!((rboth.greeks.vega - (ra.greeks.vega + rb.greeks.vega)).abs() < 1e-9);
        assert!((rboth.greeks.charm - (ra.greeks.charm + rb.greeks.charm)).abs() < 1e-9);
        assert!((rboth.market_value - (ra.market_value + rb.market_value)).abs() < 1e-9);
        assert!((rboth.total_profit - (ra.total_profit + rb.total_profit)).abs() < 1e-9);
    }

    #[test]
    fn option_greeks_scale_by_qty_and_multiplier() {
        let r = aggregate_portfolio(&[call_position(2)], &MARKET).unwrap();
        let g = black_scholes::greeks(
            &BsInputs {
                spot: 100.0,
                strike: 100.0,
                t_years: 30.0 / 365.0,
                vol: 0.2,
                rate: 0.05,
                div_yield: 0.0,
            },
            OptionRight::Call,
        )
        .unwrap();
        assert!((r.greeks.delta - g.delta * 200.0).abs() < 1e-12);
        assert!((r.greeks.theta - g.theta * 200.0).abs() < 1e-9);
        assert!((r.greeks.vanna - g.vanna * 200.0).abs() < 1e-9);
    }

    #[test]
    fn short_option_pnl_is_correct() {
        // D2 regression: short 1 call, entry 5, current 3 → +$200 profit
        // (legacy |qty| cost basis reported −$800).
        let r = aggregate_portfolio(&[call_position(-1)], &MARKET).unwrap();
        assert!((r.market_value - (-300.0)).abs() < 1e-12);
        assert!((r.cost_basis - (-500.0)).abs() < 1e-12);
        assert!((r.total_profit - 200.0).abs() < 1e-12);
    }

    #[test]
    fn stock_contributes_delta_only_and_empty_book_is_zero() {
        let r = aggregate_portfolio(
            &[Position::Stock {
                quantity: -25.0,
                entry_price: 100.0,
                current_price: 90.0,
            }],
            &MARKET,
        )
        .unwrap();
        assert_eq!(r.greeks.delta, -25.0);
        assert_eq!(r.greeks.gamma, 0.0);
        assert_eq!(r.greeks.vega, 0.0);
        assert!((r.total_profit - 250.0).abs() < 1e-12); // short stock gained

        let empty = aggregate_portfolio(&[], &MARKET).unwrap();
        assert_eq!(empty.market_value, 0.0);
        assert_eq!(empty.greeks, AggregateGreeks::ZERO);
    }

    #[test]
    fn expiry_gex_buckets_signed_and_sorted() {
        let contracts = [
            GexContract {
                expiry: expiry(17),
                strike: 100.0,
                right: OptionRight::Put,
                gamma: 0.02,
                open_interest: 1000.0,
            },
            GexContract {
                expiry: expiry(10),
                strike: 100.0,
                right: OptionRight::Call,
                gamma: 0.02,
                open_interest: 1000.0,
            },
        ];
        let curve = expiry_gex_curve(&contracts, 100.0, 100.0).unwrap();
        assert_eq!(curve.len(), 2);
        // Sorted ascending by expiry.
        assert_eq!(curve[0].expiry, expiry(10));
        assert_eq!(curve[1].expiry, expiry(17));
        // gex = gamma·OI·mult·spot²·0.01·sign = 0.02·1000·100·10000·0.01 = 2e5.
        assert!((curve[0].call_gex - 200_000.0).abs() < 1e-6);
        assert_eq!(curve[0].put_gex, 0.0);
        assert!((curve[0].total_gex - 200_000.0).abs() < 1e-6);
        assert!((curve[1].put_gex - (-200_000.0)).abs() < 1e-6);
        assert!((curve[1].total_gex - (-200_000.0)).abs() < 1e-6);
        assert_eq!(expiry_gex_curve(&[], 100.0, 100.0).unwrap(), vec![]);
    }

    #[test]
    fn dominant_strike_aggregates_per_strike() {
        // D21 regression: strike 100 has the single largest |contract gex|
        // (call +8e5) but nets to +2e5 against its put (−6e5); strike 105
        // nets +5e5 and must win under per-strike aggregation.
        let contracts = [
            GexContract {
                expiry: expiry(10),
                strike: 100.0,
                right: OptionRight::Call,
                gamma: 0.08,
                open_interest: 1000.0,
            },
            GexContract {
                expiry: expiry(10),
                strike: 100.0,
                right: OptionRight::Put,
                gamma: 0.06,
                open_interest: 1000.0,
            },
            GexContract {
                expiry: expiry(10),
                strike: 105.0,
                right: OptionRight::Call,
                gamma: 0.05,
                open_interest: 1000.0,
            },
        ];
        let curve = expiry_gex_curve(&contracts, 100.0, 100.0).unwrap();
        assert_eq!(curve.len(), 1);
        assert_eq!(curve[0].dominant_strike, Some(105.0));

        // All-zero exposure: no dominant strike is fabricated.
        let flat = [GexContract {
            expiry: expiry(10),
            strike: 100.0,
            right: OptionRight::Call,
            gamma: 0.0,
            open_interest: 1000.0,
        }];
        let curve = expiry_gex_curve(&flat, 100.0, 100.0).unwrap();
        assert_eq!(curve[0].dominant_strike, None);
    }

    #[test]
    fn decay_clock_charm_grows_near_expiry_for_otm() {
        // 1% OTM call: charm magnitude must be larger 1 trading hour from
        // expiry than 6.5 hours out.
        let inputs = ClockInputs {
            spot: 100.0,
            strike: 101.0,
            vol: 0.2,
            rate: 0.0,
            div_yield: 0.0,
            right: OptionRight::Call,
        };
        let pts = charm_vanna_clock(&inputs, &[SESSION_HOURS, 1.0]).unwrap();
        assert_eq!(pts.len(), 2);
        assert!(
            pts[1].charm.abs() > pts[0].charm.abs(),
            "OTM charm must accelerate into expiry: {} vs {}",
            pts[1].charm,
            pts[0].charm
        );
        assert!(pts.iter().all(|p| p.vanna.is_finite()));
    }

    #[test]
    fn decay_clock_matches_kernel_and_expires_honestly() {
        let inputs = ClockInputs {
            spot: 100.0,
            strike: 101.0,
            vol: 0.2,
            rate: 0.03,
            div_yield: 0.01,
            right: OptionRight::Put,
        };
        let pts = charm_vanna_clock(&inputs, &[3.25, 0.0]).unwrap();
        let g = black_scholes::greeks(
            &BsInputs {
                spot: 100.0,
                strike: 101.0,
                t_years: trading_hours_to_years(3.25),
                vol: 0.2,
                rate: 0.03,
                div_yield: 0.01,
            },
            OptionRight::Put,
        )
        .unwrap();
        assert_eq!(pts[0].charm, g.charm);
        assert_eq!(pts[0].vanna, g.vanna);
        // Expired: the kernel's degenerate branch, not a floored blow-up.
        assert_eq!(pts[1].t_years, 0.0);
        assert_eq!(pts[1].charm, 0.0);
        assert_eq!(pts[1].vanna, 0.0);
    }

    #[test]
    fn session_grid_covers_legacy_cadence() {
        let grid = session_clock_grid();
        assert_eq!(grid.len(), 14);
        assert_eq!(grid[0], SESSION_HOURS);
        assert_eq!(grid[13], 0.0);
        for pair in grid.windows(2) {
            assert!((pair[0] - pair[1] - DECAY_CLOCK_STEP_HOURS).abs() < 1e-12);
        }
    }

    #[test]
    fn errors_are_values() {
        // Fabrication-proof domains.
        assert!(matches!(
            aggregate_portfolio(
                &[Position::Stock {
                    quantity: f64::NAN,
                    entry_price: 1.0,
                    current_price: 1.0
                }],
                &MARKET
            ),
            Err(PortfolioError::Domain(_))
        ));
        // Kernel domain violations propagate as values.
        let bad_option = Position::Option {
            right: OptionRight::Call,
            strike: -5.0,
            t_years: 0.1,
            iv: 0.2,
            quantity: 1,
            entry_price: 1.0,
            current_price: 1.0,
            multiplier: 100.0,
        };
        assert!(matches!(
            aggregate_portfolio(&[bad_option], &MARKET),
            Err(PortfolioError::Quant(_))
        ));
        assert!(matches!(
            expiry_gex_curve(&[], 0.0, 100.0),
            Err(PortfolioError::Domain(_))
        ));
        let inputs = ClockInputs {
            spot: 100.0,
            strike: 100.0,
            vol: 0.2,
            rate: 0.0,
            div_yield: 0.0,
            right: OptionRight::Call,
        };
        assert!(matches!(
            charm_vanna_clock(&inputs, &[-1.0]),
            Err(PortfolioError::Domain(_))
        ));
    }

    #[test]
    fn outputs_serde_round_trip() {
        let r = aggregate_portfolio(&[call_position(1), put_position()], &MARKET).unwrap();
        let json = serde_json::to_string(&r).unwrap();
        let back: PortfolioSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);

        let contracts = [GexContract {
            expiry: expiry(10),
            strike: 100.0,
            right: OptionRight::Call,
            gamma: 0.02,
            open_interest: 500.0,
        }];
        let curve = expiry_gex_curve(&contracts, 100.0, 100.0).unwrap();
        let json = serde_json::to_string(&curve).unwrap();
        let back: Vec<ExpiryGexNode> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, curve);

        let inputs = ClockInputs {
            spot: 100.0,
            strike: 101.0,
            vol: 0.2,
            rate: 0.0,
            div_yield: 0.0,
            right: OptionRight::Call,
        };
        let pts = charm_vanna_clock(&inputs, &session_clock_grid()).unwrap();
        let json = serde_json::to_string(&pts).unwrap();
        let back: Vec<DecayClockPoint> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, pts);
    }
}
