/**
 * Slayer wordmark — a faithful port of the slayerterminal.com terminal logo.
 *
 *   collapsed:  >S▌
 *   expanded:   >slayer_terminal▌
 *
 * Exact colors / weights / caret timing from the source brand:
 *   prompt ">"        = #6B7177 (weight 700, 0.84em)
 *   wordmark + caret  = #F4F5F6 (weight 800)
 *   caret blink       = 1.08s steps(1) (see brand.css)
 *
 * Rendered in the system monospace stack (SF Mono on Apple hardware) rather
 * than a bundled web font, per the terminal's self-contained/SF-first
 * typography doctrine.
 */

import './brand.css'

const PROMPT = '#6B7177'
const WHITE = '#f4f5f6'

export function BrandLogo({ expanded = true }: { expanded?: boolean }) {
  return (
    <span className="brand-logo" aria-label="Slayer Terminal">
      <span className="brand-logo-prompt" aria-hidden="true" style={{ color: PROMPT }}>
        {'>'}
      </span>
      <span className="brand-logo-word" style={{ color: WHITE }}>
        {expanded ? 'slayer_terminal' : 'S'}
      </span>
      <span className="brand-logo-caret slayer-caret" aria-hidden="true" />
    </span>
  )
}
