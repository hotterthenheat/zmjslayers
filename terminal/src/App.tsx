/**
 * Root component. Owns the single gateway connection, pipes frames into the
 * store, and lays out header / surface / status bar. All rendering below is
 * driven purely by wire frames.
 */

import { useEffect } from 'react'
import { useTerminal } from './store'
import { GatewayClient } from './ws/client'
import type { WireFrame } from './wire/snapshot'
import { Sidebar } from './components/Sidebar'
import { TopBar } from './components/TopBar'
import { TerminalShell } from './components/TerminalShell'
import { StatusBar } from './components/StatusBar'
import { CommandPalette } from './components/CommandPalette'

/** WebSocket URL: same-origin in prod, Vite proxy in dev. */
function gatewayUrl(): string {
  const proto = window.location.protocol === 'https:' ? 'wss' : 'ws'
  return `${proto}://${window.location.host}/ws`
}

export function App() {
  const { applyFrame, setConnected } = useTerminal()

  useEffect(() => {
    const client = new GatewayClient(
      gatewayUrl(),
      (frame) => applyFrame(frame as unknown as WireFrame),
      (connected) => setConnected(connected),
    )
    client.connect()
    return () => client.close()
  }, [applyFrame, setConnected])

  return (
    <div className="terminal-root">
      <Sidebar />
      <div className="terminal-column">
        <TopBar />
        <main className="terminal-main">
          <TerminalShell />
        </main>
        <StatusBar />
      </div>
      <CommandPalette />
    </div>
  )
}
