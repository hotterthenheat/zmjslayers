/**
 * The trading surface. An ultra-wide CSS grid designed at 3440×1440 density:
 * the gamma ladder anchors the center, positioning and flow flank it, and
 * the engine board runs the bottom. Narrower viewports drop columns; they
 * never reflow into stacked retail cards.
 */

import { useActiveSnapshot } from '@/store'
import { DealerLadder } from './DealerLadder'
import { DealerStats } from './DealerStats'
import { ThesisReadout } from './ThesisReadout'
import { RegimeReadout } from './RegimeReadout'
import { VolReadout } from './VolReadout'
import { FlowReadout } from './FlowReadout'
import { EngineBoard } from './EngineBoard'
import './shell.css'

export function TerminalShell() {
  const snap = useActiveSnapshot()

  if (!snap) {
    return (
      <div className="shell-empty">
        <span className="shell-empty-mark">◆</span>
        <span>AWAITING FIRST SNAPSHOT</span>
      </div>
    )
  }

  return (
    <div className="terminal-shell">
      <ThesisReadout thesis={snap.thesis} />
      <DealerLadder dealer={snap.dealer} spot={snap.spot} />
      <DealerStats dealer={snap.dealer} spot={snap.spot} />
      <RegimeReadout regime={snap.regime} />
      <VolReadout vol={snap.vol} />
      <FlowReadout flow={snap.flow} />
      <EngineBoard engines={snap.engines} />
    </div>
  )
}
