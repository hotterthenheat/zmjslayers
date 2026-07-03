/**
 * Strike-ladder renderer.
 *
 * A per-strike dealer-exposure ladder drawn on a 2D canvas: diverging bars
 * (positive GEX one way, negative the other) with the spot row, gamma flip,
 * and walls marked. React never touches this draw loop — it hands the
 * renderer a snapshot and the renderer paints. Canvas 2D is the right tool
 * at strike-ladder cardinality (tens of rows); the interface is kept
 * renderer-agnostic so a WebGL backend can replace it without touching
 * callers.
 */

import type { DealerPanel } from '@/wire/snapshot'

/** Right gutter reserved for strike labels, px. */
const LABEL_GUTTER = 52
/** Minimum row height before the ladder switches to compressed mode, px. */
const MIN_ROW_H = 9
/** Bar inset from the center axis, px. */
const AXIS_INSET = 1

interface LadderColors {
  bg: string
  gexPos: string
  gexNeg: string
  axis: string
  spot: string
  flip: string
  wall: string
  text: string
  textDim: string
}

function readColors(root: HTMLElement): LadderColors {
  const s = getComputedStyle(root)
  const v = (name: string, fallback: string) => s.getPropertyValue(name).trim() || fallback
  return {
    bg: v('--surface', '#0d1017'),
    gexPos: v('--gex-positive', '#3fb27f'),
    gexNeg: v('--gex-negative', '#d0629a'),
    axis: v('--border-strong', '#273042'),
    spot: v('--state-active', '#4da3ff'),
    flip: v('--accent-amber', '#d29922'),
    wall: v('--text', '#c8d1dc'),
    text: v('--num', '#e8eef5'),
    textDim: v('--text-faint', '#4d5665'),
  }
}

/** Draw the ladder into `ctx` sized to `w`×`h` device-independent pixels. */
export function drawLadder(
  ctx: CanvasRenderingContext2D,
  dealer: DealerPanel,
  spot: number,
  w: number,
  h: number,
  colors: LadderColors,
): void {
  ctx.clearRect(0, 0, w, h)
  ctx.fillStyle = colors.bg
  ctx.fillRect(0, 0, w, h)

  const rows = dealer.ladder
  if (rows.length === 0) {
    ctx.fillStyle = colors.textDim
    ctx.font = '11px var(--font-ui)'
    ctx.textBaseline = 'middle'
    ctx.fillText('NO CHAIN', 8, h / 2)
    return
  }

  const plotW = w - LABEL_GUTTER
  const axisX = plotW / 2
  const rowH = Math.max(MIN_ROW_H, h / rows.length)
  const maxAbs = Math.max(...rows.map((r) => Math.abs(r.gex)), 1)

  // Ladder is sorted ascending by strike in the wire; render high strikes at
  // the top (price axis convention).
  const ordered = [...rows].reverse()

  ctx.textBaseline = 'middle'
  ctx.font = '10px ui-monospace, monospace'

  ordered.forEach((row, i) => {
    const y = i * rowH
    const barLen = (Math.abs(row.gex) / maxAbs) * (axisX - AXIS_INSET)
    ctx.fillStyle = row.gex >= 0 ? colors.gexPos : colors.gexNeg
    if (row.gex >= 0) {
      ctx.fillRect(axisX + AXIS_INSET, y + 1, barLen, rowH - 2)
    } else {
      ctx.fillRect(axisX - AXIS_INSET - barLen, y + 1, barLen, rowH - 2)
    }

    // Strike label in the gutter.
    ctx.fillStyle = row.at_spot ? colors.spot : colors.textDim
    ctx.textAlign = 'left'
    ctx.fillText(row.strike.toFixed(0), plotW + 6, y + rowH / 2)

    if (row.at_spot) {
      ctx.strokeStyle = colors.spot
      ctx.lineWidth = 1
      ctx.beginPath()
      ctx.moveTo(0, y + rowH / 2)
      ctx.lineTo(plotW, y + rowH / 2)
      ctx.stroke()
    }
  })

  // Center axis.
  ctx.strokeStyle = colors.axis
  ctx.lineWidth = 1
  ctx.beginPath()
  ctx.moveTo(axisX, 0)
  ctx.lineTo(axisX, h)
  ctx.stroke()

  // Strike→y helper for overlays.
  const lo = ordered[ordered.length - 1]?.strike ?? spot
  const hi = ordered[0]?.strike ?? spot
  const yOf = (strike: number) => {
    if (hi === lo) return h / 2
    return ((hi - strike) / (hi - lo)) * (h - rowH) + rowH / 2
  }

  const marker = (strike: number, color: string, label: string) => {
    const y = yOf(strike)
    ctx.strokeStyle = color
    ctx.setLineDash([3, 2])
    ctx.beginPath()
    ctx.moveTo(0, y)
    ctx.lineTo(plotW, y)
    ctx.stroke()
    ctx.setLineDash([])
    ctx.fillStyle = color
    ctx.textAlign = 'left'
    ctx.fillText(label, 3, y - rowH / 2 - 1)
  }

  if (dealer.gamma_flip !== null) marker(dealer.gamma_flip, colors.flip, 'FLIP')
  if (dealer.call_wall.strike !== null) marker(dealer.call_wall.strike, colors.wall, 'CW')
  if (dealer.put_wall.strike !== null) marker(dealer.put_wall.strike, colors.wall, 'PW')
}

export { readColors }
export type { LadderColors }
