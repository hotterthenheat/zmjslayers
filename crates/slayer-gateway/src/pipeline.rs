//! The engine pipeline: normalized market state → [`TerminalSnapshot`].
//!
//! This is where the pure engines are composed. Given a symbol's rolling
//! book, it runs the candle-only engines (thesis, regime, realized vol) and,
//! when a chain snapshot is present, the chain engines (GEX structure, dealer
//! zones, dealer-flow dynamics, IV metrics, risk-neutral density), then maps
//! every output into the authoritative wire schema.
//!
//! Per-symbol [`EngineMemory`] carries the small amount of history the pure
//! engines need as parameters: previous binary states for hysteresis, prior
//! dealer aggregates for the time-derivative flow engines, and an ATM-IV ring
//! for the IV rank/percentile. The engines stay pure; the memory lives here.

use crate::timecalc::years_to_expiry;
use slayer_core::wire::{
    DealerPanel, EngineStatus, FlowPanel, Metric, RegimePanel, StrikeRow, TerminalSnapshot,
    ThesisPanel, VolPanel, WIRE_VERSION, WallReadout,
};
use slayer_core::{BinaryState, ExpiryDate, OptionChain, OptionRight, Readout, Symbol, TsMillis};
use slayer_engines::dealer::{self, DealerDynamicsInput, DealerPrevStates};
use slayer_engines::gex::{self, GexParams, GexStructure, Wall};
use slayer_engines::regime::{self, RegimeLabel, RegimePrevStates};
use slayer_engines::thesis::{self, ThesisReadout};
use slayer_engines::zones::{self, GravityConfig, GravityStrikeInput, WallInput, ZoneInput};
use slayer_quant::realized_vol;
use slayer_quant::rnd;
use slayer_quant::vol_metrics;
use std::collections::HashMap;

/// Risk-free rate applied across the chain engines (annualized decimal).
const RATE: f64 = 0.045;
/// Dividend yield assumed for index/ETF options (annualized decimal).
const DIV_YIELD: f64 = 0.0;
/// Annualization for 1-minute bars: 252 sessions × 390 minutes.
const MINUTE_BARS_PER_YEAR: f64 = 252.0 * 390.0;
/// ATM-IV observations retained for IV rank/percentile.
const IV_HISTORY_CAP: usize = 480;
/// Charm-magnitude observations retained for the dealer charm-intensity floor.
const CHARM_HISTORY_CAP: usize = 20;
/// ATR fallback as a fraction of spot when the series is too short to warm up.
const ATR_FALLBACK_PCT: f64 = 0.01;
/// Minimum candles before the candle engines are meaningful.
const MIN_CANDLES: usize = 30;

/// Chain-derived panels, cached so an unchanged chain (the feed emits candles
/// far more often than chain snapshots) is not re-run through the dealer
/// time-derivative engines — which would see `prev == current` and collapse
/// the velocities, and would key `dt_min` off the wrong interval.
#[derive(Debug, Clone)]
struct ChainCache {
    dealer: DealerPanel,
    flow: FlowPanel,
    vol_extra: VolExtra,
    expiry: Option<ExpiryDate>,
}

/// Per-symbol engine memory: the history the pure engines take as parameters.
#[derive(Debug, Clone, Default)]
pub struct EngineMemory {
    prev_thesis_state: BinaryState,
    prev_regime_states: RegimePrevStates,
    dealer_prev_states: DealerPrevStates,
    prev_call_wall_state: BinaryState,
    prev_put_wall_state: BinaryState,
    prev_net_gex: Option<f64>,
    prev2_net_gex: Option<f64>,
    prev_net_vanna: Option<f64>,
    prev_gex_com: Option<f64>,
    prev_total_oi: Option<f64>,
    /// Timestamp of the last *chain snapshot* whose dynamics we advanced —
    /// the divisor base for OI/gamma velocity, distinct from the recompute
    /// clock (which ticks on every candle).
    last_chain_ts: Option<TsMillis>,
    iv_history: Vec<f64>,
    charm_history: Vec<f64>,
    /// Last chain-derived output, reused while the chain is unchanged.
    chain_cache: Option<ChainCache>,
}

