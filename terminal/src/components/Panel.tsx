/**
 * Shared panel chrome: a titled, bordered region. Dense header, no padding
 * waste. Panels are the atomic unit of the terminal grid.
 */

import type { ReactNode } from 'react'
import './panel.css'

/** Domain accent for the panel title tick. */
type Accent = 'greek' | 'dealer' | 'warning' | 'thesis'

interface Props {
  title: string
  /** Optional right-aligned header slot (e.g. a state chip). */
  aside?: ReactNode
  children: ReactNode
  /** Grid area name for placement in the shell. */
  area?: string
  /** Domain accent color for the title tick. */
  accent?: Accent
}

export function Panel({ title, aside, children, area, accent }: Props) {
  return (
    <section
      className="panel"
      style={area ? { gridArea: area } : undefined}
      data-accent={accent}
    >
      <header className="panel-head">
        <span className="panel-title">{title}</span>
        {aside && <span className="panel-aside">{aside}</span>}
      </header>
      <div className="panel-body">{children}</div>
    </section>
  )
}

/** A dense label→value row. */
export function Row({
  label,
  children,
  emphasis,
}: {
  label: string
  children: ReactNode
  emphasis?: boolean
}) {
  return (
    <div className={`kv-row${emphasis ? ' kv-emph' : ''}`}>
      <span className="kv-label">{label}</span>
      <span className="kv-value num">{children}</span>
    </div>
  )
}
