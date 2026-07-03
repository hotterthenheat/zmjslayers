/**
 * TypeScript mirror of `slayer-core::wire`.
 *
 * This file is a hand-maintained reflection of the authoritative Rust wire
 * schema. Field names and shapes MUST match `crates/slayer-core/src/wire.rs`
 * exactly — the two are kept honest by review and by the fact that the
 * terminal fails loudly on a missing field (see the panel accessors).
 */

import type { BinaryState, Readout } from './state'

export const WIRE_VERSION = 2

export interface Metric {
  label: string
  value: number
  unit: string
}

export interface StrikeRow {
  strike: number
  gex: number
  dex: number
  vex: number
  open_interest: number
  at_spot: boolean
}

export interface WallReadout {
  strike: number | null
  margin: number
  strength: number
  state: Readout
}

export interface DealerPanel {
  net_gex: number
  gross_gex: number
  net_dex: number
  net_vex: number
  net_charm: number
  dsi: number
  dealer01: number
  gamma_flip: number | null
  gamma_flip_state: Readout
  call_wall: WallReadout
  put_wall: WallReadout
  expected_move_pct: number
  magnet: number | null
  ladder: StrikeRow[]
  excluded_quotes: number
}

export interface ThesisPanel {
  long_score: number
  short_score: number
  direction: number
  engagement: Readout
  sub_scores: Metric[]
}

export interface RegimePanel {
  hurst: number
  half_life_bars: number | null
  label: string
  probabilities: Metric[]
  confidence: Readout
  compression: Readout
  expansion: Readout
  term_structure_slope: number
}

export interface VolPanel {
  realized_vol: number
  implied_vol: number
  variance_risk_premium: number
  iv_rank: number
  iv_percentile: number
  rnd_percentiles: Metric[]
}

export interface FlowPanel {
  vanna_flow: number
  charm_bias: number
  migration: number
  gamma_dynamics: Readout
  oi_flow: Readout
}

export interface GateCondition {
  label: string
  pass: boolean
  value: number
}

export interface DecisionPanel {
  opportunity: Readout
  action: string
  expected_value: number
  calibrated_p: number
  reward_risk: number
  tail_risk: number
  conditions: GateCondition[]
}

export interface TerminalReadPanel {
  score: number
  bias: string
  regime: string
  outlook: string
  confidence: number
  no_trade: boolean
  engaged: Readout
  zero_dte: Metric[]
}

export interface EngineStatus {
  engine: string
  state: BinaryState
  score: number
}

export interface ExpiryDate {
  year: number
  month: number
  day: number
}

export interface TerminalSnapshot {
  type: 'SNAPSHOT'
  wire_version: number
  symbol: string
  spot: number
  ts: number
  expiry: ExpiryDate | null
  synthetic: boolean
  dealer: DealerPanel
  thesis: ThesisPanel
  regime: RegimePanel
  vol: VolPanel
  flow: FlowPanel
  decision: DecisionPanel
  read: TerminalReadPanel
  engines: EngineStatus[]
}

export interface HelloFrame {
  type: 'HELLO'
  wire_version: number
  feed: string
}

export type WireFrame = TerminalSnapshot | HelloFrame

export function isSnapshot(frame: { type: string }): frame is TerminalSnapshot {
  return frame.type === 'SNAPSHOT'
}