impl EngineMemory {
    fn push_iv(&mut self, iv: f64) {
        if iv.is_finite() && iv > 0.0 {
            if self.iv_history.len() == IV_HISTORY_CAP {
                self.iv_history.remove(0);
            }
            self.iv_history.push(iv);
        }
    }

    fn push_charm(&mut self, abs_charm: f64) {
        if abs_charm.is_finite() {
            if self.charm_history.len() == CHARM_HISTORY_CAP {
                self.charm_history.remove(0);
            }
            self.charm_history.push(abs_charm);
        }
    }

    fn max_abs_charm(&self) -> f64 {
        self.charm_history.iter().copied().fold(0.0_f64, f64::max)
    }
}

/// All symbols' engine memories.
#[derive(Debug, Default)]
pub struct PipelineState {
    memory: HashMap<Symbol, EngineMemory>,
}

impl PipelineState {
    /// New, empty pipeline state.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Compose a snapshot for `symbol` from its book, advancing that symbol's
    /// memory. Returns `None` until the symbol has enough candle history for
    /// the candle engines to be meaningful.
    pub fn compose(
        &mut self,
        symbol: &Symbol,
        candles: &[slayer_core::Candle],
        chain: Option<&OptionChain>,
        spot: f64,
        ts: TsMillis,
        synthetic: bool,
    ) -> Option<TerminalSnapshot> {
        if candles.len() < MIN_CANDLES {
            return None;
        }
        let mem = self.memory.entry(symbol.clone()).or_default();

        // ── Candle engines ────────────────────────────────────────────────
        let atr_fallback = spot * ATR_FALLBACK_PCT;
        let thesis = thesis::thesis_from(candles, atr_fallback, mem.prev_thesis_state);
        let regime = regime::analyze_regime_from(candles, mem.prev_regime_states);
        let realized = realized_vol::yang_zhang(candles, MINUTE_BARS_PER_YEAR)
            .or_else(|_| realized_vol::close_to_close(candles, MINUTE_BARS_PER_YEAR))
            .unwrap_or(0.0);

        // ── Chain engines (only when the chain snapshot actually changes) ──
        // The runtime recomputes on every candle, but the dealer structure and
        // time-derivative engines are a function of the chain snapshot; running
        // them against an unchanged chain would corrupt the velocity divisor
        // and collapse the flow readouts. Recompute on a new chain, else reuse.
        let (dealer_panel, flow_panel, vol_extra, expiry) = match chain {
            Some(c) if !c.quotes.is_empty() => {
                let is_new = mem.last_chain_ts != Some(c.ts);
                if is_new {
                    let expiry = nearest_expiry(c);
                    let t_years = expiry.map_or(1.0 / 365.0, |e| years_to_expiry(c.ts, e));
                    let out = compose_chain(mem, c, spot, t_years);
                    mem.chain_cache = Some(ChainCache {
                        dealer: out.0.clone(),
                        flow: out.1.clone(),
                        vol_extra: out.2.clone(),
                        expiry: out.3,
                    });
                    out
                } else if let Some(cache) = &mem.chain_cache {
                    (
                        cache.dealer.clone(),
                        cache.flow.clone(),
                        cache.vol_extra.clone(),
                        cache.expiry,
                    )
                } else {
                    (
                        empty_dealer_panel(),
                        empty_flow_panel(),
                        VolExtra::default(),
                        None,
                    )
                }
            }
            _ => (
                empty_dealer_panel(),
                empty_flow_panel(),
                VolExtra::default(),
                None,
            ),
        };

        // ── Volatility panel ──────────────────────────────────────────────
        let vol = VolPanel {
            realized_vol: realized,
            implied_vol: vol_extra.implied_vol,
            variance_risk_premium: vol_metrics::vol_risk_premium(vol_extra.implied_vol, realized),
            iv_rank: vol_extra.iv_rank,
            iv_percentile: vol_extra.iv_percentile,
            rnd_percentiles: vol_extra.rnd_percentiles,
        };

        // ── Engine board ──────────────────────────────────────────────────
        let engines = engine_board(&thesis, &regime, &dealer_panel, &flow_panel);

        // ── Advance candle-engine memory ──────────────────────────────────
        mem.prev_thesis_state = thesis.engagement.state;
        mem.prev_regime_states = RegimePrevStates {
            persistence: regime.persistence.state,
            compression: regime.compression.state,
            expansion: regime.expansion.state,
            confident: regime.classification.confident.state,
        };

        Some(TerminalSnapshot {
            wire_version: WIRE_VERSION,
            symbol: symbol.as_str().to_owned(),
            spot,
            ts,
            expiry,
            synthetic,
            dealer: dealer_panel,
            thesis: map_thesis(&thesis),
            regime: map_regime(&regime),
            vol,
            flow: flow_panel,
            engines,
        })
    }
}

