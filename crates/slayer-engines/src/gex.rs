//! Dealer Positioning Engine — gamma/delta/vanna/charm structure from an
//! [`OptionChain`].
//!
//! This is the flagship structural engine (spec `docs/spec/01-skyvision-v11.md`
//! E8, `computeDealerInventory` + skyQuantCore `netGexStrike`/`netVannaStrike`/
//! `netCharmStrike`/`gammaFlipSpot`/`expectedMove`). Given a normalized option
//! chain and a spot, it resolves per-strike signed exposures, net/gross
//! aggregates, the cumulative-GEX gamma flip, call/put gamma walls, a directionless
//! Dealer State Index, and an expected move.
//!
//! Per-strike exposures use the spec's exact scaling with `sign = +1` for calls
//! and `−1` for puts (the contract multiplier is read from
//! [`OptionChain::multiplier`], not hard-coded):
//!
//! - `GEX   = gamma · OI · multiplier · spot² · PCT_MOVE_SCALE · sign`
//! - `DEX   = delta · OI · multiplier · spot · sign`
//! - `VEX   = vanna · OI · multiplier · spot · PCT_MOVE_SCALE · sign`
//! - `Charm = charm · OI · multiplier · sign`
//!
//! Greeks are taken from the provider ([`OptionQuote::greeks`]) when present and
//! otherwise computed from the quote's implied vol via
//! [`slayer_quant::black_scholes`]. Because the provider [`Greeks`] set carries
//! only first-order greeks, vanna and charm are *always* sourced from the kernel
//! and are therefore available only for quotes that carry an IV. Quotes that
//! carry neither provider greeks nor an IV are excluded and counted in
//! [`Coverage`] — never invented.
//!
//! # State doctrine
//!
//! The gamma flip and each wall ship a [`Readout`] (binary state resolved from a
//! continuous score through a named [`HysteresisBand`]); see `docs/ARCHITECTURE.md`
//! §2. The Dealer State Index is *descriptive, signed* data — a score in
//! `[−1, 1]`, not a readout — per the same doctrine (directional data is data,
//! not state).
//!
//! # Deviations from legacy
//!
//! - **D4** — VEX is defined from **vanna** only (`vanna·OI·mult·spot·0.01·sign`);
//!   the legacy skyScore fallback mixed a vega-based form into the same field.
//!   Provider `vega` is never used for VEX here.
//! - **Fabricated gamma flip** (E8 `FLIP_FALLBACK_MULT = spot·0.995`) — dropped.
//!   When the cumulative GEX profile has no zero-crossing the flip is
//!   [`Option::None`] and [`GexStructure::flip_readout`] resolves `Inactive`; no
//!   `spot·0.995` is invented.
//! - **Fabricated walls** (E8 `CALL/PUT_WALL_FALLBACK_MULT = spot·1.015/0.985`) —
//!   dropped. A wall is `Some` only when a real dominant strike exists on that
//!   side; otherwise [`Option::None`].
//! - **Fabricated ATM IV** (E8 `DEFAULT_ATM_IV = 0.15`) — dropped. Expected move
//!   is [`Option::None`] when no quote carries an IV; an empty chain is a
//!   [`GexError`], never a `0.15` guess.
//! - **DSI direction coupling** (E8 `DSI = 0.50·dir·e_GEX + …`) — dropped. The
//!   engine reports the dealer *structure* itself (directionless), so
//!   [`GexStructure::dsi`] is a signed score in `[−1, 1]` independent of any
//!   trade direction.
//! - **Charm unit** — charm exposure uses the kernel's per-year charm (E3b),
//!   consistent with GEX/DEX/VEX all using kernel-natural greeks, rather than the
//!   legacy daily (`/365`) charm of E3a; mixing conventions was itself a latent
//!   unit defect.
//! - **D7** context — this engine performs no strike auto-selection; it consumes
//!   exactly the chain it is handed, so the legacy strike-step mismatch cannot
//!   arise here.

use serde::{Deserialize, Serialize};
use slayer_core::{HysteresisBand, OptionChain, OptionRight, Readout};
use slayer_quant::black_scholes::{BsInputs, greeks};
use thiserror::Error;

/// Per-1% scaling shared by GEX and VEX so both read as exposure per 1% move in
/// spot (GEX) or IV (VEX). Spec E8 constant `PCT_MOVE_SCALE`.
const PCT_MOVE_SCALE: f64 = 0.01;

/// tanh steepness applied to each `net/gross` exposure ratio when forming the
/// Dealer State Index sub-scores. Spec E8 constant `EXPOSURE_TANH_K`.
const EXPOSURE_TANH_K: f64 = 3.0;

/// DSI weight on the gamma-exposure sub-score. Spec E8 `DSI_W_GEX`.
const DSI_W_GEX: f64 = 0.50;
/// DSI weight on the delta-exposure sub-score. Spec E8 `DSI_W_DEX`.
const DSI_W_DEX: f64 = 0.30;
/// DSI weight on the vanna-exposure sub-score. Spec E8 `DSI_W_VEX`.
/// The three DSI weights sum to `1.0`.
const DSI_W_VEX: f64 = 0.20;

