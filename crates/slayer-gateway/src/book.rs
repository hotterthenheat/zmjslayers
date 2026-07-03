//! Per-symbol rolling market state.
//!
//! The book is the single source the engine pipeline reads from: a bounded
//! candle history, the latest chain snapshot, and the latest spot. Pure
//! data — no clocks, no I/O; events carry their own timestamps.

use slayer_core::{Candle, MarketEvent, OptionChain, SpotQuote, Symbol, TsMillis};
use std::collections::{HashMap, VecDeque};

/// Maximum 1-minute candles retained per symbol (one regular session plus
/// warmup context).
pub const CANDLE_CAPACITY: usize = 512;

/// Rolling state for one symbol.
#[derive(Debug, Clone)]
pub struct SymbolBook {
    /// Bounded chronological candle history.
    pub candles: VecDeque<Candle>,
    /// Latest option chain snapshot, if any has arrived.
    pub chain: Option<OptionChain>,
    /// Latest spot quote, if any has arrived.
    pub spot: Option<SpotQuote>,
    /// Timestamp of the most recent event applied.
    pub last_event_ts: TsMillis,
}

impl SymbolBook {
    fn new() -> Self {
        Self {
            candles: VecDeque::with_capacity(CANDLE_CAPACITY),
            chain: None,
            spot: None,
            last_event_ts: TsMillis(0),
        }
    }

    /// Effective spot: explicit quote, else chain snapshot spot, else last
    /// candle close. `None` until any price has arrived.
    #[must_use]
    pub fn effective_spot(&self) -> Option<f64> {
        self.spot
            .as_ref()
            .map(|s| s.price)
            .or_else(|| self.chain.as_ref().map(|c| c.spot))
            .or_else(|| self.candles.back().map(|c| c.close))
    }

    /// Candle history as a contiguous slice (allocates only when the ring
    /// has wrapped).
    #[must_use]
    pub fn candle_slice(&mut self) -> &[Candle] {
        self.candles.make_contiguous()
    }

    fn apply_candle(&mut self, candle: Candle) {
        match self.candles.back() {
            // Same bar re-emitted: replace in place (live feeds update the
            // open bar).
            Some(last) if last.ts == candle.ts => {
                if let Some(slot) = self.candles.back_mut() {
                    *slot = candle;
                }
            }
            // Out-of-order bar: drop. History is append-only.
            Some(last) if last.ts > candle.ts => {}
            _ => {
                if self.candles.len() == CANDLE_CAPACITY {
                    self.candles.pop_front();
                }
                self.candles.push_back(candle);
            }
        }
    }
}

/// All symbol books.
#[derive(Debug, Default)]
pub struct MarketBook {
    books: HashMap<Symbol, SymbolBook>,
}

impl MarketBook {
    /// Empty book set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply one normalized event; returns the symbol it touched.
    pub fn apply(&mut self, event: MarketEvent) -> Symbol {
        let (symbol, ts) = match &event {
            MarketEvent::Chain(c) => (c.underlying.clone(), c.ts),
            MarketEvent::Candle { symbol, candle, .. } => (symbol.clone(), candle.ts),
            MarketEvent::Spot(s) => (s.symbol.clone(), s.ts),
        };
        let book = self
            .books
            .entry(symbol.clone())
            .or_insert_with(SymbolBook::new);
        book.last_event_ts = book.last_event_ts.max(ts);
        match event {
            MarketEvent::Chain(chain) => book.chain = Some(chain),
            MarketEvent::Candle { candle, .. } => book.apply_candle(candle),
            MarketEvent::Spot(spot) => book.spot = Some(spot),
        }
        symbol
    }

    /// Book for a symbol, if events have arrived for it. Used by tests and
    /// reserved for future REST introspection endpoints.
    #[must_use]
    #[allow(dead_code)]
    pub fn get(&self, symbol: &Symbol) -> Option<&SymbolBook> {
        self.books.get(symbol)
    }

    /// Mutable book access (engine pipeline needs `candle_slice`).
    pub fn get_mut(&mut self, symbol: &Symbol) -> Option<&mut SymbolBook> {
        self.books.get_mut(symbol)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn sym(s: &str) -> Symbol {
        Symbol::new(s).unwrap()
    }

    fn candle(ts: u64, close: f64) -> Candle {
        Candle {
            ts: TsMillis(ts),
            open: close,
            high: close,
            low: close,
            close,
            volume: 1.0,
        }
    }

    #[test]
    fn candles_append_replace_and_drop_out_of_order() {
        let mut book = MarketBook::new();
        let s = sym("SPX");
        let ev = |ts, close| MarketEvent::Candle {
            symbol: s.clone(),
            interval: slayer_core::BarInterval::Min1,
            candle: candle(ts, close),
        };
        book.apply(ev(1000, 10.0));
        book.apply(ev(2000, 11.0));
        book.apply(ev(2000, 11.5)); // same-bar update replaces
        book.apply(ev(1500, 99.0)); // out-of-order dropped
        let b = book.get(&s).unwrap();
        assert_eq!(b.candles.len(), 2);
        assert_eq!(b.candles.back().unwrap().close, 11.5);
    }

    #[test]
    fn capacity_is_bounded() {
        let mut book = MarketBook::new();
        let s = sym("SPX");
        for i in 0..(CANDLE_CAPACITY as u64 + 100) {
            book.apply(MarketEvent::Candle {
                symbol: s.clone(),
                interval: slayer_core::BarInterval::Min1,
                candle: candle(i * 60_000, 1.0),
            });
        }
        assert_eq!(book.get(&s).unwrap().candles.len(), CANDLE_CAPACITY);
    }

    #[test]
    fn effective_spot_prefers_quote_then_chain_then_candle() {
        let mut book = MarketBook::new();
        let s = sym("SPX");
        book.apply(MarketEvent::Candle {
            symbol: s.clone(),
            interval: slayer_core::BarInterval::Min1,
            candle: candle(1000, 42.0),
        });
        assert_eq!(book.get(&s).unwrap().effective_spot(), Some(42.0));
        book.apply(MarketEvent::Spot(SpotQuote {
            symbol: s.clone(),
            price: 43.0,
            ts: TsMillis(2000),
        }));
        assert_eq!(book.get(&s).unwrap().effective_spot(), Some(43.0));
    }
}
