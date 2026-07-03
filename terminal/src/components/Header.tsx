/** Top strip: brand, symbol tabs, live spot, session clock, feed status. */

import { useTerminal } from '@/store'
import { price, clock } from '@/format'
import './header.css'

export function Header() {
  const { symbols, activeSymbol, snapshots, connected, feed, setActiveSymbol } = useTerminal()
  const active = activeSymbol ? snapshots[activeSymbol] : undefined

  return (
    <header className="terminal-header">
      <div className="brand">
        <span className="brand-mark">◆</span>
        <span className="brand-name">SLAYER</span>
        <span className="brand-sub">TERMINAL</span>
      </div>

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

      <div className="header-spot">
        {active && (
          <>
            <span className="header-spot-sym">{active.symbol}</span>
            <span className="header-spot-px num">{price(active.spot)}</span>
          </>
        )}
      </div>

      <div className="header-right">
        {active && <span className="header-clock num">{clock(active.ts)} UTC</span>}
        <span className={`feed-badge${connected ? ' is-live' : ''}`}>
          {connected ? feed.toUpperCase() : 'DISCONNECTED'}
        </span>
      </div>
    </header>
  )
}
