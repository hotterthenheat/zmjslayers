//! Normalized market data types.
//!
//! Everything downstream of `slayer-feed` consumes these shapes and nothing
//! else — engines never see a provider payload. Time is always a parameter
//! ([`TsMillis`], UTC); no type in this module reads a clock.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Milliseconds since the Unix epoch, UTC.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct TsMillis(pub u64);

impl TsMillis {
    /// Milliseconds elapsed from `earlier` to `self`, saturating at zero.
    #[must_use]
    pub const fn saturating_since(self, earlier: TsMillis) -> u64 {
        self.0.saturating_sub(earlier.0)
    }
}

/// An uppercase ticker symbol (e.g. `SPX`, `SPY`, `QQQ`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Symbol(String);

impl Symbol {
    /// Normalize to uppercase. Rejects empty and non-alphanumeric input
    /// (dots and slashes for share classes/futures roots are permitted).
    pub fn new(raw: &str) -> Result<Self, crate::CoreError> {
        let s = raw.trim().to_ascii_uppercase();
        let valid = !s.is_empty()
            && s.len() <= 12
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '/');
        if valid {
            Ok(Self(s))
        } else {
            Err(crate::CoreError::InvalidSymbol(raw.to_owned()))
        }
    }

    /// The normalized ticker string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Option right.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OptionRight {
    /// Call option.
    Call,
    /// Put option.
    Put,
}

/// Calendar expiry date. Plain data — session/time-zone math happens at the
/// edges, not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ExpiryDate {
    /// Four-digit year.
    pub year: u16,
    /// Month, 1–12.
    pub month: u8,
    /// Day of month, 1–31.
    pub day: u8,
}

impl fmt::Display for ExpiryDate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

/// First-order option greeks, per contract, as supplied by a provider or
/// computed by the kernel.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Greeks {
    /// ∂V/∂S, in [-1, 1] per share.
    pub delta: f64,
    /// ∂²V/∂S², per share per point.
    pub gamma: f64,
    /// ∂V/∂t, per calendar day (negative for long options).
    pub theta: f64,
    /// ∂V/∂σ, per volatility point.
    pub vega: f64,
}

/// One option quote within a chain, normalized.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OptionQuote {
    /// Strike price in underlying points.
    pub strike: f64,
    /// Call or put.
    pub right: OptionRight,
    /// Expiry date.
    pub expiry: ExpiryDate,
    /// Best bid, points. `None` when the market is one-sided or crossed out.
    pub bid: Option<f64>,
    /// Best ask, points.
    pub ask: Option<f64>,
    /// Contracts traded this session.
    pub volume: u64,
    /// Open interest, contracts.
    pub open_interest: u64,
    /// Annualized implied volatility as a decimal (0.20 = 20%), if known.
    pub iv: Option<f64>,
    /// Provider-supplied greeks, if any. The kernel recomputes when absent.
    pub greeks: Option<Greeks>,
}

impl OptionQuote {
    /// Midpoint of bid/ask when both sides exist and are sane.
    #[must_use]
    pub fn mid(&self) -> Option<f64> {
        match (self.bid, self.ask) {
            (Some(b), Some(a)) if b >= 0.0 && a >= b => Some(0.5 * (a + b)),
            _ => None,
        }
    }
}

/// A normalized option chain snapshot for one underlying.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OptionChain {
    /// Underlying ticker.
    pub underlying: Symbol,
    /// Underlying spot at snapshot time, points.
    pub spot: f64,
    /// Snapshot time.
    pub ts: TsMillis,
    /// Contract multiplier (100 for standard US equity/index options).
    pub multiplier: f64,
    /// All quotes in the snapshot window.
    pub quotes: Vec<OptionQuote>,
}

/// One OHLCV bar. `ts` is the bar open time.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Candle {
    /// Bar open time.
    pub ts: TsMillis,
    /// Open price, points.
    pub open: f64,
    /// High price, points.
    pub high: f64,
    /// Low price, points.
    pub low: f64,
    /// Close price, points.
    pub close: f64,
    /// Shares/contracts traded in the bar.
    pub volume: f64,
}

/// Bar interval for candle series.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BarInterval {
    /// One-minute bars.
    Min1,
    /// Five-minute bars.
    Min5,
    /// Daily bars.
    Day1,
}

/// A spot (underlying) quote.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpotQuote {
    /// Ticker.
    pub symbol: Symbol,
    /// Last/mark price, points.
    pub price: f64,
    /// Quote time.
    pub ts: TsMillis,
}

/// A normalized inbound market event. The single currency between
/// `slayer-feed` and the engine scheduler.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MarketEvent {
    /// Fresh option chain snapshot.
    Chain(OptionChain),
    /// New or updated candle.
    Candle {
        /// Ticker the bar belongs to.
        symbol: Symbol,
        /// Bar interval.
        interval: BarInterval,
        /// The bar itself.
        candle: Candle,
    },
    /// Spot tick.
    Spot(SpotQuote),
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn symbol_normalizes_and_validates() {
        assert_eq!(Symbol::new(" spx ").unwrap().as_str(), "SPX");
        assert_eq!(Symbol::new("brk.b").unwrap().as_str(), "BRK.B");
        assert!(Symbol::new("").is_err());
        assert!(Symbol::new("BAD SYMBOL").is_err());
    }

    #[test]
    fn mid_requires_sane_two_sided_market() {
        let q = |bid, ask| OptionQuote {
            strike: 100.0,
            right: OptionRight::Call,
            expiry: ExpiryDate {
                year: 2026,
                month: 12,
                day: 18,
            },
            bid,
            ask,
            volume: 0,
            open_interest: 0,
            iv: None,
            greeks: None,
        };
        assert_eq!(q(Some(1.0), Some(2.0)).mid(), Some(1.5));
        assert_eq!(q(None, Some(2.0)).mid(), None);
        assert_eq!(q(Some(3.0), Some(2.0)).mid(), None); // crossed
    }

    #[test]
    fn expiry_displays_iso() {
        let e = ExpiryDate {
            year: 2026,
            month: 7,
            day: 2,
        };
        assert_eq!(e.to_string(), "2026-07-02");
    }
}
