//! Opportunity quality, the binary decision gate, and barrier-touch
//! probability.
//!
//! Provenance: legacy `v11Math.ts` `calculateOpportunityQuality` (E15) and
//! `evaluateDecisionGate` (E16), and `skyQuantCore.ts` §3–4
//! `barrierTouchProb` / `spotForTargetPremium` / `probOptionHitsTarget`
//! (E24), per `docs/spec/01-skyvision-v11.md`.
//!
//! Deviations from legacy:
//! - Binary-state doctrine (`docs/ARCHITECTURE.md` §2): the five-way
//!   `BUY/WAIT/HOLD/REDUCE/EXIT` label collapses to a [`Readout`] —
//!   `Active` ⟺ every entry condition passes — whose score is the continuous
//!   opportunity quality (0–100). The action verb survives as descriptive
//!   data ([`DecisionAction`]), and the per-condition breakdown replaces the
//!   legacy prose `reason` string so the terminal can show *why*.
//! - D12: the legacy open-position branch was unreachable from the pipeline
//!   (`positionOpen` hardcoded false). Both branches are implemented and
//!   reachable here; the caller states the position explicitly.
//! - D13: target probabilities are the honest reflection-principle
//!   barrier-touch probabilities (E24), not the legacy fixed multipliers of
//!   the calibrated win probability.
//! - D11: every gate input is an explicit parameter (`regime_stability` is
//!   the regime engine's measured confidence, not the legacy hardcoded
//!   0.85). Nothing is defaulted from fabricated analytics.

use crate::risk::{LiquidityScore, ModelTrust, TailRisk};
use serde::{Deserialize, Serialize};
use slayer_core::OptionRight;
use slayer_core::{BinaryState, HysteresisBand, Readout};
use slayer_quant::QuantError;
use slayer_quant::black_scholes::{self, BsInputs};
use slayer_quant::first_passage;

// ── E15 opportunity-quality constants ────────────────────────────────────

/// EV normalization floor (fractional return).
pub const EV_FLOOR: f64 = 0.0;
/// EV normalization ceiling: a 30% expected value saturates the EV term.
pub const EV_CEILING: f64 = 0.30;
/// Opportunity-quality blend weight (sum 100): EV points.
pub const OQ_W_EV: f64 = 25.0;
/// Calibrated-probability points.
pub const OQ_W_PCAL: f64 = 20.0;
/// Inverse-tail-risk points.
pub const OQ_W_TAIL: f64 = 15.0;
/// Liquidity points.
pub const OQ_W_LIQ: f64 = 15.0;
/// Model-trust points.
pub const OQ_W_TRUST: f64 = 10.0;
/// Sample-strength points.
pub const OQ_W_SAMPLE: f64 = 10.0;
/// Regime-stability points.
pub const OQ_W_REGIME: f64 = 5.0;

// ── E16 gate constants ───────────────────────────────────────────────────

/// Hard invalidation: minimum thesis stability (0–100).
pub const GATE_STABILITY_FLOOR: f64 = 40.0;
/// Hard invalidation: minimum calibrated probability.
pub const GATE_PCAL_FLOOR: f64 = 0.50;
/// Hard invalidation: minimum liquidity score (0–100) while open.
pub const GATE_LIQ_FLOOR_OPEN: f64 = 30.0;
/// Hard invalidation: minimum trust while open.
pub const GATE_TRUST_FLOOR_OPEN: f64 = 0.20;
/// REDUCE: thesis stability below this trims an open position.
pub const GATE_STABILITY_REDUCE: f64 = 70.0;
/// Maximum tolerable tail-risk score (REDUCE and BUY share it).
pub const GATE_TAIL_MAX: f64 = 0.70;
/// REDUCE: minimum dealer01 while open.
pub const GATE_DEALER_REDUCE: f64 = 0.50;
/// BUY: minimum calibrated probability.
pub const GATE_PCAL_BUY: f64 = 0.62;
/// BUY: minimum reward-to-risk ratio.
pub const GATE_RR_MIN: f64 = 1.50;
/// BUY: minimum liquidity score (0–100).
pub const GATE_LIQ_BUY: f64 = 60.0;
/// BUY: minimum similar-trade sample size.
pub const GATE_SAMPLE_MIN: u64 = 30;
/// BUY: minimum model trust.
pub const GATE_TRUST_BUY: f64 = 0.40;
/// BUY: minimum dealer01.
pub const GATE_DEALER_BUY: f64 = 0.60;

