/** Directional-thesis panel: dual-sided scores, engagement, sub-scores. */

import type { ThesisPanel } from '@/wire/snapshot'
import { Panel, Row } from './Panel'
import { BinaryChip } from './BinaryChip'
import { score100 } from '@/format'

export function ThesisReadout({ thesis }: { thesis: ThesisPanel }) {
  const dir = thesis.direction > 0 ? 'LONG' : thesis.direction < 0 ? 'SHORT' : 'BALANCED'
  const dirClass = thesis.direction > 0 ? 'dir-up' : thesis.direction < 0 ? 'dir-down' : ''
  return (
    <Panel
      title="Thesis"
      area="thesis"
      aside={<BinaryChip label="engaged" readout={thesis.engagement} />}
    >
      <Row label="Direction" emphasis>
        <span className={dirClass}>{dir}</span>
      </Row>
      <div className="dual-bar">
        <div className="dual-side">
          <span className="label">LONG</span>
          <span className="num dir-up">{score100(thesis.long_score)}</span>
        </div>
        <div className="dual-side">
          <span className="label">SHORT</span>
          <span className="num dir-down">{score100(thesis.short_score)}</span>
        </div>
      </div>
      {thesis.sub_scores.map((m) => (
        <Row key={m.label} label={m.label}>
          {m.value.toFixed(1)}
        </Row>
      ))}
    </Panel>
  )
}
