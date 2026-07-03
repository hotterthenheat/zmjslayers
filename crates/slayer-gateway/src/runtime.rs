//! Engine runtime: the loop that turns feed events into published snapshots.
//!
//! A single task owns the [`MarketBook`] and [`PipelineState`], consuming
//! normalized events from the feed. On each candle close and each chain
//! snapshot it recomposes the affected symbol's [`TerminalSnapshot`],
//! serializes it once, and hands the bytes to the [`SnapshotHub`] for fanout.
//! Spot ticks update the book but do not by themselves trigger a recompute —
//! the following candle reflects them.

use crate::book::MarketBook;
use crate::hub::SnapshotHub;
use crate::pipeline::PipelineState;
use slayer_core::wire::WireFrame;
use slayer_core::{MarketEvent, Symbol};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Drive the runtime until the feed closes. `synthetic` marks emitted
/// snapshots so the terminal can label a non-live feed.
pub async fn run(mut events: mpsc::Receiver<MarketEvent>, hub: SnapshotHub, synthetic: bool) {
    let mut book = MarketBook::new();
    let mut pipeline = PipelineState::new();

    while let Some(event) = events.recv().await {
        let recompute = matches!(event, MarketEvent::Candle { .. } | MarketEvent::Chain(_));
        let symbol = book.apply(event);
        if recompute {
            publish(&mut book, &mut pipeline, &hub, &symbol, synthetic).await;
        }
    }
    tracing::warn!("feed stream ended; runtime stopping");
}

async fn publish(
    book: &mut MarketBook,
    pipeline: &mut PipelineState,
    hub: &SnapshotHub,
    symbol: &Symbol,
    synthetic: bool,
) {
    let Some(sym_book) = book.get_mut(symbol) else {
        return;
    };
    let ts = sym_book.last_event_ts;
    let Some(spot) = sym_book.effective_spot() else {
        return;
    };
    // Clone the small amount of state compose needs so the mutable book
    // borrow is released before composition (candles are a bounded ring).
    let candles = sym_book.candle_slice().to_vec();
    let chain = sym_book.chain.clone();

    let Some(snapshot) = pipeline.compose(symbol, &candles, chain.as_ref(), spot, ts, synthetic)
    else {
        return;
    };

    let frame = WireFrame::Snapshot(Box::new(snapshot));
    match serde_json::to_string(&frame) {
        Ok(json) => hub.publish(symbol, Arc::from(json.as_str())).await,
        Err(e) => tracing::error!("snapshot serialization failed for {symbol}: {e}"),
    }
}