/// Volatility fields sourced from the chain (absent without one).
#[derive(Debug, Default, Clone)]
struct VolExtra {
    implied_vol: f64,
    iv_rank: f64,
    iv_percentile: f64,
    rnd_percentiles: Vec<Metric>,
}

/// Run the chain engines and map them into the dealer + flow panels.
fn compose_chain(
    mem: &mut EngineMemory,
    chain: &OptionChain,
    spot: f64,
    t_years: f64,
) -> (DealerPanel, FlowPanel, VolExtra, Option<ExpiryDate>) {
    let params = GexParams::new(t_years, RATE, DIV_YIELD);
    let Ok(gexs) = gex::analyze(chain, &params) else {
        return (
            empty_dealer_panel(),
            empty_flow_panel(),
            VolExtra::default(),
            None,
        );
    };

    // Dealer zones: build per-strike gravity inputs by joining the GEX
    // profile (net gamma) with per-strike OI/volume aggregated from the chain.
    let oi_by_strike = aggregate_oi(chain);
    let gravity_inputs: Vec<GravityStrikeInput> = gexs
        .profile
        .iter()
        .map(|r| {
            let agg = oi_by_strike
                .get(&strike_key(r.strike))
                .copied()
                .unwrap_or_default();
            GravityStrikeInput {
                strike: r.strike,
                net_gex: r.gex,
                call_oi: agg.call_oi,
                put_oi: agg.put_oi,
                call_volume: agg.call_vol,
                put_volume: agg.put_vol,
            }
        })
        .collect();

    let wall_input = |w: Option<&Wall>, prev: BinaryState| -> WallInput {
        WallInput {
            strike: w.map_or(spot, |w| w.strike),
            strength_0_100: w.map_or(0.0, |w| w.dominance.score * 100.0),
            previous_state: prev,
        }
    };
    let zone_input = ZoneInput {
        spot,
        strikes: &gravity_inputs,
        config: GravityConfig::default(),
        // Thread the prior wall states so WALL_ZONE_BAND hysteresis actually
        // holds through a graze around the strike (else every snapshot
        // cold-starts and the wall flaps).
        call_wall: wall_input(gexs.call_wall.as_ref(), mem.prev_call_wall_state),
        put_wall: wall_input(gexs.put_wall.as_ref(), mem.prev_put_wall_state),
    };
    let zones = zones::zone_structure(&zone_input);

    // Dealer-flow dynamics from aggregate exposures + the prior *chain*.
    let gex_com = gamma_center_of_mass(&gexs);
    let total_oi: f64 = oi_by_strike.values().map(|a| a.call_oi + a.put_oi).sum();
    // Velocity divisor is the interval to the previous chain snapshot, not the
    // recompute clock. First chain (no prior) → priors are None, so the value
    // is irrelevant; the engines emit zero velocity.
    let dt_min = mem.last_chain_ts.map_or(1.0, |p| {
        (chain.ts.saturating_since(p) as f64 / 60_000.0).max(1e-3)
    });
    let dyn_input = DealerDynamicsInput {
        spot,
        net_gex: gexs.net_gex,
        net_vanna: gexs.net_vex,
        net_charm: gexs.net_charm,
        gex_com,
        total_oi,
        prev_net_gex: mem.prev_net_gex,
        prev2_net_gex: mem.prev2_net_gex,
        prev_net_vanna: mem.prev_net_vanna,
        prev_gex_com: mem.prev_gex_com,
        prev_total_oi: mem.prev_total_oi,
        prior_max_abs_charm: mem.max_abs_charm(),
        dt_min,
        prev_states: mem.dealer_prev_states,
    };
    let flow = dealer::dealer_dynamics(&dyn_input);

    // IV metrics: rank against PRIOR history, then record this observation —
    // the current IV must not sit in its own percentile denominator.
    let atm_iv = atm_iv(chain, spot);
    let iv_metrics = vol_metrics::iv_metrics(atm_iv, &mem.iv_history);
    mem.push_iv(atm_iv);
    let rnd_percentiles = rnd::implied_rnd(&chain.quotes, spot, t_years, RATE)
        .map(|c| {
            let p = c.percentiles;
            vec![
                Metric::new("P05", p.p05, ""),
                Metric::new("P25", p.p25, ""),
                Metric::new("P50", p.p50, ""),
                Metric::new("P75", p.p75, ""),
                Metric::new("P95", p.p95, ""),
            ]
        })
        .unwrap_or_default();

    let dealer_panel = map_dealer(&gexs, &zones, spot);
    let flow_panel = FlowPanel {
        vanna_flow: flow.vanna.trend_score.clamp(-1.0, 1.0),
        charm_bias: signed_intensity(&flow.charm.bias, flow.charm.intensity),
        migration: flow.migration.score.clamp(-1.0, 1.0),
        gamma_dynamics: flow.gamma.hedging_active,
        oi_flow: flow.oi_flow.flowing,
    };
    let vol_extra = VolExtra {
        implied_vol: atm_iv,
        iv_rank: iv_metrics.rank.unwrap_or(0.0),
        iv_percentile: iv_metrics.percentile.unwrap_or(0.0),
        rnd_percentiles,
    };

    // Advance chain memory (only reached on a new chain snapshot).
    mem.push_charm(gexs.net_charm.abs());
    mem.prev2_net_gex = mem.prev_net_gex;
    mem.prev_net_gex = Some(gexs.net_gex);
    mem.prev_net_vanna = Some(gexs.net_vex);
    mem.prev_gex_com = Some(gex_com);
    mem.prev_total_oi = Some(total_oi);
    mem.last_chain_ts = Some(chain.ts);
    mem.prev_call_wall_state = dealer_panel.call_wall.state.state;
    mem.prev_put_wall_state = dealer_panel.put_wall.state.state;
    mem.dealer_prev_states = DealerPrevStates {
        vanna: flow.vanna.engaged.state,
        charm: flow.charm.engaged.state,
        migration: flow.migration.migrating.state,
        gamma: flow.gamma.hedging_active.state,
        oi_flow: flow.oi_flow.flowing.state,
    };

    (dealer_panel, flow_panel, vol_extra, nearest_expiry(chain))
}

