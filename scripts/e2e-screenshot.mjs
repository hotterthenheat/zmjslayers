// End-to-end smoke: launch the built terminal against a live gateway, wait
// for the first snapshot to render, and screenshot the 3440x1440 desk.
import { chromium } from 'playwright'

const URL = process.env.TERMINAL_URL ?? 'http://127.0.0.1:5173'
const OUT = process.env.SHOT ?? '/tmp/slayer-terminal.png'

const browser = await chromium.launch({ executablePath: '/opt/pw-browsers/chromium' })
const page = await browser.newPage({ viewport: { width: 3440, height: 1440 } })

const errors = []
// Benign resource 404s (e.g. a favicon race on first paint) are not app errors.
const isBenign = (t) => /favicon/i.test(t) || /404 \(Not Found\)/.test(t)
page.on('console', (m) => {
  if (m.type() === 'error' && !isBenign(m.text())) errors.push(m.text())
})
page.on('pageerror', (e) => errors.push(String(e)))

await page.goto(URL, { waitUntil: 'networkidle' })

// Wait until a symbol tab appears (proves a snapshot arrived and the store filled).
await page.waitForSelector('.symbol-tab', { timeout: 20000 })
await page.waitForSelector('.engine-cell', { timeout: 20000 })
// Let a few frames stream so the ladder canvas paints.
await page.waitForTimeout(2500)

const summary = await page.evaluate(() => {
  const feed = document.querySelector('.feed-pill-label')?.textContent ?? '?'
  const spot = document.querySelector('.hero-px')?.textContent ?? '?'
  const engines = [...document.querySelectorAll('.engine-cell')].map((c) => ({
    name: c.querySelector('.engine-name')?.textContent,
    state: c.querySelector('.engine-state')?.textContent,
  }))
  const states = [...document.querySelectorAll('.engine-state, .binary-chip-state')].map(
    (e) => e.textContent,
  )
  const bad = states.filter((s) => s !== 'ACTIVE' && s !== 'INACTIVE')
  return { feed, spot, engineCount: engines.length, badStates: bad }
})

await page.screenshot({ path: OUT })
await browser.close()

console.log(JSON.stringify({ ...summary, consoleErrors: errors }, null, 2))
if (errors.length) {
  console.error('CONSOLE ERRORS PRESENT')
  process.exit(1)
}
if (summary.badStates.length) {
  console.error('NON-BINARY STATE RENDERED:', summary.badStates)
  process.exit(2)
}
if (summary.engineCount === 0) {
  console.error('NO ENGINE BOARD RENDERED')
  process.exit(3)
}
console.log('E2E OK')
