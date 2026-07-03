/**
 * The engine board: every engine's binary state at a glance. This is the
 * doctrine made visible — a wall of ACTIVE/INACTIVE, nothing in between.
 */

import type { EngineStatus } from '@/wire/snapshot'
import { Panel } from './Panel'
import './engine-board.css'

export function EngineBoard({ engines }: { engines: EngineStatus[] }) {
  return (
    <Panel title="Engine Board" area="engines">
      <div className="engine-grid">
        {engines.map((e) => {
          const active = e.state === 'ACTIVE'
          return (
            <div key={e.engine} className={`engine-cell ${active ? 'is-active' : 'is-inactive'}`}>
              <span className="engine-name">{e.engine}</span>
              <span className="engine-state">{e.state}</span>
              <span className="engine-score num">{e.score.toFixed(2)}</span>
            </div>
          )
        })}
      </div>
    </Panel>
  )
}
