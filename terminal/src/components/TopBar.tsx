/**
 * Top band — the instrument hero. Symbol tabs, the live spot with tick-flash,
 * expected-move and net-GEX quick readouts, and the ⌘K affordance. Mono,
 * dense, Bloomberg-band styling.
 */

import { useEffect, useRef, useState } from 'react'
import { useTerminal, useActiveSnapshot } from '@/store'
import { price, pct, money } from '@/format'
import './topbar.css'

function useTickDirection(value: number | undefined): 'up' | 'down' | null {
  const prev = useRef<number | undefined>(undefined)
  const [dir, setDir] = useState<'up' | 'down' | null>(null)
  useEffect(() => {
    if (value === undefined) return
    const p = prev.current
    if (p !== undefined && value !== p) setDir(value > p ? 'up' : 'down')
    prev.current = value
    const t = setTimeout(() => setDir(null), 380)
    return () => clearTimeout(t)
  }, [value])
  return dir
}

export function TopBar() {
  const { symbols, activeSymbol, setActiveSymbol, setPaletteOpen } = useTerminal()
  const snap = useActiveSnapshot()
  const tick = useTickDirection(snap?.spot)

  return (
    <header className="topbar">
      <nav className="symbol-tabs">
        {symbols.map((s) => (
          <button
            key={s}
            className={`symbol-tab${s === activeSymbol ? ' is-active' : ''}`}
            onClick={() => setActiveSymbol(s)}
          >
            {s}
          </button>
        ))}
      </nav>

      {snap && (
        <div className="hero">
          <span className="hero-sym mono">{snap.symbol}</span>
          <span className={`hero-px num mono${tick ? ` tick-${tick}` : ''}`}>
            {price(snap.spot)}
          </span>
        </div>
      )}

      {snap && (
        <div className="hero-metrics">
          <HeroMetric label="Net GEX" value={money(snap.dealer.net_gex)} tone={snap.dealer.net_gex >= 0 ? 'up' : 'down'} />
          <HeroMetric label="Exp Move" value={`±${pct(snap.dealer.expected_move_pct)}`} />
          <HeroMetric
            label="γ-Flip"
            value={snap.dealer.gamma_flip === null ? '—' : price(snap.dealer.gamma_flip)}
            tone="warn"
          />
          <HeroMetric label="Regime" value={snap.regime.label.replace(/_/g, ' ')} />
        </div>
      )}

      <button className="palette-hint" onClick={() => setPaletteOpen(true)}>
        <span className="palette-prompt">&gt;</span>
        <span className="palette-hint-text">command</span>
        <kbd>⌘K</kbd>
      </button>
    </header>
  )
}

function HeroMetric({
  label,
  value,
  tone,
}: {
  label: string
  value: string
  tone?: 'up' | 'down' | 'warn'
}) {
  const cls = tone === 'up' ? 'dir-up' : tone === 'down' ? 'dir-down' : tone === 'warn' ? 'tone-warn' : ''
  return (
    <div className="hero-metric">
      <span className="hero-metric-label">{label}</span>
      <span className={`hero-metric-value num mono ${cls}`}>{value}</span>
    </div>
  )
}
