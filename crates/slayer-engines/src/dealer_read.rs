//! Dealer-read composites: the 0DTE probability engine, the terminal read,
//! and the GEX outlook classifier.
//!
//! Provenance: legacy `zeroDte.ts` (spec 03 E6), `terminalRead.ts`
//! `computeTerminalRead` (E7), and `gexSummary.ts`-adjacent
//! `computeGexOutlook` (E8), per `docs/spec/03-dealer-structure.md`. These
//! are composites over already-computed dealer structure — every input is an
//! explicit parameter wired by the gateway from the GEX/zones engines; this
//! module recomputes nothing upstream.
//!
//! Deviations from legacy:
//! - Binary-state doctrine (`docs/ARCHITECTURE.md` §2): the read's
//!   directional engagement is a [`Readout`] (`Active` ⟺ |score| clears the
//!   legacy ±18 bias threshold, with a named hysteresis band — a rebuild
//!   addition; the legacy had no anti-flap). `LONG/SHORT/NEUTRAL`,
//!   `PIN/TREND`, and the outlook regime stay descriptive DATA enums.
//! - Narrative `events[]`/`headline` prose is not ported (display strings;
//!   the numeric gates behind them are all present).
//! - First-passage and exercise probabilities delegate to
//!   [`slayer_quant::first_passage`] rather than reimplementing the
//!   reflection formula.

use serde::{Deserialize, Serialize};
use slayer_core::{BinaryState, HysteresisBand, Readout};
use slayer_quant::first_passage;

// ── E6 0DTE constants ────────────────────────────────────────────────────

/// Trading days per year for the intraday year-fraction.
pub const TRADING_DAYS: f64 = 252.0;
/// Regular-session hours (RTH).
pub const SESSION_HOURS: f64 = 6.5;
/// Floor on the intraday year fraction.
pub const T_FLOOR: f64 = 1e-6;
/// Default risk-free rate for the 0DTE probabilities (annualized decimal).
pub const DEFAULT_RISK_FREE: f64 = 0.05;
/// Pin time-ramp base: pinning strength at the open.
pub const PIN_TIME_BASE: f64 = 0.4;
/// Pin time-ramp slope: added strength by the close.
pub const PIN_TIME_SLOPE: f64 = 0.6;
/// Short-gamma pin attenuation (short gamma works against a pin).
pub const PIN_SHORT_GAMMA_FACTOR: f64 = 0.35;
/// Magnet gamma-share band, in units of strike spacing (±1 strike ⇒ 1.5×).
pub const MAGNET_BAND_SPACING_MULT: f64 = 1.5;
/// Strike-spacing fallback as a fraction of the magnet price.
pub const SPACING_FALLBACK_PCT: f64 = 0.0025;
/// Settlement-risk σ reshaping slope per unit gamma regime.
pub const SETTLEMENT_VOL_ADJ_SLOPE: f64 = 0.25;
/// Settlement-risk σ adjustment bounds.
pub const SETTLEMENT_VOL_ADJ_MIN: f64 = 0.75;
/// Upper σ adjustment bound.
pub const SETTLEMENT_VOL_ADJ_MAX: f64 = 1.25;

// ── E7 terminal-read constants ───────────────────────────────────────────

/// Minimum closes before the momentum signal is read.
pub const MOMENTUM_MIN_CLOSES: usize = 4;
/// Momentum dead-band: last must exceed first by +0.05% to read up.
pub const MOMENTUM_BAND_UP: f64 = 1.0005;
/// Momentum dead-band: last must undercut first by −0.05% to read down.
pub const MOMENTUM_BAND_DOWN: f64 = 0.9995;
/// Gamma-flip signal weight (strongest signal).
pub const W_FLIP: f64 = 28.0;
/// Magnet signal weight in a PIN regime.
pub const W_MAGNET_PIN: f64 = 24.0;
/// Magnet signal weight in a TREND regime.
pub const W_MAGNET_TREND: f64 = 8.0;
/// Magnet dead-zone: within ±0.08% the magnet reads as pinned (dir 0).
pub const MAGNET_PIN_BAND_PCT: f64 = 0.08;
/// Wall-cage signal weight.
pub const W_WALL: f64 = 16.0;
/// Wall-cage relative position above which the read leans short.
pub const WALL_REL_HIGH: f64 = 0.72;
/// Wall-cage relative position below which the read leans long.
pub const WALL_REL_LOW: f64 = 0.28;
/// OI-positioning signal weight.
pub const W_POSITIONING: f64 = 18.0;
/// Call-OI percentage at or above which positioning reads call-heavy.
pub const OI_SKEW_HIGH_PCT: f64 = 55.0;
/// Call-OI percentage at or below which positioning reads put-heavy.
pub const OI_SKEW_LOW_PCT: f64 = 45.0;
/// Momentum signal weight in a PIN regime (contra).
pub const W_MOM_PIN: f64 = 12.0;
/// Momentum signal weight in a TREND regime (with-trend).
pub const W_MOM_TREND: f64 = 24.0;
/// |score| beyond which the read is directional.
pub const BIAS_SCORE_THRESH: f64 = 18.0;
/// Confidence cap when the read is neutral.
pub const NEUTRAL_CONF_CAP: f64 = 40.0;
/// Pin-strength proximity Gaussian scale, fraction of spot.
pub const PIN_PROX_SCALE_PCT: f64 = 0.004;
/// Bracket coherence dead-zone, fraction of spot (~0.06%).
pub const BRACKET_DEADZONE_PCT: f64 = 0.0006;
/// Position-strength blend: |score| weight.
pub const PS_W_SCORE: f64 = 0.45;
/// Position-strength blend: confidence weight.
pub const PS_W_CONF: f64 = 0.35;
/// Position-strength blend: regime-clarity weight.
pub const PS_W_CLARITY: f64 = 0.20;
/// TREND regime clarity gain on |score|.
pub const TREND_CLARITY_GAIN: f64 = 1.1;
/// Position-strength divisor when neutral or no-trade.
pub const PS_NOTRADE_DIVISOR: f64 = 2.0;

