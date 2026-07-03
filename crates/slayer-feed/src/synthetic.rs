//! Deterministic synthetic market feed.
//!
//! Dev/demo/test infrastructure: a seeded GBM spot process with a
//! smile-consistent options chain synthesized around it through the pricing
//! kernel. Explicitly *synthetic* — it exists so the full pipeline can run
//! and be tested without upstream credentials, and it is labeled as such at
//! the wire level by the gateway. This replaces the legacy mock generator
//! (spec 01 §E9) which fed fabricated data into production paths unlabeled.
//!
//! Determinism: all randomness derives from the configured seed; simulated
//! time advances by fixed steps from a configured epoch. Wall-clock time is
//! used only to pace emission.

use crate::{FeedError, FeedProvider, provider::FEED_CHANNEL_DEPTH};
use async_trait::async_trait;
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha12Rng;
use slayer_core::{
    BarInterval, Candle, ExpiryDate, Greeks, MarketEvent, OptionChain, OptionQuote, OptionRight,
    SpotQuote, Symbol, TsMillis,
};
use slayer_quant::black_scholes::{self, BsInputs};
use std::time::Duration;
use tokio::sync::mpsc;

/// Simulated milliseconds advanced per tick (one simulated minute). Each
/// tick closes one 1-minute candle.
const SIM_STEP_MS: u64 = 60_000;
/// Intrabar GBM sub-steps integrated per 1-minute candle so each bar carries a
/// real high/low path (single-step bars starve range-based vol estimators).
const INTRA_STEPS_PER_BAR: usize = 24;
/// Ticks per emitted chain snapshot.
const TICKS_PER_CHAIN: u32 = 5;
/// Strikes generated on each side of spot.
const STRIKES_PER_SIDE: i32 = 15;
/// Simulated days to expiry for the synthetic chain.
const CHAIN_DTE_DAYS: f64 = 5.0;
/// Risk-free rate used to price the synthetic chain.
const RATE: f64 = 0.045;
/// ATM implied vol lift over realized vol (a persistent variance premium).
const IV_LIFT: f64 = 1.08;
/// Smile curvature: IV increase per squared 10%-moneyness unit.
const SMILE_CURVATURE: f64 = 1.5;
/// Put-side linear skew per 10% moneyness.
const PUT_SKEW: f64 = 0.10;
/// Open-interest hump peak, contracts.
const OI_PEAK: f64 = 18_000.0;
/// Open-interest floor at far strikes, contracts.
const OI_FLOOR: f64 = 150.0;
/// Width of the OI hump in strike-step units.
const OI_HUMP_WIDTH: f64 = 5.0;
/// OI multiplier at round strikes (institutional clustering).
const ROUND_STRIKE_OI_BOOST: f64 = 2.0;
/// Fraction of OI that trades per session at the ATM strike.
const ATM_TURNOVER: f64 = 0.6;
/// Bid/ask half-spread as a fraction of theoretical price.
const HALF_SPREAD_FRAC: f64 = 0.015;
/// Minimum quoted bid, points.
const MIN_BID: f64 = 0.05;
/// Minimum bid/ask separation so a floored quote never crosses, points.
const MIN_TICK: f64 = 0.01;
/// Milliseconds per calendar day, for stamping the chain expiry.
const MS_PER_DAY: u64 = 86_400_000;
/// Vol-of-vol for the wandering base-volatility regime. Sized so the base vol
/// meaningfully drifts (stationary σ ≈ a few vol points) over a session —
/// otherwise the compression/expansion reads and the [`VOL_BOUNDS`] clamp stay
/// inert.
const VOL_OF_VOL: f64 = 0.9;
/// Base-vol mean-reversion rate, anchoring the drift to the symbol's base vol.
const VOL_MEAN_REVERT: f64 = 0.06;
/// Bounds for the wandering base volatility (a safety clamp on the SDE tails).
const VOL_BOUNDS: (f64, f64) = (0.06, 0.9);

