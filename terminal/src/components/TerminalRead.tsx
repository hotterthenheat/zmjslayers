/**
 * Terminal read — the dealer-structure directional verdict: bias, dealer
 * regime, GEX outlook, a signed conviction meter, 0DTE finish probabilities,
 * and the engaged binary state.
 */

import type { TerminalReadPanel } from '@/wire/snapshot'
import { Panel, Row } from './Panel'
import { BinaryChip } from './BinaryChip'
import { pct } from '@/format'
import './terminal-read.css'

export function TerminalRead({ read }: { read: TerminalReadPanel }) {
  const biasClass = read.bias === 'LONG' ? 'dir-up' : read.bias === 'SHORT' ? 'dir-down' : ''
  // Conviction meter: signed score [-100, 100] → centered fill.
  const mag = Math.min(50, (Math.abs(read.score) / 100) * 50)
  const positive = read.score >= 0
  return (
    <Panel
      title="Terminal Read"
      accent="thesis"
      area="read"
      aside={<BinaryChip label="engaged" readout={read.engaged} />}
    >
      <div className="read-verdict">
        <span className={`read-bias ${biasClass}`}>{read.bias}</span>
        {read.no_trade && <span className="read-notrade">NO-TRADE</span>}
      </div>

      <div className="read-meter">
        <div className="read-meter-track">
          <span
            className={`read-meter-fill ${positive ? 'pos' : 'neg'}`}
            style={{ width: `${mag}%`, [positive ? 'left' : 'right']: '50%' }}
          />
          <span className="read-meter-center" />
        </div>
        <span className="read-meter-val num mono">{read.score.toFixed(0)}</span>
      </div>

      <Row label="Dealer Regime">{read.regime}</Row>
      <Row label="GEX Outlook">{read.outlook.replace(/_/g, ' ')}</Row>
      <Row label="Confidence">{read.confidence.toFixed(0)}</Row>

      {read.zero_dte.length > 0 && <div className="section-rule">0DTE FINISH</div>}
      {read.zero_dte.map((m) => (
        <Row key={m.label} label={m.label}>
          {pct(m.value, 0)}
        </Row>
      ))}
    </Panel>
  )
}