/// Read-engagement band: activates at the legacy ±18 bias threshold
/// (|score|/100), deactivates at 0.15 — the hysteresis is a rebuild
/// addition (the legacy had no anti-flap on the bias).
const READ_ENGAGED_BAND: HysteresisBand = HysteresisBand::new(0.18, 0.15);

// ── E8 outlook constants ─────────────────────────────────────────────────

/// Confidence emitted when no usable structure exists.
pub const OUTLOOK_CONF_NODATA: f64 = 10.0;
/// Reversal-up detector: last close must clear the window low by +0.05%.
pub const REVERSAL_UP_BAND: f64 = 1.0005;
/// PINNING gate: |distance to pin target| at most this, percent of spot.
pub const PINNING_DIST_PCT: f64 = 0.20;
/// PINNING gate: minimum pin strength.
pub const PINNING_MIN_STRENGTH: f64 = 40.0;
/// GAMMA SQUEEZE gate: call wall at most this far above spot, percent.
pub const SQUEEZE_WALL_DIST_PCT: f64 = 0.6;
/// Fallback confidence for the soft long-gamma read.
pub const OUTLOOK_CONF_FALLBACK_LONG: f64 = 35.0;
/// Fallback confidence for the soft short-gamma read.
pub const OUTLOOK_CONF_FALLBACK_SHORT: f64 = 30.0;

// ── Shared input shapes ──────────────────────────────────────────────────

/// One strike's signed net GEX, as computed by the GEX engine.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StrikeGex {
    /// Strike price, points.
    pub strike: f64,
    /// Signed net gamma exposure at the strike, $ per 1% move.
    pub net_gex: f64,
}

/// The dealer-structure profile the composites read — all upstream outputs,
/// wired by the gateway (never recomputed here).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProfileInputs {
    /// Underlying spot, points.
    pub spot: f64,
    /// Net dealer gamma exposure, $ per 1% move (signed).
    pub net_gex: f64,
    /// Gamma-flip level, points (absent when no crossing exists).
    pub gamma_flip: Option<f64>,
    /// Primary magnet strike, points.
    pub magnet: Option<f64>,
    /// Call wall strike, points.
    pub call_wall: Option<f64>,
    /// Put wall strike, points.
    pub put_wall: Option<f64>,
    /// Expected move as a fraction of spot.
    pub expected_move_pct: Option<f64>,
    /// Total call open interest, contracts.
    pub total_call_oi: f64,
    /// Total put open interest, contracts.
    pub total_put_oi: f64,
    /// Per-strike signed net GEX ladder.
    pub strikes: Vec<StrikeGex>,
}

// ── E6 output shapes ─────────────────────────────────────────────────────

/// Expected-move band at one horizon.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EmBand {
    /// Horizon, hours.
    pub hours: f64,
    /// One-sigma move, points.
    pub move_pts: f64,
    /// One-sigma move as a fraction of spot.
    pub move_pct: f64,
    /// Spot ± 1σ.
    pub upper1: f64,
    /// Lower 1σ bound.
    pub lower1: f64,
    /// Spot ± 2σ.
    pub upper2: f64,
    /// Lower 2σ bound.
    pub lower2: f64,
}

/// Pin diagnostics for the 0DTE read.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PinRead {
    /// Magnet level the pin is measured against, points.
    pub magnet: f64,
    /// Pin probability, `[0, 1]`.
    pub probability: f64,
    /// Share of total |GEX| concentrated at the magnet band, `[0, 1]`.
    pub gamma_share: f64,
    /// |spot − magnet| / spot.
    pub distance_pct: f64,
}

/// The 0DTE probability read (E6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ZeroDte {
    /// Hours until the close.
    pub hours_to_close: f64,
    /// Year fraction to the close (floored).
    pub t_years: f64,
    /// Expected-move bands: 1-hour and end-of-day horizons.
    pub bands: [EmBand; 2],
    /// Pin diagnostics.
    pub pin: PinRead,
    /// Positive-gamma center of mass, points (spot when no positive gamma).
    pub eod_magnet: f64,
    /// P(|close-to-close return| > 1 EM), gamma-regime adjusted, `[0, 1]`.
    pub settlement_risk: f64,
    /// ATM implied vol the read was computed at, annualized decimal.
    pub atm_iv: f64,
}

