/**
 * Left rail — hover-expand navigation (64px collapsed → 240px expanded),
 * ported from the Slayer AppShell. Brand block at the top, workspace nav
 * grouped under "Main Views", feed pill pinned to the bottom.
 */

import type { Workspace } from '@/store'
import { useTerminal } from '@/store'
import { BrandLogo } from './BrandLogo'
import './sidebar.css'

interface NavItem {
  id: Workspace
  label: string
  glyph: string
  accent: string
}

const MAIN_VIEWS: NavItem[] = [
  { id: 'cockpit', label: 'Cockpit', glyph: '◧', accent: 'var(--info)' },
  { id: 'pinpoint', label: 'Pinpoint GEX', glyph: '◈', accent: 'var(--greek)' },
  { id: 'volatility', label: 'Volatility', glyph: '◭', accent: 'var(--warning)' },
  { id: 'engines', label: 'Engine Board', glyph: '▤', accent: 'var(--dealer)' },
]

export function Sidebar() {
  const { workspace, setWorkspace, connected, feed, setPaletteOpen } = useTerminal()

  return (
    <aside className="sidebar">
      <div className="sidebar-brand" onClick={() => setWorkspace('cockpit')}>
        <span className="sidebar-brand-collapsed">
          <BrandLogo expanded={false} />
        </span>
        <span className="sidebar-brand-expanded">
          <BrandLogo expanded />
        </span>
      </div>

      <nav className="sidebar-nav">
        <div className="sidebar-group-label">Main Views</div>
        {MAIN_VIEWS.map((item) => (
          <button
            key={item.id}
            className={`sidebar-item${workspace === item.id ? ' is-active' : ''}`}
            onClick={() => setWorkspace(item.id)}
            title={item.label}
          >
            <span className="sidebar-glyph" style={{ color: item.accent }}>
              {item.glyph}
            </span>
            <span className="sidebar-item-label">{item.label}</span>
          </button>
        ))}

        <div className="sidebar-group-label sidebar-group-tools">Tools</div>
        <button className="sidebar-item" onClick={() => setPaletteOpen(true)} title="Command (⌘K)">
          <span className="sidebar-glyph" style={{ color: 'var(--text-dim)' }}>
            ⌘
          </span>
          <span className="sidebar-item-label">Command</span>
        </button>
      </nav>

      <div className="sidebar-foot">
        <span className={`feed-pill${connected ? ' is-live' : ''}`}>
          <span className="feed-pill-dot" />
          <span className="feed-pill-label">{connected ? feed.toUpperCase() : 'OFFLINE'}</span>
        </span>
      </div>
    </aside>
  )
}