/// Per-symbol synthetic parameters.
#[derive(Debug, Clone)]
pub struct SyntheticSymbol {
    /// Ticker.
    pub symbol: Symbol,
    /// Starting spot, points.
    pub spot: f64,
    /// Starting annualized volatility, decimal.
    pub vol: f64,
    /// Strike grid step, points.
    pub strike_step: f64,
}

/// Synthetic feed configuration.
#[derive(Debug, Clone)]
pub struct SyntheticConfig {
    /// Master seed; every stream derived from it is deterministic.
    pub seed: u64,
    /// Simulated epoch for the first event.
    pub start_ts: TsMillis,
    /// Real-time pacing between ticks. Zero paces as fast as possible
    /// (tests).
    pub tick_interval: Duration,
    /// Number of historical 1-minute candles emitted immediately on
    /// subscribe so downstream indicator warmup is instant.
    pub warmup_bars: usize,
    /// Symbols to simulate.
    pub symbols: Vec<SyntheticSymbol>,
}

impl SyntheticConfig {
    /// A standard demo universe: an index-like and an ETF-like symbol.
    ///
    /// # Panics
    /// Never — the symbol literals are statically valid.
    #[must_use]
    pub fn demo(seed: u64) -> Self {
        #[allow(clippy::unwrap_used)]
        let sym = |s: &str| Symbol::new(s).unwrap();
        Self {
            seed,
            start_ts: TsMillis(1_767_225_600_000), // 2026-01-01T00:00:00Z, fixed epoch
            tick_interval: Duration::from_millis(500),
            warmup_bars: 120,
            symbols: vec![
                SyntheticSymbol {
                    symbol: sym("SPX"),
                    spot: 6800.0,
                    vol: 0.14,
                    strike_step: 25.0,
                },
                SyntheticSymbol {
                    symbol: sym("QQQ"),
                    spot: 620.0,
                    vol: 0.19,
                    strike_step: 5.0,
                },
            ],
        }
    }
}

/// Deterministic synthetic market feed provider.
pub struct SyntheticFeed {
    config: SyntheticConfig,
}

impl SyntheticFeed {
    /// Build a synthetic feed from config.
    #[must_use]
    pub fn new(config: SyntheticConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl FeedProvider for SyntheticFeed {
    fn name(&self) -> &'static str {
        "synthetic"
    }

    async fn subscribe(
        &self,
        universe: &[Symbol],
    ) -> Result<mpsc::Receiver<MarketEvent>, FeedError> {
        let selected: Vec<SyntheticSymbol> = self
            .config
            .symbols
            .iter()
            .filter(|s| universe.contains(&s.symbol))
            .cloned()
            .collect();
        if selected.is_empty() {
            return Err(FeedError::UnsupportedUniverse(format!(
                "none of {universe:?} are configured for the synthetic feed"
            )));
        }
        let (tx, rx) = mpsc::channel(FEED_CHANNEL_DEPTH);
        let config = self.config.clone();
        tokio::spawn(async move {
            run_simulation(config, selected, tx).await;
        });
        Ok(rx)
    }
}

/// One symbol's evolving simulation state.
struct SymState {
    params: SyntheticSymbol,
    spot: f64,
    vol: f64,
    /// OHLC accumulator for the candle under construction.
    bar_open: f64,
    bar_high: f64,
    bar_low: f64,
    bar_volume: f64,
    rng: ChaCha12Rng,
}

impl SymState {
    fn new(params: SyntheticSymbol, master_seed: u64) -> Self {
        // Derive a per-symbol stream from the master seed and the full
        // ticker bytes (legacy defect D2 seeded off the first character
        // only, colliding SPY/SPX/SMH).
        let mut seed = master_seed;
        for b in params.symbol.as_str().bytes() {
            seed = seed
                .wrapping_mul(0x0100_0000_01b3)
                .wrapping_add(u64::from(b));
        }
        let spot = params.spot;
        let vol = params.vol;
        Self {
            params,
            spot,
            vol,
            bar_open: spot,
            bar_high: spot,
            bar_low: spot,
            bar_volume: 0.0,
            rng: ChaCha12Rng::seed_from_u64(seed),
        }
    }