/// Signed [-1, 1] intensity from a directional bias enum + magnitude.
fn signed_intensity(bias: &dealer::BiasDirection, intensity: f64) -> f64 {
    let sign = match bias {
        dealer::BiasDirection::Bullish => 1.0,
        dealer::BiasDirection::Bearish => -1.0,
        dealer::BiasDirection::Neutral => 0.0,
    };
    (sign * intensity).clamp(-1.0, 1.0)
}

#[derive(Debug, Clone, Copy, Default)]
struct OiAgg {
    call_oi: f64,
    put_oi: f64,
    call_vol: f64,
    put_vol: f64,
}

/// Aggregate per-strike OI and volume by side. Strikes are keyed to
/// milli-precision so floating-point strikes group deterministically.
fn aggregate_oi(chain: &OptionChain) -> HashMap<i64, OiAgg> {
    let mut map: HashMap<i64, OiAgg> = HashMap::new();
    for q in &chain.quotes {
        let agg = map.entry(strike_key(q.strike)).or_default();
        let oi = q.open_interest as f64;
        let vol = q.volume as f64;
        match q.right {
            OptionRight::Call => {
                agg.call_oi += oi;
                agg.call_vol += vol;
            }
            OptionRight::Put => {
                agg.put_oi += oi;
                agg.put_vol += vol;
            }
        }
    }
    map
}

