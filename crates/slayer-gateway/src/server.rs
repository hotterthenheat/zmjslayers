//! HTTP/WebSocket surface.
//!
//! Thin by design: every route reads pre-serialized frames from the
//! [`SnapshotHub`]. No computation happens here.
//!
//! Handlers use axum's `Response` as their error type (the idiomatic
//! fallible-handler pattern); `Response` is a large enum, so the
//! `result_large_err` lint is allowed module-wide rather than boxing every
//! early return.
#![allow(clippy::result_large_err)]

use crate::hub::SnapshotHub;
use axum::{
    Json, Router,
    extract::{
        Path, Query, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Deserialize;
use slayer_core::Symbol;
use tokio::sync::broadcast::error::RecvError;

/// Shared route state.
#[derive(Clone)]
pub struct AppState {
    /// Snapshot fanout hub.
    pub hub: SnapshotHub,
    /// Feed provider name, reported on /health and the WS hello frame.
    pub feed_name: &'static str,
}

/// Build the router.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/v1/health", get(health))
        .route("/api/v1/snapshot/{symbol}", get(snapshot))
        .route("/api/v1/replay/{symbol}", get(replay))
        .route("/ws", get(ws_upgrade))
        .with_state(state)
}

async fn health(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "ok",
        "feed": state.feed_name,
        "wire_version": slayer_core::wire::WIRE_VERSION,
    }))
}

fn parse_symbol(raw: &str) -> Result<Symbol, Response> {
    Symbol::new(raw).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()).into_response())
}

async fn snapshot(
    State(state): State<AppState>,
    Path(symbol): Path<String>,
) -> Result<Response, Response> {
    let symbol = parse_symbol(&symbol)?;
    match state.hub.latest(&symbol).await {
        Some(frame) => Ok(json_response(&frame)),
        None => Err((StatusCode::NOT_FOUND, "no snapshot for symbol").into_response()),
    }
}

/// Replay query: `n` frames, capped by the hub's ring depth.
#[derive(Deserialize)]
struct ReplayQuery {
    n: Option<usize>,
}

/// Default replay length when the client does not specify `n`.
const DEFAULT_REPLAY_FRAMES: usize = 120;

async fn replay(
    State(state): State<AppState>,
    Path(symbol): Path<String>,
    Query(q): Query<ReplayQuery>,
) -> Result<Response, Response> {
    let symbol = parse_symbol(&symbol)?;
    let frames = state.hub.replay(&symbol, q.n.unwrap_or(DEFAULT_REPLAY_FRAMES)).await;
    // Frames are already JSON; join without re-parsing.
    let mut body = String::with_capacity(frames.iter().map(|f| f.len() + 1).sum::<usize>() + 2);
    body.push('[');
    for (i, f) in frames.iter().enumerate() {
        if i > 0 {
            body.push(',');
        }
        body.push_str(f);
    }
    body.push(']');
    Ok(json_response(&body))
}

fn json_response(body: &str) -> Response {
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body.to_owned(),
    )
        .into_response()
}

async fn ws_upgrade(State(state): State<AppState>, upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(move |socket| ws_session(socket, state))
}

async fn ws_session(mut socket: WebSocket, state: AppState) {
    let hello = serde_json::json!({
        "type": "HELLO",
        "wire_version": slayer_core::wire::WIRE_VERSION,
        "feed": state.feed_name,
    });
    if socket.send(Message::Text(hello.to_string().into())).await.is_err() {
        return;
    }
    // Prime with the latest frame per symbol so panels render immediately.
    for frame in state.hub.latest_all().await {
        if socket.send(Message::Text(frame.to_string().into())).await.is_err() {
            return;
        }
    }
    let mut rx = state.hub.subscribe();
    loop {
        tokio::select! {
            recv = rx.recv() => match recv {
                Ok(frame) => {
                    if socket.send(Message::Text(frame.to_string().into())).await.is_err() {
                        return;
                    }
                }
                // Client fell behind the ring: skip forward, keep streaming.
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return,
            },
            // Drain (and ignore) inbound messages; detect disconnect.
            inbound = socket.recv() => {
                match inbound {
                    Some(Ok(_)) => continue,
                    _ => return,
                }
            }
        }
    }
}
