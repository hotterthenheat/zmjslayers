/** Bottom strip: wire version, symbol count, synthetic-feed warning. */

import { useTerminal, useActiveSnapshot } from '@/store'
import { WIRE_VERSION } from '@/wire/snapshot'

export function StatusBar() {
  const { connected, symbols } = useTerminal()
  const snap = useActiveSnapshot()
  return (
    <footer className="status-bar">
      <span className={`sb-dot${connected ? ' live' : ''}`} />
      <span className="sb-item">WIRE v{WIRE_VERSION}</span>
      <span className="sb-item">{symbols.length} SYMBOLS</span>
      {snap?.synthetic && <span className="sb-item sb-warn">SYNTHETIC FEED</span>}
      <span className="sb-spacer" />
      <span className="sb-item">SLAYER TERMINAL · 3440×1440</span>
    </footer>
  )
}
