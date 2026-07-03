//! Snapshot fanout hub.
//!
//! Frames are serialized exactly once by the pipeline; the hub stores and
//! fans out shared immutable strings (`Arc<str>`). Slow WebSocket clients
//! lag on the broadcast channel and skip forward — they never apply
//! backpressure to the pipeline.

use crate::config::REPLAY_DEPTH;
use slayer_core::Symbol;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::{RwLock, broadcast};

/// Broadcast channel depth. A client further behind than this many frames
/// skips ahead rather than stalling the publisher.
const BROADCAST_DEPTH: usize = 256;

/// Shared snapshot hub: latest frame per symbol, bounded replay history,
/// and a live broadcast.
#[derive(Clone)]
pub struct SnapshotHub {
    inner: Arc<HubInner>,
}

struct HubInner {
    tx: broadcast::Sender<Arc<str>>,
    latest: RwLock<HashMap<Symbol, Arc<str>>>,
    replay: RwLock<HashMap<Symbol, VecDeque<Arc<str>>>>,
}

impl SnapshotHub {
    /// New hub with an idle broadcast channel.
    #[must_use]
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(BROADCAST_DEPTH);
        Self {
            inner: Arc::new(HubInner {
                tx,
                latest: RwLock::new(HashMap::new()),
                replay: RwLock::new(HashMap::new()),
            }),
        }
    }

    /// Publish a serialized frame for `symbol`: updates latest, appends to
    /// the replay ring, and broadcasts to live subscribers.
    pub async fn publish(&self, symbol: &Symbol, frame: Arc<str>) {
        {
            let mut latest = self.inner.latest.write().await;
            latest.insert(symbol.clone(), Arc::clone(&frame));
        }
        {
            let mut replay = self.inner.replay.write().await;
            let ring = replay.entry(symbol.clone()).or_default();
            if ring.len() == REPLAY_DEPTH {
                ring.pop_front();
            }
            ring.push_back(Arc::clone(&frame));
        }
        // send() only errors when no receiver exists — idle is fine.
        let _ = self.inner.tx.send(frame);
    }

    /// Latest frame for a symbol.
    pub async fn latest(&self, symbol: &Symbol) -> Option<Arc<str>> {
        self.inner.latest.read().await.get(symbol).cloned()
    }

    /// Latest frame for every symbol (WS connect priming).
    pub async fn latest_all(&self) -> Vec<Arc<str>> {
        self.inner.latest.read().await.values().cloned().collect()
    }

    /// Up to `n` most recent frames for a symbol, oldest first.
    pub async fn replay(&self, symbol: &Symbol, n: usize) -> Vec<Arc<str>> {
        let replay = self.inner.replay.read().await;
        replay
            .get(symbol)
            .map(|ring| ring.iter().rev().take(n).rev().cloned().collect())
            .unwrap_or_default()
    }

    /// Subscribe to the live frame stream.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<str>> {
        self.inner.tx.subscribe()
    }
}

impl Default for SnapshotHub {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn sym(s: &str) -> Symbol {
        Symbol::new(s).unwrap()
    }

    #[tokio::test]
    async fn publish_updates_latest_replay_and_broadcast() {
        let hub = SnapshotHub::new();
        let s = sym("SPX");
        let mut rx = hub.subscribe();
        hub.publish(&s, Arc::from(r#"{"n":1}"#)).await;
        hub.publish(&s, Arc::from(r#"{"n":2}"#)).await;

        assert_eq!(&*hub.latest(&s).await.unwrap(), r#"{"n":2}"#);
        let replay = hub.replay(&s, 10).await;
        assert_eq!(replay.len(), 2);
        assert_eq!(&*replay[0], r#"{"n":1}"#);
        assert_eq!(&*rx.recv().await.unwrap(), r#"{"n":1}"#);
        assert_eq!(&*rx.recv().await.unwrap(), r#"{"n":2}"#);
    }

    #[tokio::test]
    async fn replay_ring_is_bounded() {
        let hub = SnapshotHub::new();
        let s = sym("SPX");
        for i in 0..(REPLAY_DEPTH + 50) {
            hub.publish(&s, Arc::from(format!("{i}"))).await;
        }
        let replay = hub.replay(&s, usize::MAX).await;
        assert_eq!(replay.len(), REPLAY_DEPTH);
        assert_eq!(&*replay[0], "50");
    }

    #[tokio::test]
    async fn unknown_symbol_yields_empty() {
        let hub = SnapshotHub::new();
        assert!(hub.latest(&sym("ZZZ")).await.is_none());
        assert!(hub.replay(&sym("ZZZ"), 5).await.is_empty());
    }
}
