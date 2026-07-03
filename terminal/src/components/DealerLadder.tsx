/**
 * Dealer-exposure ladder panel: the flagship structural view. Wraps the
 * canvas renderer, handling DPR scaling and resize; the draw itself lives in
 * `gl/ladder.ts`.
 */

import { useEffect, useRef } from 'react'
import type { DealerPanel } from '@/wire/snapshot'
import { drawLadder, readColors } from '@/gl/ladder'
import { Panel } from './Panel'
import { BinaryChip } from './BinaryChip'
import { money } from '@/format'

export function DealerLadder({ dealer, spot }: { dealer: DealerPanel; spot: number }) {
  const canvasRef = useRef<HTMLCanvasElement | null>(null)
  const wrapRef = useRef<HTMLDivElement | null>(null)

  useEffect(() => {
    const canvas = canvasRef.current
    const wrap = wrapRef.current
    if (!canvas || !wrap) return
    const ctx = canvas.getContext('2d')
    if (!ctx) return

    const colors = readColors(document.documentElement)
    const render = () => {
      const dpr = window.devicePixelRatio || 1
      const w = wrap.clientWidth
      const h = wrap.clientHeight
      canvas.width = Math.round(w * dpr)
      canvas.height = Math.round(h * dpr)
      canvas.style.width = `${w}px`
      canvas.style.height = `${h}px`
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0)
      drawLadder(ctx, dealer, spot, w, h, colors)
    }

    render()
    const ro = new ResizeObserver(render)
    ro.observe(wrap)
    return () => ro.disconnect()
  }, [dealer, spot])

  return (
    <Panel
      title="Dealer Gamma Ladder"
      accent="greek"
      area="ladder"
      aside={<BinaryChip label="flip" readout={dealer.gamma_flip_state} />}
    >
      <div className="ladder-meta">
        <span className="label">NET GEX</span>
        <span className={`num ${dealer.net_gex >= 0 ? 'dir-up' : 'dir-down'}`}>
          {money(dealer.net_gex)}
        </span>
        <span className="label">GROSS</span>
        <span className="num">{money(dealer.gross_gex)}</span>
      </div>
      <div ref={wrapRef} className="ladder-canvas-wrap">
        <canvas ref={canvasRef} />
      </div>
    </Panel>
  )
}