/// Minimum expected-move fraction; the expected move never reads below this.
/// Spec E8 `EM_PCT_FLOOR`.
const EM_PCT_FLOOR: f64 = 0.0005;
/// Minimum year-fraction used inside the expected-move `√T`. Spec E8
/// `EM_TAU_FLOOR`.
const EM_TAU_FLOOR: f64 = 0.0001;

/// Minimum number of distinct strikes required before a gamma flip can be
/// resolved. Spec E8 gamma-flip step 2 ("need ≥ 2 unique strikes else null").
const MIN_UNIQUE_STRIKES_FOR_FLIP: usize = 2;

/// Default time to expiry when the caller supplies none: one calendar day as an
/// ACT/365 year fraction. Spec E8 `DEFAULT_DTE_DAYS = 1`.
pub const DEFAULT_T_YEARS: f64 = 1.0 / 365.0;

/// Default continuously-compounded risk-free rate for the BS greek fallback.
/// Spec E3a `DEFAULT_RISK_FREE_RATE`.
pub const DEFAULT_RISK_FREE_RATE: f64 = 0.05;

/// Default continuous dividend yield for the BS greek fallback. Spec E3a
/// `DEFAULT_DIV_YIELD`.
pub const DEFAULT_DIV_YIELD: f64 = 0.0;

/// Wall dominance (its `|GEX|` share of gross GEX) at or above which the wall
/// readout activates. Rebuild-chosen — legacy carried only a boolean
/// `wallsConfident`; a single strike holding ≥ 15% of gross gamma is a
/// structurally dominant wall.
const WALL_DOMINANCE_ACTIVATE: f64 = 0.15;
/// Wall dominance at or below which the wall readout deactivates.
const WALL_DOMINANCE_DEACTIVATE: f64 = 0.10;
/// Hysteresis band resolving wall dominance into a [`Readout`].
const WALL_DOMINANCE_BAND: HysteresisBand =
    HysteresisBand::new(WALL_DOMINANCE_ACTIVATE, WALL_DOMINANCE_DEACTIVATE);

/// Flip-quality score (the cumulative-GEX swing through the crossing, normalized
/// by the peak `|cumulative GEX|`) at or above which the flip readout activates.
/// Rebuild-chosen — legacy carried only a boolean `gammaFlipConfident`; a
/// crossing whose flipping strike carries ≥ 30% of the peak cumulative imbalance
/// is a decisive flip.
const FLIP_QUALITY_ACTIVATE: f64 = 0.30;
/// Flip-quality score at or below which the flip readout deactivates.
const FLIP_QUALITY_DEACTIVATE: f64 = 0.20;
/// Hysteresis band resolving flip quality into a [`Readout`].
const FLIP_QUALITY_BAND: HysteresisBand =
    HysteresisBand::new(FLIP_QUALITY_ACTIVATE, FLIP_QUALITY_DEACTIVATE);

/// Errors producible by [`analyze`]. Never a panic on market data — malformed
/// inputs are values.
#[derive(Debug, Error, PartialEq)]
pub enum GexError {
    /// The chain carried no quotes at all.
    #[error("option chain has no quotes")]
    EmptyChain,
    /// Spot was non-finite or non-positive.
    #[error("spot must be finite and positive, got {0}")]
    InvalidSpot(f64),
    /// Contract multiplier was non-finite or non-positive.
    #[error("contract multiplier must be finite and positive, got {0}")]
    InvalidMultiplier(f64),
    /// Time to expiry was non-finite or negative.
    #[error("time to expiry must be finite and non-negative, got {0}")]
    InvalidTime(f64),
    /// Risk-free rate or dividend yield was non-finite.
    #[error("rate and dividend yield must be finite")]
    NonFiniteParams,
}

/// Pricing context for the Black–Scholes greek fallback and the expected move.
///
/// The engine is pure: time is a parameter, never a clock read. `t_years` is the
/// common time to expiry applied both when computing kernel greeks for quotes
/// that lack provider greeks and when scaling the expected move. It models a
/// single-expiry dealer snapshot; a caller with a multi-expiry chain should
/// split it by expiry.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GexParams {
    /// Time to expiry in ACT/365 years (`≥ 0`).
    pub t_years: f64,
    /// Continuously-compounded risk-free rate, decimal.
    pub rate: f64,
    /// Continuous dividend yield, decimal.
    pub div_yield: f64,
}

impl GexParams {
    /// Build a pricing context.
    #[must_use]
    pub const fn new(t_years: f64, rate: f64, div_yield: f64) -> Self {
        Self { t_years, rate, div_yield }
    }

    fn validate(&self) -> Result<(), GexError> {
        if !self.t_years.is_finite() || self.t_years < 0.0 {
            return Err(GexError::InvalidTime(self.t_years));
        }
        if !self.rate.is_finite() || !self.div_yield.is_finite() {
            return Err(GexError::NonFiniteParams);
        }
        Ok(())
    }
}

impl Default for GexParams {
    /// One-day expiry at the spec's default rate and zero dividend yield.
    fn default() -> Self {
        Self::new(DEFAULT_T_YEARS, DEFAULT_RISK_FREE_RATE, DEFAULT_DIV_YIELD)
    }
}

