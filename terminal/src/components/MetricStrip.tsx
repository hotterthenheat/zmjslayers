/**
 * Metric strip — a row of instrument-panel cards, each with a 2px colored
 * "tone spine" on the left in its semantic color (the Dealer Flow header
 * motif from the product). Values are mono, tabular, tone-colored.
 */

import './metric-strip.css'

export interface MetricSpec {
  label: string
  value: string
  tone: 'gex' | 'dealer' | 'greek' | 'warning' | 'up' | 'down' | 'neutral'
}

const TONE_VAR: Record<MetricSpec['tone'], string> = {
  gex: 'var(--greek)',
  dealer: 'var(--dealer)',
  greek: 'var(--greek)',
  warning: 'var(--warning)',
  up: 'var(--dir-up)',
  down: 'var(--dir-down)',
  neutral: 'var(--text-dim)',
}

export function MetricStrip({ metrics }: { metrics: MetricSpec[] }) {
  return (
    <div className="metric-strip">
      {metrics.map((m) => (
        <div className="metric-card" key={m.label} style={{ ['--spine' as string]: TONE_VAR[m.tone] }}>
          <span className="metric-card-label">{m.label}</span>
          <span className="metric-card-value num mono" style={{ color: TONE_VAR[m.tone] }}>
            {m.value}
          </span>
        </div>
      ))}
    </div>
  )
}