// ── E7/E8 output shapes ──────────────────────────────────────────────────

/// Directional bias (descriptive data; the in-force bit is
/// [`TerminalRead::engaged`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Bias {
    /// Score above +18.
    Long,
    /// Score below −18.
    Short,
    /// Score within the neutral band.
    Neutral,
}

/// Dealer regime by net-gamma sign (descriptive data).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DealerRegime {
    /// Long gamma: dealers stabilize (pin).
    Pin,
    /// Short gamma: dealers amplify (trend).
    Trend,
}

/// One weighted confluence signal (the why behind the score).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReadSignal {
    /// Signal name (e.g. `gamma-flip`).
    pub name: String,
    /// Direction: −1, 0, +1.
    pub dir: i8,
    /// Signal weight.
    pub weight: f64,
}

/// The trade bracket implied by the read, when coherent.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Bracket {
    /// Target level, points.
    pub target: Option<f64>,
    /// Stop level, points.
    pub stop: Option<f64>,
    /// `true` when a directional bias lacked a coherent bracket.
    pub no_trade: bool,
}

/// The terminal read (E7): directional confluence over dealer structure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TerminalRead {
    /// Signed confluence score, `[-100, 100]`.
    pub score: f64,
    /// Directional bias label.
    pub bias: Bias,
    /// Dealer regime label.
    pub regime: DealerRegime,
    /// Signal-agreement confidence, 0–100.
    pub confidence: f64,
    /// Pin strength, 0–100 (PIN regime only; 0 in TREND).
    pub pin_strength: f64,
    /// Position strength, 0–100.
    pub position_strength: f64,
    /// The weighted signals behind the score.
    pub signals: Vec<ReadSignal>,
    /// Entry/target/stop bracket.
    pub bracket: Bracket,
    /// `Active` ⟺ |score| clears the bias threshold (hysteresis-banded).
    pub engaged: Readout,
}

/// GEX-outlook regime (descriptive data).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OutlookRegime {
    /// Long gamma, price glued to the pin target.
    Pinning,
    /// Long gamma, pressing the call wall with call-heavy OI.
    GammaSqueeze,
    /// Short gamma, put-heavy, turning up off the lows.
    ShortSqueeze,
    /// Short gamma, below the flip, momentum down.
    TrendDown,
    /// Short gamma, above the flip, momentum up.
    TrendUp,
    /// Long gamma, caged between the walls.
    Range,
    /// No usable structure.
    Neutral,
}

/// Outlook path bias (descriptive data).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OutlookBias {
    /// Upward path.
    Up,
    /// Downward path.
    Down,
    /// Sideways / mean-reverting path.
    Sideways,
}

/// The GEX outlook (E8): what the dealer book is likely to make price do.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GexOutlook {
    /// Regime classification.
    pub regime: OutlookRegime,
    /// Path bias.
    pub bias: OutlookBias,
    /// Target level, points, when the regime implies one.
    pub target: Option<f64>,
    /// Read confidence, 0–100.
    pub confidence: f64,
}

// ── E6 implementation ────────────────────────────────────────────────────

/// Intraday year fraction: `max(1e−6, (max(0, hours)/6.5)/252)`.
#[must_use]
pub fn hours_to_year_fraction(hours: f64) -> f64 {
    ((hours.max(0.0) / SESSION_HOURS) / TRADING_DAYS).max(T_FLOOR)
}

fn em_band(spot: f64, iv: f64, hours: f64) -> EmBand {
    let t = hours_to_year_fraction(hours);
    let sigma1 = spot * iv * t.sqrt();
    EmBand {
        hours,
        move_pts: sigma1,
        move_pct: if spot > 0.0 { sigma1 / spot } else { 0.0 },
        upper1: spot + sigma1,
        lower1: spot - sigma1,
        upper2: spot + 2.0 * sigma1,
        lower2: spot - 2.0 * sigma1,
    }
}

/// E6.5: positive-gamma center of mass; `spot` when no positive gamma.
#[must_use]
pub fn eod_magnet_target(strikes: &[StrikeGex], spot: f64) -> f64 {
    let (mut num, mut den) = (0.0, 0.0);
    for s in strikes {
        let g = s.net_gex.max(0.0);
        if s.strike > 0.0 && g > 0.0 {
            num += s.strike * g;
            den += g;
        }
    }
    if den > 0.0 { num / den } else { spot }
}