/// How much of the chain contributed to the exposures. Nothing is invented for
/// the quotes that could not be priced — they are counted here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Coverage {
    /// Total quotes in the chain.
    pub total: usize,
    /// Quotes that contributed to GEX/DEX (had provider greeks or an IV, and a
    /// usable strike).
    pub resolved: usize,
    /// Quotes excluded because they carried neither provider greeks nor an IV
    /// (or had a non-finite / non-positive strike).
    pub excluded: usize,
    /// Quotes that additionally contributed to VEX/charm (carried an IV, so
    /// vanna and charm could be computed by the kernel).
    pub vanna_resolved: usize,
}

/// One merged strike row of the dealer ladder, sorted position filled by
/// [`analyze`]. Signed exposures merge call and put contributions at the strike;
/// `call_gex`/`put_gex` keep the split for wall detection and ladder rendering.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StrikeExposure {
    /// Strike price, underlying points.
    pub strike: f64,
    /// Net signed gamma exposure at this strike (`call_gex + put_gex`).
    pub gex: f64,
    /// Net signed delta exposure at this strike.
    pub dex: f64,
    /// Net signed vanna exposure at this strike.
    pub vex: f64,
    /// Net signed charm exposure at this strike.
    pub charm: f64,
    /// Signed gamma exposure of the call side only.
    pub call_gex: f64,
    /// Signed gamma exposure of the put side only.
    pub put_gex: f64,
}

impl StrikeExposure {
    fn empty(strike: f64) -> Self {
        Self { strike, gex: 0.0, dex: 0.0, vex: 0.0, charm: 0.0, call_gex: 0.0, put_gex: 0.0 }
    }
}

/// A gamma wall: the dominant `|GEX|` strike on one side of spot.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Wall {
    /// Wall strike, underlying points.
    pub strike: f64,
    /// Signed gamma exposure concentrated at the wall (call side for the call
    /// wall, put side for the put wall).
    pub gex: f64,
    /// Dominance readout: score is `|gex| / gross_gex ∈ [0, 1]` resolved through
    /// [`WALL_DOMINANCE_BAND`].
    pub dominance: Readout,
}

/// Full dealer-positioning structure for one chain snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GexStructure {
    /// Underlying spot the exposures were computed at, points.
    pub spot: f64,
    /// Net signed gamma exposure over the chain.
    pub net_gex: f64,
    /// Gross (sum of `|·|`) gamma exposure over the chain.
    pub gross_gex: f64,
    /// Net signed delta exposure.
    pub net_dex: f64,
    /// Gross delta exposure.
    pub gross_dex: f64,
    /// Net signed vanna exposure.
    pub net_vex: f64,
    /// Gross vanna exposure.
    pub gross_vex: f64,
    /// Net signed charm exposure (per-year charm convention).
    pub net_charm: f64,
    /// Gross charm exposure.
    pub gross_charm: f64,
    /// Per-strike ladder, ascending by strike, call/put merged.
    pub profile: Vec<StrikeExposure>,
    /// Gamma-flip price from the cumulative-GEX zero-crossing, or `None` when
    /// the profile has no crossing (no fabricated fallback).
    pub gamma_flip: Option<f64>,
    /// Readout over the flip: score is the crossing quality (swing through the
    /// crossing normalized by peak `|cumulative GEX|`), resolved through
    /// [`FLIP_QUALITY_BAND`]. Resolves `Inactive` (score `0`) when no flip.
    pub flip_readout: Readout,
    /// Dominant call-gamma wall at or above spot, or `None`.
    pub call_wall: Option<Wall>,
    /// Dominant put-gamma wall at or below spot, or `None`.
    pub put_wall: Option<Wall>,
    /// Gamma-exposure DSI sub-score, `tanh(k·net/gross) ∈ [−1, 1]`.
    pub e_gex: f64,
    /// Delta-exposure DSI sub-score.
    pub e_dex: f64,
    /// Vanna-exposure DSI sub-score.
    pub e_vex: f64,
    /// Dealer State Index, directionless, `∈ [−1, 1]`.
    pub dsi: f64,
    /// DSI mapped to `[0, 1]` as `(dsi + 1) / 2`.
    pub dealer01: f64,
    /// Expected move as a fraction of spot (`ATM IV · √T`, floored), or `None`
    /// when no quote carries an IV.
    pub expected_move_pct: Option<f64>,
    /// Expected move in underlying points (`expected_move_pct · spot`), or
    /// `None`.
    pub expected_move_points: Option<f64>,
    /// Chain coverage accounting.
    pub coverage: Coverage,
}

/// Which side of spot a wall search runs on.
#[derive(Clone, Copy)]
enum WallSide {
    Call,
    Put,
}

/// A resolved gamma flip and its crossing quality.
struct Flip {
    price: f64,
    quality: f64,
}

