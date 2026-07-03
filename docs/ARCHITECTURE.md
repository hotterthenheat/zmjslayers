# Slayer Terminal — System Architecture

Slayer Terminal is a standalone, institutional-grade options-structure terminal.
It ingests live options-chain and index data, resolves dealer-positioning
structure through a deterministic engine pipeline, and renders a dense,
ultra-wide trading surface. Every component in this repository exists to serve
one loop:

```
feed tick ──▶ normalize ──▶ engine pipeline ──▶ TerminalSnapshot ──▶ WS fanout ──▶ render
             (slayer-feed)  (slayer-engines)    (slayer-core)       (slayer-gateway) (terminal/)
```

## 1. Topology

```
┌──────────────────────────────────────────────────────────────────────────┐
│                              slayer-gateway                              │
│   axum · REST snapshot · WebSocket fanout · engine scheduler · replay    │
└───────────────▲──────────────────────────────────────▲───────────────────┘
                │ EngineOutputs                        │ MarketEvent
┌───────────────┴───────────────┐        ┌─────────────┴───────────────────┐
│        slayer-engines         │        │           slayer-feed           │
│ GEX · zones · regime · vol ·  │        │ FeedProvider trait · synthetic  │
│ displacement · skyvision      │        │ generator · Tradier · ThetaData │
└───────────────▲───────────────┘        └─────────────▲───────────────────┘
                │                                      │
┌───────────────┴──────────────────────────────────────┴───────────────────┐
│                    slayer-quant (pure math kernel)                       │
│  distributions · Black–Scholes · greeks · realized vol · RND · MC        │
└───────────────────────────────▲──────────────────────────────────────────┘
                                │
┌───────────────────────────────┴──────────────────────────────────────────┐
│              slayer-core (types, state doctrine, wire schema)            │
└──────────────────────────────────────────────────────────────────────────┘
```

Crate dependency DAG (an arrow means "depends on"):

```
gateway ──▶ engines ──▶ quant ──▶ core
gateway ──▶ feed    ──▶ core
```

`slayer-core` depends on nothing but `serde`. `slayer-quant` is `#![forbid(unsafe_code)]`
pure math with zero I/O. Engines are pure functions from market state to
readouts. All I/O lives at the edges: `slayer-feed` (inbound) and
`slayer-gateway` (outbound).

Alongside the Rust runtime:

- **`quant/`** — Python research library (`slayer_quant`). Mirrors the kernel
  math in vectorized NumPy/SciPy for calibration, backtesting, and as the
  independent reference implementation for parity testing.
- **`terminal/`** — React + TypeScript + WebGL front end. Renders the
  `TerminalSnapshot` wire schema. Holds **no** quant logic: the terminal never
  recomputes what an engine already resolved.
- **`schema/golden/`** — cross-language golden vectors. Fixed inputs with
  expected outputs generated from the SciPy reference. Rust and Python test
  suites both assert against the same JSON files; drift between languages is a
  test failure, not a code-review debate.

## 2. Binary State Doctrine

Every engine, structural zone, and UI readout resolves to exactly one of two
states:

- **`ACTIVE`**
- **`INACTIVE`**

There is no third variant. Not in the Rust enum, not in the wire schema, not in
the TypeScript union, not in the CSS. Legacy labels (`HOLDING`, `TESTING`,
`FAILING`) were ambiguity laundered as information; they are gone at the type
level:

```rust
// slayer-core
pub enum BinaryState { Active, Inactive }
```

```typescript
// terminal
type BinaryState = 'ACTIVE' | 'INACTIVE'
```

The information those ternary labels tried to carry is preserved *properly*: every
stateful readout ships `state: BinaryState` **plus** `score: f64` — the
continuous quantity the state was resolved from (documented per engine in
`docs/spec/`). The terminal renders the binary state as the signal and the
score as a numeric readout. Interpretation happens in the trader's head, not in
a mushy enum.

**Hysteresis.** State resolution uses a two-threshold band
(`activate_at` > `deactivate_at`) so a score oscillating at the boundary cannot
flap the state. Output remains strictly binary and deterministic given the
score history. Thresholds are named constants defined next to each engine and
recorded in the spec — never inline literals.

## 3. Engineering Tenets (enforced, not aspirational)

1. **No magic numbers.** Every threshold, coefficient, window, cap, and epsilon
   is a named constant with a doc comment stating units and provenance
   (`docs/spec/` reference). Clippy and review both gate on this.
2. **Determinism.** Engines are pure: `fn resolve(&Inputs) -> Readout`. Same
   snapshot in, same readout out. All RNG (Monte Carlo) takes an explicit seed;
   the gateway derives seeds from the snapshot clock so replays reproduce.
3. **No I/O in math.** `slayer-quant` and `slayer-engines` cannot open sockets,
   read clocks, or touch env vars. Time is a parameter.
4. **Single wire schema.** `slayer-core` is the one definition of every payload.
   The TypeScript types in `terminal/src/wire/` are a hand-checked mirror and the
   parity test fixture keeps them honest.
5. **Errors are values.** `thiserror` enums at the edges; `unwrap`/`expect` are
   lint-gated out of library code.
6. **Latency budget.** Feed normalize < 1 ms, full engine pipeline < 5 ms per
   snapshot on 4 cores, WS fanout via `tokio::sync::broadcast` (no per-client
   serialization — serialize once, clone bytes).

## 4. Frontend Doctrine

- **Native canvas: 3440 × 1440.** The layout grid is designed at ultra-wide
  density first; narrower viewports degrade by dropping columns, never by
  reflowing into retail-style stacked cards.
- **Typography: SF Pro system stack.**
  `-apple-system, BlinkMacSystemFont, "SF Pro Display", "SF Pro Text", "Segoe UI", "Helvetica Neue", sans-serif`
  with `font-feature-settings: "tnum" 1` everywhere numbers render. (SF Pro is
  Apple-licensed and must not be bundled; the system stack resolves it natively
  on macOS and falls back metrically-sanely elsewhere.)
- **Dark, dense, still.** One theme. No celebration overlays, no onboarding
  tours, no marketing chrome, no animation that isn't data changing.
- **Hot paths on the GPU.** Strike ladders, heatmaps, and the price chart render
  through WebGL/canvas renderers in `terminal/src/gl/`; React never touches a
  per-frame draw.
- **State discipline.** A readout is `ACTIVE` (signal color) or `INACTIVE`
  (structural gray). The score renders as a number next to it. Nothing pulses,
  nothing says "almost".

## 5. Data Plane

`slayer-feed` exposes one trait:

```rust
pub trait FeedProvider {
    fn subscribe(&self, universe: &[Symbol]) -> BoxStream<'static, MarketEvent>;
}
```

Implementations: `SyntheticFeed` (deterministic generator for dev/test/demo),
`TradierFeed`, `ThetaDataFeed` (HTTP adapters; credentials via env at the
gateway edge only). The engine pipeline is provider-agnostic — it consumes
normalized `MarketEvent`s and nothing else.

Replay is a first-class citizen: the gateway journals every snapshot; the
terminal's scrubber requests historical snapshots over the same wire schema.

## 6. Legacy Provenance

The quantitative logic in this repository was extracted from the legacy
`Slayer-Production` codebase at formula level. `docs/spec/01…08` are the
extraction documents: every engine's exact math, constants catalogue, state
semantics, and known defects of the legacy implementation (fixed here, noted
there). When code and spec disagree, the spec is corrected in the same PR — the
two never drift silently.