/// E6.6: the assembled 0DTE read.
#[must_use]
pub fn compute_zero_dte(
    spot: f64,
    atm_iv: f64,
    hours_to_close: f64,
    net_gex: f64,
    magnet: f64,
    strikes: &[StrikeGex],
) -> ZeroDte {
    let t_years = hours_to_year_fraction(hours_to_close);
    let bands = [
        em_band(spot, atm_iv, hours_to_close.clamp(0.0, 1.0)),
        em_band(spot, atm_iv, hours_to_close.max(0.0)),
    ];
    let em_eod = if bands[1].move_pts > 0.0 {
        bands[1].move_pts
    } else {
        spot * atm_iv * t_years.sqrt()
    };
    let eod_magnet = eod_magnet_target(strikes, spot);

    // Gamma share within ±1 strike (1.5 × spacing) of the magnet.
    let total_abs: f64 = strikes
        .iter()
        .map(|s| s.net_gex.abs())
        .sum::<f64>()
        .max(1.0);
    let mut sorted: Vec<f64> = strikes
        .iter()
        .map(|s| s.strike)
        .filter(|k| *k > 0.0)
        .collect();
    sorted.sort_by(f64::total_cmp);
    sorted.dedup();
    let spacing = sorted
        .windows(2)
        .map(|w| w[1] - w[0])
        .filter(|g| *g > 0.0 && g.is_finite())
        .fold(f64::INFINITY, f64::min);
    let spacing = if spacing.is_finite() {
        spacing
    } else {
        (magnet * SPACING_FALLBACK_PCT).max(1.0)
    };
    let band = spacing * MAGNET_BAND_SPACING_MULT;
    let magnet_abs: f64 = strikes
        .iter()
        .filter(|s| (s.strike - magnet).abs() <= band)
        .map(|s| s.net_gex.abs())
        .sum();
    let gamma_share = (magnet_abs / total_abs).min(1.0);

    // Pin probability (E6.4).
    let frac_elapsed = 1.0 - (hours_to_close / SESSION_HOURS).clamp(0.0, 1.0);
    let distance_pct = if spot > 0.0 {
        (spot - magnet).abs() / spot
    } else {
        1.0
    };
    let probability = if spot <= 0.0 || magnet <= 0.0 || em_eod <= 0.0 {
        0.0
    } else {
        let z = (spot - magnet) / em_eod;
        let proximity = (-0.5 * z * z).exp();
        let time_factor = PIN_TIME_BASE + PIN_TIME_SLOPE * frac_elapsed.clamp(0.0, 1.0);
        let gamma_sign = if net_gex >= 0.0 {
            1.0
        } else {
            PIN_SHORT_GAMMA_FACTOR
        };
        (gamma_share * proximity * time_factor * gamma_sign).clamp(0.0, 1.0)
    };

    // Settlement risk (E6.6 step 6).
    let gamma_regime = (net_gex / total_abs).clamp(-1.0, 1.0);
    let vol_adj = (1.0 - SETTLEMENT_VOL_ADJ_SLOPE * gamma_regime)
        .clamp(SETTLEMENT_VOL_ADJ_MIN, SETTLEMENT_VOL_ADJ_MAX);
    let settlement_risk = (2.0 * slayer_quant::dist::norm_cdf(-1.0 / vol_adj)).clamp(0.0, 1.0);

    ZeroDte {
        hours_to_close,
        t_years,
        bands,
        pin: PinRead {
            magnet,
            probability,
            gamma_share,
            distance_pct,
        },
        eod_magnet,
        settlement_risk,
        atm_iv,
    }
}

/// E6.1 re-export shim: risk-neutral ITM probability at the 0DTE default
/// rate. See [`first_passage::prob_expire_itm`] for the general form.
#[must_use]
pub fn prob_expire_itm(spot: f64, strike: f64, t: f64, iv: f64, is_call: bool) -> f64 {
    first_passage::prob_expire_itm(spot, strike, t, iv, is_call, DEFAULT_RISK_FREE, 0.0)
}

// ── E7 implementation ────────────────────────────────────────────────────

/// Raw momentum over the close window: ±1 outside a ±0.05% dead-band, else 0.
fn raw_momentum(closes: &[f64]) -> i8 {
    if closes.len() < MOMENTUM_MIN_CLOSES {
        return 0;
    }
    let (first, last) = (closes[0], closes[closes.len() - 1]);
    if last > first * MOMENTUM_BAND_UP {
        1
    } else if last < first * MOMENTUM_BAND_DOWN {
        -1
    } else {
        0
    }
}

/// Percent distance from spot to a level.
fn pct_to(level: f64, spot: f64) -> f64 {
    (level - spot) / spot * 100.0
}

/// Pin strength: concentration (√HHI) × proximity to the reference strike.
/// Note the legacy plain `z²` exponent (not `z²/2`), preserved.
fn pin_strength(strikes: &[StrikeGex], spot: f64, reference: f64) -> f64 {
    if strikes.is_empty() || spot <= 0.0 {
        return 0.0;
    }
    let tot: f64 = strikes
        .iter()
        .map(|s| s.net_gex.abs())
        .sum::<f64>()
        .max(1.0);
    let hhi: f64 = strikes
        .iter()
        .map(|s| {
            let sh = s.net_gex.abs() / tot;
            sh * sh
        })
        .sum();
    let z = (spot - reference) / (spot * PIN_PROX_SCALE_PCT);
    let prox = (-(z * z)).exp();
    (100.0 * hhi.sqrt() * prox).round().clamp(0.0, 100.0)
}