/// Resolve the full dealer-positioning structure for `chain` under `params`.
///
/// # Errors
///
/// Returns [`GexError::EmptyChain`] for a chain with no quotes,
/// [`GexError::InvalidSpot`] / [`GexError::InvalidMultiplier`] for non-finite or
/// non-positive spot/multiplier, and [`GexError::InvalidTime`] /
/// [`GexError::NonFiniteParams`] for a malformed pricing context.
pub fn analyze(chain: &OptionChain, params: &GexParams) -> Result<GexStructure, GexError> {
    params.validate()?;

    let spot = chain.spot;
    if !spot.is_finite() || spot <= 0.0 {
        return Err(GexError::InvalidSpot(spot));
    }
    let mult = chain.multiplier;
    if !mult.is_finite() || mult <= 0.0 {
        return Err(GexError::InvalidMultiplier(mult));
    }
    if chain.quotes.is_empty() {
        return Err(GexError::EmptyChain);
    }

    // Exposure scale factors, shared across contracts.
    let f_gex = mult * spot * spot * PCT_MOVE_SCALE;
    let f_dex = mult * spot;
    let f_vex = mult * spot * PCT_MOVE_SCALE;
    let f_charm = mult;

    let mut net_gex = 0.0;
    let mut gross_gex = 0.0;
    let mut net_dex = 0.0;
    let mut gross_dex = 0.0;
    let mut net_vex = 0.0;
    let mut gross_vex = 0.0;
    let mut net_charm = 0.0;
    let mut gross_charm = 0.0;

    let total = chain.quotes.len();
    let mut excluded = 0usize;
    let mut resolved = 0usize;
    let mut vanna_resolved = 0usize;
    let mut rows: Vec<StrikeExposure> = Vec::new();

    for q in &chain.quotes {
        if !q.strike.is_finite() || q.strike <= 0.0 {
            excluded += 1;
            continue;
        }

        let sign = match q.right {
            OptionRight::Call => 1.0,
            OptionRight::Put => -1.0,
        };
        let oi = q.open_interest as f64;

        let provider = q.greeks;
        let bs = q
            .iv
            .filter(|iv| iv.is_finite() && *iv >= 0.0)
            .and_then(|iv| {
                let inputs = BsInputs {
                    spot,
                    strike: q.strike,
                    t_years: params.t_years,
                    vol: iv,
                    rate: params.rate,
                    div_yield: params.div_yield,
                };
                greeks(&inputs, q.right).ok()
            });

        if provider.is_none() && bs.is_none() {
            excluded += 1;
            continue;
        }
        resolved += 1;
        if bs.is_some() {
            vanna_resolved += 1;
        }

        // Delta/gamma: provider-preferred, kernel fallback. Vanna/charm: kernel
        // only (never provider-supplied) — D4: VEX is vanna-based, never vega.
        let delta = provider.map(|g| g.delta).or(bs.map(|g| g.delta));
        let gamma = provider.map(|g| g.gamma).or(bs.map(|g| g.gamma));
        let vanna = bs.map(|g| g.vanna);
        let charm = bs.map(|g| g.charm);

        let gex = finite(gamma.map(|g| g * oi * f_gex * sign));
        let dex = finite(delta.map(|d| d * oi * f_dex * sign));
        let vex = finite(vanna.map(|v| v * oi * f_vex * sign));
        let charm_ex = finite(charm.map(|c| c * oi * f_charm * sign));

        if let Some(v) = gex {
            net_gex += v;
            gross_gex += v.abs();
        }
        if let Some(v) = dex {
            net_dex += v;
            gross_dex += v.abs();
        }
        if let Some(v) = vex {
            net_vex += v;
            gross_vex += v.abs();
        }
        if let Some(v) = charm_ex {
            net_charm += v;
            gross_charm += v.abs();
        }

        let idx = match rows.iter().position(|r| r.strike == q.strike) {
            Some(i) => i,
            None => {
                rows.push(StrikeExposure::empty(q.strike));
                rows.len() - 1
            }
        };
        let row = &mut rows[idx];
        let gex_v = gex.unwrap_or(0.0);
        row.gex += gex_v;
        row.dex += dex.unwrap_or(0.0);
        row.vex += vex.unwrap_or(0.0);
        row.charm += charm_ex.unwrap_or(0.0);
        match q.right {
            OptionRight::Call => row.call_gex += gex_v,
            OptionRight::Put => row.put_gex += gex_v,
        }
    }

    rows.sort_by(|a, b| a.strike.total_cmp(&b.strike));

    let flip = gamma_flip(&rows);
    let (gamma_flip_price, flip_score) = match &flip {
        Some(f) => (Some(f.price), f.quality),
        None => (None, 0.0),
    };
    let flip_readout = Readout::resolve(&FLIP_QUALITY_BAND, flip_score);

    let call_wall = wall(&rows, spot, gross_gex, WallSide::Call);
    let put_wall = wall(&rows, spot, gross_gex, WallSide::Put);

    let e_gex = exposure_score(net_gex, gross_gex);
    let e_dex = exposure_score(net_dex, gross_dex);
    let e_vex = exposure_score(net_vex, gross_vex);
    let dsi = DSI_W_GEX * e_gex + DSI_W_DEX * e_dex + DSI_W_VEX * e_vex;
    let dealer01 = (dsi + 1.0) / 2.0;

    let atm_iv = nearest_atm_iv(chain, spot);
    let expected_move_pct =
        atm_iv.map(|iv| (iv * params.t_years.max(EM_TAU_FLOOR).sqrt()).max(EM_PCT_FLOOR));
    let expected_move_points = expected_move_pct.map(|p| p * spot);

    Ok(GexStructure {
        spot,
        net_gex,
        gross_gex,
        net_dex,
        gross_dex,
        net_vex,
        gross_vex,
        net_charm,
        gross_charm,
        profile: rows,
        gamma_flip: gamma_flip_price,
        flip_readout,
        call_wall,
        put_wall,
        e_gex,
        e_dex,
        e_vex,
        dsi,
        dealer01,
        expected_move_pct,
        expected_move_points,
        coverage: Coverage { total, resolved, excluded, vanna_resolved },
    })
}

