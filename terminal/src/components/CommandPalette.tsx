/**
 * ⌘K command palette — the Bloomberg command line. A green `>` prompt, fuzzy
 * command list (workspace navigation + symbol switch), keyboard-driven. Opens
 * on ⌘K / Ctrl-K, closes on Escape.
 */

import { useEffect, useMemo, useRef, useState } from 'react'
import type { Workspace } from '@/store'
import { useTerminal } from '@/store'
import './command-palette.css'

interface Command {
  id: string
  label: string
  hint: string
  run: () => void
}

export function CommandPalette() {
  const {
    paletteOpen,
    setPaletteOpen,
    setWorkspace,
    symbols,
    setActiveSymbol,
  } = useTerminal()
  const [query, setQuery] = useState('')
  const [cursor, setCursor] = useState(0)
  const inputRef = useRef<HTMLInputElement | null>(null)

  // Global ⌘K / Ctrl-K toggle.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'k') {
        e.preventDefault()
        setPaletteOpen(!useTerminal.getState().paletteOpen)
      }
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [setPaletteOpen])

  useEffect(() => {
    if (paletteOpen) {
      setQuery('')
      setCursor(0)
      inputRef.current?.focus()
    }
  }, [paletteOpen])

  const commands = useMemo<Command[]>(() => {
    const nav = (id: Workspace, label: string): Command => ({
      id: `go:${id}`,
      label,
      hint: 'view',
      run: () => {
        setWorkspace(id)
        setPaletteOpen(false)
      },
    })
    const workspaces = [
      nav('cockpit', 'Cockpit'),
      nav('pinpoint', 'Pinpoint GEX'),
      nav('volatility', 'Volatility'),
      nav('engines', 'Engine Board'),
    ]
    const syms = symbols.map<Command>((s) => ({
      id: `sym:${s}`,
      label: s,
      hint: 'symbol',
      run: () => {
        setActiveSymbol(s)
        setPaletteOpen(false)
      },
    }))
    return [...workspaces, ...syms]
  }, [symbols, setWorkspace, setActiveSymbol, setPaletteOpen])

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase()
    if (!q) return commands
    return commands.filter((c) => c.label.toLowerCase().includes(q) || c.hint.includes(q))
  }, [commands, query])

  if (!paletteOpen) return null

  const clampedCursor = Math.min(cursor, Math.max(0, filtered.length - 1))

  const onKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === 'Escape') setPaletteOpen(false)
    else if (e.key === 'ArrowDown') {
      e.preventDefault()
      setCursor((c) => (filtered.length ? (c + 1) % filtered.length : 0))
    } else if (e.key === 'ArrowUp') {
      e.preventDefault()
      setCursor((c) => (filtered.length ? (c - 1 + filtered.length) % filtered.length : 0))
    } else if (e.key === 'Enter') {
      filtered[clampedCursor]?.run()
    }
  }

  return (
    <div className="palette-overlay" onMouseDown={() => setPaletteOpen(false)}>
      <div className="palette" onMouseDown={(e) => e.stopPropagation()}>
        <div className="palette-input-row">
          <span className="palette-caret">&gt;</span>
          <input
            ref={inputRef}
            className="palette-input mono"
            value={query}
            placeholder="Type a command or symbol…"
            onChange={(e) => {
              setQuery(e.target.value)
              setCursor(0)
            }}
            onKeyDown={onKeyDown}
          />
          <kbd className="palette-esc">ESC</kbd>
        </div>
        <div className="palette-results">
          {filtered.length === 0 && <div className="palette-empty">no matches</div>}
          {filtered.map((c, i) => (
            <button
              key={c.id}
              className={`palette-row${i === clampedCursor ? ' is-active' : ''}`}
              onMouseEnter={() => setCursor(i)}
              onClick={() => c.run()}
            >
              <span className="palette-row-label">{c.label}</span>
              <span className="palette-row-hint">{c.hint}</span>
            </button>
          ))}
        </div>
        <div className="palette-foot">↑↓ navigate · ↵ select · ⌘K toggle</div>
      </div>
    </div>
  )
}
