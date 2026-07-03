# 03 — Dealer Structure

Formula-level specification of the legacy dealer-positioning cluster, extracted from:

- `src/lib/greekExposure.ts` — per-strike dealer Greek exposure profiles (GEX/DEX/VEX/Charm/VegX)
- `src/lib/dealerDynamics.ts` — time-derivative & structural layers (vanna/charm engines, CoM migration, gamma velocity, OI flow, concentration, NBRS anomalies, liquidity vacuums, wall strength)
- `src/lib/dealerHedging.ts` — hedge-requirement simulator (Gaussian γ kernel, flip solving, squeeze)
- `src/lib/dealerSignals.ts` — trap/flip-proximity/stress/convexity signals + BUY veto & sizing
- `src/lib/strikeGravity.ts` — strike gravity/magnetism map + dealer zones (support/resistance walls)
- `src/lib/zeroDte.ts` — 0DTE probability engine (ITM, touch, EM bands, pin, EOD magnet, settlement risk)
- `src/lib/terminalRead.ts` — terminal synthesis read + GEX outlook regime classifier
- `src/lib/tradePlan.ts` — composite trade-plan engine (dealer layer of the 40/30/20/10 blend)
- `src/lib/gexSummary.ts` — deterministic prose generator (mostly discarded; gates catalogued)
- `src/components/DealerFlowView.tsx` (lines 76–213) — the HOLDING / TESTING / FAILING wall zone-state scoring (referenced from `dealerSignals.ts` comments; the math physically lives in this component)

Helper functions transcribed from upstream modules so this spec is self-contained: `nbrsRatio`, `oiVelocity` (`skyQuantCore.ts`), `clamp`, `normSaturate` (`normalize.ts`), `stdNormalCDF` (`v11Math.ts`, standard normal CDF).

Notation: `spot` = current underlying price `S`; `OI` = open interest in contracts; `iv`/`σ` = annualized implied vol as a decimal; `clamp(v, lo, hi) = min(max(v, lo), hi)`; `fin(v)` = `v` if it is a finite number else `0`; `N(·)` = standard normal CDF. All `$` exposure values use contract multiplier 100.

## Overview

This cluster computes **dealer positioning structure**: how much delta-hedging pressure option dealers exert at each strike, where that pressure flips sign (gamma flip), where it concentrates (walls, magnets, gravity zones), where it is absent (liquidity vacuums), how it is evolving in time (vanna/charm/migration/velocity), and what all of that implies for intraday price behavior (pinning vs amplification, squeeze risk, 0DTE settlement, trade brackets). It is the "market physics" layer of the terminal: every support/resistance band, regime label, and dealer-flow narrative the UI shows derives from these engines.

