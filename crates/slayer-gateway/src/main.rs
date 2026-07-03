//! # slayer-gateway
//!
//! Terminal gateway: subscribes to a feed, runs the engine pipeline, and
//! fans composed [`slayer_core::wire::TerminalSnapshot`] frames out over
//! REST and WebSocket. All I/O and configuration live at this edge; the
//! engines it drives are pure.

mod book;
mod config;
mod hub;
mod pipeline;
mod runtime;
mod server;
mod timecalc;

use config::{FeedKind, GatewayConfig};
use hub::SnapshotHub;
use server::AppState;
use slayer_feed::{FeedProvider, SyntheticConfig, SyntheticFeed};
use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "slayer_gateway=info,tower_http=warn".into()),
        )
        .init();

    match serve().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("fatal: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn serve() -> Result<(), Box<dyn std::error::Error>> {
    let cfg = GatewayConfig::from_env()?;
    tracing::info!(bind = %cfg.bind, universe = ?cfg.universe, "starting slayer-gateway");

    let hub = SnapshotHub::new();

    // Wire the feed to the engine runtime.
    let (feed_name, synthetic, events) = match &cfg.feed {
        FeedKind::Synthetic { seed } => {
            let feed = SyntheticFeed::new(SyntheticConfig::demo(*seed));
            let rx = feed.subscribe(&cfg.universe).await?;
            (feed.name(), true, rx)
        }
    };

    let runtime_hub = hub.clone();
    tokio::spawn(async move {
        runtime::run(events, runtime_hub, synthetic).await;
    });

    // Serve.
    let state = AppState { hub, feed_name };
    let app = server::router(state);
    let listener = tokio::net::TcpListener::bind(cfg.bind).await?;
    tracing::info!(bind = %cfg.bind, "gateway listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutdown signal received");
}
