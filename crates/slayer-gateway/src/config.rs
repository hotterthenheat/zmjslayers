//! Gateway configuration.
//!
//! All environment reads happen here — once, at startup, at the I/O edge.
//! Everything downstream receives plain values.

use slayer_core::Symbol;
use std::net::SocketAddr;

/// Default bind address for the gateway.
pub const DEFAULT_BIND: &str = "127.0.0.1:8787";
/// Default synthetic feed seed when none is configured.
pub const DEFAULT_SEED: u64 = 20_260_703;
/// Default symbol universe.
pub const DEFAULT_UNIVERSE: [&str; 2] = ["SPX", "QQQ"];
/// Snapshot frames retained per symbol for replay.
pub const REPLAY_DEPTH: usize = 900;

/// Which feed drives the gateway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedKind {
    /// Deterministic synthetic feed (dev/demo/test).
    Synthetic {
        /// Master seed for the simulation.
        seed: u64,
    },
}

/// Resolved gateway configuration.
#[derive(Debug, Clone)]
pub struct GatewayConfig {
    /// Socket to bind.
    pub bind: SocketAddr,
    /// Symbols to subscribe and serve.
    pub universe: Vec<Symbol>,
    /// Feed selection.
    pub feed: FeedKind,
}

/// Configuration errors are fatal at startup by design.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// Bind address failed to parse.
    #[error("invalid SLAYER_BIND: {0}")]
    Bind(String),
    /// Universe entry failed symbol validation.
    #[error("invalid SLAYER_UNIVERSE entry: {0}")]
    Universe(String),
    /// Seed failed to parse.
    #[error("invalid SLAYER_SEED: {0}")]
    Seed(String),
}

impl GatewayConfig {
    /// Resolve config from process environment, falling back to defaults.
    ///
    /// Recognized variables: `SLAYER_BIND`, `SLAYER_UNIVERSE`
    /// (comma-separated tickers), `SLAYER_SEED`.
    pub fn from_env() -> Result<Self, ConfigError> {
        let bind_raw = std::env::var("SLAYER_BIND").unwrap_or_else(|_| DEFAULT_BIND.to_owned());
        let bind: SocketAddr = bind_raw.parse().map_err(|_| ConfigError::Bind(bind_raw))?;

        let universe_raw = std::env::var("SLAYER_UNIVERSE")
            .unwrap_or_else(|_| DEFAULT_UNIVERSE.join(","));
        let universe = universe_raw
            .split(',')
            .filter(|s| !s.trim().is_empty())
            .map(|s| Symbol::new(s).map_err(|_| ConfigError::Universe(s.to_owned())))
            .collect::<Result<Vec<_>, _>>()?;
        if universe.is_empty() {
            return Err(ConfigError::Universe("(empty)".to_owned()));
        }

        let seed = match std::env::var("SLAYER_SEED") {
            Ok(raw) => raw.parse().map_err(|_| ConfigError::Seed(raw))?,
            Err(_) => DEFAULT_SEED,
        };

        Ok(Self { bind, universe, feed: FeedKind::Synthetic { seed } })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn defaults_resolve() {
        // Note: relies on the test environment not defining SLAYER_* vars.
        let cfg = GatewayConfig::from_env().unwrap();
        assert_eq!(cfg.bind.port(), 8787);
        assert_eq!(cfg.universe.len(), 2);
        assert_eq!(cfg.feed, FeedKind::Synthetic { seed: DEFAULT_SEED });
    }
}
