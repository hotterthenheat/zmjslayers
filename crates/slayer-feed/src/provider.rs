//! The feed provider contract.
//!
//! A provider owns its own production tasks and hands the gateway a bounded
//! channel of normalized [`MarketEvent`]s. Everything downstream is
//! provider-agnostic.

use crate::FeedError;
use async_trait::async_trait;
use slayer_core::{MarketEvent, Symbol};
use tokio::sync::mpsc;

/// Bounded channel depth between a provider and the gateway. Backpressure
/// beyond this indicates the engine pipeline has stalled; dropping the feed
/// connection is preferable to unbounded memory growth.
pub const FEED_CHANNEL_DEPTH: usize = 1024;

/// A market data source producing normalized events for a symbol universe.
#[async_trait]
pub trait FeedProvider: Send + Sync + 'static {
    /// Stable provider name for diagnostics and config.
    fn name(&self) -> &'static str;

    /// Begin streaming events for `universe`. The provider spawns and owns
    /// its production tasks; the stream ends when the provider drops the
    /// sender (fatal upstream error or shutdown).
    async fn subscribe(
        &self,
        universe: &[Symbol],
    ) -> Result<mpsc::Receiver<MarketEvent>, FeedError>;
}
