/** Dealer-flow dynamics panel: vanna / charm / migration + flow states. */

import type { FlowPanel } from '@/wire/snapshot'
import { Panel, Row } from './Panel'
import { BinaryChip } from './BinaryChip'
import { signed } from '@/format'

function bar(value: number) {
  // Signed intensity in [-1, 1] rendered as a centered mini-bar.
  const clamped = Math.max(-1, Math.min(1, value))
  const w = Math.abs(clamped) * 50
  return (
    <span className="flow-bar">
      <span className="flow-bar-track">
        <span
          className={`flow-bar-fill ${clamped >= 0 ? 'pos' : 'neg'}`}
          style={{ width: `${w}%`, [clamped >= 0 ? 'left' : 'right']: '50%' }}
        />
      </span>
      <span className="num">{signed(value, 2)}</span>
    </span>
  )
}

export function FlowReadout({ flow }: { flow: FlowPanel }) {
  return (
    <Panel title="Dealer Flow" area="flow">
      <Row label="Vanna hedge-flow">{bar(flow.vanna_flow)}</Row>
      <Row label="Charm bias">{bar(flow.charm_bias)}</Row>
      <Row label="OI migration">{bar(flow.migration)}</Row>
      <div className="flag-row">
        <BinaryChip label="gamma dyn" readout={flow.gamma_dynamics} />
        <BinaryChip label="oi flow" readout={flow.oi_flow} />
      </div>
    </Panel>
  )
}