// ── E24 inversion constants ──────────────────────────────────────────────

/// Bisection lower spot bracket, points.
pub const INVERT_SPOT_LO: f64 = 1e-6;
/// Bisection upper bracket as a multiple of spot.
pub const INVERT_SPOT_HI_MULT: f64 = 5.0;
/// Maximum bisection iterations.
pub const INVERT_MAX_ITER: u32 = 200;
/// Bisection value/width tolerance.
pub const INVERT_TOL: f64 = 1e-7;

/// The gate readout resolves strictly at "all conditions pass" — hysteresis
/// is deliberately zero-width: a gate that half-passes is not a gate.
const GATE_BAND: HysteresisBand = HysteresisBand::strict(1.0);

/// Descriptive action verb (data, not state — the binary state lives in
/// [`DecisionReadout::opportunity`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DecisionAction {
    /// Entry conditions all pass (closed position).
    Enter,
    /// Entry conditions incomplete (closed position).
    Wait,
    /// Open position, structure intact.
    Hold,
    /// Open position, degraded structure — trim.
    Reduce,
    /// Open position, hard invalidation — exit.
    Exit,
}

/// One evaluated gate condition: the label, the measured value, and whether
/// it passes. Replaces the legacy prose `reason` string.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Condition {
    /// Human-readable predicate (e.g. `p_cal ≥ 0.62`).
    pub label: String,
    /// Measured value of the quantity the predicate tests.
    pub value: f64,
    /// Whether the predicate currently passes.
    pub pass: bool,
}

/// Inputs to the decision gate — every quantity explicit, nothing defaulted.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GateInputs {
    /// Whether a position is currently open.
    pub position_open: bool,
    /// Expected value of the modeled trade, fractional return.
    pub ev: f64,
    /// Calibrated win probability, `[0, 1]`.
    pub p_cal: f64,
    /// Reward-to-risk ratio (NaN fails the gate, per legacy).
    pub reward_risk: f64,
    /// Tail-risk score, `[0, 1]`.
    pub tail_risk: f64,
    /// Liquidity score, 0–100.
    pub liquidity: f64,
    /// Similar-trade sample size.
    pub n_samples: u64,
    /// Model-trust score, `[0, 1]`.
    pub trust: f64,
    /// Dealer positioning, `[0, 1]`.
    pub dealer01: f64,
    /// Thesis stability, 0–100.
    pub thesis_stability: f64,
}

/// Inputs to the opportunity-quality composite beyond the gate quantities.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct QualityInputs {
    /// Sample strength, `[0, 1]` (log-scaled sample size; see
    /// [`crate::risk::model_trust`]).
    pub sample_strength: f64,
    /// Regime stability, `[0, 1]` — a *measured* quantity (the legacy
    /// hardcoded 0.85 was defect D11; the caller supplies the regime
    /// engine's confidence score).
    pub regime_stability: f64,
}

/// The resolved decision: binary opportunity + quality + action + the why.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionReadout {
    /// `Active` ⟺ every entry condition passes; score = opportunity quality
    /// (0–100).
    pub opportunity: Readout,
    /// Descriptive action verb.
    pub action: DecisionAction,
    /// Opportunity quality, 0–100 (mirrors `opportunity.score`).
    pub quality: f64,
    /// Per-condition breakdown, in gate order.
    pub conditions: Vec<Condition>,
}

/// E15: the 0–100 opportunity-quality composite.
#[must_use]
pub fn opportunity_quality(
    gate: &GateInputs,
    quality: &QualityInputs,
    liquidity: &LiquidityScore,
    trust: &ModelTrust,
    tail: &TailRisk,
) -> f64 {
    let norm_ev = ((gate.ev - EV_FLOOR) / (EV_CEILING - EV_FLOOR)).clamp(0.0, 1.0);
    let score = OQ_W_EV * norm_ev
        + OQ_W_PCAL * gate.p_cal.clamp(0.0, 1.0)
        + OQ_W_TAIL * (1.0 - tail.score.clamp(0.0, 1.0))
        + OQ_W_LIQ * (liquidity.total / 100.0).clamp(0.0, 1.0)
        + OQ_W_TRUST * trust.score.clamp(0.0, 1.0)
        + OQ_W_SAMPLE * quality.sample_strength.clamp(0.0, 1.0)
        + OQ_W_REGIME * quality.regime_stability.clamp(0.0, 1.0);
    score.clamp(0.0, 100.0)
}