    /// Advance one simulated minute of GBM with a mean-reverting vol regime.
    ///
    /// The minute is integrated as [`INTRA_STEPS_PER_BAR`] finer sub-steps so
    /// each candle carries a genuine intrabar high/low path — range-based
    /// realized-vol estimators would systematically under-read a single-step
    /// bar (open→close only).
    fn step(&mut self) {
        const MINUTES_PER_YEAR: f64 = 252.0 * 390.0;
        let dt = 1.0 / (MINUTES_PER_YEAR * INTRA_STEPS_PER_BAR as f64);
        let sqrt_dt = dt.sqrt();
        let mut path_abs_z = 0.0;
        for _ in 0..INTRA_STEPS_PER_BAR {
            let z: f64 = sample_normal(&mut self.rng);
            self.spot *= (self.vol * sqrt_dt * z - 0.5 * self.vol * self.vol * dt).exp();
            self.bar_high = self.bar_high.max(self.spot);
            self.bar_low = self.bar_low.min(self.spot);
            path_abs_z += z.abs();
        }
        // Vol regime wanders once per bar (its own √(bar-dt) scale).
        let bar_dt = 1.0 / MINUTES_PER_YEAR;
        let vz: f64 = sample_normal(&mut self.rng);
        self.vol = (self.vol
            + VOL_OF_VOL * bar_dt.sqrt() * vz * self.vol
            + VOL_MEAN_REVERT * (self.params.vol - self.vol) * bar_dt.sqrt())
        .clamp(VOL_BOUNDS.0, VOL_BOUNDS.1);
        self.bar_volume += 1_000.0 * (1.0 + path_abs_z / INTRA_STEPS_PER_BAR as f64);
    }

    fn close_candle(&mut self, ts: TsMillis) -> Candle {
        let candle = Candle {
            ts,
            open: self.bar_open,
            high: self.bar_high,
            low: self.bar_low,
            close: self.spot,
            volume: self.bar_volume,
        };
        self.bar_open = self.spot;
        self.bar_high = self.spot;
        self.bar_low = self.spot;
        self.bar_volume = 0.0;
        candle
    }

