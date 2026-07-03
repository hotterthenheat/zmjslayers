# Slayer Terminal

Institutional options-structure terminal. Rust engine core, Python quant
research layer, WebGL terminal front end. Built for a 3440 × 1440 desk.

## Layout

| Path | What it is |
| --- | --- |
| `crates/slayer-core` | Shared types, binary-state doctrine, wire schema |
| `crates/slayer-quant` | Pure math kernel: distributions, Black–Scholes, greeks, realized vol, RND, Monte Carlo |
| `crates/slayer-engines` | Signal engines: GEX structure, dealer zones, regime, displacement, SkyVision |
| `crates/slayer-feed` | Feed providers: synthetic, Tradier, ThetaData |
| `crates/slayer-gateway` | axum gateway: engine scheduler, REST snapshot, WebSocket fanout |
| `quant/` | Python research library + golden-vector generator |
| `terminal/` | React + TypeScript + WebGL front end |
| `schema/golden/` | Cross-language golden test vectors |
| `docs/` | Architecture doctrine and legacy extraction specs |

## Run

```sh
# Engine + gateway (synthetic feed, deterministic)
cargo run -p slayer-gateway

# Terminal
cd terminal && pnpm install && pnpm dev

# Python research layer
cd quant && pip install -e .[dev] && pytest
```

Gateway serves REST at `http://127.0.0.1:8787/api/v1/snapshot/:symbol` and
streams `TerminalSnapshot` frames at `ws://127.0.0.1:8787/ws`.

## Doctrine

Read `docs/ARCHITECTURE.md` before writing code. The short version: binary
state everywhere (`ACTIVE`/`INACTIVE`, no third variant), no magic numbers,
engines are pure and deterministic, I/O only at the edges, one wire schema.