fn strike_key(strike: f64) -> i64 {
    (strike * 1000.0).round() as i64
}

/// Gamma center of mass: `Σ strike·|gex| / Σ|gex|`, spot when no gamma.
fn gamma_center_of_mass(gexs: &GexStructure) -> f64 {
    let (mut num, mut den) = (0.0, 0.0);
    for r in &gexs.profile {
        let w = r.gex.abs();
        num += r.strike * w;
        den += w;
    }
    if den > 0.0 { num / den } else { gexs.spot }
}

/// ATM implied vol: the IV of the quote nearest spot, or 0 when none carry IV.
fn atm_iv(chain: &OptionChain, spot: f64) -> f64 {
    chain
        .quotes
        .iter()
        .filter_map(|q| q.iv.map(|iv| ((q.strike - spot).abs(), iv)))
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map_or(0.0, |(_, iv)| iv)
}

fn nearest_expiry(chain: &OptionChain) -> Option<ExpiryDate> {
    chain.quotes.iter().map(|q| q.expiry).min()
}

// ── Wire mapping ──────────────────────────────────────────────────────────

fn map_dealer(gexs: &GexStructure, zones: &zones::ZoneStructure, spot: f64) -> DealerPanel {
    let ladder = gexs
        .profile
        .iter()
        .map(|r| StrikeRow {
            strike: r.strike,
            gex: r.gex,
            dex: r.dex,
            vex: r.vex,
            open_interest: 0, // OI lives in the gravity layer; ladder shows exposure.
            at_spot: (r.strike - spot).abs() < f64::EPSILON.max(spot * 1e-9),
        })
        .collect();

    DealerPanel {
        net_gex: gexs.net_gex,
        gross_gex: gexs.gross_gex,
        net_dex: gexs.net_dex,
        net_vex: gexs.net_vex,
        net_charm: gexs.net_charm,
        dsi: gexs.dsi,
        dealer01: gexs.dealer01,
        gamma_flip: gexs.gamma_flip,
        gamma_flip_state: gexs.flip_readout,
        call_wall: map_wall(gexs.call_wall.as_ref(), &zones.call_wall),
        put_wall: map_wall(gexs.put_wall.as_ref(), &zones.put_wall),
        expected_move_pct: gexs.expected_move_pct.unwrap_or(0.0),
        magnet: zones.gravity.primary_magnet,
        ladder,
        excluded_quotes: gexs.coverage.excluded as u32,
    }
}

fn map_wall(gex_wall: Option<&Wall>, zone: &zones::WallZone) -> WallReadout {
    WallReadout {
        strike: gex_wall.map(|w| w.strike),
        margin: zone.margin,
        strength: zone.strength_0_100,
        // The wall's binary state is the zone-state (breached or not); its
        // score carries the signed margin band.
        state: if gex_wall.is_some() {
            zone.readout
        } else {
            Readout {
                state: BinaryState::Inactive,
                score: 0.0,
            }
        },
    }
}

