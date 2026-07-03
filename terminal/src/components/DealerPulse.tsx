/**
 * Dealer pulse — the force-balance bar. The Dealer State Index in [-1, 1]
 * renders as a fill growing right of a center hairline (long-gamma /
 * stabilizing) or left (short-gamma / amplifying), echoing the product's
 * bullish/bearish book bar. dealer01 is shown as the normalized readout.
 */

import type { DealerPanel } from '@/wire/snapshot'
import { signed } from '@/format'
import './dealer-pulse.css'

export function DealerPulse({ dealer }: { dealer: DealerPanel }) {
  const dsi = Math.max(-1, Math.min(1, dealer.dsi))
  const magnitude = Math.abs(dsi) * 50 // half-width percent
  const positive = dsi >= 0
  return (
    <div className="dealer-pulse">
      <div className="pulse-head">
        <span className="label">Dealer Book</span>
        <span className={`num mono ${positive ? 'dir-up' : 'dir-down'}`}>{signed(dsi, 3)}</span>
      </div>
      <div className="pulse-track">
        <div
          className={`pulse-fill ${positive ? 'pos' : 'neg'}`}
          style={{ width: `${magnitude}%`, [positive ? 'left' : 'right']: '50%' }}
        />
        <div className="pulse-center" />
      </div>
      <div className="pulse-legend">
        <span className="dir-down">AMPLIFYING</span>
        <span className="pulse-d01 num mono">dealer01 {dealer.dealer01.toFixed(3)}</span>
        <span className="dir-up">STABILIZING</span>
      </div>
    </div>
  )
}
