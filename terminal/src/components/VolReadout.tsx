/** Volatility / distribution panel: RV, IV, VRP, IV rank, RND percentiles. */

import type { VolPanel } from '@/wire/snapshot'
import { Panel, Row } from './Panel'
import { pct, signed } from '@/format'

export function VolReadout({ vol }: { vol: VolPanel }) {
  return (
    <Panel title="Volatility" area="vol">
      <Row label="Realized Vol">{pct(vol.realized_vol, 1)}</Row>
      <Row label="Implied Vol">{pct(vol.implied_vol, 1)}</Row>
      <Row label="Var Risk Premium" emphasis>
        <span className={vol.variance_risk_premium >= 0 ? 'dir-up' : 'dir-down'}>
          {signed(vol.variance_risk_premium * 100, 2)}
        </span>
      </Row>
      <Row label="IV Rank">{pct(vol.iv_rank, 0)}</Row>
      <Row label="IV Percentile">{pct(vol.iv_percentile, 0)}</Row>
      {vol.rnd_percentiles.length > 0 && <div className="section-rule">RND PERCENTILES</div>}
      {vol.rnd_percentiles.map((m) => (
        <Row key={m.label} label={m.label}>
          {m.value.toFixed(2)}
        </Row>
      ))}
    </Panel>
  )
}