    /// Synthesize a smile-consistent chain around current spot.
    fn chain(&mut self, ts: TsMillis) -> OptionChain {
        let t_years = CHAIN_DTE_DAYS / 365.0;
        // Stamp an expiry consistent with the priced tenor: `CHAIN_DTE_DAYS`
        // civil days after the snapshot date. A fixed far-dated expiry would
        // make the gateway recover a ~1-year tenor from the date while the
        // greeks were priced at 5 DTE — an 8× expected-move error.
        #[allow(clippy::cast_possible_wrap)]
        let snapshot_day = (ts.0 / MS_PER_DAY) as i64;
        let expiry = civil_from_days(snapshot_day + CHAIN_DTE_DAYS as i64);
        let step = self.params.strike_step;
        let center = (self.spot / step).round() * step;
        let mut quotes = Vec::with_capacity((STRIKES_PER_SIDE as usize * 2 + 1) * 2);
        for offset in -STRIKES_PER_SIDE..=STRIKES_PER_SIDE {
            let strike = f64::from(offset).mul_add(step, center);
            if strike <= 0.0 {
                continue;
            }
            // Moneyness in 10%-of-spot units drives smile curvature.
            let m = (strike - self.spot) / (0.1 * self.spot);
            for right in [OptionRight::Call, OptionRight::Put] {
                let skew = match right {
                    OptionRight::Call => 0.0,
                    OptionRight::Put => PUT_SKEW * (-m).max(0.0),
                };
                let iv = (self.vol * IV_LIFT * (1.0 + SMILE_CURVATURE * m * m * 0.1) + skew)
                    .clamp(0.03, 3.0);
                let inputs = BsInputs {
                    spot: self.spot,
                    strike,
                    t_years,
                    vol: iv,
                    rate: RATE,
                    div_yield: 0.0,
                };
                let (Ok(theo), Ok(g)) = (
                    black_scholes::price(&inputs, right),
                    black_scholes::greeks(&inputs, right),
                ) else {
                    continue;
                };
                // OI hump centered slightly OTM per side, boosted at round
                // strikes, with deterministic per-quote jitter.
                let hump_center = match right {
                    OptionRight::Call => 2.0,
                    OptionRight::Put => -2.0,
                };
                let x = (f64::from(offset) - hump_center) / OI_HUMP_WIDTH;
                #[allow(clippy::cast_possible_truncation)]
                let steps_from_zero = (strike / step).round() as i64;
                let round_boost = if steps_from_zero.rem_euclid(5) == 0 {
                    ROUND_STRIKE_OI_BOOST
                } else {
                    1.0
                };
                let jitter: f64 = self.rng.random_range(0.85..1.15);
                let oi = ((OI_PEAK * (-x * x).exp() + OI_FLOOR) * round_boost * jitter).round();
                let turnover = ATM_TURNOVER * (-0.5 * m * m).exp();
                let volume = (oi * turnover).round();
                let half_spread = (theo * HALF_SPREAD_FRAC).max(MIN_BID / 2.0);
                // Keep the two-sided market uncrossed: once the bid is floored
                // at MIN_BID, the ask must clear it by at least one tick (a
                // deep-OTM theoretical below the floor would otherwise invert).
                let bid = (theo - half_spread).max(MIN_BID);
                let ask = (theo + half_spread).max(bid + MIN_TICK);
                quotes.push(OptionQuote {
                    strike,
                    right,
                    expiry,
                    bid: Some(bid),
                    ask: Some(ask),
                    volume: volume as u64,
                    open_interest: oi as u64,
                    iv: Some(iv),
                    greeks: Some(Greeks {
                        delta: g.delta,
                        gamma: g.gamma,
                        theta: g.theta / black_scholes::THETA_PER_DAY,
                        vega: g.vega / black_scholes::VEGA_PER_VOL_POINT,
                    }),
                });
            }
        }
        OptionChain {
            underlying: self.params.symbol.clone(),
            spot: self.spot,
            ts,
            multiplier: 100.0,
            quotes,
        }
    }
}

/// Civil (Gregorian) date from days since the Unix epoch (Howard Hinnant's
/// algorithm). Used to stamp a chain expiry consistent with its priced tenor.
fn civil_from_days(z: i64) -> ExpiryDate {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    ExpiryDate {
        year: year as u16,
        month: m as u8,
        day: d as u8,
    }
}

/// Standard normal via Box–Muller on the provider's seeded stream.
fn sample_normal(rng: &mut ChaCha12Rng) -> f64 {
    let u1: f64 = rng.random_range(f64::MIN_POSITIVE..1.0);
    let u2: f64 = rng.random_range(0.0..1.0);
    (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
}

async fn run_simulation(
    config: SyntheticConfig,
    symbols: Vec<SyntheticSymbol>,
    tx: mpsc::Sender<MarketEvent>,
) {
    let mut states: Vec<SymState> = symbols
        .into_iter()
        .map(|s| SymState::new(s, config.seed))
        .collect();
    let mut sim_ts = config.start_ts;

    // Warmup burst: emit history so indicator warmup is instant.
    for _ in 0..config.warmup_bars {
        for st in &mut states {
            st.step();
            let candle = st.close_candle(sim_ts);
            let event = MarketEvent::Candle {
                symbol: st.params.symbol.clone(),
                interval: BarInterval::Min1,
                candle,
            };
            if tx.send(event).await.is_err() {
                return;
            }
        }
        sim_ts = TsMillis(sim_ts.0 + SIM_STEP_MS);
    }

    // Live loop.
    let mut tick: u32 = 0;
    loop {
        if !config.tick_interval.is_zero() {
            tokio::time::sleep(config.tick_interval).await;
        }
        tick = tick.wrapping_add(1);
        sim_ts = TsMillis(sim_ts.0 + SIM_STEP_MS);
        for st in &mut states {
            st.step();
            let spot_event = MarketEvent::Spot(SpotQuote {
                symbol: st.params.symbol.clone(),
                price: st.spot,
                ts: sim_ts,
            });
            if tx.send(spot_event).await.is_err() {
                return;
            }
            let candle = st.close_candle(sim_ts);
            let event = MarketEvent::Candle {
                symbol: st.params.symbol.clone(),
                interval: BarInterval::Min1,
                candle,
            };
            if tx.send(event).await.is_err() {
                return;
            }
            if tick.is_multiple_of(TICKS_PER_CHAIN) {
                let event = MarketEvent::Chain(st.chain(sim_ts));
                if tx.send(event).await.is_err() {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn test_config(seed: u64) -> SyntheticConfig {
        SyntheticConfig {
            tick_interval: Duration::ZERO,
            warmup_bars: 30,
            ..SyntheticConfig::demo(seed)
        }
    }

    async fn collect(seed: u64, n: usize) -> Vec<MarketEvent> {
        let feed = SyntheticFeed::new(test_config(seed));
        let universe = vec![Symbol::new("SPX").unwrap()];
        let mut rx = feed.subscribe(&universe).await.unwrap();
        let mut out = Vec::with_capacity(n);
        while out.len() < n {
            out.push(rx.recv().await.unwrap());
        }
        out
    }

    #[tokio::test]
    async fn deterministic_for_fixed_seed() {
        let a = collect(42, 200).await;
        let b = collect(42, 200).await;
        assert_eq!(a, b);
    }

    #[tokio::test]
    async fn different_seeds_diverge() {
        let a = collect(1, 200).await;
        let b = collect(2, 200).await;
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn warmup_candles_arrive_first_and_are_sane() {
        let events = collect(7, 30).await;
        for e in &events {
            match e {
                MarketEvent::Candle { candle, .. } => {
                    assert!(candle.high >= candle.low);
                    assert!(candle.high >= candle.open.max(candle.close));
                    assert!(candle.low <= candle.open.min(candle.close));
                    assert!(candle.volume > 0.0);
                }
                other => panic!("expected warmup candle, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn chains_are_smile_consistent_and_arbitrage_sane() {
        let events = collect(11, 600).await;
        let chain = events
            .iter()
            .find_map(|e| match e {
                MarketEvent::Chain(c) => Some(c),
                _ => None,
            })
            .expect("a chain within 600 events");
        assert!(!chain.quotes.is_empty());
        for q in &chain.quotes {
            let (bid, ask) = (q.bid.unwrap(), q.ask.unwrap());
            assert!(bid > 0.0 && ask >= bid, "quoted market must be two-sided");
            assert!(q.iv.unwrap() > 0.0);
            assert!(q.open_interest > 0);
            let g = q.greeks.unwrap();
            assert!(g.gamma >= 0.0, "long option gamma is non-negative");
            match q.right {
                OptionRight::Call => assert!((0.0..=1.0).contains(&g.delta)),
                OptionRight::Put => assert!((-1.0..=0.0).contains(&g.delta)),
            }
        }
        // Smile: far-wing IV exceeds ATM IV.
        let atm = chain
            .quotes
            .iter()
            .min_by(|a, b| {
                (a.strike - chain.spot)
                    .abs()
                    .total_cmp(&(b.strike - chain.spot).abs())
            })
            .unwrap();
        let wing = chain
            .quotes
            .iter()
            .max_by(|a, b| {
                (a.strike - chain.spot)
                    .abs()
                    .total_cmp(&(b.strike - chain.spot).abs())
            })
            .unwrap();
        assert!(wing.iv.unwrap() > atm.iv.unwrap());
    }

    #[tokio::test]
    async fn unknown_universe_is_rejected() {
        let feed = SyntheticFeed::new(test_config(1));
        let universe = vec![Symbol::new("ZZZ").unwrap()];
        assert!(matches!(
            feed.subscribe(&universe).await,
            Err(FeedError::UnsupportedUniverse(_))
        ));
    }
}