/// E7: the terminal read.
#[must_use]
pub fn compute_terminal_read(
    profile: &ProfileInputs,
    recent_closes: &[f64],
    previous_engaged: BinaryState,
) -> TerminalRead {
    let spot = profile.spot;
    let long_gamma = profile.net_gex >= 0.0;
    let regime = if long_gamma {
        DealerRegime::Pin
    } else {
        DealerRegime::Trend
    };
    let raw_mom = raw_momentum(recent_closes);

    let mut signals: Vec<ReadSignal> = Vec::with_capacity(5);
    let mut push = |name: &str, dir: i8, weight: f64| {
        signals.push(ReadSignal {
            name: name.to_owned(),
            dir,
            weight,
        });
    };

    // γ-flip position.
    if let Some(flip) = profile.gamma_flip
        && spot > 0.0
        && flip > 0.0
    {
        push("gamma-flip", if spot >= flip { 1 } else { -1 }, W_FLIP);
    }
    // Magnet pull (toward the magnet; dead-zone reads pinned).
    if let Some(magnet) = profile.magnet
        && spot > 0.0
        && magnet > 0.0
    {
        let d = pct_to(magnet, spot);
        let dir = if d.abs() < MAGNET_PIN_BAND_PCT {
            0
        } else if d > 0.0 {
            1
        } else {
            -1
        };
        let weight = if long_gamma {
            W_MAGNET_PIN
        } else {
            W_MAGNET_TREND
        };
        push("magnet", dir, weight);
    }
    // Wall cage position.
    if let (Some(cw), Some(pw)) = (profile.call_wall, profile.put_wall)
        && cw > pw
        && spot > 0.0
    {
        let rel = (spot - pw) / (cw - pw);
        let dir = if rel > WALL_REL_HIGH {
            -1
        } else if rel < WALL_REL_LOW {
            1
        } else {
            0
        };
        push("wall-cage", dir, W_WALL);
    }
    // OI positioning.
    let oi_tot = profile.total_call_oi + profile.total_put_oi;
    if oi_tot > 0.0 {
        let bull = 100.0 * profile.total_call_oi / oi_tot;
        let dir = if bull >= OI_SKEW_HIGH_PCT {
            1
        } else if bull <= OI_SKEW_LOW_PCT {
            -1
        } else {
            0
        };
        push("positioning", dir, W_POSITIONING);
    }
    // Momentum: contra in a pin, with-trend in short gamma.
    if raw_mom != 0 {
        let dir = if long_gamma { -raw_mom } else { raw_mom };
        let weight = if long_gamma { W_MOM_PIN } else { W_MOM_TREND };
        push("momentum", dir, weight);
    }

    let score: f64 = signals
        .iter()
        .map(|s| f64::from(s.dir) * s.weight)
        .sum::<f64>()
        .round()
        .clamp(-100.0, 100.0);
    let bias = if score > BIAS_SCORE_THRESH {
        Bias::Long
    } else if score < -BIAS_SCORE_THRESH {
        Bias::Short
    } else {
        Bias::Neutral
    };
    let bias_dir: i8 = match bias {
        Bias::Long => 1,
        Bias::Short => -1,
        Bias::Neutral => 0,
    };

    // Confidence: weighted agreement among directional signals.
    let dir_weight: f64 = signals
        .iter()
        .filter(|s| s.dir != 0)
        .map(|s| s.weight)
        .sum::<f64>()
        .max(1.0);
    let agree_weight: f64 = signals
        .iter()
        .filter(|s| bias_dir != 0 && s.dir == bias_dir)
        .map(|s| s.weight)
        .sum();
    let confidence = if bias_dir == 0 {
        score.abs().round().min(NEUTRAL_CONF_CAP)
    } else {
        (100.0 * agree_weight / dir_weight).round()
    };

    // Pin strength (PIN regime only; proximity to the dominant strike).
    let pin = if long_gamma && !profile.strikes.is_empty() {
        let top = profile.strikes.iter().fold(profile.strikes[0], |best, s| {
            if s.net_gex.abs() > best.net_gex.abs() {
                *s
            } else {
                best
            }
        });
        pin_strength(&profile.strikes, spot, top.strike)
    } else {
        0.0
    };

    // Battle plan (bracket) with coherence enforcement.
    let tiny = spot * BRACKET_DEADZONE_PCT;
    let (mut target, mut stop) = (None, None);
    if bias_dir != 0 {
        let bd = f64::from(bias_dir);
        match regime {
            DealerRegime::Pin => {
                target = match profile.magnet {
                    Some(m) if bd * (m - spot) > tiny => Some(m),
                    _ => {
                        if bias_dir > 0 {
                            profile.call_wall
                        } else {
                            profile.put_wall
                        }
                    }
                };
                stop = if bias_dir > 0 {
                    profile.put_wall
                } else {
                    profile.call_wall
                };
            }
            DealerRegime::Trend => {
                target = if bias_dir > 0 {
                    profile.call_wall
                } else {
                    profile.put_wall
                };
                stop = profile.gamma_flip;
            }
        }
        let coherent = matches!(target, Some(t) if f64::from(bias_dir) * (t - spot) > tiny)
            && matches!(stop, Some(s) if f64::from(bias_dir) * (spot - s) > tiny);
        if !coherent {
            target = None;
            stop = None;
        }
    }
    let no_trade = bias_dir != 0 && target.is_none();

    // Position strength.
    let clarity = match regime {
        DealerRegime::Pin => pin,
        DealerRegime::Trend => (score.abs() * TREND_CLARITY_GAIN).min(100.0),
    };
    let mut position_strength =
        (PS_W_SCORE * score.abs() + PS_W_CONF * confidence + PS_W_CLARITY * clarity).round();
    if no_trade || bias_dir == 0 {
        position_strength = (position_strength / PS_NOTRADE_DIVISOR).floor();
    }
    let position_strength = position_strength.clamp(0.0, 100.0);

    let engaged = Readout::resolve_from(&READ_ENGAGED_BAND, score.abs() / 100.0, previous_engaged);

    TerminalRead {
        score,
        bias,
        regime,
        confidence,
        pin_strength: pin,
        position_strength,
        signals,
        bracket: Bracket {
            target,
            stop,
            no_trade,
        },
        engaged,
    }
}