/// Keep a value only when it is finite; guards against non-finite provider
/// greeks or overflow polluting the aggregates.
fn finite(v: Option<f64>) -> Option<f64> {
    v.filter(|x| x.is_finite())
}

/// DSI sub-score `tanh(k·net/gross)`, zero when there is no gross exposure.
fn exposure_score(net: f64, gross: f64) -> f64 {
    if gross > 0.0 { (EXPOSURE_TANH_K * net / gross).tanh() } else { 0.0 }
}

/// Cumulative-GEX gamma flip (SqueezeMetrics convention). Returns the crossing
/// price and its quality, or `None` when there is no gamma structure or no
/// zero-crossing.
fn gamma_flip(rows: &[StrikeExposure]) -> Option<Flip> {
    if rows.len() < MIN_UNIQUE_STRIKES_FOR_FLIP {
        return None;
    }
    let mut cums = Vec::with_capacity(rows.len());
    let mut acc = 0.0;
    for r in rows {
        acc += r.gex;
        cums.push(acc);
    }
    let peak = cums.iter().fold(0.0_f64, |m, &c| m.max(c.abs()));
    if peak <= 0.0 {
        // No gamma structure at all — a flip would be invented, not measured.
        return None;
    }
    for (i, row) in rows.iter().enumerate() {
        let ci = cums[i];
        if ci == 0.0 {
            let cj = if i + 1 < rows.len() { cums[i + 1] } else { cums[i - 1] };
            let quality = ((cj - ci).abs() / peak).clamp(0.0, 1.0);
            return Some(Flip { price: row.strike, quality });
        }
        if i + 1 < rows.len() {
            let cj = cums[i + 1];
            if ci * cj < 0.0 {
                let x0 = row.strike;
                let x1 = rows[i + 1].strike;
                let price = x0 - ci * (x1 - x0) / (cj - ci);
                let quality = ((cj - ci).abs() / peak).clamp(0.0, 1.0);
                return Some(Flip { price, quality });
            }
        }
    }
    None
}

/// Dominant `|GEX|` wall on one side of spot, or `None` when no strike on that
/// side carries any gamma. On ties the higher strike wins (rows are ascending).
fn wall(rows: &[StrikeExposure], spot: f64, gross_gex: f64, side: WallSide) -> Option<Wall> {
    let (strike, gex) = rows
        .iter()
        .filter(|r| match side {
            WallSide::Call => r.strike >= spot,
            WallSide::Put => r.strike <= spot,
        })
        .map(|r| {
            let g = match side {
                WallSide::Call => r.call_gex,
                WallSide::Put => r.put_gex,
            };
            (r.strike, g)
        })
        .filter(|(_, g)| g.abs() > 0.0)
        .max_by(|(_, a), (_, b)| a.abs().total_cmp(&b.abs()))?;

    let dominance_score =
        if gross_gex > 0.0 { (gex.abs() / gross_gex).clamp(0.0, 1.0) } else { 0.0 };
    let dominance = Readout::resolve(&WALL_DOMINANCE_BAND, dominance_score);
    Some(Wall { strike, gex, dominance })
}

/// IV of the quote whose strike is nearest spot (ties broken by lower strike
/// then call-before-put), among quotes carrying a usable IV. `None` when no
/// quote carries an IV — never the legacy `0.15` guess.
fn nearest_atm_iv(chain: &OptionChain, spot: f64) -> Option<f64> {
    chain
        .quotes
        .iter()
        .filter(|q| q.strike.is_finite() && q.strike > 0.0)
        .filter_map(|q| q.iv.filter(|iv| iv.is_finite() && *iv >= 0.0).map(|iv| (q, iv)))
        .min_by(|(a, _), (b, _)| {
            (a.strike - spot)
                .abs()
                .total_cmp(&(b.strike - spot).abs())
                .then(a.strike.total_cmp(&b.strike))
                .then(right_ord(a.right).cmp(&right_ord(b.right)))
        })
        .map(|(_, iv)| iv)
}

