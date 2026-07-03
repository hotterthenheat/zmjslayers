/**
 * Strike exposure matrix — the flagship "itmatrix" heat grid.
 *
 * Strikes run high→low top-to-bottom. Each exposure column (GEX / DEX / VEX)
 * is normalized to its own peak and rendered as a diverging green/red cell
 * whose intensity follows a steep curve so only walls blaze against the
 * near-black field. The spot row carries an accent ring; call/put walls get
 * a bright ring + tag; the gamma flip is an amber divider between rows.
 */

import type { DealerPanel } from '@/wire/snapshot'
import { Panel } from './Panel'
import { money } from '@/format'
import './strike-matrix.css'

/** Intensity curve exponent — steepens so only dominant strikes light up. */
const HEAT_GAMMA = 1.7
/** Floor alpha so a non-zero cell is faintly visible. */
const HEAT_FLOOR = 0.04
/** Ceiling alpha for the hottest cell. */
const HEAT_CEIL = 0.9

const POS = [74, 222, 128] // --success
const NEG = [248, 113, 113] // --danger

function cellColor(value: number, columnMax: number): string {
  if (columnMax <= 0 || value === 0) return 'transparent'
  const heat = Math.min(1, Math.abs(value) / columnMax)
  const alpha = HEAT_FLOOR + Math.pow(heat, HEAT_GAMMA) * (HEAT_CEIL - HEAT_FLOOR)
  const [r, g, b] = value >= 0 ? POS : NEG
  return `rgba(${r}, ${g}, ${b}, ${alpha.toFixed(3)})`
}

export function StrikeMatrix({ dealer, spot }: { dealer: DealerPanel; spot: number }) {
  const rows = [...dealer.ladder].sort((a, b) => b.strike - a.strike)
  const maxGex = Math.max(...rows.map((r) => Math.abs(r.gex)), 1)
  const maxDex = Math.max(...rows.map((r) => Math.abs(r.dex)), 1)
  const maxVex = Math.max(...rows.map((r) => Math.abs(r.vex)), 1)

  const callWall = dealer.call_wall.strike
  const putWall = dealer.put_wall.strike
  const flip = dealer.gamma_flip

  // Find the row index the flip sits just above (rows are descending).
  const flipBelowIdx =
    flip === null ? -1 : rows.findIndex((r, i) => i < rows.length - 1 && rows[i + 1] && r.strike >= flip && rows[i + 1]!.strike < flip)

  return (
    <Panel title="Strike Matrix" accent="greek" area="matrix">
      <div className="matrix">
        <div className="matrix-head">
          <span className="matrix-h matrix-h-strike">Strike</span>
          <span className="matrix-h">GEX</span>
          <span className="matrix-h">DEX</span>
          <span className="matrix-h">VEX</span>
        </div>
        <div className="matrix-body">
          {rows.map((r, i) => {
            const isCall = callWall !== null && r.strike === callWall
            const isPut = putWall !== null && r.strike === putWall
            const tag = isCall ? 'CW' : isPut ? 'PW' : r.at_spot ? '◀' : ''
            return (
              <div key={r.strike}>
                <div
                  className={`matrix-row${r.at_spot ? ' at-spot' : ''}${isCall || isPut ? ' at-wall' : ''}`}
                >
                  <span className="matrix-strike num mono">
                    {r.strike.toFixed(0)}
                    {tag && <span className="matrix-tag">{tag}</span>}
                  </span>
                  <span className="matrix-cell num mono" style={{ background: cellColor(r.gex, maxGex) }}>
                    {compact(r.gex)}
                  </span>
                  <span className="matrix-cell num mono" style={{ background: cellColor(r.dex, maxDex) }}>
                    {compact(r.dex)}
                  </span>
                  <span className="matrix-cell num mono" style={{ background: cellColor(r.vex, maxVex) }}>
                    {compact(r.vex)}
                  </span>
                </div>
                {i === flipBelowIdx && (
                  <div className="matrix-flip">
                    <span className="matrix-flip-tag">Γ FLIP {flip?.toFixed(0)}</span>
                  </div>
                )}
              </div>
            )
          })}
        </div>
        <div className="matrix-foot">
          <span className="matrix-h matrix-h-strike">Net</span>
          <span className={`num mono ${dealer.net_gex >= 0 ? 'dir-up' : 'dir-down'}`}>
            {money(dealer.net_gex)}
          </span>
          <span className={`num mono ${dealer.net_dex >= 0 ? 'dir-up' : 'dir-down'}`}>
            {money(dealer.net_dex)}
          </span>
          <span className={`num mono ${dealer.net_vex >= 0 ? 'dir-up' : 'dir-down'}`}>
            {money(dealer.net_vex)}
          </span>
        </div>
        {rows.length === 0 && <div className="matrix-empty">NO CHAIN</div>}
        <span className="matrix-spot-note label">SPOT {spot.toFixed(2)}</span>
      </div>
    </Panel>
  )
}

/** Compact ±B/M/K with one significant scale, no currency symbol. */
function compact(v: number): string {
  const a = Math.abs(v)
  const s = v < 0 ? '-' : ''
  if (a >= 1e9) return `${s}${(a / 1e9).toFixed(1)}B`
  if (a >= 1e6) return `${s}${(a / 1e6).toFixed(0)}M`
  if (a >= 1e3) return `${s}${(a / 1e3).toFixed(0)}K`
  if (a === 0) return '·'
  return `${s}${a.toFixed(0)}`
}
