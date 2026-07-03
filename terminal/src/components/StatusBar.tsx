/**
 * Footer system-status band — the Bloomberg terminal status strip. NY session
 * clock (amber, from the product), feed honesty dot, wire version, symbol
 * count, and the non-advice disclaimer.
 */

import { useEffect, useState } from 'react'
import { useTerminal, useActiveSnapshot } from '@/store'
import { WIRE_VERSION } from '@/wire/snapshot'
import './status-bar.css'

function useNyClock(): string {
  const [now, setNow] = useState(() => Date.now())
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), 1000)
    return () => clearInterval(t)
  }, [])
  return new Intl.DateTimeFormat('en-US', {
    timeZone: 'America/New_York',
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit',
    hour12: false,
  }).format(now)
}

export function StatusBar() {
  const { connected, symbols, feed } = useTerminal()
  const snap = useActiveSnapshot()
  const clock = useNyClock()

  return (
    <footer className="status-bar">
      <span className="sb-cell">
        <span className="sb-key">NY</span>
        <span className="sb-clock num mono">{clock}</span>
      </span>
      <span className="sb-cell">
        <span className="sb-key">WIRE</span>
        <span className="num">v{WIRE_VERSION}</span>
      </span>
      <span className="sb-cell">
        <span className="sb-key">SYMBOLS</span>
        <span className="num">{symbols.length}</span>
      </span>
      {snap?.synthetic && <span className="sb-cell sb-warn">SYNTHETIC FEED</span>}

      <span className="sb-center">Not investment advice.</span>

      <span className="sb-cell sb-feed">
        <span className={`sb-dot${connected ? ' live' : ''}`} />
        {connected ? `${feed.toUpperCase()} · LIVE` : 'OFFLINE'}
      </span>
      <span className="sb-cell sb-brand">SLAYER TERMINAL · 3440×1440</span>
    </footer>
  )
}