fn cond(label: &str, value: f64, pass: bool) -> Condition {
    Condition {
        label: label.to_owned(),
        value,
        pass,
    }
}

/// E16: evaluate the decision gate. The action verb follows the legacy
/// branch structure exactly; the binary opportunity state is `Active` iff
/// the position-closed entry checklist fully passes (an open position's
/// HOLD/REDUCE/EXIT is management, not a fresh opportunity).
#[must_use]
pub fn decision_gate(gate: &GateInputs, quality_score: f64) -> DecisionReadout {
    // Hard invalidation predicate (both branches).
    let invalidated = gate.thesis_stability < GATE_STABILITY_FLOOR
        || gate.p_cal < GATE_PCAL_FLOOR
        || gate.ev < 0.0
        || gate.liquidity < GATE_LIQ_FLOOR_OPEN
        || gate.trust < GATE_TRUST_FLOOR_OPEN;

    // Entry checklist (closed-position branch) — the binary opportunity.
    #[allow(clippy::cast_precision_loss)]
    let n_samples = gate.n_samples as f64;
    let checklist = [
        cond("ev > 0", gate.ev, gate.ev > 0.0),
        cond("p_cal > 0.62", gate.p_cal, gate.p_cal > GATE_PCAL_BUY),
        // NaN reward_risk fails (NaN > x is false), matching legacy.
        cond(
            "r:r > 1.50",
            gate.reward_risk,
            gate.reward_risk > GATE_RR_MIN,
        ),
        cond(
            "tail <= 0.70",
            gate.tail_risk,
            gate.tail_risk <= GATE_TAIL_MAX,
        ),
        cond("liq >= 60", gate.liquidity, gate.liquidity >= GATE_LIQ_BUY),
        cond("n >= 30", n_samples, gate.n_samples >= GATE_SAMPLE_MIN),
        cond("trust >= 0.40", gate.trust, gate.trust >= GATE_TRUST_BUY),
        cond(
            "dealer01 >= 0.60",
            gate.dealer01,
            gate.dealer01 >= GATE_DEALER_BUY,
        ),
    ];
    let all_pass = checklist.iter().all(|c| c.pass);

    let action = if gate.position_open {
        if invalidated {
            DecisionAction::Exit
        } else if gate.thesis_stability < GATE_STABILITY_REDUCE
            || gate.tail_risk > GATE_TAIL_MAX
            || gate.dealer01 < GATE_DEALER_REDUCE
        {
            DecisionAction::Reduce
        } else {
            DecisionAction::Hold
        }
    } else if all_pass {
        DecisionAction::Enter
    } else {
        DecisionAction::Wait
    };

    let gate_score = if all_pass { 1.0 } else { 0.0 };
    let state = GATE_BAND.resolve(gate_score, BinaryState::Inactive);
    DecisionReadout {
        opportunity: Readout {
            state,
            score: quality_score,
        },
        action,
        quality: quality_score,
        conditions: checklist.to_vec(),
    }
}

/// E24: probability that a GBM underlying touches barrier `k` before `tau`
/// years elapse (reflection principle; delegates to
/// [`slayer_quant::first_passage`]). Driftless unless `drift` supplies
/// `r − q`, from which the log-drift `ν = r − q − σ²/2` is formed.
#[must_use]
pub fn barrier_touch_prob(spot: f64, k: f64, sigma: f64, tau: f64, drift: Option<f64>) -> f64 {
    let nu = drift.map_or(0.0, |rq| rq - sigma * sigma / 2.0);
    first_passage::gbm_touch_probability(spot, k, sigma, tau, nu)
}

