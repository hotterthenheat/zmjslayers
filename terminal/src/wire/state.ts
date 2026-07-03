/**
 * Binary state — the TypeScript mirror of `slayer-core`'s doctrine.
 *
 * Exactly two states exist. The type system, not convention, enforces it:
 * there is no way to render a third state because no third value exists.
 */

export type BinaryState = 'ACTIVE' | 'INACTIVE'

/** A stateful readout: binary state plus the continuous score behind it. */
export interface Readout {
  state: BinaryState
  score: number
}

export function isActive(r: Readout | undefined): boolean {
  return r?.state === 'ACTIVE'
}
