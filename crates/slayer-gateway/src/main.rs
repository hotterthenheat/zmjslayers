//! # slayer-gateway
//!
//! Terminal gateway: feed subscription, engine scheduling, snapshot fanout.
//! The engine pipeline module lands with `slayer-engines`; until then the
//! binary only wires config and serves health.

mod book;
mod config;
mod hub;
mod server;

fn main() {
    // Real entrypoint lands with the engine pipeline.
}