/// E24: invert BSM in spot — the spot `S*` at which the option reprices to
/// `target_premium`, holding everything else fixed. `None` when the target is
/// non-positive or unreachable within the bracket.
#[must_use]
pub fn spot_for_target_premium(
    target_premium: f64,
    inputs: &BsInputs,
    right: OptionRight,
) -> Option<f64> {
    if target_premium <= 0.0 || !target_premium.is_finite() {
        return None;
    }
    let price_at = |s: f64| -> Result<f64, QuantError> {
        black_scholes::price(&BsInputs { spot: s, ..*inputs }, right)
    };
    let (mut lo, mut hi) = (INVERT_SPOT_LO, INVERT_SPOT_HI_MULT * inputs.spot);
    let f_lo = price_at(lo).ok()? - target_premium;
    let f_hi = price_at(hi).ok()? - target_premium;
    if f_lo * f_hi > 0.0 {
        return None; // target unreachable within the bracket
    }
    let mut f_lo = f_lo;
    let mut mid = 0.5 * (lo + hi);
    for _ in 0..INVERT_MAX_ITER {
        mid = 0.5 * (lo + hi);
        let f_mid = price_at(mid).ok()? - target_premium;
        if f_mid.abs() < INVERT_TOL || hi - lo < INVERT_TOL {
            return Some(mid);
        }
        if f_mid * f_lo > 0.0 {
            lo = mid;
            f_lo = f_mid;
        } else {
            hi = mid;
        }
    }
    Some(mid)
}