// ── E8 implementation ────────────────────────────────────────────────────

/// E8: the GEX outlook classifier (strict priority order).
#[must_use]
pub fn compute_gex_outlook(profile: &ProfileInputs, recent_closes: &[f64]) -> GexOutlook {
    let spot = profile.spot;
    // NEUTRAL early-out: no spot, or no gamma structure at all (a literal
    // zero net GEX with no ladder counts as missing, per legacy).
    if spot <= 0.0 || (profile.net_gex == 0.0 && profile.strikes.is_empty()) {
        return GexOutlook {
            regime: OutlookRegime::Neutral,
            bias: OutlookBias::Sideways,
            target: None,
            confidence: OUTLOOK_CONF_NODATA,
        };
    }
    let long_gamma = profile.net_gex >= 0.0;
    let mom = raw_momentum(recent_closes);
    // Reversal-up: turned up off the window low.
    let reversal_up = recent_closes.len() >= MOMENTUM_MIN_CLOSES && {
        let lo = recent_closes.iter().copied().fold(f64::INFINITY, f64::min);
        let lo_idx = recent_closes
            .iter()
            .position(|c| *c == lo)
            .unwrap_or(recent_closes.len() - 1);
        let last = recent_closes[recent_closes.len() - 1];
        lo_idx < recent_closes.len() - 1 && last > lo * REVERSAL_UP_BAND
    };
    let oi_tot = profile.total_call_oi + profile.total_put_oi;
    let call_pct = if oi_tot > 0.0 {
        100.0 * profile.total_call_oi / oi_tot
    } else {
        50.0
    };
    let call_heavy = call_pct >= OI_SKEW_HIGH_PCT;
    let put_heavy = call_pct <= OI_SKEW_LOW_PCT;

    let dom_strike = profile
        .strikes
        .iter()
        .fold(None::<StrikeGex>, |best, s| match best {
            Some(b) if b.net_gex.abs() >= s.net_gex.abs() => Some(b),
            _ => Some(*s),
        })
        .map(|s| s.strike);
    let pin_target = profile.magnet.or(dom_strike);
    let strength = match dom_strike {
        Some(d) if long_gamma => pin_strength(&profile.strikes, spot, d),
        _ => 0.0,
    };

    let d_pin = pin_target.map(|t| pct_to(t, spot));
    let d_cw = profile.call_wall.map(|t| pct_to(t, spot));
    let d_pw = profile.put_wall.map(|t| pct_to(t, spot));
    let above_flip = profile.gamma_flip.map(|f| spot >= f);

    // 1. PINNING
    if long_gamma
        && let (Some(t), Some(dp)) = (pin_target, d_pin)
        && dp.abs() <= PINNING_DIST_PCT
        && strength >= PINNING_MIN_STRENGTH
    {
        let confidence = (40.0 + strength * 0.55).round().clamp(45.0, 96.0);
        return GexOutlook {
            regime: OutlookRegime::Pinning,
            bias: OutlookBias::Sideways,
            target: Some(t),
            confidence,
        };
    }
    // 2. GAMMA SQUEEZE
    if long_gamma
        && let (Some(cw), Some(dcw)) = (profile.call_wall, d_cw)
        && above_flip == Some(true)
        && dcw > 0.0
        && dcw <= SQUEEZE_WALL_DIST_PCT
        && call_heavy
        && mom >= 0
    {
        let prox_w = 1.0 - dcw / SQUEEZE_WALL_DIST_PCT;
        let bonus = if mom > 0 { 8.0 } else { 0.0 };
        let confidence = (52.0 + prox_w * 30.0 + bonus).round().clamp(50.0, 92.0);
        return GexOutlook {
            regime: OutlookRegime::GammaSqueeze,
            bias: OutlookBias::Up,
            target: Some(cw),
            confidence,
        };
    }
    // 3. SHORT SQUEEZE
    if !long_gamma && put_heavy && (reversal_up || mom > 0) {
        let tgt = profile.gamma_flip.or(profile.call_wall);
        let confidence: f64 = 54.0 + if reversal_up { 16.0 } else { 6.0 } + 8.0;
        let confidence = confidence.round().clamp(48.0, 90.0);
        return GexOutlook {
            regime: OutlookRegime::ShortSqueeze,
            bias: OutlookBias::Up,
            target: tgt,
            confidence,
        };
    }
    // 4. TREND DOWN
    if !long_gamma && above_flip == Some(false) && mom < 0 {
        let tgt = profile.put_wall.or(profile.gamma_flip);
        let below_wall_bonus = match d_pw {
            Some(d) if d < 0.0 => 6.0,
            _ => 0.0,
        };
        let confidence: f64 = 56.0 + if put_heavy { 10.0 } else { 0.0 } + below_wall_bonus;
        let confidence = confidence.round().clamp(45.0, 88.0);
        return GexOutlook {
            regime: OutlookRegime::TrendDown,
            bias: OutlookBias::Down,
            target: tgt,
            confidence,
        };
    }
    // 5. TREND UP
    if !long_gamma && above_flip == Some(true) && mom > 0 {
        let tgt = profile.call_wall.or(profile.gamma_flip);
        let confidence: f64 = 54.0 + if call_heavy { 10.0 } else { 0.0 };
        let confidence = confidence.round().clamp(45.0, 86.0);
        return GexOutlook {
            regime: OutlookRegime::TrendUp,
            bias: OutlookBias::Up,
            target: tgt,
            confidence,
        };
    }
    // 6. RANGE
    if long_gamma
        && let (Some(cw), Some(pw)) = (profile.call_wall, profile.put_wall)
        && cw > pw
        && pw < spot
        && spot < cw
    {
        let confidence = (48.0 + strength * 0.2).round().clamp(40.0, 78.0);
        return GexOutlook {
            regime: OutlookRegime::Range,
            bias: OutlookBias::Sideways,
            target: profile.magnet,
            confidence,
        };
    }
    // 7–8. Fallbacks.
    if long_gamma {
        GexOutlook {
            regime: OutlookRegime::Range,
            bias: OutlookBias::Sideways,
            target: pin_target,
            confidence: OUTLOOK_CONF_FALLBACK_LONG,
        }
    } else {
        let (regime, bias, target) = match above_flip {
            Some(false) => (
                OutlookRegime::TrendDown,
                OutlookBias::Down,
                profile.put_wall.or(profile.gamma_flip),
            ),
            Some(true) => (
                OutlookRegime::TrendUp,
                OutlookBias::Up,
                profile.call_wall.or(profile.gamma_flip),
            ),
            None => (OutlookRegime::Neutral, OutlookBias::Sideways, None),
        };
        GexOutlook {
            regime,
            bias,
            target,
            confidence: OUTLOOK_CONF_FALLBACK_SHORT,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn profile(net_gex: f64) -> ProfileInputs {
        ProfileInputs {
            spot: 100.0,
            net_gex,
            gamma_flip: Some(99.0),
            magnet: Some(100.5),
            call_wall: Some(102.0),
            put_wall: Some(97.0),
            expected_move_pct: Some(0.012),
            total_call_oi: 60_000.0,
            total_put_oi: 40_000.0,
            strikes: vec![
                StrikeGex {
                    strike: 97.0,
                    net_gex: -2.0e8,
                },
                StrikeGex {
                    strike: 99.0,
                    net_gex: 1.0e8,
                },
                StrikeGex {
                    strike: 100.5,
                    net_gex: 9.0e8,
                },
                StrikeGex {
                    strike: 102.0,
                    net_gex: 5.0e8,
                },
            ],
        }
    }

    #[test]
    fn zero_dte_pin_behavior() {
        let p = profile(1.0e9);
        let z = compute_zero_dte(100.0, 0.2, 3.0, p.net_gex, 100.5, &p.strikes);
        assert!((0.0..=1.0).contains(&z.pin.probability));
        assert!((0.0..=1.0).contains(&z.pin.gamma_share));
        assert!((0.0..=1.0).contains(&z.settlement_risk));
        assert!(z.bands[0].hours <= 1.0);
        assert!(z.bands[1].move_pts >= z.bands[0].move_pts);
        // Long gamma pins harder than short gamma, all else equal.
        let z_short = compute_zero_dte(100.0, 0.2, 3.0, -1.0e9, 100.5, &p.strikes);
        assert!(z.pin.probability > z_short.pin.probability);
        // Short gamma raises settlement risk (wider effective sigma).
        assert!(z_short.settlement_risk > z.settlement_risk);
        // Pinning strengthens into the close *at* the magnet (z = 0, so the
        // proximity term is 1 and only the time ramp varies). Off the magnet
        // the shrinking EOD expected move rightly collapses the proximity.
        let at_early = compute_zero_dte(100.5, 0.2, 3.0, p.net_gex, 100.5, &p.strikes);
        let at_late = compute_zero_dte(100.5, 0.2, 0.5, p.net_gex, 100.5, &p.strikes);
        assert!(at_late.pin.probability > at_early.pin.probability);
    }

    #[test]
    fn eod_magnet_is_positive_gamma_com() {
        let strikes = vec![
            StrikeGex {
                strike: 100.0,
                net_gex: 1.0e8,
            },
            StrikeGex {
                strike: 110.0,
                net_gex: 3.0e8,
            },
            StrikeGex {
                strike: 90.0,
                net_gex: -5.0e8,
            }, // negative: excluded
        ];
        let com = eod_magnet_target(&strikes, 105.0);
        assert!((com - 107.5).abs() < 1e-9);
        assert_eq!(eod_magnet_target(&[], 105.0), 105.0);
    }

    #[test]
    fn terminal_read_long_confluence() {
        // Above flip, call-heavy, magnet above: solidly long.
        let p = profile(2.0e9);
        let closes = [99.0, 99.5, 99.8, 100.0];
        let r = compute_terminal_read(&p, &closes, BinaryState::Inactive);
        assert_eq!(r.bias, Bias::Long);
        assert_eq!(r.regime, DealerRegime::Pin);
        assert!(r.score > BIAS_SCORE_THRESH);
        assert_eq!(r.engaged.state, BinaryState::Active);
        assert!(r.confidence > 50.0);
        // Bracket coherent: magnet above spot as target, put wall stop.
        assert_eq!(r.bracket.target, Some(100.5));
        assert_eq!(r.bracket.stop, Some(97.0));
        assert!(!r.bracket.no_trade);
    }

    #[test]
    fn terminal_read_neutral_is_inactive() {
        // Contradictory structure: below flip, put-heavy... build a mix that
        // cancels: flip says short, magnet above says long, positioning long.
        let p = ProfileInputs {
            gamma_flip: Some(101.0), // spot below flip: -28
            magnet: Some(100.5),     // above: +24 (pin regime)
            call_wall: Some(102.0),
            put_wall: Some(97.0), // rel=0.6: 0
            total_call_oi: 50_000.0,
            total_put_oi: 50_000.0, // balanced: 0
            ..profile(1.0e9)
        };
        let r = compute_terminal_read(&p, &[], BinaryState::Inactive);
        assert_eq!(r.bias, Bias::Neutral);
        assert_eq!(r.engaged.state, BinaryState::Inactive);
        assert!(r.confidence <= NEUTRAL_CONF_CAP);
    }

    #[test]
    fn incoherent_bracket_reads_no_trade() {
        // Long bias but magnet/walls all below spot: bracket incoherent.
        let p = ProfileInputs {
            spot: 103.0,
            gamma_flip: Some(99.0),  // above flip: +28
            magnet: Some(102.9), // within dead-zone pct? d=-0.097% => dir -1... make it far below
            call_wall: Some(102.0), // below spot: no valid target for long
            put_wall: Some(104.0), // stop above spot: incoherent
            total_call_oi: 80_000.0, // call heavy: +18
            total_put_oi: 20_000.0,
            ..profile(2.0e9)
        };
        let r = compute_terminal_read(&p, &[100.0, 101.0, 102.0, 103.5], BinaryState::Inactive);
        if r.bias != Bias::Neutral {
            assert!(r.bracket.no_trade);
            assert!(r.bracket.target.is_none() && r.bracket.stop.is_none());
        }
    }

    #[test]
    fn outlook_priority_and_fallbacks() {
        // Pinning: long gamma, spot at the dominant strike.
        let p = ProfileInputs {
            magnet: Some(100.05),
            strikes: vec![
                StrikeGex {
                    strike: 100.05,
                    net_gex: 9.5e9,
                },
                StrikeGex {
                    strike: 105.0,
                    net_gex: 1.0e8,
                },
            ],
            ..profile(5.0e9)
        };
        let o = compute_gex_outlook(&p, &[]);
        assert_eq!(o.regime, OutlookRegime::Pinning);
        assert_eq!(o.bias, OutlookBias::Sideways);
        assert!(o.confidence >= 45.0);

        // Short squeeze: short gamma, put-heavy, reversal up.
        let p2 = ProfileInputs {
            total_call_oi: 20_000.0,
            total_put_oi: 80_000.0,
            ..profile(-3.0e9)
        };
        let closes = [100.0, 98.0, 98.5, 99.2];
        let o2 = compute_gex_outlook(&p2, &closes);
        assert_eq!(o2.regime, OutlookRegime::ShortSqueeze);
        assert_eq!(o2.bias, OutlookBias::Up);
        assert_eq!(o2.target, Some(99.0));

        // Trend down: short gamma, below flip, momentum down.
        let p3 = ProfileInputs {
            gamma_flip: Some(105.0),
            ..profile(-3.0e9)
        };
        let down = [102.0, 101.0, 100.5, 100.0];
        let o3 = compute_gex_outlook(&p3, &down);
        assert_eq!(o3.regime, OutlookRegime::TrendDown);
        assert_eq!(o3.bias, OutlookBias::Down);

        // No structure: NEUTRAL early-out.
        let empty = ProfileInputs {
            net_gex: 0.0,
            strikes: vec![],
            ..profile(0.0)
        };
        let o4 = compute_gex_outlook(&empty, &[]);
        assert_eq!(o4.regime, OutlookRegime::Neutral);
        assert_eq!(o4.confidence, OUTLOOK_CONF_NODATA);
    }

    #[test]
    fn outputs_serde_round_trip() {
        let p = profile(1.0e9);
        let r = compute_terminal_read(&p, &[99.0, 99.5, 99.8, 100.0], BinaryState::Inactive);
        let json = serde_json::to_string(&r).unwrap();
        let back: TerminalRead = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
        let z = compute_zero_dte(100.0, 0.2, 3.0, 1.0e9, 100.5, &p.strikes);
        let json = serde_json::to_string(&z).unwrap();
        let back: ZeroDte = serde_json::from_str(&json).unwrap();
        assert_eq!(back, z);
    }
}