**Master sign convention (used everywhere):** per-contract dealer exposure = `greek × OI × 100 × scale × sgn` with `sgn = +1` for calls and `−1` for puts (the platform's "net call − put" dealer convention). Positive net GEX ⇒ dealers long gamma ⇒ they sell rallies / buy dips ⇒ price is dampened/pinned. Negative net GEX ⇒ dealers short gamma ⇒ they buy rallies / sell dips ⇒ moves are amplified/squeeze-prone.

**Dollar scaling (per Greek):**

| Greek | scale | exposure units |
|---|---|---|
| gamma (GEX) | `S² × 0.01` | $ per 1% spot move |
| delta (DEX) | `S` | $ delta inventory |
| vanna (VEX) | `S × 0.01` | $ delta per 1% vol move |
| charm | `S / 365` | $ delta drift per day |
| vega (VegX) | `0.01` | $ per 1% vol move |
| speed (convexity engine only) | `S³ × 0.0001` | (nominal; normalized away — see defects) |

Dependency order (upstream → downstream): option chain → per-strike exposure profiles / dealer inventory (walls, flip, magnet, net Greeks — computed in `v11Math.computeDealerInventory`, a different cluster) → this cluster's engines → terminal read / outlook / trade plan / UI status labels.

---

## Engines

### E1 — Per-Strike Greek Exposure Profile (`computeGreekExposureProfile`)

**Purpose.** Aggregate the front-expiry chain into a signed per-strike dealer-exposure curve for one Greek, plus net/gross totals and the cumulative-zero "flip" strike.

**Inputs.**
- `chain: ChainContract[]` — per-contract `{ strike, type: 'call'|'put', openInterest (contracts), gamma, delta, vanna, charm, vega (per-contract Greeks), volume? }`
- `spot: number` — underlying price, $ 
- `greek: 'gamma'|'delta'|'vanna'|'charm'|'vega'`
- `windowPct = 0.12` — strike window half-width as fraction of spot

**Algorithm.**
1. Guard: return `null` if chain is not an array, `chain.length < 3`, or `!(spot > 0)`.
2. `scale = scaleFor(greek, spot)` per the dollar-scaling table above (exactly: gamma `spot*spot*0.01`; delta `spot`; vanna `spot*0.01`; charm `spot/365`; vega `0.01`).
3. Window: keep contracts with `lo ≤ strike ≤ hi` where `lo = spot*(1−windowPct)`, `hi = spot*(1+windowPct)`.
4. For each kept contract: `sign = (type === 'call') ? 1 : −1`; `e = greekValue * oi * 100 * scale * sign` (`oi = openInterest || 0`). Skip if `greekValue`, `oi`, or `e` is non-finite. Accumulate per strike: `node.exposure += e`; and per side: `node.call += e` if call else `node.put += e`.
5. Sort nodes ascending by strike. Return `null` if fewer than 3 nodes.
6. Single pass over nodes computing:
   - `net = Σ exposure`; `gross = Σ |exposure|`; `maxAbs = max |exposure|` (returned as `maxAbs || 1`, i.e. floored to 1).
   - `topPositive` = node with largest positive `exposure`; `topNegative` = node with most-negative `exposure` (strict `>` / `<` comparisons ⇒ first occurrence wins ties).
   - Running cumulative `cum[i] = Σ_{j≤i} exposure_j`.
7. **Flip** (cumulative-zero): scan `i = 1..n−1`; on the FIRST index where `(cum[i−1] ≤ 0 && cum[i] > 0) || (cum[i−1] ≥ 0 && cum[i] < 0)`:
   `t = (cum[i] === cum[i−1]) ? 0 : (0 − cum[i−1]) / (cum[i] − cum[i−1])`;
   `flip = strike[i−1] + t*(strike[i] − strike[i−1])`; `break`. Else `flip = null`.
   Gloss: linear interpolation of the running-total zero crossing, taken at the **lowest-strike** crossing (contrast E3's flip, which picks the crossing **nearest spot**).

**Constants.**

| value | where used | semantic meaning | proposed name |
|---|---|---|---|
| 0.12 | window half-width default | ±12% strike window around spot | GREEK_PROFILE_WINDOW_PCT |
| 3 | min chain length & min nodes | sparse-chain guard | GREEK_PROFILE_MIN_NODES |
| 100 | exposure formula | option contract multiplier | CONTRACT_MULTIPLIER |
| 0.01 | gamma/vanna/vega scale | per-1%-move normalization | PCT_MOVE_UNIT |
| 365 | charm scale | calendar days per year (charm per-day) | CHARM_DAYS_PER_YEAR |
| 1 | `maxAbs \|\| 1` | floor so downstream bar-scaling never divides by 0 | MAX_ABS_FLOOR |

**Outputs.** `GreekExposureProfile { greek; nodes[{strike, exposure($), call($), put($)}] ascending; net($); gross($ ≥ 0); maxAbs($ ≥ 1); flip(price|null); topPositive; topNegative }` or `null`.

---

### E2 — Dealer Dynamics (`computeDealerDynamics`)

**Purpose.** Time-derivative and structural layers over the dealer book: hedge-flow direction from vanna, charm decay intensity, gamma center-of-mass migration, gamma/OI velocities, concentration, neighbor anomalies, liquidity vacuums, wall strength. Requires a caller-persisted rolling snapshot history (one snapshot per tick).

**Inputs.**
- `chain: ChainContract[]`
- `spot: number` ($)
- `inventory: { netGex, netVanna, netCharm }` — current net dealer Greeks ($; charm is per-day) from the dealer-inventory engine
- `history: DealerSnapshot[]` — oldest→newest; `{ t(ms epoch), netGex, netVanna, netCharm, gexCoM, totalOi }`. **Mutated in place**: a new snapshot is appended at the end of the call and the array trimmed to the last 180 entries.

**Algorithm.**

*Step 0 — per-strike aggregation (`aggregateStrikes`).* For every contract with `fin(strike) > 0`:
`gexMag = |fin(gamma) * fin(openInterest) * 100 * spot * spot * 0.01|` (UNSIGNED magnitude — no call/put sign; used only in relative terms), `oi += fin(openInterest)`, `volume += fin(volume)`; grouped by strike, sorted ascending. No strike window is applied (whole chain).

*Step 1 — gamma center of mass.* `totGex = Σ gexMag`; `gexCoM = totGex > 0 ? Σ(strike·gexMag)/totGex : spot`.

*Step 2 — history references.* `prev` = last history entry (or null); `prev2` = second-to-last (or null). NOTE: the current snapshot has NOT been appended yet, so `prev` is genuinely the prior tick.

*Step 3 — Vanna engine.*
- `vannaVel = prev ? netVanna − prev.netVanna : 0`
- `trend = |vannaVel| < max(1e−9, |netVanna|·0.02) ? 'FLAT' : (vannaVel > 0 ? 'RISING' : 'FALLING')`
- `hedgeFlow = |netVanna| < 1e−9 ? 'NEUTRAL' : (netVanna > 0 ? 'SUPPORTIVE' : 'PRESSURING')`
  Gloss: positive net vanna ⇒ falling IV makes dealers buy futures (supportive); negative ⇒ falling IV makes them sell (pressuring).

*Step 4 — Charm engine.*
- `recentCharm = history.slice(−20).map(h ⇒ |h.netCharm|)` (prior window; current value excluded because it isn't appended yet)
- `priorMaxCharm = recentCharm.length ≥ 3 ? max(recentCharm) : 0`
- `intensity = priorMaxCharm > 0 ? min(1, |netCharm| / priorMaxCharm) : 0` (stays 0 until ≥3 snapshots have accrued)
- `bias = |netCharm| < 1e−9 ? 'NEUTRAL' : (netCharm > 0 ? 'BULLISH' : 'BEARISH')`

*Step 5 — Strike migration.*
- `comPrev = prev ? prev.gexCoM : gexCoM` (⇒ shift 0 on the first tick)
- `shift = gexCoM − comPrev` (price units)
- `migScore = spot > 0 ? clamp(shift / (spot·0.01), −1, 1) : 0` — ±1 ≡ CoM moved ±1% of spot in one tick
- `direction = |migScore| < 0.05 ? 'STABLE' : (migScore > 0 ? 'BULLISH' : 'BEARISH')`

*Step 6 — Gamma velocity/acceleration.*
- `gVel = prev ? netGex − prev.netGex : 0`; `gPrevVel = (prev && prev2) ? prev.netGex − prev2.netGex : 0`; `gAcc = gVel − gPrevVel`
- `gThresh = max(1e6, |netGex|·0.03)` ($)
- `state = |gVel| < gThresh ? 'STABLE' : (gVel > 0 ? 'ADDING_HEDGES' : 'REMOVING_HEDGES')`

*Step 7 — OI flow.*
- `totalOi = Σ per-strike oi`; `now = Date.now()`; `dtMin = prev ? max(1e−6, (now − prev.t)/60000) : 0`
- `oiVel = prev ? (totalOi − fin(prev.totalOi)) / dtMin : 0` (contracts/min; `oiVelocity(a,b,dt) = dt ? (a−b)/dt : 0`)
- `oiThresh = max(1, totalOi·0.005)` (0.5% of book per minute)
- `state = |oiVel| < oiThresh ? 'STABLE' : (oiVel > 0 ? 'BUILDING' : 'UNWINDING')`

*Step 8 — Concentration.*
- Gamma shares `s_i = totGex > 0 ? gexMag_i/totGex : 0`; `hhi = Σ s_i²` (rounded to 4 dp).
- `top3Share(vals, tot) = tot > 0 ? min(100, 100 · (sum of 3 largest vals)/tot) : 0`; applied to gexMag (vs `totGex`) and oi (vs `totalOi`); each rounded to 1 dp.
- Density cluster: for each index `i`, `cluster_i = Σ oi_j for j ∈ [i−2, i+2]` (index-clipped, i.e. ±2 array positions, NOT price distance); `densityPct = max_i 100·cluster_i/totalOi` (1 dp), `densityStrike = strike at the argmax` (defaults `spot`/`0` when the chain is empty or `totalOi ≤ 0`). Strict `>` ⇒ first argmax wins.

*Step 9 — NBRS anomalies* (per axis: gexMag, oi, volume). If `per.length < 3` ⇒ null. Else for each index `i` compute
`nbrsRatio(vals, i, n=3)`: `lo = max(0, i−3)`, `hi = min(len, i+4)`; `windowSum = Σ_{j∈[lo,hi)} |vals_j|`; `target = |vals_i|`; `neigh = windowSum − target`; `cnt = (hi−lo) − 1`; return `1` if `cnt ≤ 0 || neigh ≤ 0`, else `target / (neigh/cnt)` — the strike's value over the mean of up to 6 neighbors. Keep the max-ratio strike; `ratio` rounded to 2 dp.

*Step 10 — Liquidity vacuums (`detectVacuums`).* Return empty result if `per.length < 3` or `!(spot > 0)`.
- `maxGex = max(gexMag) || 1`, `maxOi = max(oi) || 1`, `maxVol = max(volume) || 1`
- Per-strike liquidity: `liq_i = 0.4·gexMag_i/maxGex + 0.3·oi_i/maxOi + 0.3·volume_i/maxVol`
- Scan strikes ascending, accumulating runs where `liq_i < 0.15` (THRESH). A run tracks `runStart` (index of first low-liquidity strike), `runSum`, `runCount` **over the low-liquidity strikes only**.
- When a run is broken by a liquid strike at index `i`, flush with `endIdx = i` (so `hi` is the LIQUID terminator strike); if a run reaches the end of the array, flush with `endIdx = per.length − 1` (last strike, which is itself low-liquidity). In both cases `lo = per[runStart].strike`.
- Flush math: `widthPct = (hi − lo)/spot`; `avgLiq = runSum/runCount`;
  `score = clamp((1 − avgLiq/0.15)·0.6 + min(1, widthPct/0.03)·0.4, 0, 1)` — emptier and wider ⇒ higher; width saturates at 3% of spot.
- Zone kept only if `hi > lo`. Side: `hi ≤ spot ⇒ 'below'`; else `lo ≥ spot ⇒ 'above'`; else (straddling) `'above'` if `|hi − spot| < |spot − lo|` else `'below'` (see defects D3).
- Sort zones by score desc; keep top 6.
- `nearestAbove` = among zones with `lo ≥ spot || side === 'above'`, the one with smallest `lo`; `nearestBelow` = among zones with `hi ≤ spot || side === 'below'`, the one with largest `hi` (each null if none). NOTE: nearest* are selected from ALL zones before the top-6 truncation... actually from the full sorted `zones` array prior to `.slice(0,6)` — both filters run on the untruncated array; only the returned `zones` list is truncated.

*Step 11 — Wall strength (`computeWallStrength`).* If no strikes ⇒ both null. With the same per-axis maxima (each `|| 1`):
`strength(p) = round(100 · (0.5·gexMag/maxGex + 0.25·oi/maxOi + 0.25·volume/maxVol))`.
Support = the strike `< spot` with the largest `gexMag`; resistance = the strike `> spot` with the largest `gexMag`; each scored with `strength`. (A strike exactly at spot belongs to neither side.) This 0–100 score is the "wall strength" number the UI shows next to the HOLDING/TESTING/FAILING labels (E10).

*Step 12 — snapshot append.* `history.push({ t: now, netGex, netVanna, netCharm, gexCoM, totalOi })`; if `history.length > 180`, delete from the front so exactly 180 remain.

**Constants.**

| value | where used | semantic meaning | proposed name |
|---|---|---|---|
| 180 | history trim | rolling snapshot cap (ticks) | DEALER_HISTORY_CAP |
| 100, 0.01 | gexMag | contract multiplier; per-1% unit | CONTRACT_MULTIPLIER, PCT_MOVE_UNIT |
| 1e−9 | vanna FLAT floor, vanna/charm neutral epsilon | absolute zero-band | DEALER_EPS |
| 0.02 | vanna trend threshold | 2% relative change ⇒ trend | VANNA_TREND_REL_THRESH |
| 20 | charm window | snapshots in charm-intensity lookback | CHARM_LOOKBACK |
| 3 | charm min history | snapshots before intensity is live | CHARM_MIN_HISTORY |
| 0.01 | migration normalizer | 1% of spot ≡ score ±1 | MIGRATION_FULL_SCALE_PCT |
| 0.05 | migration STABLE band | \|score\| below ⇒ STABLE | MIGRATION_STABLE_BAND |
| 1e6 | gamma velocity floor | $1M absolute velocity floor | GAMMA_VEL_ABS_FLOOR |
| 0.03 | gamma velocity rel threshold | 3% of \|netGex\| | GAMMA_VEL_REL_THRESH |
| 1e−6 | dtMin floor | minute-delta floor | DT_MIN_FLOOR |
| 60000 | ms→min | milliseconds per minute | MS_PER_MINUTE |
| 1 | OI velocity floor | ≥1 contract/min | OI_VEL_ABS_FLOOR |
| 0.005 | OI velocity rel threshold | 0.5% of book per minute | OI_VEL_REL_THRESH |
| 3 | top-N share | strikes in top-3 concentration | TOP_N_CONCENTRATION |
| 2 | density cluster half-width | ±2 array positions | DENSITY_CLUSTER_HALF_WIDTH |
| 3 (n) | nbrsRatio window | ±3 neighbors | NBRS_NEIGHBOR_HALF_WIDTH |
| 4, 1, 1, 2 | toFixed rounding | display rounding (hhi 4dp; shares/density 1dp; nbrs 2dp) | (display only) |
| 0.4/0.3/0.3 | vacuum liquidity blend | gex/OI/volume weights | VACUUM_LIQ_W_GEX/OI/VOL |
| 0.15 | vacuum threshold | low-liquidity cutoff | VACUUM_LIQ_THRESH |
| 0.6/0.4 | vacuum score blend | emptiness vs width weights | VACUUM_SCORE_W_EMPTY/WIDTH |
| 0.03 | vacuum width saturation | width scores 1.0 at 3% of spot | VACUUM_WIDTH_SAT_PCT |
| 6 | zones cap | max vacuum zones returned | VACUUM_MAX_ZONES |
| 0.5/0.25/0.25 | wall strength blend | gamma/OI/volume weights | WALL_W_GEX/OI/VOL |
| 100 | wall strength scale | 0–100 output | WALL_SCORE_SCALE |

**Outputs.** `DealerDynamics { vanna{net,velocity,trend,hedgeFlow,note}, charm{netPerDay,intensity∈[0,1],bias,note}, migration{comCurrent,comPrevious,shift,score∈[−1,1],direction}, gamma{velocity,acceleration,state}, oiFlow{totalOi,velocity,state}, concentration{hhi∈[0,1],gammaTop3Pct∈[0,100],oiTop3Pct∈[0,100],densityStrike,densityPct}, nbrs{gamma,oi,volume: {strike,ratio}|null}, vacuums{zones≤6, nearestAbove, nearestBelow}, walls{support,resistance: {strike,score∈[0,100]}|null} }` — plus the in-place history mutation.

---

### E3 — Dealer Hedging Simulator (`simulateDealerHedging`)

**Purpose.** Project dealer net $-gamma over a hypothetical spot grid using a Gaussian proximity kernel over per-strike signed GEX; derive the hedge path-integral, the gamma flip nearest spot, and downside squeeze risk.

**Inputs.**
- `strikes: {strike, netGex($, signed call−put)}[]` — from `gex_profile.strikes`
- `spot: number` ($)
- `emPct: number` — one-sigma expected move as a FRACTION of spot (kernel width driver)
- `rangePct = 0.06` — grid half-width (fraction of spot)
- `steps = 121` — grid resolution

**Algorithm.**
1. Filter rows to finite `strike > 0` with finite `netGex`. Return `null` if `< 2` rows or `!(spot > 0)`.
2. Kernel width: `w = max(spot · min(0.08, max(0.003, emPct)), spot · 0.003)` — i.e. `emPct` clamped to `[0.003, 0.08]` times spot (the outer max is redundant given the inner clamp).
3. `Γ$(S) = Σ_k netGex_k · exp(−0.5 · ((S − K_k)/w)²)`.
4. Grid: `lo = spot(1−rangePct)`, `hi = spot(1+rangePct)`, `dP = (hi−lo)/(steps−1)`, `price_i = lo + i·dP`, `g_i = Γ$(price_i)` for `i = 0..steps−1`. `spotIdx = round(((spot−lo)/(hi−lo))·(steps−1))` (with a symmetric grid this is the center index).
5. Cumulative hedge, measured outward from spot (`cum[spotIdx] = 0`), in $-per-1% units:
   - upward, `i = spotIdx+1 .. steps−1`: `midG = (g_i + g_{i−1})/2`; `cum_i = cum_{i−1} + midG · ((price_i − price_{i−1}) / price_i) · 100`
   - downward, `i = spotIdx−1 .. 0`: `midG = (g_i + g_{i+1})/2`; `cum_i = cum_{i+1} − midG · ((price_{i+1} − price_i) / price_i) · 100`
   Gloss: trapezoidal integral of Γ$ against the percent move; note the divisor is `price_i` (the node being assigned) in BOTH directions — the upper node on the way up but the lower node on the way down (defect D5).
6. Nodes: `{price_i, gammaDollar: g_i, cumHedge: cum_i, regime: g_i ≥ 0 ? 'stabilizing' : 'amplifying'}`.
7. `netGammaNow = Γ$(spot)` (evaluated exactly at spot, not at the nearest grid node); `regimeNow = netGammaNow ≥ 0 ? 'stabilizing' : 'amplifying'`.
8. **Gamma flip:** scan `i = 1..steps−1` for sign changes `(g_{i−1} ≤ 0 && g_i > 0) || (g_{i−1} ≥ 0 && g_i < 0)`; interpolate `t = (g_i === g_{i−1}) ? 0 : −g_{i−1}/(g_i − g_{i−1})`, `cross = price_{i−1} + t·(price_i − price_{i−1})`; keep the crossing with the smallest `|cross − spot|`. `gammaFlip = null` if none within the ±6% grid.
9. **Squeeze:** `minG = min(0, min over grid nodes with price < spot of g)` and `squeezePrice` = the price of that most-negative node (null if no node below spot has `g < 0`). `maxAbs = max_i |g_i|` (`|| 1`); `squeezeScore = clamp(−minG/maxAbs, 0, 1)`. Downside only — no upside analogue.
10. `hedgePer1PctUp = netGammaNow`; `hedgePer1PctDown = −netGammaNow` (signed $ dealers trade for a ±1% move; positive means selling into the rally / buying the dip).

**Constants.**

| value | where used | semantic meaning | proposed name |
|---|---|---|---|
| 0.06 | rangePct default | ±6% simulated grid | HEDGE_GRID_RANGE_PCT |
| 121 | steps default | grid resolution (0.1% spacing at ±6%) | HEDGE_GRID_STEPS |
| 0.003 | kernel width floor | min kernel width = 0.3% of spot | HEDGE_KERNEL_MIN_PCT |
| 0.08 | kernel width cap | max kernel width = 8% of spot | HEDGE_KERNEL_MAX_PCT |
| 0.5 | Gaussian exponent | standard −z²/2 kernel | (structural) |
| 100 | cum integration | percent-move conversion | PCT_SCALE |
| 2 | min strike rows | sparse guard | HEDGE_MIN_STRIKES |
| 1 | `maxAbs \|\| 1` | squeeze-score divisor floor | MAX_ABS_FLOOR |

**Outputs.** `DealerHedgingResult { spot; nodes[121]{price, gammaDollar($/1%), cumHedge($/1% cumulative, 0 at spot), regime}; netGammaNow($/1%); regimeNow; gammaFlip(price|null); hedgePer1PctUp/Down($); squeezePrice(price|null); squeezeScore∈[0,1] }` or `null`.

---

### E4 — Dealer Signals & Decision Coupling (`dealerSignals.ts`)

**Purpose.** Four macro/timing signals from the dealer inventory (trap, flip proximity, stress, convexity) feeding a (default-off) BUY veto and position-size trim. Not displayed, not ranker inputs.

**Inputs.** `spot($)`, `callWall/putWall/gammaFlipPrice(price)`, `grossGex($)`, `expectedMovePct(fraction)`, `gexAtCallWall/gexAtPutWall($, signed)`, `wallsConfident/gammaFlipConfident(bool — false when the upstream engine used a fabricated fallback)`, `volExpansionNorm∈[0,1]` (default 0.5 when absent), `convexityChain[{gamma, speed?, vanna?, charm?, oi}]`. Config `{TRAP_CAP = 0.5, GEX_REF = 5e9}`; `EPS = 1e−9`.

Helper: `normSaturate(x, cap) = (!isFinite(x) || !(cap > 0)) ? 0 : 100·clamp(x/cap, 0, 1)`.

**§3.1 Dealer Trap Score (0–100, abstainable).**
1. If `!wallsConfident` ⇒ `{value: 50, confident: false}`.
2. `wallMag = (|gexAtCallWall| + |gexAtPutWall|)/2`
3. `wallMagShare = wallMag / max(grossGex, 1)`
4. `wallDistPct = |callWall − putWall| / max(spot, EPS)`
5. `value = normSaturate(wallMagShare / max(wallDistPct, EPS), 0.5)`; `confident: true`.
   Gloss: heavy walls close together ⇒ caged/mean-reverting; the ratio saturates at TRAP_CAP=0.5 → 100.

**§3.2 Gamma Flip Proximity (0–1, abstainable).**
1. If `!gammaFlipConfident` ⇒ `{value: 0, confident: false}`.
2. `flipDistPct = |spot − gammaFlipPrice| / max(spot, EPS)`
3. `value = clamp(1 − flipDistPct / max(expectedMovePct, EPS), 0, 1)` — 1 at the flip, 0 beyond one expected move.

**§3.3 Dealer Stress Index (0–100).**
`g = clamp(grossGex / max(GEX_REF, EPS), 0, 1)`; `v = clamp(volExpansionNorm, 0, 1)`; `p = clamp(gammaFlipProximity, 0, 1)`;
`stress = 100 · cbrt(g·v·p)` (geometric mean so one small factor doesn't collapse it; degrades to 0 when the flip is unconfident since p=0).

**§3.4 Dealer Convexity (0–100).** Over the chain (0 if empty):
- `gexEx_i = |gamma_i · oi_i · 100 · S² · 0.01|`
- `spdEx_i = |speed_i · oi_i · 100 · S³ · 0.0001|` (speed defaults 0)
- `vexEx_i = |vanna_i · oi_i · 100|` (vanna defaults 0)
- `chmEx_i = |charm_i · oi_i · 100|` (charm defaults 0)
- Per-component normalization: `norm(arr): mx = max(...arr, EPS); x ↦ x/mx` (scale-invariant; whole-array max includes EPS as a floor).
- `convexity = 100 · (Σ_i (gN_i + sN_i + vN_i + cN_i)/4) / n`.
  Gloss: average across strikes of the average normalized exposure across four Greeks (averaging, not summing, prevents speed double-driving gamma).

**Decision/size coupling (`modulateDecision`, default OFF).** Config `{enabled=false, trapHighThreshold=70, flipNearThreshold=0.7, stressSizeK=0.5, sizeFloor=0.5}`.
1. If `!enabled` ⇒ pass decision through, `sizeMultiplier = 1`, `modulated = false`.
2. `sizeMultiplier = clamp(1 − stressSizeK · (clamp(stress, 0, 100)/100), sizeFloor, 1)` — bounded linear trim, never below 0.5×, never amplifies.
3. `trapHigh = trapConfident && trapScore ≥ 70`; `nearFlip = flipConfident && flipProximity ≥ 0.7`.
4. If `decision === 'BUY' && trapHigh && nearFlip` ⇒ decision becomes `'WAIT'` (`modulated = true`, reason annotated). All other decisions (`WAIT/HOLD/REDUCE/EXIT`) pass through with the size multiplier.

**Constants.**

| value | where used | semantic meaning | proposed name |
|---|---|---|---|
| 1e−9 | division floors | epsilon | DEALER_SIGNAL_EPS |
| 0.5 | TRAP_CAP | trap ratio saturation cap | TRAP_CAP |
| 5e9 | GEX_REF | $5B gross-GEX full-scale reference (per-asset placeholder) | GEX_REF |
| 50 | trap abstain value | neutral trap score | TRAP_ABSTAIN_VALUE |
| 1 | grossGex floor | $1 floor in wallMagShare | GROSS_GEX_FLOOR |
| 0.5 | volExpansionNorm default | neutral vol-expansion when absent | VOL_EXPANSION_DEFAULT |
| 100·cbrt | stress scale | geometric-mean 0–100 | (structural) |
| 0.0001 | speed scale | S³ speed dollar-scale coefficient | SPEED_SCALE_COEF |
| 4 | convexity divisor | number of Greek components | CONVEXITY_N_COMPONENTS |
| 70 | trapHighThreshold | caged-regime cutoff | TRAP_HIGH_THRESHOLD |
| 0.7 | flipNearThreshold | at-the-flip cutoff | FLIP_NEAR_THRESHOLD |
| 0.5 | stressSizeK | max size cut fraction | STRESS_SIZE_K |
| 0.5 | sizeFloor | min size multiplier | SIZE_FLOOR |

**Outputs.** `DealerSignals { dealerTrapScore∈[0,100] + confident, gammaFlipProximity∈[0,1] + confident, dealerStressIndex∈[0,100], dealerConvexity∈[0,100] }`; `ModulatedDecision { decision, reason, sizeMultiplier∈[0.5,1], modulated }`.

---

### E5 — Strike Gravity & Dealer Zones (`computeStrikeGravity`)

**Purpose.** Score every strike as a dealer "magnet" (composite of GEX/OI/volume/proximity), rank them, and cluster the top strikes into contiguous zones — directional support/resistance walls plus straddle (pin) zones.

**Inputs.**
- `strikes: GexStrikeDetail[]` — `{strike, netGex($, signed), callOi, putOi (contracts), callVolume, putVolume (contracts)}` from `gex_profile.strikes`
- `spot($)`; `topN = 10`; `weights = {gex: 0.4, oi: 0.2, volume: 0.2, proximity: 0.2}`; `proximityScale = 0.04`

**Algorithm.**
1. Guard: empty result if not an array / length 0 / `!(spot > 0)`.
2. Per strike: `netGex = fin(netGex)`; `absGex = |netGex|`; `oi = fin(callOi)+fin(putOi)`; `volume = fin(callVolume)+fin(putVolume)`; drop rows with `strike ≤ 0`. Empty result if none remain.
3. Axis maxima: `maxAbsGex`, `maxOi`, `maxVol` over kept rows.
4. **Weight redistribution:** `hasGex = maxAbsGex > 0` etc.; zero out the weight of any signal-less axis (`wGex = hasGex ? 0.4 : 0`, ...); proximity always keeps its weight; then renormalize all four by `wSum = wGex + wOi + wVol + wProx || 1` so weights sum to 1.
5. Per strike scores:
   - `gexWeight = hasGex ? absGex/maxAbsGex : 0`; `oiWeight`, `volWeight` analogous
   - `distancePct = (strike − spot)/spot` (signed)
   - `proximityWeight = exp(−|distancePct| / 0.04)` (a strike 4% away scores e⁻¹ ≈ 0.37)
   - `gravityScore = wGex·gexWeight + wOi·oiWeight + wVol·volWeight + wProx·proximityWeight`
   - `side = |distancePct| < 0.001 ? 'atm' : (distancePct > 0 ? 'resistance' : 'support')`
6. `ranked` = top `topN` by gravityScore desc; `primary` = #1; `upperNeighbor` = highest-gravity strike with `strike > spot`; `lowerNeighbor` = highest-gravity with `strike < spot` (each found in the FULL gravity-desc list, not just top-N).
7. **Strike step estimate:** gaps between consecutive distinct sorted strikes (all scored strikes); `sortedGaps` ascending; `step = sortedGaps[floor(len·0.25)]` (25th percentile — a low percentile so missing/illiquid strikes don't over-merge walls) or `spot·0.005` if no gaps. `zoneGap = max(step·2.5, spot·0.001)`.
8. **Zone clustering (`buildZones`):** sort the given strikes ascending; start a new cluster whenever the gap to the previous clustered strike exceeds `zoneGap`. Per zone: `lo/hi` = first/last strike; `netGex = Σ signed netGex`; `gravity = Σ gravityScore`; `side = hi < spot ? 'support' : lo > spot ? 'resistance' : 'straddle'`.
9. `zones = buildZones(ranked)` sorted gravity desc (may straddle spot = pin zone).
10. Directional walls: `supportWall = ` highest-gravity zone from `buildZones(ranked.filter(strike < spot))`; `resistanceWall` analogous with `strike > spot` (clustered per side so a wall exists even when the overall zone straddles).
11. `clusterScore = zones.length ? min(1, zones[0].gravity / (Σ gravityScore over ranked || 1)) : 0` — gravity concentration in the top zone.

**Constants.**

| value | where used | semantic meaning | proposed name |
|---|---|---|---|
| 0.4/0.2/0.2/0.2 | default weights | gex/oi/volume/proximity blend | GRAVITY_W_GEX/OI/VOL/PROX |
| 10 | topN default | ranked strikes returned | GRAVITY_TOP_N |
| 0.04 | proximityScale | exponential decay scale (fraction of spot) | GRAVITY_PROXIMITY_SCALE |
| 0.001 | atm band | ±0.1% of spot ⇒ 'atm' | GRAVITY_ATM_BAND_PCT |
| 0.25 | gap percentile | strike-step estimator quantile | STRIKE_STEP_QUANTILE |
| 0.005 | step fallback | 0.5% of spot when no gaps | STRIKE_STEP_FALLBACK_PCT |
| 2.5 | zoneGap multiplier | cluster if within 2.5 strike steps | ZONE_GAP_STEP_MULT |
| 0.001 | zoneGap floor | ≥0.1% of spot | ZONE_GAP_MIN_PCT |
| 1 | totalGravity floor | clusterScore divisor | (floor) |

**Outputs.** `StrikeGravityResult { spot; ranked[≤10]; primary; upperNeighbor; lowerNeighbor; resistanceWall; supportWall; zones (gravity desc); clusterScore∈[0,1]; weightsUsed }`. `GravityStrike` carries all component weights, signed `netGex`, `distancePct`, `side`.

---

### E6 — 0DTE Probability Engine (`zeroDte.ts`)

**Purpose.** Closed-form risk-neutral probabilities for same-day expiry: ITM probability, barrier touch, expected-move bands, strike-pinning probability, EOD magnet, settlement risk.

**Time convention.** `TRADING_DAYS = 252`, `SESSION_HOURS = 6.5`. `hoursToYearFraction(h) = max(1e−6, (max(0, h)/6.5)/252)`.

**Inputs (master `compute0DTE`).** `spot($)`, `atmIv(annualized decimal)`, `hoursToClose(hours)`, `netGex($, signed)`, `magnet(price — the upstream profile's pin magnet)`, `strikes[{strike, netGex($, signed)}]`.

**E6.1 probExpireITM(spot, strike, T, iv, isCall, r=0.05, q=0).**
- Degenerate (`spot ≤ 0 || strike ≤ 0 || T ≤ 0 || iv ≤ 0`): call ⇒ `spot > strike ? 1 : 0`; put ⇒ `spot < strike ? 1 : 0`.
- Else `d2 = (ln(spot/strike) + (r − q − 0.5·iv²)·T) / (iv·√T)`; call ⇒ `N(d2)`, put ⇒ `N(−d2)`.

**E6.2 probabilityOfTouch(spot, barrier, T, iv, r=0.05, q=0).** Exact GBM first-passage, continuous monitoring.
- Guards: 0 if `spot ≤ 0 || barrier ≤ 0 || T ≤ 0 || iv ≤ 0`; 1 if `barrier === spot`.
- `m = ln(barrier/spot)`; `ν = r − q − 0.5·iv²`; `sigT = iv·√T`; `expTerm = exp(2νm/iv²)`.
- Up (`barrier > spot`): `p = N((−m + νT)/sigT) + expTerm·N((−m − νT)/sigT)`
- Down: `p = N((m − νT)/sigT) + expTerm·N((m + νT)/sigT)`
- Return `clamp(p, 0, 1)`.

**E6.3 expectedMoveBands(spot, iv, hoursToClose).** Two horizons: `1H` with `hours = min(1, max(0, hoursToClose))` and `EOD` with `hours = max(0, hoursToClose)`. Per horizon: `T = hoursToYearFraction(hours)`; `σ₁ = spot·iv·√T`; band = `{movePts: σ₁, movePct: spot>0 ? σ₁/spot : 0, upper1/lower1 = spot ± σ₁, upper2/lower2 = spot ± 2σ₁}`.

**E6.4 pinProbability({spot, magnet, gammaShare, netGex, emToClosePts, fracSessionElapsed}).**
- `distancePct = spot > 0 ? |spot − magnet|/spot : 1`.
- Degenerate (`spot ≤ 0 || magnet ≤ 0 || emToClosePts ≤ 0`) ⇒ probability 0.
- `z = (spot − magnet)/emToClosePts`; `proximity = exp(−0.5·z²)`
- `timeFactor = 0.4 + 0.6·clamp(fracSessionElapsed, 0, 1)` (pinning strengthens into the close: 0.4 → 1.0)
- `gammaSign = netGex ≥ 0 ? 1 : 0.35` (long gamma pins; short gamma works against a pin)
- `pinProbability = clamp(gammaShare · proximity · timeFactor · gammaSign, 0, 1)`.

**E6.5 eodMagnetTarget(strikes, spot).** Positive-gamma center of mass: over strikes with `strike > 0` and `g = max(0, netGex) > 0`, `Σ(strike·g)/Σg`; `spot` if the sum is empty.

**E6.6 compute0DTE assembly.**
1. `T = hoursToYearFraction(hoursToClose)`; `bands = expectedMoveBands(...)`; `emEod = EOD band movePts || spot·atmIv·√T` (fallback fires when movePts is 0).
2. `eodMagnet = eodMagnetTarget(strikes, spot)` (reported; NOT used for the pin — see D9).
3. **Gamma share at the magnet** (±1-strike band, not one exact strike): `totalAbsGex = Σ|netGex| || 1`; `spacing = min positive gap` between sorted distinct strikes `> 0`, fallback `max(1, magnet·0.0025)` when no finite positive gap; `band = spacing·1.5`; `magnetAbsGex = Σ |netGex|` over strikes with `|strike − magnet| ≤ band`; `gammaShare = min(1, magnetAbsGex/totalAbsGex)`.
4. `fracSessionElapsed = 1 − clamp(hoursToClose/6.5, 0, 1)`.
5. `pin = pinProbability({spot, magnet, gammaShare, netGex, emToClosePts: emEod, fracSessionElapsed})`.
6. **Settlement risk** (P(|close-to-close return| > 1 EM), gamma-regime-adjusted): `gammaRegime = clamp(netGex/totalAbsGex, −1, 1)`; `volAdj = clamp(1 − 0.25·gammaRegime, 0.75, 1.25)`; `settlementRiskPct = clamp(2·N(−1/volAdj), 0, 1)`. Gloss: baseline 2·N(−1) ≈ 31.7%; short gamma (negative regime) widens the effective σ up to +25% ⇒ higher risk; long gamma tightens ⇒ lower.

**Constants.**

| value | where used | semantic meaning | proposed name |
|---|---|---|---|
| 252 | year fraction | trading days per year | TRADING_DAYS |
| 6.5 | year fraction, session frac | session hours (RTH) | SESSION_HOURS |
| 1e−6 | year-fraction floor | T floor | T_FLOOR |
| 0.05 | r default | risk-free rate | DEFAULT_RISK_FREE |
| 0 | q default | dividend yield | DEFAULT_DIV_YIELD |
| 0.5 | d2 / ν | ½σ² Itô term | (structural) |
| 1, 2 | band multiples | ±1σ, ±2σ bands | BAND_SIGMA_1/2 |
| 0.4, 0.6 | timeFactor | pin time ramp base/slope | PIN_TIME_BASE, PIN_TIME_SLOPE |
| 0.35 | gammaSign short | short-gamma pin attenuation | PIN_SHORT_GAMMA_FACTOR |
| 1.5 | magnet band | ±1 strike inclusive (1.5 × spacing) | MAGNET_BAND_SPACING_MULT |
| 0.0025 | spacing fallback | 0.25% of magnet | SPACING_FALLBACK_PCT |
| 1 | spacing fallback floor; totalAbsGex floor | $1 / 1-pt floors | (floors) |
| 0.25 | volAdj slope | ±25% σ reshaping by gamma regime | SETTLEMENT_VOL_ADJ_SLOPE |
| 0.75 / 1.25 | volAdj clamp | σ adjustment bounds | SETTLEMENT_VOL_ADJ_MIN/MAX |
| −1 | settlement z | 1-EM threshold | (structural) |

**Outputs.** `ZeroDteResult { hoursToClose; T; expectedMove[2]; pin{magnet, pinProbability∈[0,1], gammaShare∈[0,1], distancePct}; eodMagnet(price); settlementRiskPct∈[0,1]; atmIv }`. Plus standalone `probExpireITM∈[0,1]`, `probabilityOfTouch∈[0,1]`.

---

### E7 — Terminal Read (`computeTerminalRead`)

**Purpose.** Synthesize the GEX profile + recent closes into a directional bias with a weighted confluence score, regime (PIN/TREND), pin strength, a battle plan (entry/target/stop or explicit no-trade), position strength, and narrative events.

**Inputs.** `profile: GexProfileData` — `{spot, netGex($), gammaFlip?, magnet?, callWall?, putWall?, expectedMovePct?(fraction), totalCallOi?, totalPutOi?, netVex?, strikes?[{strike, netGex, netVex?, callVex?, putVex?}]}`; `recentCloses: number[]` (assumed oldest→newest; default `[]`).

**Algorithm.**
1. `spot = profile.spot || 0`; `netGex = profile.netGex || 0`; `longGamma = netGex ≥ 0`; `regime = longGamma ? 'PIN' : 'TREND'`.
2. Helper `pct(lvl) = (lvl && spot) ? ((lvl − spot)/spot)·100 : null` — **percent units**.
3. Honest net vanna: `strikeVex = Σ (s.netVex ?? ((s.callVex ?? 0) + (s.putVex ?? 0)))`; `netVex = hasStrikeVex ? strikeVex : (typeof profile.netVex === 'number' ? profile.netVex : undefined)` where `hasStrikeVex` = any strike has any vex field non-null.
4. Raw momentum: needs `recentCloses.length ≥ 4`; `a = last`, `b = first`; `rawMom = a > b·1.0005 ? 1 : a < b·0.9995 ? −1 : 0` (±0.05% dead-band over the window).
5. **Signals** (each `{dir ∈ {−1,0,1}, weight}`; skipped when its gate fails):
   - γ-flip position (gate `flip && spot`): `dir = spot ≥ flip ? +1 : −1`, weight 28.
   - Magnet pull (gate `magnet && spot`): `d = pct(magnet) ?? 0`; `dir = |d| < 0.08 ? 0 : (d > 0 ? 1 : −1)` (points toward the magnet; 0.08 is in percent = 0.08%); weight `regime === 'PIN' ? 24 : 8`.
   - Wall position (gate `cw && pw && cw > pw && spot`): `rel = (spot − pw)/(cw − pw)`; `dir = rel > 0.72 ? −1 : rel < 0.28 ? 1 : 0`; weight 16.
   - Positioning (gate `callOi + putOi > 0`): `bull = 100·callOi/(callOi + putOi)`; `dir = bull ≥ 55 ? 1 : bull ≤ 45 ? −1 : 0`; weight 18.
   - Momentum (gate `rawMom ≠ 0`): `dir = regime === 'PIN' ? −rawMom : rawMom` (contra in a pin, with-trend in short gamma); weight `PIN ? 12 : 24`.
6. `score = clamp(round(Σ dir·weight), −100, 100)`.
7. `bias = score > 18 ? 'LONG' : score < −18 ? 'SHORT' : 'NEUTRAL'`; `biasDir ∈ {1, −1, 0}`.
8. `dirWeight = Σ weight over signals with dir ≠ 0` (`|| 1`); `agreeWeight = Σ weight over signals with dir === biasDir` (0 when neutral); `confidence = biasDir === 0 ? min(40, round(|score|)) : round(100·agreeWeight/dirWeight)`.
9. `confidenceLabel = confidence ≥ 75 ? 'High' : ≥ 50 ? 'Moderate' : ≥ 30 ? 'Low' : 'Mixed'`.
10. **Pin strength (0–100, PIN regime only; 0 otherwise or when strikes/spot missing):**
    - `tot = Σ|netGex_s| || 1`; shares `sh_s = |netGex_s|/tot`; `hhi = Σ sh_s²`; `top` = strike with max `|netGex|` (first-seen wins ties; initialized to `strikes[0]`).
    - `prox = exp(−((spot − top.strike)/(spot·0.004))²)` — NOTE plain `z²`, not `z²/2`; scale 0.4% of spot.
    - `pinStrength = clamp(round(100·√hhi·prox), 0, 100)`.
11. **Battle plan.** `tiny = spot·0.0006` (~0.06% dead-zone).
    - `biasDir === 0`: no target/stop; entry = "Trade the break of γ-flip"; not marked noTrade.
    - PIN: `target = (magnet && biasDir·(magnet − spot) > tiny) ? magnet : (biasDir > 0 ? cw : pw)`; `stop = biasDir > 0 ? pw : cw`. (Fade toward the magnet, stopped at the far wall.)
    - TREND: `target = biasDir > 0 ? cw : pw`; `stop = flip`. (Trade with the amplification, flip is the line in the sand.)
    - Coherence enforcement (only when `biasDir ≠ 0`): require `target` a number with `biasDir·(target − spot) > tiny` AND `stop` a number with `biasDir·(spot − stop) > tiny`; otherwise `noTrade = true`, target/stop cleared.
12. **Position strength (0–100):** `regimeClarity = regime === 'PIN' ? pinStrength : min(100, |score|·1.1)`;
    `positionStrength = round(0.45·|score| + 0.35·confidence + 0.20·regimeClarity)`; if `noTrade || biasDir === 0` then `positionStrength = floor(positionStrength/2)`; clamp to [0, 100].
13. **Events** (narrative, threshold-gated): above/below flip (gate `flip && spot`); "Pressing Call Wall" when `|pct(cw)| < 0.35` (0.35%); "Testing Put Wall" when `|pct(pw)| < 0.35`; "Pinned to magnet" when `|pct(magnet)| < 0.12` else "Magnet drawing price"; net-gamma line (always); implied day range when `emPct` present: `±(emPct·100)%` and levels `spot·(1 ∓ emPct)`.

**Constants.**

| value | where used | semantic meaning | proposed name |
|---|---|---|---|
| 4 | momentum window gate | min closes | MOMENTUM_MIN_CLOSES |
| 1.0005 / 0.9995 | momentum | ±0.05% dead-band | MOMENTUM_BAND_UP/DOWN |
| 28 | flip weight | strongest signal | W_FLIP |
| 24 / 8 | magnet weight PIN/TREND | regime-dependent | W_MAGNET_PIN, W_MAGNET_TREND |
| 0.08 | magnet dead-zone | 0.08% ⇒ "pinned" | MAGNET_PIN_BAND_PCT |
| 16 | wall weight | cage position | W_WALL |
| 0.72 / 0.28 | wall rel thresholds | top/bottom 28% of cage | WALL_REL_HIGH/LOW |
| 18 | positioning weight | OI skew | W_POSITIONING |
| 55 / 45 | call-OI % thresholds | call-/put-heavy | OI_SKEW_HIGH/LOW_PCT |
| 12 / 24 | momentum weight PIN/TREND | contra vs with-trend | W_MOM_PIN, W_MOM_TREND |
| 18 | bias threshold | ±18 score ⇒ directional | BIAS_SCORE_THRESH |
| 40 | neutral confidence cap | max confidence when neutral | NEUTRAL_CONF_CAP |
| 75 / 50 / 30 | confidence labels | High/Moderate/Low cuts | CONF_HIGH/MOD/LOW |
| 0.004 | pin proximity scale | 0.4%-of-spot Gaussian scale | PIN_PROX_SCALE_PCT |
| 0.0006 | tiny | bracket dead-zone (~0.06%) | BRACKET_DEADZONE_PCT |
| 0.45 / 0.35 / 0.20 | positionStrength blend | score/confidence/clarity | PS_W_SCORE/CONF/CLARITY |
| 1.1 | TREND clarity gain | \|score\|·1.1 clarity | TREND_CLARITY_GAIN |
| 2 | no-trade halving | PS penalty divisor | PS_NOTRADE_DIVISOR |
| 0.35 / 0.12 | event thresholds (%) | wall-press / magnet-pin proximity | EVENT_WALL_PCT, EVENT_MAGNET_PCT |

**Outputs.** `TerminalRead { bias; score∈[−100,100]; confidence∈[0,100]; confidenceLabel; regime; regimeLabel; pinStrength∈[0,100]; positionStrength∈[0,100]; signals[]; play; entry; target?; stop?; noTrade; netVex?; events[] }`.

---

### E8 — GEX Outlook (`computeGexOutlook`)

**Purpose.** Descriptive regime/path classifier — "what is the dealer book likely to make price do" — with a target level and confidence. A read, not a trade.

**Inputs.** Same `profile` + `recentCloses` as E7.

**Algorithm.**
1. NEUTRAL early-out when `!spot || (!profile.netGex && !(profile.strikes||[]).length)` ⇒ `{regime:'NEUTRAL', bias:'sideways', confidence:10}` (note falsy test: `netGex === 0` counts as missing).
2. `mom` exactly as E7 step 4. `reversalUp` (short-squeeze tell): needs ≥4 closes; `lo = min(closes)`, `loIdx = indexOf(lo)`; true iff `loIdx < len−1 && last > lo·1.0005` (turned up off the window low).
3. OI skew: `callPct = oiTot > 0 ? 100·callOi/oiTot : 50`; `callHeavy = callPct ≥ 55`; `putHeavy = callPct ≤ 45`.
4. Dominant strike & HHI: over `profile.strikes` (if any): `tot = Σ|netGex| || 1`; `hhi = Σ (|netGex|/tot)²`; `domStrike` = strike of max `|netGex|`. `pinTarget = magnet ?? domStrike`.
5. `pinStrength` (0 unless `longGamma && strikes.length && domStrike != null`): `prox = exp(−((spot − domStrike)/(spot·0.004))²)`; `clamp(round(100·√hhi·prox), 0, 100)`. (Same mechanic as E7 but proximity is to `domStrike`, while the *pin classification distance* below uses `pinTarget` = magnet when present.)
6. Distances in percent: `dPin = pct(pinTarget)`, `dCw = pct(cw)`, `dPw = pct(pw)`; `aboveFlip = flip != null ? spot ≥ flip : null`.
7. **Classification (strict priority order; first match returns):**
   1. **PINNING**: `longGamma && pinTarget != null && dPin != null && |dPin| ≤ 0.20 && pinStrength ≥ 40`. `confidence = clamp(round(40 + pinStrength·0.55), 45, 96)`; bias sideways; target = pinTarget.
   2. **GAMMA SQUEEZE**: `longGamma && cw != null && aboveFlip === true && dCw != null && 0 < dCw ≤ 0.6 && callHeavy && mom ≥ 0`. `proxW = 1 − dCw/0.6`; `confidence = clamp(round(52 + proxW·30 + (mom > 0 ? 8 : 0)), 50, 92)`; bias up; target = cw.
   3. **SHORT SQUEEZE**: `!longGamma && putHeavy && (reversalUp || mom > 0)`. `tgt = flip ?? cw`; `confidence = clamp(round(54 + (reversalUp ? 16 : 6) + (putHeavy ? 8 : 0)), 48, 90)` (putHeavy is always true here, so effectively 78 or 68); bias up.
   4. **TREND DOWN**: `!longGamma && aboveFlip === false && mom < 0`. `tgt = pw ?? flip`; `confidence = clamp(round(56 + (putHeavy ? 10 : 0) + (dPw != null && dPw < 0 ? 6 : 0)), 45, 88)`; bias down.
   5. **TREND UP**: `!longGamma && aboveFlip === true && mom > 0`. `tgt = cw ?? flip`; `confidence = clamp(round(54 + (callHeavy ? 10 : 0)), 45, 86)`; bias up.
   6. **RANGE**: `longGamma && cw != null && pw != null && cw > pw && pw < spot < cw`. `tgt = magnet`; `confidence = clamp(round(48 + pinStrength·0.2), 40, 78)`; bias sideways.
   7. Fallback long gamma ⇒ RANGE, target = pinTarget, confidence 35, bias sideways.
   8. Fallback short gamma ⇒ `bias = aboveFlip === false ? 'down' : aboveFlip === true ? 'up' : 'sideways'`; `tgt = below-flip ? (pw ?? flip) : (cw ?? flip)`; regime `TREND DOWN` / `TREND UP` / `NEUTRAL`; confidence 30; target undefined when sideways.

**Constants.**

| value | where used | semantic meaning | proposed name |
|---|---|---|---|
| 10 | NEUTRAL early-out conf | insufficient data | OUTLOOK_CONF_NODATA |
| 1.0005 | reversalUp | +0.05% off the low | REVERSAL_UP_BAND |
| 55 / 45 | callPct thresholds | call-/put-heavy | OI_SKEW_HIGH/LOW_PCT |
| 0.004 | pin prox scale | 0.4% of spot | PIN_PROX_SCALE_PCT |
| 0.20 | PINNING distance | ≤0.20% from pin target | PINNING_DIST_PCT |
| 40 | PINNING pinStrength gate | min concentration | PINNING_MIN_STRENGTH |
| 40, 0.55, 45, 96 | PINNING confidence | base, slope, clamp | PINNING_CONF_* |
| 0.6 | squeeze wall distance | ≤0.6% below the call wall | SQUEEZE_WALL_DIST_PCT |
| 52, 30, 8, 50, 92 | GAMMA SQUEEZE confidence | base, prox gain, mom bonus, clamp | GSQ_CONF_* |
| 54, 16, 6, 8, 48, 90 | SHORT SQUEEZE confidence | base, reversal/mom bonus, putHeavy bonus, clamp | SSQ_CONF_* |
| 56, 10, 6, 45, 88 | TREND DOWN confidence | base, putHeavy, below-wall bonus, clamp | TDN_CONF_* |
| 54, 10, 45, 86 | TREND UP confidence | base, callHeavy, clamp | TUP_CONF_* |
| 48, 0.2, 40, 78 | RANGE confidence | base, pinStrength slope, clamp | RANGE_CONF_* |
| 35 / 30 | fallback confidences | soft long-/short-gamma reads | OUTLOOK_CONF_FALLBACK_LONG/SHORT |

**Outputs.** `GexOutlook { regime ∈ {PINNING, GAMMA SQUEEZE, SHORT SQUEEZE, TREND UP, TREND DOWN, RANGE, NEUTRAL}; bias ∈ {up, down, sideways}; headline; detail; target?(price); confidence∈[0,100] }`.

---

### E9 — Trade Plan Composite (`buildTradePlan`)

**Purpose.** Blend technical (40%) / dealer (30%) / contract (20%) / learning (10%) into a directional plan: direction, confidence, target ladder, entry/stop/TPs, expected hold. Only the dealer-layer math is normative for this cluster; technical inputs are opaque scores from the technical engine.

**Inputs.** `ticker`, `spot($)`, `step` (strike increment, $), `emPts` (expected move, price points), `hoursToClose`, `regimeState` (string), `technical: {direction∈[−1,1], score∈[0,100], emaTargets{ema8,ema21,ema50,ema200}, emaAlignment, rsi, squeeze, vwapPosition}`, `dealer: {netGex, gammaFlip, callWall, putWall}`, `contractScore∈[0,100]`, `winRate∈[0,100]`, `loadedStrike|null`, `liquidityHigh|null`, `liquidityLow|null`.

**Algorithm.**
1. `em = emPts > 0 ? emPts : max(spot·0.0005, spot·0.004)` (the max always yields `spot·0.004`; see D7).
2. **Dealer direction:** `dealerDir = dealer.gammaFlip > 0 ? tanh((spot − gammaFlip)/em) : 0`.
3. `directionalScore = clamp(0.6·clamp(technical.direction, −1, 1) + 0.4·dealerDir, −1, 1)`.
4. `direction = directionalScore > 0.15 ? 'BULLISH' : < −0.15 ? 'BEARISH' : 'NEUTRAL'`; `isCall = direction !== 'BEARISH'` (NEUTRAL ⇒ call, see D8); `dir = direction === 'BEARISH' ? −1 : 1`.
5. **Dealer score (0–100):**
   - `flipAlign = sign(dealerDir) === sign(directionalScore || dir) ? |dealerDir| : −|dealerDir|` (when `directionalScore` is 0, `dir` substitutes)
   - `roomToWall = dir > 0 ? clamp((callWall − spot)/(2·em), 0, 1) : clamp((spot − putWall)/(2·em), 0, 1)`
   - `dealerScore = clamp(50 + 40·flipAlign + 20·(roomToWall − 0.5)·2, 0, 100)` — i.e. 50 base ± 40 flip alignment ± 20 wall room (roomToWall recentered so 0.5 ⇒ 0).
6. **Composite:** each engine score rounded & clamped [0,100]; `composite = round(0.40·technical + 0.30·dealer + 0.20·contract + 0.10·learning)`; `confidence = clamp(composite, 5, 97)`.
7. **Target ladder (`buildTargets`):** candidates in fixed label order [EMA Projection = nearest EMA of {8,21,50,200} strictly ahead of spot in `dir` (min above for longs, max below for shorts); Liquidity Sweep = `dir > 0 ? liquidityHigh : liquidityLow`; Loaded Strike; GEX Wall = `dir > 0 ? callWall : putWall`]; keep finite candidates strictly ahead of spot; sort by proximity in trade direction (ascending price for longs, descending for shorts); dedupe any candidate within `dedupeGap = max(step·0.3, spot·0.0008)` of an already-kept one; price rounded to 2 dp; `distancePct = (price − spot)/spot`.
8. TPs & stop: `emTp1 = spot + dir·0.5·em`; `emTp2 = spot + dir·1.0·em`; `tp1 = targets[0]?.price ?? round2(emTp1)`; `tp2Fallback = dir > 0 ? max(tp1 + 0.5·em, emTp2) : min(tp1 − 0.5·em, emTp2)`; `tp2 = targets[1]?.price ?? round2(tp2Fallback)`; `stop = round2(spot − dir·0.5·em)`.
9. Entry zone: `entryHalf = max(step·0.15, 0.1·em)`; `entryZone = [spot − entryHalf, spot + entryHalf]` (2 dp).
10. Contract: `atmStrike = round(spot/step)·step`; `targetStrike = isCall ? atmStrike + step : atmStrike − step` (one step OTM); `decimals = step ≥ 50 ? 0 : 2`.
11. **Expected hold (minutes):** `regimeSpeed = /EXPANSION|TAIL/ ? 1.5 : /MEAN_REVERSION/ ? 0.7 : 1.0` (regex on uppercased regimeState); `reachUnits = em > 0 ? max(|tp1 − spot|/em, 0.25) : 1` (floor BEFORE squaring so a TP1 on spot doesn't collapse the hold); `reachFrac = reachUnits²`; `expectedHoldMin = clamp(round(reachFrac·hoursToClose·60/regimeSpeed), 3, max(5, hoursToClose·60))`.
12. `dealerFlow = netGex ≥ 0 ? 'Positive Gamma' : 'Negative Gamma'`; `flowConfirmation = sign(dealerDir) === sign(technical.direction) && |technical.direction| > 0.15`.

**Constants.**

| value | where used | semantic meaning | proposed name |
|---|---|---|---|
| 0.0005 / 0.004 | em fallback | dead lower bound / effective fallback EM (0.4% of spot) | EM_FALLBACK_PCT (see D7) |
| 0.6 / 0.4 | direction blend | technical vs dealer-flip weights | DIR_W_TECH, DIR_W_DEALER |
| 0.15 | direction threshold | ±0.15 ⇒ directional | DIR_SCORE_THRESH |
| 50 / 40 / 20 | dealerScore | base / flip-align gain / wall-room gain | DEALER_SCORE_BASE/FLIP_GAIN/ROOM_GAIN |
| 2 | roomToWall denominator | 2 EMs = full room | ROOM_FULL_SCALE_EM |
| 0.40/0.30/0.20/0.10 | composite | tech/dealer/contract/learning | COMPOSITE_W_* |
| 5 / 97 | confidence clamp | never certain either way | CONF_MIN/MAX |
| 0.3 / 0.0008 | dedupeGap | 0.3 strike steps or 0.08% of spot | TARGET_DEDUPE_* |
| 0.5 / 1.0 | EM TP multiples | fallback TP1/TP2 | TP1_EM_MULT, TP2_EM_MULT |
| 0.5 | stop EM multiple | stop distance | STOP_EM_MULT |
| 0.15 / 0.1 | entryHalf | 0.15 steps or 0.1 EM | ENTRY_HALF_* |
| 50 | decimals cutoff | index-style formatting | DECIMALS_STEP_CUTOFF |
| 1.5 / 0.7 / 1.0 | regimeSpeed | expansion / mean-reversion / balanced | REGIME_SPEED_* |
| 0.25 | reachUnits floor | quarter-EM min diffusion distance | REACH_UNITS_FLOOR |
| 60 | hours→min | minutes per hour | MIN_PER_HOUR |
| 3 / 5 | hold clamps | min hold 3 min; upper floor 5 min | HOLD_MIN/HOLD_CAP_FLOOR |

**Outputs.** `TradePlan { direction; confidence∈[5,97]; contract string; targetStrike; isCall; entryZone; stop; tp1; tp2; targets[]; expectedHoldMin∈[3, max(5, session·60)]; dealerFlow; flowConfirmation; trendRegime; winRate; directionalScore∈[−1,1] (3 dp); engineScores; technical echo; rationale strings }`.

---

### E10 — Wall Zone-State: HOLDING / TESTING / FAILING (`DealerFlowView.tsx`)

**Purpose.** The ternary status label the UI attaches to the call-wall and put-wall strikes of the exposure profile (per exposure tab GEX/DEX/VEX). This is the scoring the rebuild collapses to a continuous score + binary ACTIVE/INACTIVE. It is currently computed inline in the render (lines 148–213), NOT in a lib module — `dealerSignals.ts` only references it in a comment.

**Inputs.** `profile.strikes` (per-strike `callGex/putGex/netGex`, `callDex/putDex/netDex`, `callVex/putVex/netVex`, OI, volume), `profile.spot`, `profile.gammaFlip`, exposure `type ∈ {'gex','dex','vex'}`.

**Algorithm.**
1. Map each strike to `{callValue, putValue, netValue}` for the selected exposure type (`dex`/`vex` fields default 0 when absent).
2. **Display window:** if more than 21 strikes, sort ascending, find `centerIdx` = index of the strike nearest spot (strict `<` on `|strike − spot|`, first wins), then keep `rows = sorted.slice(max(0, centerIdx − 10), max(0, centerIdx − 10) + 21)` — at most 21 strikes roughly centered on spot. **All subsequent maxima are taken within this window, not the full chain.**
3. Wall identification (within the window):
   - `maxCallValStrike` = strike maximizing `|callValue|` (reduce with strict `>`; first occurrence wins ties) — the call wall for this exposure type.
   - `maxPutValStrike` = strike maximizing `|putValue|` — the put wall.
4. **Zone state — call wall** (evaluated at `strike = maxCallValStrike`):
   - `isFailing = strike < profile.spot`
   - `isTesting = |strike − profile.spot| / profile.spot < 0.005`
   - `status = isFailing ? 'FAILING' : isTesting ? 'TESTING' : 'HOLDING'` — **FAILING takes precedence over TESTING.**
5. **Zone state — put wall** (mirror): `isFailing = strike > profile.spot`; same `isTesting`; same precedence.
6. Equivalent closed form on the signed relative distance `dRel = (strike − spot)/spot`:
   - Call wall: `dRel < 0 ⇒ FAILING`; `0 ≤ dRel < 0.005 ⇒ TESTING`; `dRel ≥ 0.005 ⇒ HOLDING`.
   - Put wall: `dRel > 0 ⇒ FAILING`; `−0.005 < dRel ≤ 0 ⇒ TESTING`; `dRel ≤ −0.005 ⇒ HOLDING`.
   - Exactly at spot (`dRel = 0`): both walls read TESTING.
   Note the asymmetry: because FAILING is checked first, the TESTING band exists only on the non-breached side. A call wall 0.1% below spot is FAILING while 0.1% above is TESTING (see D11).
7. **Continuous quantity for the rebuild:** the state is a pure threshold of the signed, defended-side distance
   `m_call = (strike − spot)/spot` (positive = wall intact above price) and `m_put = (spot − strike)/spot` (positive = wall intact below price), with a single breakpoint at 0 (FAILING) and at `+0.005` (TESTING→HOLDING). A faithful continuous score is `score = m / 0.005` (≤ 0 ⇒ breached, ∈ (0,1) ⇒ testing, ≥ 1 ⇒ holding); binary resolution: ACTIVE ⟺ `m ≥ 0` (wall not breached), with the E2 wall-strength 0–100 (`computeWallStrength`) as the magnitude companion — that pairing (0–100 score + status label) is exactly what the legacy interface displays per the `dealerSignals.ts` comment.

**Constants.**

| value | where used | semantic meaning | proposed name |
|---|---|---|---|
| 0.005 | isTesting | ±0.5% of spot testing band | WALL_TESTING_BAND_PCT |
| 21 | display window | strikes rendered around spot | WALL_WINDOW_STRIKES |
| 10 | window half-width | centerIdx − 10 | WALL_WINDOW_HALF |
| 0.001 | isSpot row highlight | exact-spot row match (UI only) | (display only) |

**Outputs.** Per wall (call & put, per exposure type): `status ∈ {HOLDING, TESTING, FAILING}` (UI label only — never persisted or fed back into any engine; `WorkspaceWidgets.tsx` maps HOLDING→'up', FAILING→'down' for chip coloring).

---

### E11 — GEX Summary Prose (`buildGexSummary`)

**Purpose.** Deterministic 3–5-sentence narrative from already-computed fields; no math beyond gates and formatting. Included only for its numeric gates.

**Gates (in emission order).** (1) `netGex ≥ 0` ⇒ long-gamma sentence, else short-gamma. (2) flip sentence when `gammaFlip > 0 && spot > 0`, branching on `spot ≥ gammaFlip`. (3) call-wall clause when `callWall > 0`; put-wall clause when `putWall > 0`. (4) magnet sentence when `magnet > 0`. (5) non-neutral vanna hedgeFlow / charm bias / migration direction clauses from E2 outputs. Formatting: B/M/K scaling at 1e9/1e6/1e3 with 2/0/0 dp.

**Constants.** 1e9/1e6/1e3 formatting cutoffs (FMT_BILLION/MILLION/THOUSAND) — display only.

**Outputs.** A string. Port only if the rebuild wants the same canned narrative; all inputs come from E2/upstream.

---

## State semantics

Every enum this cluster produces, its exact trigger condition, and the continuous quantity underneath (for the binary ACTIVE/INACTIVE + score rebuild). "cq" = continuous quantity.

| Enum | Values | Exact condition | cq to preserve |
|---|---|---|---|
| **Wall zone-state (E10)** — call wall | FAILING / TESTING / HOLDING | `strike < spot` ⇒ FAILING; else `\|strike−spot\|/spot < 0.005` ⇒ TESTING; else HOLDING | signed defended-side margin `m_call = (strike−spot)/spot`; breakpoints 0 and +0.005; suggested score `m/0.005`, ACTIVE ⟺ m ≥ 0 |
| **Wall zone-state (E10)** — put wall | FAILING / TESTING / HOLDING | `strike > spot` ⇒ FAILING; else same 0.005 band ⇒ TESTING; else HOLDING | `m_put = (spot−strike)/spot`; same breakpoints |
| VannaEngine.trend (E2) | RISING / FALLING / FLAT | FLAT iff `\|Δvanna\| < max(1e−9, 0.02·\|netVanna\|)`; else sign of Δvanna | `vannaVel / max(1e−9, 0.02·\|netVanna\|)` |
| VannaEngine.hedgeFlow (E2) | SUPPORTIVE / PRESSURING / NEUTRAL | NEUTRAL iff `\|netVanna\| < 1e−9`; else sign(netVanna): + SUPPORTIVE, − PRESSURING | `netVanna` ($) |
| CharmEngine.bias (E2) | BULLISH / BEARISH / NEUTRAL | NEUTRAL iff `\|netCharm\| < 1e−9`; else sign(netCharm) | `netCharm` ($/day); intensity `min(1, \|netCharm\|/priorMax20)` |
| MigrationEngine.direction (E2) | BULLISH / BEARISH / STABLE | STABLE iff `\|migScore\| < 0.05`; else sign | `migScore = clamp(ΔCoM/(0.01·spot), −1, 1)` |
| GammaDynamics.state (E2) | ADDING_HEDGES / REMOVING_HEDGES / STABLE | STABLE iff `\|ΔnetGex\| < max(1e6, 0.03·\|netGex\|)`; else sign | `gVel` ($/tick), plus `gAcc` |
| OiFlow.state (E2) | BUILDING / UNWINDING / STABLE | STABLE iff `\|oiVel\| < max(1, 0.005·totalOi)`; else sign | `oiVel` (contracts/min) |
| VacuumZone.side (E2) | above / below | `hi ≤ spot` ⇒ below; `lo ≥ spot` ⇒ above; straddle: above iff `\|hi−spot\| < \|spot−lo\|` | zone edges vs spot |
| HedgeNode.regime / regimeNow (E3) | stabilizing / amplifying | `Γ$ ≥ 0` ⇒ stabilizing else amplifying | `Γ$(S)` ($/1%) |
| GravityStrike.side (E5) | support / resistance / atm | atm iff `\|distancePct\| < 0.001`; else sign of (strike−spot) | `distancePct` |
| GravityZone.side (E5) | support / resistance / straddle | `hi < spot` / `lo > spot` / otherwise | zone edges vs spot |
| Abstain flags (E4) | confident true/false | passthrough of upstream `wallsConfident` / `gammaFlipConfident` (false = fabricated fallback) | n/a (data-quality bit) |
| GateDecision modulation (E4) | BUY→WAIT veto | `enabled && decision==='BUY' && trapConfident && trap ≥ 70 && flipConfident && prox ≥ 0.7` | trap score (0–100), flip proximity (0–1), stress (0–100 → sizeMultiplier) |
| TerminalRead.bias (E7) | LONG / SHORT / NEUTRAL | `score > 18` / `score < −18` / else | `score = Σ dir·weight ∈ [−100,100]` |
| TerminalRead.regime (E7) | PIN / TREND | `netGex ≥ 0` / `< 0` | `netGex` ($); pinStrength (0–100) as the PIN-clarity cq |
| TerminalRead.confidenceLabel (E7) | High / Moderate / Low / Mixed | `≥75 / ≥50 / ≥30 / else` on confidence | confidence (0–100) |
| TerminalRead.noTrade (E7) | true/false | directional bias but bracket incoherent: `biasDir·(target−spot) ≤ tiny` or `biasDir·(spot−stop) ≤ tiny`, `tiny = 0.0006·spot` | signed target/stop margins |
| Terminal signal dirs (E7) | −1 / 0 / +1 per signal | thresholds per E7 step 5 (0.08% magnet band; 0.72/0.28 wall rel; 55/45 OI %; ±0.05% momentum) | underlying: pct distances, `rel`, `bull%`, window return |
| GexOutlook.regime (E8) | PINNING / GAMMA SQUEEZE / SHORT SQUEEZE / TREND UP / TREND DOWN / RANGE / NEUTRAL | priority-ordered gates in E8 step 7 | per-branch: `\|dPin\|` vs 0.20 & pinStrength vs 40; `dCw` vs 0.6; mom/reversal; flip side |
| GexOutlook.bias (E8) | up / down / sideways | fixed per regime branch | confidence (0–100) |
| TradePlan.direction (E9) | BULLISH / BEARISH / NEUTRAL | `directionalScore > 0.15` / `< −0.15` / else | `directionalScore ∈ [−1,1]` |
| TradePlan.dealerFlow (E9) | Positive Gamma / Negative Gamma | `netGex ≥ 0` / `< 0` | netGex |
| TradePlan.flowConfirmation (E9) | true/false | `sign(dealerDir) === sign(tech.direction) && \|tech.direction\| > 0.15` | dealerDir = tanh((spot−flip)/em) |

Rebuild guidance for the headline ternary (E10): ACTIVE ⟺ wall not breached on its defended side (`m ≥ 0`); continuous score = `clamp(m/0.005, …)` (or unclamped signed margin) so TESTING is recoverable as `0 ≤ score < 1`; pair with the E2 wall-strength 0–100 for magnitude. Preserve the legacy asymmetry (or explicitly fix it per D11) — as coded, there is no TESTING band on the breached side.

## Data dependencies

Upstream inputs (produced OUTSIDE this cluster):
- **Option chain** (`ChainContract[]`: strike, type, OI, per-contract greeks incl. vanna/charm/vega, volume) — feeds E1, E2, E4 (convexity).
- **Dealer inventory** (`v11Math.computeDealerInventory`, separate cluster): `netGex/netVanna/netCharm`, `grossGex`, `callWall/putWall`, `gammaFlipPrice`, `gammaFlipConfident/wallsConfident`, `gex_profile.strikes` (`GexStrikeDetail`: signed netGex + call/put GEX/DEX/VEX/OI/volume per strike), `magnet`, `expectedMovePct`, `totalCallOi/totalPutOi` — feeds E3, E4, E5, E6, E7, E8, E9, E10, E11.
- **Spot price** — every engine.
- **Expected move** (`emPct` fraction or `emPts` points; convention `expectedMovePct(σ, τ) = max(0.0005, σ·√max(τ, 0.0001))` from skyQuantCore) — E3, E4, E9.
- **Recent 1-min closes** (assumed **oldest→newest**; momentum sign inverts if the caller passes newest-first) — E7, E8.
- **ATM IV, hours to close** (session = 6.5h RTH) — E6, E9.
- **Technical engine read** (direction, score, EMA targets, RSI, squeeze, VWAP), **contract score**, **learning win-rate**, **regime state string**, **liquidity high/low**, **loaded strike** — E9 only.
- **volExpansionNorm** [0,1] microstructure score — E4 stress.
- **Rolling `DealerSnapshot[]` history** — persisted by the caller across ticks; E2 both reads and mutates it (append + trim 180).

Intra-cluster order: E1/E2 need only chain+spot(+inventory+history). E3, E5, E6 need `gex_profile.strikes` (upstream). E4 needs inventory + E3-style flip/walls + volExpansion. E7/E8 need the assembled `GexProfileData` (+closes). E9 needs dealer levels + technical + E6-style EM/hours. E10 needs `profile.strikes` + spot. E11 needs E2 outputs + profile levels. No engine in this cluster feeds another engine in this cluster except E2→E11 (dynamics enums into prose) and conceptually E2's wall strength pairing with E10's labels in the UI.

## Suspected defects

- **D1 — E2 gamma-velocity absolute floor `1e6` is scale-dependent.** For any underlying whose net GEX never reaches ~$33M (`1e6/0.03`), `|gVel|` can never beat the $1M floor and `state` is permanently STABLE. As-coded: `max(1e6, 0.03·|netGex|)`. Suspected correct: per-asset or purely relative threshold.
- **D2 — E2 OI-velocity wall-clock gaps.** `dtMin` uses `Date.now()` deltas; across halts/overnight the divisor balloons and velocity → ~0, masking real unwind at the open. No timezone logic anywhere in the cluster (all epoch-ms), so no TZ bug per se, but session boundaries are not handled.
- **D3 — E2 vacuum straddling-zone side looks inverted.** For `lo < spot < hi`, side = 'above' iff `|hi − spot| < |spot − lo|`, i.e. the zone is labeled by its NEARER edge, which is the side holding the SMALLER share of the zone. A vacuum mostly below spot gets labeled 'above'. Suspected intent: label by the side holding the bulk of the zone (or by nearer edge deliberately — undocumented either way).
- **D4 — E2 vacuum run anchoring asymmetry.** Interior runs are flushed with `hi` = the terminating LIQUID strike (comment claims anchoring "between the last loaded strike and this one", yet `lo` is the first EMPTY strike, not the preceding loaded one), while an end-of-array run uses `hi` = the last (empty) strike. Zone edges are thus inconsistently defined at the two boundaries; single-strike runs terminated at the end of the array yield `hi === lo` and are silently dropped.
- **D5 — E3 hedge-integral denominator asymmetry.** Upward step divides the price delta by `price_i` (the UPPER node of the pair); downward step also divides by `price_i`, which is the LOWER node of its pair. The per-1% conversion is therefore biased slightly differently on each side of spot. Suspected correct: midpoint price (or consistent endpoint) in both directions.
- **D6 — E6 `probabilityOfTouch` NaN for tiny `iv`.** `expTerm = exp(2νm/iv²)` overflows to `Infinity` for small-but-positive `iv` (guard only rejects `iv ≤ 0`); `Infinity · N(large negative)` can produce `Infinity·0 = NaN`, and `clamp` propagates NaN (`min(1, NaN) = NaN`). Suspected fix: compute in log space or clamp the exponent.
- **D7 — E9 EM fallback dead branch.** `em = emPts > 0 ? emPts : Math.max(spot·0.0005, spot·0.004)` — the max of the two is always `spot·0.004`; the `0.0005` term is dead code (likely a leftover from an intended `min`/different floor pairing). As-coded behavior: fallback EM is always 0.4% of spot.
- **D8 — E9 NEUTRAL direction still emits a CALL contract.** `isCall = direction !== 'BEARISH'` maps NEUTRAL to a bullish contract with `dir = +1` targets/stops. Suspected intent: abstain or symmetric handling on NEUTRAL.
- **D9 — E6 pin uses the upstream `magnet` param, not the freshly computed `eodMagnet`.** `compute0DTE` computes `eodMagnet` (positive-gamma CoM) but feeds `pinProbability` and the gamma-share band the caller-supplied `magnet`. If the two diverge the reported pin probability and the reported magnet target refer to different levels.
- **D10 — E7/E8 falsy-zero guards on prices and netGex.** `if (flip && spot)`, `pct(lvl) = (lvl && spot) ? …`, `magnet &&`, and E8's NEUTRAL gate `!profile.netGex` all treat legitimate `0` values as "absent". Harmless for equity prices, but `netGex === 0` (exactly balanced book) is misclassified as missing data in E8's early-out when strikes are also empty, and any 0-priced level is silently dropped.
- **D11 — E10 asymmetric TESTING band (the headline zone-state quirk).** Because FAILING is evaluated before TESTING, the ±0.5% testing band only exists on the unbreached side: call wall 0.1% ABOVE spot ⇒ TESTING, 0.1% BELOW ⇒ FAILING. The label is discontinuous at `strike = spot` (jump from TESTING to FAILING for an infinitesimal crossing). Suspected intent: symmetric ±0.5% TESTING band with FAILING only beyond it. The rebuild must decide explicitly; the as-coded form is specified in E10 step 6.
- **D12 — E10 walls depend on the render window and exposure tab.** The call/put wall strikes are the max-|value| strikes WITHIN the ≤21-strike display window and for the CURRENTLY SELECTED exposure type (GEX/DEX/VEX) — they can disagree with `profile.callWall/putWall` computed upstream on the full chain, and the HOLDING/TESTING/FAILING labels change when the user switches tabs. Presentation logic leaking into signal semantics.
- **D13 — E4 convexity speed scale is nominal.** `S³·0.0001` doesn't correspond to a per-(1%)³ unit (that would be `1e−6`); irrelevant to output because each component is max-normalized, but the constant is misleading and must not be "fixed" independently of the normalization.
- **D14 — E4 trap-score $1 floor.** `wallMagShare = wallMag/max(grossGex, 1)` uses a $1 floor on a multi-billion-dollar quantity; with a degenerate near-zero book the share explodes and the score saturates at 100 (the abstain flag usually, but not always, shields this).
- **D15 — E7 vs E1/E3 flip-convention mismatch (port hazard, not a bug per se).** Three different "flip" definitions coexist: E1 = first cumulative-sum zero crossing by strike order; E3 = kernel-smoothed Γ$ zero crossing nearest spot; upstream inventory flip = its own solver. They will not agree numerically; the rebuild must pick one per surface deliberately.
- **D16 — E7/E8 pin-proximity Gaussian lacks the ½ factor.** `exp(−z²)` with `z = (spot−strike)/(0.004·spot)` vs E6's `exp(−0.5·z²)`; the two pin mechanics decay at different rates for the same nominal scale. As-coded both are specified; do not silently unify.
- **D17 — E2 charm-intensity ratchet.** Intensity normalizes by the max |charm| of the last 20 snapshots; after one extreme print, intensity is suppressed (≤ its ratio to the spike) for up to 20 ticks — by design per the comment, but it makes 1.0 readings rare and regime-dependent.

## Discarded

- `gexSummary.ts` sentence templates, `fmtPrice/fmtGexDollars/cap` — display prose/formatting (gates kept in E11).
- `greekExposure.ts` `GREEK_META` / `GREEK_ORDER` — UI labels, chips, formula strings for panels.
- `dealerDynamics.ts` `note` strings on vanna/charm engines — narrative copy.
- `dealerSignals.ts` veto `reason` string interpolation — log copy.
- `terminalRead.ts` `r0/fmtGex` formatting, `play`/`entry`/`detail`/`headline` sentence text, event strings — copy (numeric gates kept in E7/E8).
- `tradePlan.ts` `rationale` strings, `regimeLabel` display mapping, contract-string formatting — copy.
- `DealerFlowView.tsx` everything except lines 76–119 (row mapping/window) and 148–213 (wall id + zone state): React/Tailwind markup, tooltips, colors, spot-line pixel math (`23 + 12 + pct·(rows−1)·27`), lazy-loading of three.js/recharts, `FeedChip` — pure presentation.
- `WorkspaceWidgets.tsx` HOLDING/FAILING → up/down chip-color mapping — presentation.
- `toFixed` roundings throughout (hhi 4dp, shares 1dp, nbrs 2dp, prices 2dp, directionalScore 3dp) — display precision; the rebuild should keep full precision internally and round at the presentation edge.