/// E24: probability an option's premium reaches `entry × target_mult` before
/// expiry — the honest replacement for the legacy fixed target-probability
/// multipliers (D13). Returns the touch probability and the implied spot.
#[must_use]
pub fn prob_option_hits_target(
    entry_premium: f64,
    target_mult: f64,
    inputs: &BsInputs,
    right: OptionRight,
    drift: Option<f64>,
) -> (f64, Option<f64>) {
    let target = entry_premium * target_mult;
    match spot_for_target_premium(target, inputs, right) {
        Some(spot_star) => (
            barrier_touch_prob(inputs.spot, spot_star, inputs.vol, inputs.t_years, drift),
            Some(spot_star),
        ),
        None => (0.0, None),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::risk::{TrustComponents, TrustGrade};
    use slayer_quant::dist::norm_cdf;

    fn passing_gate() -> GateInputs {
        GateInputs {
            position_open: false,
            ev: 0.12,
            p_cal: 0.70,
            reward_risk: 2.0,
            tail_risk: 0.3,
            liquidity: 75.0,
            n_samples: 100,
            trust: 0.6,
            dealer01: 0.7,
            thesis_stability: 80.0,
        }
    }

    fn liq(total: f64) -> LiquidityScore {
        LiquidityScore {
            total,
            spread: 0.8,
            volume: 0.7,
            oi: 0.7,
            stability: 0.9,
        }
    }

    fn trust(score: f64) -> ModelTrust {
        ModelTrust {
            score,
            grade: TrustGrade::B,
            components: TrustComponents {
                calibration: 0.8,
                forecast_accuracy: 0.7,
                prediction_stability: 0.8,
                sample_strength: 0.6,
                recent_performance: 0.6,
            },
        }
    }

    fn tail(score: f64) -> TailRisk {
        TailRisk {
            var95: 0.1,
            var99: 0.2,
            es95: score,
            es99: 0.3,
            worst: 0.4,
            score,
        }
    }

    #[test]
    fn all_pass_resolves_active_enter() {
        let g = passing_gate();
        let q = opportunity_quality(
            &g,
            &QualityInputs {
                sample_strength: 0.7,
                regime_stability: 0.6,
            },
            &liq(75.0),
            &trust(0.6),
            &tail(0.3),
        );
        let d = decision_gate(&g, q);
        assert_eq!(d.opportunity.state, BinaryState::Active);
        assert_eq!(d.action, DecisionAction::Enter);
        assert!(d.conditions.iter().all(|c| c.pass));
        assert!(d.quality > 50.0 && d.quality <= 100.0);
    }

    #[test]
    fn each_single_failure_resolves_inactive() {
        let base = passing_gate();
        let failures: [(&str, GateInputs); 8] = [
            ("ev", GateInputs { ev: -0.01, ..base }),
            (
                "p_cal",
                GateInputs {
                    p_cal: 0.60,
                    ..base
                },
            ),
            (
                "rr",
                GateInputs {
                    reward_risk: 1.2,
                    ..base
                },
            ),
            (
                "tail",
                GateInputs {
                    tail_risk: 0.9,
                    ..base
                },
            ),
            (
                "liq",
                GateInputs {
                    liquidity: 40.0,
                    ..base
                },
            ),
            (
                "n",
                GateInputs {
                    n_samples: 10,
                    ..base
                },
            ),
            ("trust", GateInputs { trust: 0.3, ..base }),
            (
                "dealer",
                GateInputs {
                    dealer01: 0.5,
                    ..base
                },
            ),
        ];
        for (name, g) in failures {
            let d = decision_gate(&g, 50.0);
            assert_eq!(d.opportunity.state, BinaryState::Inactive, "{name}");
            assert_eq!(d.action, DecisionAction::Wait, "{name}");
            assert!(d.conditions.iter().any(|c| !c.pass), "{name}");
        }
    }

    #[test]
    fn nan_reward_risk_fails_closed() {
        let g = GateInputs {
            reward_risk: f64::NAN,
            ..passing_gate()
        };
        let d = decision_gate(&g, 50.0);
        assert_eq!(d.opportunity.state, BinaryState::Inactive);
    }

    #[test]
    fn open_position_branches() {
        let open = GateInputs {
            position_open: true,
            ..passing_gate()
        };
        assert_eq!(decision_gate(&open, 50.0).action, DecisionAction::Hold);
        // Degraded structure -> REDUCE.
        let reduce = GateInputs {
            thesis_stability: 60.0,
            ..open
        };
        assert_eq!(decision_gate(&reduce, 50.0).action, DecisionAction::Reduce);
        // Hard invalidation -> EXIT.
        let exit = GateInputs {
            p_cal: 0.40,
            ..open
        };
        assert_eq!(decision_gate(&exit, 50.0).action, DecisionAction::Exit);
    }

    #[test]
    fn quality_weights_saturate_at_100() {
        let g = GateInputs {
            ev: 0.5,
            p_cal: 1.0,
            ..passing_gate()
        };
        let q = opportunity_quality(
            &g,
            &QualityInputs {
                sample_strength: 1.0,
                regime_stability: 1.0,
            },
            &liq(100.0),
            &trust(1.0),
            &tail(0.0),
        );
        assert!((q - 100.0).abs() < 1e-9);
    }

    #[test]
    fn barrier_touch_prob_properties() {
        // Barrier at spot: certain touch.
        assert_eq!(barrier_touch_prob(100.0, 100.0, 0.2, 0.5, None), 1.0);
        // Degenerate inputs: zero.
        assert_eq!(barrier_touch_prob(100.0, 110.0, 0.0, 0.5, None), 0.0);
        assert_eq!(barrier_touch_prob(100.0, 110.0, 0.2, 0.0, None), 0.0);
        // Monotone: nearer barrier touches more often.
        let near = barrier_touch_prob(100.0, 102.0, 0.2, 0.25, None);
        let far = barrier_touch_prob(100.0, 115.0, 0.2, 0.25, None);
        assert!(near > far && (0.0..=1.0).contains(&near) && (0.0..=1.0).contains(&far));
        // Driftless closed form: 2*CDF(-|x|/sigma*sqrt(tau)).
        let x: f64 = (110.0_f64 / 100.0_f64).ln();
        let closed = 2.0 * norm_cdf(-x.abs() / (0.2 * 0.25_f64.sqrt()));
        let p = barrier_touch_prob(100.0, 110.0, 0.2, 0.25, None);
        assert!((p - closed).abs() < 1e-12);
        // Log-symmetric barriers, driftless: equal touch probability.
        let below = barrier_touch_prob(100.0, 100.0 / 1.1, 0.2, 0.25, None);
        let above = barrier_touch_prob(100.0, 110.0, 0.2, 0.25, None);
        assert!((below - above).abs() < 1e-12);
    }

    #[test]
    fn spot_inversion_roundtrips() {
        let inputs = BsInputs {
            spot: 100.0,
            strike: 105.0,
            t_years: 0.25,
            vol: 0.25,
            rate: 0.045,
            div_yield: 0.0,
        };
        let entry = black_scholes::price(&inputs, OptionRight::Call).unwrap();
        // Find the spot where the option doubles; repricing there must
        // recover ~2x entry.
        let (prob, s_star) = prob_option_hits_target(entry, 2.0, &inputs, OptionRight::Call, None);
        let s_star = s_star.expect("target reachable");
        let repriced = black_scholes::price(
            &BsInputs {
                spot: s_star,
                ..inputs
            },
            OptionRight::Call,
        )
        .unwrap();
        assert!((repriced - 2.0 * entry).abs() < 1e-5);
        assert!(prob > 0.0 && prob < 1.0);
        assert!(s_star > inputs.spot, "call doubling requires a rally");
        // Unreachable target -> (0, None).
        let (p0, none) = prob_option_hits_target(entry, 1e9, &inputs, OptionRight::Call, None);
        assert_eq!(p0, 0.0);
        assert!(none.is_none());
    }
}