fn map_thesis(t: &ThesisReadout) -> ThesisPanel {
    let s = &t.dominant;
    ThesisPanel {
        long_score: f64::from(t.long_score),
        short_score: f64::from(t.short_score),
        direction: t.direction,
        engagement: t.engagement,
        sub_scores: vec![
            Metric::new("Structure", f64::from(s.structure_quality), ""),
            Metric::new("Momentum", f64::from(s.rsi_cascade), ""),
            Metric::new("VWAP", f64::from(s.vwap_alignment), ""),
            Metric::new("Volume", f64::from(s.volume_expansion), ""),
            Metric::new("Liquidity", f64::from(s.liquidity_sweep), ""),
            Metric::new("Vol Regime", f64::from(s.volatility_regime), ""),
        ],
    }
}

fn map_regime(r: &regime::RegimeState) -> RegimePanel {
    let p = &r.classification.probabilities;
    RegimePanel {
        hurst: r.hurst,
        half_life_bars: r.half_life_bars,
        label: regime_label(r.classification.label),
        probabilities: vec![
            Metric::new("Trend", p.trend_expansion, ""),
            Metric::new("Revert", p.mean_reversion, ""),
            Metric::new("Tail", p.tail_risk, ""),
        ],
        confidence: r.classification.confident,
        compression: r.compression,
        expansion: r.expansion,
        term_structure_slope: r.term_structure_slope,
    }
}

fn regime_label(l: RegimeLabel) -> String {
    match l {
        RegimeLabel::TrendExpansion => "TREND_EXPANSION",
        RegimeLabel::MeanReversion => "MEAN_REVERSION",
        RegimeLabel::TailRisk => "TAIL_RISK",
    }
    .to_owned()
}

/// The engine board: one binary state + score per engine.
fn engine_board(
    thesis: &ThesisReadout,
    regime: &regime::RegimeState,
    dealer: &DealerPanel,
    flow: &FlowPanel,
) -> Vec<EngineStatus> {
    let entry = |name: &str, r: &Readout| EngineStatus {
        engine: name.to_owned(),
        state: r.state,
        score: r.score,
    };
    vec![
        entry("THESIS", &thesis.engagement),
        entry("REGIME", &regime.classification.confident),
        entry("COMPRESSION", &regime.compression),
        entry("EXPANSION", &regime.expansion),
        entry("PERSISTENCE", &regime.persistence),
        entry("GAMMA FLIP", &dealer.gamma_flip_state),
        entry("CALL WALL", &dealer.call_wall.state),
        entry("PUT WALL", &dealer.put_wall.state),
        entry("GAMMA DYN", &flow.gamma_dynamics),
        entry("OI FLOW", &flow.oi_flow),
    ]
}

/// Zeroed dealer panel used before the first chain snapshot arrives: all
/// exposures zero, walls/flip absent, states INACTIVE. Never fabricated —
/// absence is represented honestly.
fn empty_dealer_panel() -> DealerPanel {
    let inactive = Readout {
        state: BinaryState::Inactive,
        score: 0.0,
    };
    let wall = WallReadout {
        strike: None,
        margin: 0.0,
        strength: 0.0,
        state: inactive,
    };
    DealerPanel {
        net_gex: 0.0,
        gross_gex: 0.0,
        net_dex: 0.0,
        net_vex: 0.0,
        net_charm: 0.0,
        dsi: 0.0,
        dealer01: 0.5,
        gamma_flip: None,
        gamma_flip_state: inactive,
        call_wall: wall.clone(),
        put_wall: wall,
        expected_move_pct: 0.0,
        magnet: None,
        ladder: Vec::new(),
        excluded_quotes: 0,
    }
}

fn empty_flow_panel() -> FlowPanel {
    let inactive = Readout {
        state: BinaryState::Inactive,
        score: 0.0,
    };
    FlowPanel {
        vanna_flow: 0.0,
        charm_bias: 0.0,
        migration: 0.0,
        gamma_dynamics: inactive,
        oi_flow: inactive,
    }
}