/// Stable tie-break ordinal: calls before puts.
fn right_ord(r: OptionRight) -> u8 {
    match r {
        OptionRight::Call => 0,
        OptionRight::Put => 1,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use slayer_core::{ExpiryDate, Greeks, OptionQuote, Symbol, TsMillis};

    const RATE: f64 = 0.05;

    fn sym() -> Symbol {
        Symbol::new("SPX").unwrap()
    }

    fn greek(delta: f64, gamma: f64) -> Greeks {
        Greeks { delta, gamma, theta: 0.0, vega: 0.0 }
    }

    fn quote(
        strike: f64,
        right: OptionRight,
        oi: u64,
        iv: Option<f64>,
        g: Option<Greeks>,
    ) -> OptionQuote {
        OptionQuote {
            strike,
            right,
            expiry: ExpiryDate { year: 2026, month: 7, day: 17 },
            bid: None,
            ask: None,
            volume: 0,
            open_interest: oi,
            iv,
            greeks: g,
        }
    }

    fn chain(spot: f64, mult: f64, quotes: Vec<OptionQuote>) -> OptionChain {
        OptionChain { underlying: sym(), spot, ts: TsMillis(0), multiplier: mult, quotes }
    }

    fn params() -> GexParams {
        GexParams::new(0.5, RATE, 0.0)
    }

    #[test]
    fn symmetric_chain_flips_at_center_via_interpolation() {
        // Per-strike net GEX: 90:-1000, 95:-1000, 105:+4000, 110:+2000
        // cumulative: -1000, -2000, +2000, +4000 -> crossing 95..105 at 100.
        let qs = vec![
            quote(90.0, OptionRight::Put, 100, None, Some(greek(-0.5, 0.001))),
            quote(95.0, OptionRight::Put, 100, None, Some(greek(-0.5, 0.001))),
            quote(105.0, OptionRight::Call, 400, None, Some(greek(0.5, 0.001))),
            quote(110.0, OptionRight::Call, 200, None, Some(greek(0.5, 0.001))),
        ];
        let r = analyze(&chain(100.0, 100.0, qs), &params()).unwrap();
        assert!((r.gamma_flip.unwrap() - 100.0).abs() < 1e-9);
        assert!(r.flip_readout.state.is_active());
        assert!((r.flip_readout.score - 1.0).abs() < 1e-12);
        assert!((r.net_gex - 4000.0).abs() < 1e-6);
        assert!((r.gross_gex - 8000.0).abs() < 1e-6);
    }

    #[test]
    fn no_crossing_yields_no_flip_and_inactive_readout() {
        let qs = vec![
            quote(105.0, OptionRight::Call, 100, None, Some(greek(0.5, 0.01))),
            quote(110.0, OptionRight::Call, 100, None, Some(greek(0.5, 0.01))),
        ];
        let r = analyze(&chain(100.0, 100.0, qs), &params()).unwrap();
        assert!(r.gamma_flip.is_none());
        assert!(!r.flip_readout.state.is_active());
        assert!((r.flip_readout.score - 0.0).abs() < 1e-12);
    }

    #[test]
    fn single_dominant_call_is_the_call_wall_with_full_dominance() {
        let qs = vec![quote(110.0, OptionRight::Call, 500, None, Some(greek(0.5, 0.002)))];
        let r = analyze(&chain(100.0, 100.0, qs), &GexParams::default()).unwrap();
        let cw = r.call_wall.unwrap();
        assert!((cw.strike - 110.0).abs() < 1e-12);
        assert!((cw.dominance.score - 1.0).abs() < 1e-12);
        assert!(cw.dominance.state.is_active());
        // No strike at or below spot carries gamma -> no fabricated put wall.
        assert!(r.put_wall.is_none());
    }

    #[test]
    fn puts_only_below_spot_give_a_put_wall_and_no_call_wall() {
        let qs = vec![
            quote(90.0, OptionRight::Put, 100, None, Some(greek(-0.5, 0.01))),
            quote(95.0, OptionRight::Put, 100, None, Some(greek(-0.5, 0.01))),
        ];
        let r = analyze(&chain(100.0, 100.0, qs), &params()).unwrap();
        assert!(r.call_wall.is_none());
        let pw = r.put_wall.unwrap();
        // Equal |GEX| -> higher strike wins the tie.
        assert!((pw.strike - 95.0).abs() < 1e-12);
        // Each put gex = -0.01*100*10000 = -10000; gross = 20000 -> 0.5 share.
        assert!((pw.dominance.score - 0.5).abs() < 1e-12);
        assert!(pw.dominance.state.is_active());
    }

    #[test]
    fn all_put_chain_has_negative_net_gex_all_call_positive() {
        let puts = vec![
            quote(90.0, OptionRight::Put, 100, None, Some(greek(-0.5, 0.01))),
            quote(100.0, OptionRight::Put, 100, None, Some(greek(-0.5, 0.01))),
        ];
        let calls = vec![
            quote(100.0, OptionRight::Call, 100, None, Some(greek(0.5, 0.01))),
            quote(110.0, OptionRight::Call, 100, None, Some(greek(0.5, 0.01))),
        ];
        let rp = analyze(&chain(100.0, 100.0, puts), &params()).unwrap();
        let rc = analyze(&chain(100.0, 100.0, calls), &params()).unwrap();
        assert!(rp.net_gex < 0.0);
        assert!(rc.net_gex > 0.0);
    }

    #[test]
    fn coverage_counts_excluded_and_vanna_quotes() {
        let qs = vec![
            quote(95.0, OptionRight::Call, 100, None, Some(greek(0.5, 0.01))), // greeks, no iv
            quote(100.0, OptionRight::Put, 100, None, Some(greek(-0.5, 0.01))), // greeks, no iv
            quote(105.0, OptionRight::Call, 100, Some(0.2), None),             // iv only
            quote(110.0, OptionRight::Call, 100, None, None),                  // neither
        ];
        let r = analyze(&chain(100.0, 100.0, qs), &params()).unwrap();
        assert_eq!(r.coverage.total, 4);
        assert_eq!(r.coverage.resolved, 3);
        assert_eq!(r.coverage.excluded, 1);
        assert_eq!(r.coverage.vanna_resolved, 1);
        assert_eq!(r.coverage.resolved + r.coverage.excluded, r.coverage.total);
    }

    #[test]
    fn non_finite_strike_is_excluded_not_panicked() {
        let qs = vec![
            quote(f64::NAN, OptionRight::Call, 100, Some(0.2), None),
            quote(100.0, OptionRight::Call, 100, Some(0.2), None),
        ];
        let r = analyze(&chain(100.0, 100.0, qs), &params()).unwrap();
        assert_eq!(r.coverage.excluded, 1);
        assert_eq!(r.coverage.resolved, 1);
    }

    #[test]
    fn vex_is_vanna_based_not_vega_based() {
        let iv = 0.25;
        let p = GexParams::new(0.5, RATE, 0.0);
        let bs = greeks(
            &BsInputs { spot: 100.0, strike: 100.0, t_years: 0.5, vol: iv, rate: RATE, div_yield: 0.0 },
            OptionRight::Call,
        )
        .unwrap();
        let qs = vec![quote(100.0, OptionRight::Call, 100, Some(iv), None)];
        let r = analyze(&chain(100.0, 100.0, qs), &p).unwrap();
        let f_vex = 100.0 * 100.0 * PCT_MOVE_SCALE; // mult * spot * 0.01 = 100
        let vanna_based = bs.vanna * 100.0 * f_vex;
        let vega_based = bs.vega * 100.0 * f_vex;
        assert!((r.net_vex - vanna_based).abs() < 1e-9);
        // D4: the vega form is a materially different number.
        assert!((r.net_vex - vega_based).abs() > 1e-6);
    }

    #[test]
    fn provider_and_bs_paths_agree_on_first_order_exposures() {
        let iv = 0.2;
        let p = GexParams::new(0.5, RATE, 0.0);
        let bs = greeks(
            &BsInputs { spot: 100.0, strike: 100.0, t_years: 0.5, vol: iv, rate: RATE, div_yield: 0.0 },
            OptionRight::Call,
        )
        .unwrap();
        let via_provider = analyze(
            &chain(
                100.0,
                100.0,
                vec![quote(
                    100.0,
                    OptionRight::Call,
                    100,
                    None,
                    Some(Greeks { delta: bs.delta, gamma: bs.gamma, theta: 0.0, vega: 0.0 }),
                )],
            ),
            &p,
        )
        .unwrap();
        let via_iv =
            analyze(&chain(100.0, 100.0, vec![quote(100.0, OptionRight::Call, 100, Some(iv), None)]), &p)
                .unwrap();
        assert!((via_provider.net_gex - via_iv.net_gex).abs() < 1e-9);
        assert!((via_provider.net_dex - via_iv.net_dex).abs() < 1e-9);
        // Provider greeks carry no vanna -> no VEX; the IV path has it.
        assert!(via_provider.net_vex.abs() < 1e-12);
        assert!(via_iv.net_vex.abs() > 1e-6);
    }

    #[test]
    fn dsi_is_directionless_and_maps_to_dealer01() {
        // All-call chain: net == gross for GEX and DEX; no VEX (no IV).
        let qs = vec![
            quote(100.0, OptionRight::Call, 100, None, Some(greek(0.5, 0.01))),
            quote(105.0, OptionRight::Call, 200, None, Some(greek(0.6, 0.008))),
        ];
        let r = analyze(&chain(100.0, 100.0, qs), &params()).unwrap();
        let e = (EXPOSURE_TANH_K).tanh();
        assert!((r.e_gex - e).abs() < 1e-12);
        assert!((r.e_dex - e).abs() < 1e-12);
        assert!((r.e_vex - 0.0).abs() < 1e-12);
        let dsi = (DSI_W_GEX + DSI_W_DEX) * e;
        assert!((r.dsi - dsi).abs() < 1e-12);
        assert!((r.dealer01 - (dsi + 1.0) / 2.0).abs() < 1e-12);
        assert!(r.dsi >= -1.0 && r.dsi <= 1.0);
        assert!(r.dealer01 >= 0.0 && r.dealer01 <= 1.0);
    }

    #[test]
    fn expected_move_uses_atm_iv_with_floor() {
        let qs = vec![quote(100.0, OptionRight::Call, 100, Some(0.2), None)];
        let r = analyze(&chain(100.0, 100.0, qs), &GexParams::new(0.25, RATE, 0.0)).unwrap();
        // 0.2 * sqrt(0.25) = 0.1
        assert!((r.expected_move_pct.unwrap() - 0.1).abs() < 1e-12);
        assert!((r.expected_move_points.unwrap() - 10.0).abs() < 1e-12);
    }

    #[test]
    fn expected_move_absent_without_any_iv() {
        let qs = vec![quote(100.0, OptionRight::Call, 100, None, Some(greek(0.5, 0.01)))];
        let r = analyze(&chain(100.0, 100.0, qs), &GexParams::default()).unwrap();
        assert!(r.expected_move_pct.is_none());
        assert!(r.expected_move_points.is_none());
    }

    #[test]
    fn atm_iv_picks_the_strike_nearest_spot() {
        let qs = vec![
            quote(90.0, OptionRight::Put, 100, Some(0.30), None),
            quote(101.0, OptionRight::Call, 100, Some(0.20), None), // nearest to spot 100
            quote(120.0, OptionRight::Call, 100, Some(0.40), None),
        ];
        let r = analyze(&chain(100.0, 100.0, qs), &GexParams::new(0.25, RATE, 0.0)).unwrap();
        // Nearest strike IV is 0.20 -> 0.20*0.5 = 0.10.
        assert!((r.expected_move_pct.unwrap() - 0.10).abs() < 1e-12);
    }

    #[test]
    fn profile_is_sorted_and_merges_call_put_at_a_strike() {
        let qs = vec![
            quote(100.0, OptionRight::Call, 100, None, Some(greek(0.5, 0.01))),
            quote(100.0, OptionRight::Put, 100, None, Some(greek(-0.5, 0.01))),
            quote(95.0, OptionRight::Put, 50, None, Some(greek(-0.5, 0.02))),
        ];
        let r = analyze(&chain(100.0, 100.0, qs), &params()).unwrap();
        assert_eq!(r.profile.len(), 2);
        assert!(r.profile[0].strike < r.profile[1].strike);
        let at100 = r.profile[1];
        assert!((at100.call_gex - 10000.0).abs() < 1e-9);
        assert!((at100.put_gex + 10000.0).abs() < 1e-9);
        assert!((at100.gex - 0.0).abs() < 1e-9);
    }

    #[test]
    fn gross_dominates_absolute_net_for_every_exposure() {
        let qs = vec![
            quote(95.0, OptionRight::Put, 120, Some(0.22), None),
            quote(100.0, OptionRight::Call, 300, Some(0.20), None),
            quote(100.0, OptionRight::Put, 150, Some(0.21), None),
            quote(110.0, OptionRight::Call, 80, Some(0.25), None),
        ];
        let r = analyze(&chain(100.0, 100.0, qs), &params()).unwrap();
        assert!(r.gross_gex >= r.net_gex.abs() - 1e-9);
        assert!(r.gross_dex >= r.net_dex.abs() - 1e-9);
        assert!(r.gross_vex >= r.net_vex.abs() - 1e-9);
        assert!(r.gross_charm >= r.net_charm.abs() - 1e-9);
    }

    #[test]
    fn zzz_diag_bits() {
        let qs = vec![
            quote(95.0, OptionRight::Put, 120, Some(0.22), None),
            quote(100.0, OptionRight::Call, 300, Some(0.20), None),
            quote(105.0, OptionRight::Call, 80, Some(0.25), None),
        ];
        let c = chain(100.0, 100.0, qs);
        let runs: Vec<GexStructure> = (0..3).map(|_| analyze(&c, &params()).unwrap()).collect();
        for (i, r) in runs.iter().enumerate() {
            eprintln!(
                "run{i}: net_gex={:x} gross_gex={:x} e_gex={:x} dex100={:x} net_dex={:x}",
                r.net_gex.to_bits(),
                r.gross_gex.to_bits(),
                r.e_gex.to_bits(),
                r.profile[1].dex.to_bits(),
                r.net_dex.to_bits(),
            );
        }
    }

    #[test]
    fn analysis_is_deterministic_and_serde_round_trips() {
        let qs = vec![
            quote(95.0, OptionRight::Put, 120, Some(0.22), None),
            quote(100.0, OptionRight::Call, 300, Some(0.20), None),
            quote(105.0, OptionRight::Call, 80, Some(0.25), None),
        ];
        let c = chain(100.0, 100.0, qs);
        // Drive one compiled call site repeatedly: same input, same output. (Two
        // separate `analyze(...)` statements would test LLVM's per-site float
        // contraction, not the engine's determinism.)
        let runs: Vec<GexStructure> = (0..3).map(|_| analyze(&c, &params()).unwrap()).collect();
        assert_eq!(runs[0], runs[1]);
        assert_eq!(runs[1], runs[2]);
        let json = serde_json::to_string(&runs[0]).unwrap();
        let back: GexStructure = serde_json::from_str(&json).unwrap();
        assert_eq!(runs[0], back);
    }

    #[test]
    fn malformed_inputs_are_errors_not_panics() {
        let q = || vec![quote(100.0, OptionRight::Call, 100, Some(0.2), None)];
        assert_eq!(
            analyze(&chain(0.0, 100.0, q()), &params()).unwrap_err(),
            GexError::InvalidSpot(0.0)
        );
        assert_eq!(
            analyze(&chain(100.0, 0.0, q()), &params()).unwrap_err(),
            GexError::InvalidMultiplier(0.0)
        );
        assert_eq!(
            analyze(&chain(100.0, 100.0, q()), &GexParams::new(-1.0, RATE, 0.0)).unwrap_err(),
            GexError::InvalidTime(-1.0)
        );
        assert_eq!(
            analyze(&chain(100.0, 100.0, q()), &GexParams::new(0.5, f64::NAN, 0.0)).unwrap_err(),
            GexError::NonFiniteParams
        );
        assert_eq!(
            analyze(&chain(100.0, 100.0, vec![]), &params()).unwrap_err(),
            GexError::EmptyChain
        );
    }
}
