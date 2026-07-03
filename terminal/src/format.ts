/**
 * Numeric formatters. Institutional density: fixed widths, tabular numerals,
 * B/M/K scaling for exposures. No locale surprises.
 */

const BILLION = 1e9
const MILLION = 1e6
const THOUSAND = 1e3

/** Scale a dollar exposure to B/M/K with a sign and fixed precision. */
export function money(value: number): string {
  const sign = value < 0 ? '-' : ''
  const abs = Math.abs(value)
  if (abs >= BILLION) return `${sign}$${(abs / BILLION).toFixed(2)}B`
  if (abs >= MILLION) return `${sign}$${(abs / MILLION).toFixed(1)}M`
  if (abs >= THOUSAND) return `${sign}$${(abs / THOUSAND).toFixed(0)}K`
  return `${sign}$${abs.toFixed(0)}`
}

/** Price with fixed 2-decimal precision. */
export function price(value: number): string {
  return value.toFixed(2)
}

/** Percentage from a fraction, fixed precision. */
export function pct(fraction: number, digits = 2): string {
  return `${(fraction * 100).toFixed(digits)}%`
}

/** Signed fixed-precision number with an explicit + for positives. */
export function signed(value: number, digits = 2): string {
  const s = value.toFixed(digits)
  return value > 0 ? `+${s}` : s
}

/** A 0–100 score, integer. */
export function score100(value: number): string {
  return Math.round(value).toString()
}

/** HH:MM:SS UTC from epoch millis. */
export function clock(ts: number): string {
  const d = new Date(ts)
  const p = (n: number) => n.toString().padStart(2, '0')
  return `${p(d.getUTCHours())}:${p(d.getUTCMinutes())}:${p(d.getUTCSeconds())}`
}
