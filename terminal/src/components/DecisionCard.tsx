/**
 * Decision cockpit — the trader-facing centerpiece. The SkyVision gate
 * collapsed to a single binary verdict: OPPORTUNITY ACTIVE (lit) or INACTIVE
 * (abstaining), the 0–100 quality score as the hero number, the action label,
 * the key decision inputs (EV / calibrated P / R:R / tail), and the gate
 * conditions checklist that shows *why*.
 */

import type { DecisionPanel } from '@/wire/snapshot'
import { Panel } from './Panel'
import { pct, signed } from '@/format'
import './decision-card.css'

export function DecisionCard({ decision }: { decision: DecisionPanel }) {
  const active = decision.opportunity.state === 'ACTIVE'
  const quality = Math.round(decision.opportunity.score)
  return (
    <Panel title="Decision" accent="dealer" area="decision">
      <div className={`decision ${active ? 'is-active' : 'is-inactive'}`}>
        <div className="decision-verdict">
          <span className="decision-state">{active ? 'ACTIVE' : 'INACTIVE'}</span>
          <span className="decision-action">{decision.action}</span>
        </div>
        <div className="decision-quality">
          <span className="decision-quality-num num mono">{quality}</span>
          <span className="decision-quality-label">OPPORTUNITY QUALITY</span>
        </div>

        <div className="decision-inputs">
          <Metric label="EV" value={decision.expected_value === 0 ? '—' : signed(decision.expected_value * 100, 2)} tone={decision.expected_value >= 0 ? 'up' : 'down'} />
          <Metric label="P(cal)" value={decision.calibrated_p === 0 ? '—' : pct(decision.calibrated_p, 0)} />
          <Metric label="R:R" value={decision.reward_risk === 0 ? '—' : decision.reward_risk.toFixed(2)} />
          <Metric label="Tail" value={decision.tail_risk === 0 ? '—' : decision.tail_risk.toFixed(2)} tone={decision.tail_risk > 0.7 ? 'down' : 'neutral'} />
        </div>

        {decision.conditions.length > 0 ? (
          <div className="decision-gate">
            {decision.conditions.map((c) => (
              <div key={c.label} className={`gate-row ${c.pass ? 'pass' : 'fail'}`}>
                <span className="gate-mark">{c.pass ? '✓' : '✕'}</span>
                <span className="gate-label">{c.label}</span>
                <span className="gate-value num mono">{c.value.toFixed(2)}</span>
              </div>
            ))}
          </div>
        ) : (
          <div className="decision-abstain">Gate abstaining — awaiting calibrated inputs</div>
        )}
      </div>
    </Panel>
  )
}

function Metric({ label, value, tone }: { label: string; value: string; tone?: 'up' | 'down' | 'neutral' }) {
  const cls = tone === 'up' ? 'dir-up' : tone === 'down' ? 'dir-down' : ''
  return (
    <div className="decision-input">
      <span className="decision-input-label">{label}</span>
      <span className={`decision-input-value num mono ${cls}`}>{value}</span>
    </div>
  )
}
