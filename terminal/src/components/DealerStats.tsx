/** Dealer-positioning scalar readouts beside the ladder. */

import type { DealerPanel } from '@/wire/snapshot'
import { Panel, Row } from './Panel'
import { BinaryChip } from './BinaryChip'
import { money, price, pct, signed } from '@/format'

export function DealerStats({ dealer, spot }: { dealer: DealerPanel; spot: number }) {
  const wall = (strike: number | null) => (strike === null ? '—' : price(strike))
  return (
    <Panel title="Positioning" area="dealer">
      <Row label="Dealer State Index" emphasis>
        <span className={dealer.dsi >= 0 ? 'dir-up' : 'dir-down'}>{signed(dealer.dsi, 3)}</span>
      </Row>
      <Row label="dealer01">{dealer.dealer01.toFixed(3)}</Row>
      <Row label="Net DEX">{money(dealer.net_dex)}</Row>
      <Row label="Net VEX">{money(dealer.net_vex)}</Row>
      <Row label="Net Charm">{money(dealer.net_charm)}</Row>
      <Row label="Gamma Flip">{wall(dealer.gamma_flip)}</Row>
      <Row label="Expected Move">±{pct(dealer.expected_move_pct)}</Row>
      <Row label="Magnet">{wall(dealer.magnet)}</Row>
      <div className="wall-block">
        <div className="wall-line">
          <span className="label">CALL WALL</span>
          <span className="num">{wall(dealer.call_wall.strike)}</span>
          <BinaryChip label="" readout={dealer.call_wall.state} />
        </div>
        <div className="wall-line">
          <span className="label">PUT WALL</span>
          <span className="num">{wall(dealer.put_wall.strike)}</span>
          <BinaryChip label="" readout={dealer.put_wall.state} />
        </div>
      </div>
      {dealer.excluded_quotes > 0 && (
        <Row label="Excluded quotes">{dealer.excluded_quotes}</Row>
      )}
      <Row label="Spot">{price(spot)}</Row>
    </Panel>
  )
}
