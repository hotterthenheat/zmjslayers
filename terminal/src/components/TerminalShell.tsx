/**
 * The trading surface. Renders the active workspace — each an ultra-wide grid
 * designed at 3440×1440 that reframes the same live snapshot: the Cockpit
 * overview, Pinpoint GEX (dealer structure), Volatility, and the Engine Board.
 * Narrower viewports drop columns; they never reflow into stacked cards.
 */

import { useActiveSnapshot, useTerminal } from '@/store'
import { DealerLadder } from './DealerLadder'
import { DealerStats } from './DealerStats'
import { StrikeMatrix } from './StrikeMatrix'
import { DealerPulse } from './DealerPulse'
import { DecisionCard } from './DecisionCard'
import { TerminalRead } from './TerminalRead'
import { ThesisReadout } from './ThesisReadout'
import { RegimeReadout } from './RegimeReadout'
import { VolReadout } from './VolReadout'
import { FlowReadout } from './FlowReadout'
import { EngineBoard } from './EngineBoard'
import { MetricStrip, type MetricSpec } from './MetricStrip'
import { Panel } from './Panel'
import { money, price, pct } from '@/format'
import type { TerminalSnapshot } from '@/wire/snapshot'
import './shell.css'

export function TerminalShell() {
  const snap = useActiveSnapshot()
  const workspace = useTerminal((s) => s.workspace)

  if (!snap) {
    return (
      <div className="shell-empty">
        <span className="shell-empty-mark mono">&gt;_</span>
        <span>AWAITING FIRST SNAPSHOT</span>
      </div>
    )
  }

  switch (workspace) {
    case 'pinpoint':
      return <PinpointWorkspace snap={snap} />
    case 'volatility':
      return <VolatilityWorkspace snap={snap} />
    case 'engines':
      return <EnginesWorkspace snap={snap} />
    default:
      return <CockpitWorkspace snap={snap} />
  }
}

function CockpitWorkspace({ snap }: { snap: TerminalSnapshot }) {
  return (
    <div className="ws ws-cockpit">
      <DecisionCard decision={snap.decision} />
      <TerminalRead read={snap.read} />
      <DealerLadder dealer={snap.dealer} spot={snap.spot} />
      <PositioningPanel snap={snap} />
      <RegimeReadout regime={snap.regime} />
      <VolReadout vol={snap.vol} />
      <EngineBoard engines={snap.engines} />
    </div>
  )
}

function PinpointWorkspace({ snap }: { snap: TerminalSnapshot }) {
  const d = snap.dealer
  const metrics: MetricSpec[] = [
    { label: 'Net GEX', value: money(d.net_gex), tone: d.net_gex >= 0 ? 'up' : 'down' },
    { label: 'Call Wall', value: d.call_wall.strike === null ? '—' : price(d.call_wall.strike), tone: 'up' },
    { label: 'Put Wall', value: d.put_wall.strike === null ? '—' : price(d.put_wall.strike), tone: 'down' },
    { label: 'γ-Flip', value: d.gamma_flip === null ? '—' : price(d.gamma_flip), tone: 'warning' },
    { label: 'Magnet', value: d.magnet === null ? '—' : price(d.magnet), tone: 'greek' },
    { label: 'Exp Move', value: `±${pct(d.expected_move_pct)}`, tone: 'dealer' },
  ]
  return (
    <div className="ws ws-pinpoint">
      <div className="ws-strip" style={{ gridArea: 'strip' }}>
        <MetricStrip metrics={metrics} />
      </div>
      <StrikeMatrix dealer={snap.dealer} spot={snap.spot} />
      <DealerLadder dealer={snap.dealer} spot={snap.spot} />
      <PositioningPanel snap={snap} />
      <FlowReadout flow={snap.flow} />
    </div>
  )
}

function VolatilityWorkspace({ snap }: { snap: TerminalSnapshot }) {
  return (
    <div className="ws ws-volatility">
      <VolReadout vol={snap.vol} />
      <RegimeReadout regime={snap.regime} />
      <ThesisReadout thesis={snap.thesis} />
      <FlowReadout flow={snap.flow} />
    </div>
  )
}

function EnginesWorkspace({ snap }: { snap: TerminalSnapshot }) {
  return (
    <div className="ws ws-engines">
      <EngineBoard engines={snap.engines} />
      <PositioningPanel snap={snap} />
      <RegimeReadout regime={snap.regime} />
      <ThesisReadout thesis={snap.thesis} />
    </div>
  )
}

/** Positioning panel = dealer scalar stats + the force-balance pulse. */
function PositioningPanel({ snap }: { snap: TerminalSnapshot }) {
  return (
    <Panel title="Positioning" area="dealer" accent="dealer">
      <DealerPulse dealer={snap.dealer} />
      <DealerStats dealer={snap.dealer} spot={snap.spot} embedded />
    </Panel>
  )
}
