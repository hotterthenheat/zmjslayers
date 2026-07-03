/** Volatility-regime panel: Hurst, half-life, classification, flags. */

import type { RegimePanel } from '@/wire/snapshot'
import { Panel, Row } from './Panel'
import { BinaryChip } from './BinaryChip'
import { signed } from '@/format'

export function RegimeReadout({ regime }: { regime: RegimePanel }) {
  return (
    <Panel
      title="Regime"
      area="regime"
      aside={<BinaryChip label="conf" readout={regime.confidence} />}
    >
      <Row label="Classification" emphasis>
        {regime.label.replace(/_/g, ' ')}
      </Row>
      <Row label="Hurst H">{regime.hurst.toFixed(3)}</Row>
      <Row label="OU half-life">
        {regime.half_life_bars === null ? '—' : `${regime.half_life_bars.toFixed(1)} bars`}
      </Row>
      <Row label="RV term slope">{signed(regime.term_structure_slope, 4)}</Row>
      {regime.probabilities.map((m) => (
        <Row key={m.label} label={m.label}>
          {(m.value * 100).toFixed(0)}%
        </Row>
      ))}
      <div className="flag-row">
        <BinaryChip label="compression" readout={regime.compression} />
        <BinaryChip label="expansion" readout={regime.expansion} />
      </div>
    </Panel>
  )
}
