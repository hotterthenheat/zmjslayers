/**
 * Terminal state store.
 *
 * Holds the latest snapshot per symbol, the active symbol, and connection
 * status. The store is a pure sink for wire frames — it never computes
 * derived analytics (that is the gateway's job) and never mutates a
 * snapshot in place.
 */

import { create } from 'zustand'
import type { TerminalSnapshot, WireFrame } from './wire/snapshot'
import { isSnapshot } from './wire/snapshot'

/** The workspace views selectable from the left rail. */
export type Workspace = 'cockpit' | 'pinpoint' | 'volatility' | 'engines'

interface TerminalState {
  connected: boolean
  feed: string
  activeSymbol: string | null
  snapshots: Record<string, TerminalSnapshot>
  symbols: string[]
  workspace: Workspace
  paletteOpen: boolean
  setConnected: (connected: boolean) => void
  setFeed: (feed: string) => void
  applyFrame: (frame: WireFrame) => void
  setActiveSymbol: (symbol: string) => void
  setWorkspace: (workspace: Workspace) => void
  setPaletteOpen: (open: boolean) => void
}

export const useTerminal = create<TerminalState>((set) => ({
  connected: false,
  feed: 'unknown',
  activeSymbol: null,
  snapshots: {},
  symbols: [],
  workspace: 'cockpit',
  paletteOpen: false,

  setConnected: (connected) => set({ connected }),
  setFeed: (feed) => set({ feed }),
  setWorkspace: (workspace) => set({ workspace }),
  setPaletteOpen: (paletteOpen) => set({ paletteOpen }),

  applyFrame: (frame) => {
    if (frame.type === 'HELLO') {
      set({ feed: frame.feed })
      return
    }
    if (!isSnapshot(frame)) return
    set((state) => {
      const symbols = state.symbols.includes(frame.symbol)
        ? state.symbols
        : [...state.symbols, frame.symbol].sort()
      return {
        snapshots: { ...state.snapshots, [frame.symbol]: frame },
        symbols,
        activeSymbol: state.activeSymbol ?? frame.symbol,
      }
    })
  },

  setActiveSymbol: (symbol) => set({ activeSymbol: symbol }),
}))

/** The snapshot for the active symbol, if any. */
export function useActiveSnapshot(): TerminalSnapshot | null {
  return useTerminal((s) => (s.activeSymbol ? (s.snapshots[s.activeSymbol] ?? null) : null))
}
