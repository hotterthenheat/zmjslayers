/**
 * The one component allowed to render engine state.
 *
 * A readout is ACTIVE (signal register) or INACTIVE (structural register);
 * the continuous score renders beside it as a number. Nothing pulses,
 * nothing blinks, nothing says "almost".
 */

import type { Readout } from '@/wire/state'
import './binary-chip.css'

interface Props {
  label: string
  readout: Readout | undefined
  /** Optional score formatter; defaults to 2-decimal fixed. */
  format?: (score: number) => string
}

const defaultFormat = (s: number) => s.toFixed(2)

export function BinaryChip({ label, readout, format = defaultFormat }: Props) {
  const state = readout?.state ?? 'INACTIVE'
  const active = state === 'ACTIVE'
  return (
    <span className={`binary-chip ${active ? 'is-active' : 'is-inactive'}`}>
      <span className="binary-chip-label">{label}</span>
      <span className="binary-chip-state">{state}</span>
      {readout !== undefined && (
        <span className="binary-chip-score num">{format(readout.score)}</span>
      )}
    </span>
  )
}
