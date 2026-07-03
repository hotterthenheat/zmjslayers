# Quant Suite (statistical edge, info theory, scenario matrices, Monte Carlo, edge tracking)

Source files (legacy):
- `src/lib/quantSuite.ts` (BSM pricer/Greeks, Breeden–Litzenberger RND, RV suite wrappers, skew, strategy builder, shock matrix, portfolio Greeks, expiry GEX, charm/vanna clock, alerts, calibration loop)
- `src/lib/quantEdge.ts` (per-asset / per-contract edge orchestrator)
- `src/lib/infoTheory.ts` (transfer entropy, market leader, Fisher divergence)
- `src/lib/scenarioMatrix.ts` (single-contract shock grid)
- `src/lib/monteCarlo.ts` (GBM / Merton-jump / Heston path simulation)
- `src/lib/edgeTracker.ts` (forecast scoring + track record)

## Overview

This cluster is the terminal's quantitative core. It (a) prices options and Greeks under Black–Scholes–Merton; (b) extracts the market's risk-neutral density (RND) from the option chain via Breeden–Litzenberger and uses it to score multi-leg strategies (probability of profit, Kelly sizing, breakevens); (c) measures realized vol, variance risk premium, and skew; (d) reprices positions across deterministic spot×vol×time shock grids ("risk slides"); (e) simulates risk-neutral price paths under three SDEs with a seeded PRNG for VaR/ES; (f) detects cross-asset lead/lag with transfer entropy and regime shifts with Fisher divergence; and (g) closes the loop by scoring the terminal's own forecasts (Brier, ECE, hit-rate calibration) so displayed "edge" is a measured, falsifiable quantity.

Convention notes used throughout:
- All vols are **annualized decimals** (0.18 = 18%). "Vol points" = decimal × 100.
- Option contract multiplier is **100**.
- `t` in years unless a function explicitly takes `dteDays`.
- `N(x)` = standard normal CDF, `φ(x)` = standard normal PDF. Both delegate to a high-precision implementation (`normalDist.ts`, West/Hart, ~1e-15, exact N(x)+N(−x)=1). Do **not** use an Abramowitz–Stegun polynomial (the legacy code explicitly migrated away because ±1.5e-7 asymmetric tails biased put-call parity and the B-L second difference at deep OTM wings).

---

## Engines

### E1. Black–Scholes–Merton pricer — `bsmPrice`

Purpose: exact European option price; the primitive for the RND engine and every shock grid.

Inputs: `S` spot ($), `K` strike ($), `t` years, `sigma` annualized decimal, `optionType` 'call'|'put', `r` cont. rate (default 0.05), `q` cont. dividend yield (default 0.0).

Algorithm:
1. If `t <= 0` set `t = 1e-4`; if `sigma <= 0` set `sigma = 1e-3`.
2. `d1 = (ln(S/K) + (r − q + 0.5·σ²)·t) / (σ·√t)`; `d2 = d1 − σ·√t`.
3. Call: `S·e^{−q t}·N(d1) − K·e^{−r t}·N(d2)`. Put: `K·e^{−r t}·N(−d2) − S·e^{−q t}·N(−d1)`.

| value | where used | meaning | proposed name |
|---|---|---|---|
| 1e-4 | t floor | min time to expiry (yr) | `BSM_MIN_T_YEARS` |
| 1e-3 | sigma floor | min vol | `BSM_MIN_SIGMA` |
| 0.05 | default r | risk-free rate | `DEFAULT_RISK_FREE_RATE` |
| 0.0 | default q | dividend yield | `DEFAULT_DIV_YIELD` |

Output: price, $ per share (≥ 0).

### E2. Greeks — `calculateOptionGreeks`

Purpose: analytic first/second-order Greeks including vanna and charm for dealer-flow analytics.

Inputs: same as E1. Same t/σ floors, same d1/d2.

Algorithm (exact, per share; `nd1 = φ(d1)`, `Nd1 = N(d1)`):
1. `delta_call = e^{−qt}·Nd1`; `delta_put = e^{−qt}·(Nd1 − 1)`.
2. `gamma = e^{−qt}·nd1 / (S·σ·√t)`.
3. `vega = S·e^{−qt}·nd1·√t` — **per 1.00 (100 vol points) change**. The code comment claims division by 100 for per-1% units, but no division is performed (see D1).
4. `theta_call = [ −S·e^{−qt}·nd1·σ/(2√t) − r·K·e^{−rt}·N(d2) + q·S·e^{−qt}·N(d1) ] / 365` (daily $).
   `theta_put = [ −S·e^{−qt}·nd1·σ/(2√t) + r·K·e^{−rt}·N(−d2) − q·S·e^{−qt}·N(−d1) ] / 365`.
5. `vanna = −e^{−qt}·nd1·(d2/σ)`.
6. `charm_base = e^{−qt}·nd1·( (r−q)/(σ√t) − d2/(2t) )`;
   `charm_call = ( q·e^{−qt}·Nd1 − charm_base ) / 365`; `charm_put = ( −q·e^{−qt}·(1 − Nd1) − charm_base ) / 365` (daily delta decay).

| value | where used | meaning | proposed name |
|---|---|---|---|
| 365 | theta, charm | calendar-day divisor for daily Greeks | `DAYS_PER_YEAR_CAL` |

Outputs: `{delta [−1,1], gamma ≥0, vega (per 1.00 vol), theta ($/day), vanna, charm (delta/day)}`.

### E3. Breeden–Litzenberger implied RND — `solveImpliedRND`

Purpose: extract the market-implied terminal price distribution from the chain: RND `f(K) = e^{rt}·∂²C/∂K²`.

Inputs: `chain: ChainContract[]` (needs `strike`, `iv` decimal, `type`), `spot` ($), `ivBase` ATM IV decimal, `t` years (default 30/365), `r` (default 0.05).

Algorithm:
1. If `chain.length < 5` → return dummy RND (E3b).
2. **Dedup strikes**: build a Map keyed by strike; insert if strike absent **or** the contract is a call (calls overwrite puts at the same strike). Sort ascending by strike.
3. **Quadratic smile fit** `IV(x) = a + b·x + c·x²`, `x = ln(K/spot)`, by ordinary least squares. Accumulate `n, Σx, Σx², Σx³, Σx⁴, Σy, Σxy, Σx²y` over the deduped chain and solve the 3×3 normal equations by Cramer's rule (explicit `det3x3`). If `|det(M)| ≤ 1e-12` → degenerate fallback **flat smile**: `a = ivBase, b = 0, c = 0` (do NOT invent a skew; legacy previously hardcoded b=−0.15, c=0.55 and this was removed as fabrication).
4. `interpolateIV(k)`: evaluate the quadratic at `x = ln(k/spot)` and clamp to `[max(0.015, 0.25·ivBase), max(2.20, 5.0·ivBase)]`.
5. Call price `C(K) = bsmPrice(spot, K, t, interpolateIV(K), 'call', r, q=0)`.
6. **Grid**: `stdModel = spot·ivBase·√(t || 30/365)`; `minStrike = max(0.40·spot, spot − 3.2·stdModel)`; `maxStrike = spot + 3.2·stdModel`; 101 nodes: `K_i = minStrike + i·stepK`, `stepK = (maxStrike − minStrike)/100`, i = 0..100.
7. **Second difference**: `dK = max(0.5, 0.0025·spot)` (spot-scaled — a fixed $1 bump collapses into FP cancellation noise on high-priced underlyings).
   `density(K) = e^{r·t} · (C(K+dK) − 2·C(K) + C(K−dK)) / dK²`; clamp negative density to 0 (no-arbitrage).
8. If `Σ density ≤ 0` → dummy RND (E3b).
9. **Gaussian kernel smoothing**: bandwidth `h = 2.0·stepK`; for each node i, over neighbors j ∈ [i−10, i+10] (clamped): `u = (K_i − K_j)/h`, `w = e^{−0.5u²}`; `p̂_i = Σ w·p_j / Σ w`.
10. **Normalize** so Σp̂ = 1 (treat node probabilities as discrete masses); accumulate `cumulativeProb` as running sum.
11. **Moments** over the discrete masses: `mean = Σ K·p`; `var = Σ (K−mean)²·p`; `stdDev = √var`; `skewness = Σ ((K−mean)/stdDev)³·p`; `kurtosis = Σ ((K−mean)/stdDev)⁴·p − 3.0` (excess); `isFatTailed = kurtosis > 1.2`.
12. `probLessThanSpot = Σ p over K < spot`; `probGreaterThanSpot = Σ p over K ≥ spot`.

| value | where used | meaning | proposed name |
|---|---|---|---|
| 5 | step 1 | min contracts to attempt fit | `RND_MIN_CHAIN_SIZE` |
| 1e-12 | step 3 | determinant degeneracy epsilon | `RND_DET_EPS` |
| 0.015 | IV clamp lo (abs) | 1.5-vol-pt floor | `RND_IV_FLOOR_ABS` |
| 0.25 | IV clamp lo (rel) | ×ivBase floor | `RND_IV_FLOOR_REL` |
| 2.20 | IV clamp hi (abs) | 220-vol-pt cap | `RND_IV_CAP_ABS` |
| 5.0 | IV clamp hi (rel) | ×ivBase cap | `RND_IV_CAP_REL` |
| 30/365 | default t | default horizon | `RND_DEFAULT_T_YEARS` |
| 3.2 | grid | ± std devs of strike range | `RND_GRID_SIGMAS` |
| 0.40 | grid | min strike as fraction of spot | `RND_MIN_STRIKE_FRAC` |
| 100 | grid | mesh density (101 nodes) | `RND_MESH_DENSITY` |
| 0.5 | dK floor | min B-L bump ($) | `RND_MIN_DK` |
| 0.0025 | dK | B-L bump as fraction of spot | `RND_DK_SPOT_FRAC` |
| 2.0 | smoothing | bandwidth in grid steps | `RND_SMOOTH_BW_STEPS` |
| 10 | smoothing | kernel half-window (indices) | `RND_SMOOTH_HALF_WINDOW` |
| 3.0 | kurtosis | normal-kurtosis subtrahend | `EXCESS_KURTOSIS_OFFSET` |
| 1.2 | isFatTailed | excess-kurtosis threshold | `FAT_TAIL_KURTOSIS_THRESHOLD` |

Outputs: `density: {strike, probability (mass, Σ=1), cumulativeProb}[101]`, `mean` ($), `stdDev` ($), `skewness`, `kurtosis` (excess), `isFatTailed`, `probLessThanSpot`/`probGreaterThanSpot` ∈ [0,1].

### E3b. Dummy RND fallback — `generateDummyRND`

Purpose: placeholder skew-normal density when the chain is unusable. **Fabricated data — consider not porting** (see D12/Discarded).

Algorithm: `std = spot·ivBase·√(t || 1/12)`; grid 101 nodes over `[spot − 3.5·std, spot + 3.5·std]`. Skew-normal: `α = −3.5`, `ξ = spot + 0.8·std`, `ω = 1.2·std`; `z = (K−ξ)/ω`; `f = (2/ω)·φ(z)·N(α·z)` with `φ(z) = e^{−z²/2}/√(2π)`; normalize to Σ=1 and accumulate CDF. Returned moments are **hardcoded, not computed from the nodes**: `mean = spot − 0.15·std`, `stdDev = std`, `skewness = −0.95`, `kurtosis = 1.45`, `isFatTailed = true`, `probLessThanSpot = 0.58`, `probGreaterThanSpot = 0.42`.

| value | meaning | proposed name |
|---|---|---|
| 3.5 | grid ± std devs | `DUMMY_RND_GRID_SIGMAS` |
| 100 | steps (101 nodes) | `DUMMY_RND_STEPS` |
| −3.5 | skew-normal shape α | `DUMMY_RND_ALPHA` |
| 0.8 | location offset (×std) | `DUMMY_RND_XI_OFFSET` |
| 1.2 | scale (×std) | `DUMMY_RND_OMEGA_SCALE` |
| −0.15 | hardcoded mean offset (×std) | `DUMMY_RND_MEAN_OFFSET` |
| −0.95 / 1.45 / 0.58 / 0.42 | hardcoded skew / kurt / P(<spot) / P(>spot) | `DUMMY_RND_*` |
| 1/12 | t fallback (yr) | `DUMMY_RND_DEFAULT_T` |

### E4. Realized-vol suite — `calculateRealizedVolSuite` + `calculateVolatilityCone`

Purpose: Parkinson / Garman–Klass / Yang–Zhang realized vol, VRP spread, and an honest RV percentile from a rolling cone. Thin wrappers that **delegate** to `realizedVol.ts` (bar-interval-aware estimators — the earlier local copy hardcoded ×252 daily annualization against 5-min bars, understating RV ~8.8× ≈ √(390/5); the delegation is deliberate, keep it).

Inputs: `candles: {time, open, high, low, close, volume}[]`, `impliedVol` (ATM IV decimal), `lookback` bars (default 20).

Algorithm:
1. If `candles.length < 5` → fabricated fallback `{parkinson: 0.145, garmanKlass: 0.138, yangZhang: 0.142, varianceRiskPremium: impliedVol − 0.142, rvPercentile: 50}`.
2. Map candles: `timestamp = (typeof time === 'number') ? time : (Number(time) || 0)` (non-numeric → 0 → interval inference falls back to 5-minute bars in `intervalMinutes`).
3. `clampVol(v, fallback) = (isFinite(v) && v > 0) ? min(2.5, max(0.01, v)) : fallback`; apply to `parkinsonVol(mkt, lookback)` (fallback 0.145), `garmanKlassVol` (0.138), `yangZhangVol` (0.142).
4. `varianceRiskPremium = impliedVol − yangZhang` (decimal vol points; IV − RV).
5. `rvPercentile = volCone(mkt, [lookback])[0].percentile` if the cone has an entry, else **50** (abstain; there is no IV history so a VRP percentile is impossible — never fabricate one).

`calculateVolatilityCone(candles, _yzVol)`: `volCone(mkt, [5,10,20,30,45,60])` mapped to `{window, min, p25, p50: median, p75, max, current}` per window; windows without enough history are omitted. (The `_yzVol` arg is dead.)

| value | where used | meaning | proposed name |
|---|---|---|---|
| 5 | min candles | fallback threshold | `RV_MIN_CANDLES` |
| 0.145 / 0.138 / 0.142 | fallbacks | placeholder Parkinson/GK/YZ vols | `RV_FALLBACK_{PARKINSON,GK,YZ}` |
| 20 | default lookback | RV window (bars) | `RV_DEFAULT_LOOKBACK` |
| 0.01 / 2.5 | clampVol | vol clamp lo/hi | `RV_CLAMP_LO` / `RV_CLAMP_HI` |
| 50 | percentile abstain | neutral "no history" | `PERCENTILE_ABSTAIN` |
| [5,10,20,30,45,60] | cone | window set (bars) | `VOL_CONE_WINDOWS` |
| 5 (min) | intervalMinutes fallback | assumed bar interval when timestamp unusable | `DEFAULT_BAR_MINUTES` |

Outputs: vols annualized decimal ∈ [0.01, 2.5]; `varianceRiskPremium` decimal (can be negative); `rvPercentile` 0–100.

### E5. Skew analytics — `computeSkewAnalytics`

Purpose: 25-delta risk reversal / butterfly / ATM smile slope from the chain.

Inputs: `chain`, `spot`, `ivBase`.

Algorithm:
1. Split chain into calls and puts.
2. **ATM IV**: find the contract with strike nearest spot; `atmIV` = mean of `iv` over all contracts at that exact strike with `iv > 0`; if none, that contract's iv; if empty chain, `ivBase`.
3. `call25DIV = ivAtDelta(calls, 0.25) ?? (ivBase + 0.02)`; `put25DIV = ivAtDelta(puts, 0.25) ?? (ivBase + 0.04)`. (`ivAtDelta` — external, `skewAnalytics.ts` — brackets |Δ|=0.25 between the two straddling contracts and linearly interpolates; the old nearest-delta snap was removed as discontinuous.)
4. `riskReversal25D = call25DIV − put25DIV`.
5. `butterfly25D = (call25DIV + put25DIV)/2 − atmIV`.
6. `skewSlopeAtm = (put25DIV − call25DIV) / (spot · 0.1)` — labeled "estimated dVol/dK"; note sign convention (see D3).
7. `riskReversalPercentile = 50`, `butterflyPercentile = 50` — deliberate abstention stubs; a genuine rank needs the rolling history in E12 (`percentileRank` ring buffer). Never a fabricated waveform.

| value | where used | meaning | proposed name |
|---|---|---|---|
| 0.25 | wings | target abs delta | `WING_TARGET_DELTA` |
| 0.02 / 0.04 | fallbacks | call/put 25Δ IV offsets over ivBase | `FALLBACK_CALL25_IV_OFFSET` / `FALLBACK_PUT25_IV_OFFSET` |
| 0.1 | slope denom | assumed 25Δ strike spacing as fraction of spot | `SKEW_SLOPE_STRIKE_SPAN_FRAC` |
| 50 | percentile stubs | abstain | `PERCENTILE_ABSTAIN` |

Outputs: all decimal vol; RR/BF can be negative; percentiles fixed 50.

### E6. Multi-leg strategy builder — `buildStrategySuite` (+ `interpDensity`, `payoffTailTopology`, `generatePayoffCoordinates`)

Purpose: aggregate Greeks, net premium, max profit/loss, breakevens, RND-integrated probability of profit (POP), and half-Kelly sizing for a set of option legs.

Inputs: `legs: {strike, type 'call'|'put', action 'buy'|'sell', qty, iv decimal, entryPrice $/share}[]`, `spot`, `dte` days (default 30), `r` (default 0.05), `rnd` (E3 result).

Algorithm:
1. `t = dte/365`. For each leg, `dir = buy?+1:−1`; Greeks from E2 at `(spot, strike, t, iv, type, r)`; accumulate each Greek × `dir·qty` (NOT ×100 here — per-share Greek units); `netPremium += entryPrice·dir·qty·100` (debit +, credit −).
2. Payoff at terminal spot s: per leg `payout = max(0, s − K)` (call) or `max(0, K − s)` (put); `pnl += (payout − entryPrice)·dir·qty·100`.
3. **Max P/L scan** (spot-scaled, not fixed dollars): `avgIv = mean(leg.iv)` (0.2 if no legs); `sigmaPts = rnd.stdDev > 0 ? rnd.stdDev : spot·avgIv·√(max(t,1e-4))`; `em = max(sigmaPts, 0.005·spot)`; `lowTail = max(0, min(spot − 4·em, rnd.density[0].strike ?? 0.5·spot))`; `highTail = max(spot + 4·em, rnd.density[last].strike ?? 1.5·spot)`; scan set = `{lowTail, highTail} ∪ {K, K − 0.25·em, K + 0.25·em per leg}` sorted; `maxProfitVal`/`maxLossVal` = max/min of payoff over the set, both **initialized to 0**.
4. **Tail topology** (`payoffTailTopology`): `netCallQty = Σ signed call qty`, `netPutQty = Σ signed put qty` (signed = dir·qty). `profitUnbounded = netCallQty > 0`; `lossUnbounded = netCallQty < 0`. Only the call wing is treated as unbounded (puts capped at S=0). If unbounded, output `'unlimited'` in place of the scanned number.
5. **POP**: over `i = 0..199`, `testSpot = minDensityStrike + (maxDensityStrike − minDensityStrike)·(i/200)`; `p = interpDensity(rnd.density, testSpot)` (linear interpolation between the two bracketing nodes via binary search; **0 outside support, never a uniform 1/N fallback**); `totalAreaSum += p`; if `pnl(testSpot) > 0` then `profitAreaSum += p`. `pop = totalAreaSum > 0 ? profitAreaSum/totalAreaSum : 0.55`.
6. **Breakevens**: between consecutive samples with `prevPnl·pnl < 0`, linear zero-cross `cZero = prevSpot + (testSpot − prevSpot)·|prevPnl|/(|prevPnl|+|pnl|)`; dedupe if within $2 of an existing breakeven; store rounded to $0.1.
7. **Kelly** (measure-consistent: win/loss magnitudes weighted by the SAME RND as `pop`): over the 101 density nodes, `pnl_k = payoff(strike_k)`; wins: `sumWinPnl += pnl·p, sumWinProb += p`; losses: `sumLossPnl += |pnl|·p, sumLossProb += p`. `meanWin = sumWinProb>0 ? sumWinPnl/sumWinProb : 100`; `meanLoss` analogous (fallback 100); `R = meanLoss>0 ? meanWin/meanLoss : 1.0`; `kellyUnbounded = pop − (1−pop)/R`; `kellySizing = min(0.20, max(0, 0.5·kellyUnbounded))` (half-Kelly, 20% cap).

`generatePayoffCoordinates(legs, spot, rnd)`: 81 points over `[0.85·spot, 1.15·spot]`; each = `{underlyingPrice: round2, pnl: round2 (same payoff formula), probability: interpDensity(...)}`.

| value | where used | meaning | proposed name |
|---|---|---|---|
| 30 | default dte | days | `STRATEGY_DEFAULT_DTE` |
| 100 | premium & payoff | contract multiplier | `CONTRACT_MULTIPLIER` |
| 0.2 | avgIv fallback | default IV, empty legs | `STRATEGY_DEFAULT_IV` |
| 1e-4 | t floor in em | min t | `BSM_MIN_T_YEARS` |
| 0.005 | em floor | min expected move (×spot) | `STRATEGY_MIN_EM_FRAC` |
| 4 | tails | ± expected moves scanned | `STRATEGY_TAIL_EM_MULT` |
| 0.25 | strike probes | neighborhood in em units | `STRATEGY_STRIKE_PROBE_EM` |
| 0.5 / 1.5 | tail fallbacks | ×spot when density empty | `STRATEGY_LOW_TAIL_FRAC` / `STRATEGY_HIGH_TAIL_FRAC` |
| 200 | POP sampling | sample count | `POP_SAMPLE_POINTS` |
| 0.55 | pop fallback | neutral-ish POP when no mass | `POP_FALLBACK` |
| 2 | breakeven dedupe | $ tolerance | `BREAKEVEN_DEDUPE_TOL` |
| 10 (÷/×) | breakeven rounding | $0.1 rounding | `BREAKEVEN_ROUND_FACTOR` |
| 100 | meanWin/meanLoss fallback | $ placeholder | `KELLY_PNL_FALLBACK` |
| 1.0 | R fallback | payoff ratio when meanLoss=0 | `KELLY_R_FALLBACK` |
| 0.5 | half-Kelly | fraction of full Kelly | `KELLY_FRACTION` |
| 0.20 | cap | max allocation fraction | `KELLY_MAX_ALLOCATION` |
| 0.85 / 1.15 | payoff coords | chart spot range ×spot | `PAYOFF_CHART_LO/HI_FRAC` |
| 80 | payoff coords | steps (81 pts) | `PAYOFF_CHART_STEPS` |

Outputs: `combinedGreeks` (per-share aggregate × qty), `netPremium` $ (debit +), `maxProfit`/`maxLoss` $ or `'unlimited'`, `breakevens` $[], `pop` ∈ [0,1], `kellySizing` ∈ [0, 0.20].

### E7. Multi-leg scenario shock matrix — `computeScenarioShockMatrix` (quantSuite)

Purpose: reprice a multi-leg position over a spot% × vol-shift × DTE grid.

Inputs: `legs` (as E6), `spot`, `spotShocks` fractional (default `[-0.05,-0.025,0,0.025,0.05]`), `volShocks` absolute vol decimal (default same values), `targetDTEs` days (default `[30,15,0]`), `r` (default 0.05).

Algorithm — for each (sShock, vShock, dte) node:
1. `shockedSpot = spot·(1 + sShock)`.
2. Per leg: `originalPrice = bsmPrice(spot, K, 30/365, iv, type, r)` — **the entry baseline is always 30 DTE regardless of the leg's actual DTE** (see D4).
3. If `dte === 0`: `currentPrice = intrinsic(shockedSpot)`; else `currentPrice = bsmPrice(shockedSpot, K, dte/365, max(0.01, iv + vShock), type, r)`.
4. `nodePnl += (currentPrice − originalPrice)·dir·qty·100`; store rounded to cents.

| value | meaning | proposed name |
|---|---|---|
| [-0.05,-0.025,0,0.025,0.05] | default spot shocks (frac) | `SHOCK_DEFAULT_SPOT_GRID` |
| [-0.05,-0.025,0,0.025,0.05] | default vol shifts (abs decimal) | `SHOCK_DEFAULT_VOL_GRID` |
| [30,15,0] | default DTE horizons (days) | `SHOCK_DEFAULT_DTES` |
| 30/365 | baseline pricing t | `SHOCK_BASELINE_T` |
| 0.01 | shocked-vol floor | `SHOCK_MIN_VOL` |
| 100 | contract multiplier | `CONTRACT_MULTIPLIER` |

Output: `ShockNode {spotChange frac, volChange abs decimal, dteRemaining days, pnl $ (2dp)}[]`, length = |spot|·|vol|·|dte| (75 default).

### E8. Single-contract scenario matrix — `computeScenarioMatrix` (scenarioMatrix.ts)

Purpose: desk "risk slide" for ONE contract: P&L grid over spot% × IV shift at a forward-decay horizon.

Inputs: `{spot, strike, dteDays, iv decimal, isCall, entryPrice ($/share premium), quantity=1, r=0.05, spotShiftsPct=[-0.05,-0.03,-0.015,0,0.015,0.03,0.05], ivShiftsAbs=[-0.05,-0.02,0,0.02,0.05], daysForward=1}`.

Algorithm:
1. `entry = max(0.01, entryPrice)`; `newDte = max(0.05, dteDays − daysForward)` (days).
2. For each iv row `dv`, spot col `ds`: `price = computeBlackScholesPrice(spot·(1+ds), strike, newDte, max(0.01, iv+dv), isCall, r)` (external `v11Math`; **takes DTE in days**).
3. `pctFrac = (price − entry)/entry`; store `pnlPct = round1(pctFrac·100)`; `pnlAbs = round0((price − entry)·100·quantity)`.
4. Track best/worst by the **raw fraction** (not the rounded %); record `{pnlPct, spotShiftPct, ivShiftAbs}` for each extreme.

| value | meaning | proposed name |
|---|---|---|
| [-0.05,-0.03,-0.015,0,0.015,0.03,0.05] | default spot grid | `SCEN_DEFAULT_SPOT_GRID` |
| [-0.05,-0.02,0,0.02,0.05] | default IV grid (abs) | `SCEN_DEFAULT_IV_GRID` |
| 1 | default daysForward | `SCEN_DEFAULT_DAYS_FWD` |
| 0.01 | entry floor $ | `SCEN_MIN_ENTRY` |
| 0.05 | DTE floor (days) | `SCEN_MIN_DTE_DAYS` |
| 0.01 | shifted-IV floor | `SCEN_MIN_IV` |
| 100 | contract multiplier | `CONTRACT_MULTIPLIER` |

Output: `ScenarioMatrix {pnlPct[][] (% 1dp, rows=iv, cols=spot), pnlAbs[][] ($ 0dp), best, worst}`.

### E9. Portfolio Greeks aggregator — `aggregatePortfolioGreeks`

Purpose: book-level Greeks + P&L across stock and option positions.

Inputs: `positions {type 'stock'|'call'|'put', qty (signed), entryPrice, currentPrice, strike?, iv?, dte?}[]`, `spot`, `r=0.05`.

Algorithm:
1. `grossCost = entryPrice·|qty|·(stock?1:100)`; `totalCost += grossCost` (**|qty| — wrong for shorts, see D2**).
2. Stock: `marketValue += currentPrice·qty`; `delta += qty`.
3. Option: `marketValue += currentPrice·qty·100`; Greeks from E2 at `(spot, strike||spot, (dte||30)/365, iv||0.18, type, r)`; each Greek accumulated × `qty·100`.
4. `totalProfit = marketValue − totalCost`.

| value | meaning | proposed name |
|---|---|---|
| 30 | default DTE (days) | `PORTFOLIO_DEFAULT_DTE` |
| 0.18 | default IV | `PORTFOLIO_DEFAULT_IV` |
| 100 | contract multiplier | `CONTRACT_MULTIPLIER` |

### E10. Expiry GEX aggregator — `aggregateExpiryGexCurve`

Purpose: dealer gamma exposure ($ per 1% spot move) for the front expiry, with dominant strike.

Algorithm: for each contract, `gex = gamma · openInterest · 100 · spot² · 0.01 · (call ? +1 : −1)` (dealers long calls / short puts convention). Sum call vs put GEX; `dominantStrike` = strike of the single contract with max `|gex|` (per-contract, NOT per-strike aggregate). Returns exactly ONE node `{expiry: 'Front', totalGex, callGex, putGex, dominantStrike}` because `ChainContract` has no per-contract expiry — the legacy multi-bucket version faked a term structure with `exp(−idx·0.4)` scaling and was deliberately removed. Empty chain → `[]`.

| value | meaning | proposed name |
|---|---|---|
| 100 | contract multiplier | `CONTRACT_MULTIPLIER` |
| 0.01 | per-1%-move scaling | `GEX_PCT_MOVE` |
| +1/−1 | call/put dealer sign | `GEX_CALL_SIGN`/`GEX_PUT_SIGN` |

### E11. Charm/Vanna decay clock — `generateCharmVannaClock`

Purpose: deterministic intraday decay-acceleration schedule (cosmetic model, no market inputs).

Algorithm: for each half-hour label 09:30–16:00 (14 points), `h = hour + minute/60`;
`accel = h ≥ 14 ? 1.0 + 0.8·(h − 13.5)^{2.5} : 0.8 + 0.1·(h − 9.5)`; round 2dp; `isPeakDecayWindow = 14.0 ≤ h < 16.0`. Times formatted 12-hour with "EST" semantics hardcoded (no DST handling).

| value | meaning | proposed name |
|---|---|---|
| 14 | ramp start hour (ET) | `DECAY_RAMP_START_HOUR` |
| 13.5 | ramp origin hour | `DECAY_RAMP_ORIGIN_HOUR` |
| 2.5 | ramp exponent | `DECAY_RAMP_EXPONENT` |
| 0.8 | ramp coefficient | `DECAY_RAMP_COEF` |
| 0.8 | morning base | `DECAY_MORNING_BASE` |
| 0.1 | morning slope /hr | `DECAY_MORNING_SLOPE` |
| 9.5 | session open hour | `SESSION_OPEN_HOUR` |
| 16.0 | session close hour | `SESSION_CLOSE_HOUR` |

### E12. Asset/contract edge orchestrator — `computeAssetEdge` / `computeContractEdge` (quantEdge.ts)

Purpose: assemble the per-asset "edge" block once per tick from the individual engines, maintaining the rolling history that makes skew percentiles real.

`computeAssetEdge({chain, candles, spot, rndDteDays, netCharm, netVanna, history {rr[], bf[]}, ticker, flow})`:
1. `skewRaw = computeSkew(chain, spot)` (external); `atmIv = skewRaw?.atmIv ?? 0.2`.
2. `realizedVol = computeRealizedVol(candles, 20)`; `vrp = computeVRP(atmIv, candles, 20)` (external).
3. `rnd = computeRiskNeutralDensity(chain, spot, rndDteDays, 0.05)` (external `riskNeutral.ts` — the production RND; E3 is the Quant-Lab variant).
4. `dealerClock = computeDealerClock(netCharm, netVanna)` (external).
5. History ring buffers: `pushCap(arr, v)` — skip non-finite, push, `shift()` when length > **240** (`HISTORY_CAP`). Push `skewRaw.riskReversal25` into `history.rr`, `butterfly25` into `history.bf`; then `rrPercentile = percentileRank(history.rr, riskReversal25)`, `bfPercentile = percentileRank(history.bf, butterfly25)` (external). `skew = null` if `computeSkew` returned null.
6. Regime/microstructure fan-out (all external, candle-driven): `classifyRegime(candles)`, `ornsteinUhlenbeck(closes, intervalMinutes(candles))`, `volCompression`, `volExpansion`, `forwardVolMatrix`, `computeVPIN`, `computeKylesLambda`, `hawkesIntensity(candles)`, `netDeltaAggression(flow, ticker)`, `fisherDivergence(candles)` (E14).
7. `pca` and `leadLag` are set to `null` here and filled by the cross-asset engine layer.

`computeContractEdge({spot, strike, dteDays, iv, isCall, entryPrice, winPct 0..1, riskReward})`:
- `kelly = kellySize(winPct, max(0.1, riskReward), 1, 0.5)` — win prob = calibrated `winPct`, payoff ratio = R/R floored at 0.1, avg loss normalized to 1, half-Kelly fraction 0.5 (external `sizing.ts`).
- `scenario = computeScenarioMatrix({spot, strike, dteDays, iv, isCall, entryPrice, quantity: 1})` (E8 defaults).

| value | meaning | proposed name |
|---|---|---|
| 240 | history ring-buffer cap (ticks) | `EDGE_HISTORY_CAP` |
| 0.2 | atmIv fallback | `EDGE_DEFAULT_ATM_IV` |
| 20 | RV/VRP lookback (bars) | `RV_DEFAULT_LOOKBACK` |
| 0.05 | RND rate | `DEFAULT_RISK_FREE_RATE` |
| 0.1 | Kelly R/R floor | `KELLY_MIN_RR` |
| 1 | Kelly avg-loss normalizer | `KELLY_UNIT_LOSS` |
| 0.5 | Kelly fraction | `KELLY_FRACTION` |

### E13. Transfer entropy & market leader — `transferEntropy` / `marketLeader` (infoTheory.ts)

Purpose: directed (time-asymmetric) information flow between two return series — which asset LEADS.

Inputs: `srcRets`, `dstRets`: log-return arrays (`ln(close_i/close_{i−1})`, skipping non-positive closes). `marketLeader` takes `Record<ticker, Candle[]>`.

Algorithm (`transferEntropy`):
1. `n = min(len(src), len(dst))`; if `n < 30` return 0. Take the last n of each.
2. **Discretize to 3 states** each series independently: `sd = √(sample variance, ddof=1) || 1e-9`; `band = 0.33·sd`; state = 2 if `r > band`, 0 if `r < −band`, else 1 (down/flat/up).
3. Lag-1 joint counts over `t = 1..n−1` (`total = n−1`): `c(Y_t,Y_{t−1})`, `c(Y_{t−1})`, `c(Y_t,Y_{t−1},X_{t−1})`, `c(Y_{t−1},X_{t−1})` where Y = dst states, X = src states.
4. Plug-in Shannon entropy in **bits** per count table: `H = −Σ_{c>0} (c/total)·log2(c/total)`; also `K̂` = number of non-empty bins.
5. `TE_plugin = (H(Y_t,Y_{t−1}) − H(Y_{t−1})) − (H(Y_t,Y_{t−1},X_{t−1}) − H(Y_{t−1},X_{t−1}))` (= H(Y_t|Y_{t−1}) − H(Y_t|Y_{t−1},X_{t−1})).
6. **Miller–Madow de-bias** (plug-in H is negatively biased by (K̂−1)/(2N) nats; carry through the decomposition with signs):
   `mmBits = ((K̂_{YY1}−1) − (K̂_{Y1}−1) − (K̂_{YY1X1}−1) + (K̂_{Y1X1}−1)) / (2·total·ln 2)`.
   Net effect is negative (the 3-var joint has the most bins), which removes the spurious TE>0 on independent series.
7. `TE = max(0, round4(TE_plugin + mmBits))` bits.

`marketLeader(series)`: compute log returns per ticker; for every ordered pair (a,b), a≠b, `te = TE(a→b)`; keep the maximum; `active = te > 0.10` bits (threshold re-tuned for the de-biased scale: independent-pair sampling tail p99 ≈ 0.06–0.08 bits at 120–160 bars; genuine coupling ≥ 0.4 bits). Returns null if < 2 tickers.

| value | meaning | proposed name |
|---|---|---|
| 30 | min overlapping returns | `TE_MIN_SAMPLES` |
| 0.33 | deadband in std devs | `TE_DEADBAND_SIGMA` |
| 1e-9 | sd floor | `TE_SD_EPS` |
| 3 | discretization states | `TE_N_STATES` |
| 1 | lag (bars) | `TE_LAG` |
| 4 | TE rounding decimals | `TE_ROUND_DP` |
| 0.10 | leader activation floor (bits) | `MARKET_LEADER_TE_THRESHOLD` |

Outputs: `te ≥ 0` bits; `LeadLagResult {leader, follower, te, active}`.

### E14. Fisher divergence — `fisherDivergence` (infoTheory.ts)

Purpose: distance between recent and prior return distributions (Gaussian approx) — flags structural/regime shift before price breaks.

Inputs: `candles`, `window = 30` bars.

Algorithm:
1. Log returns; if `count < 2·window` → `{divergence: 0, structuralShift: false}`.
2. `recent` = last `window` returns; `prior` = the `window` before those.
3. `m1, m2` = means; `v1, v2` = sample variances (ddof=1) each floored at `1e-12`; `Δm² = (m1 − m2)²`.
4. Jeffreys (symmetric KL) divergence between N(m1,v1), N(m2,v2) — log terms cancel:
   `J = 0.5·( (v1 + Δm²)/v2 + (v2 + Δm²)/v1 − 2 )`.
5. `divergence = round3(J)`; `structuralShift = J > 1.5`.

| value | meaning | proposed name |
|---|---|---|
| 30 | window (bars) | `FISHER_WINDOW` |
| 1e-12 | variance floor | `FISHER_VAR_EPS` |
| 1.5 | structural-shift threshold | `FISHER_SHIFT_THRESHOLD` |
| 3 | rounding decimals | `FISHER_ROUND_DP` |

### E15. Monte Carlo path engine — `simulateMonteCarlo` (monteCarlo.ts)

Purpose: risk-neutral terminal-distribution simulation for VaR/ES/percentiles + a small set of full paths for rendering. Deterministic given (inputs, seed).

Inputs: `spot`, `r` annualized risk-neutral drift, `sigma` annualized vol (GBM/JUMP diffusion; seeds Heston defaults), `tYears`, `steps` (clamped to [1,1000], floored), `nPaths` (clamped to [1, 200000], floored), `model ∈ {'gbm','jump','heston'}`, `seed` uint32, `samplePaths` retained paths (default 80, clamped to [0, nPaths]), `jump {lambda /yr, muJ, sigJ}` (default {0,0,0}), `heston {kappa, theta, xi, rho, v0}` (default `{1.5, σ², 0.3, −0.6, σ²}`).

RNG (must be reproduced bit-exactly):
1. **mulberry32**: state `a = seed >>> 0`; each call: `a = (a + 0x6D2B79F5) | 0`; `t = imul(a ^ (a >>> 15), 1 | a)`; `t = (t + imul(t ^ (t >>> 7), 61 | t)) ^ t`; return `((t ^ (t >>> 14)) >>> 0) / 4294967296` (all 32-bit int ops).
2. **Normals**: Box–Muller with caching — `u1 = max(uniform(), 1e-12)`, `u2 = uniform()`, `mag = √(−2·ln u1)`; return `mag·cos(2π·u2)` now, cache `mag·sin(2π·u2)` as the next draw. Normal stream seeded with `seed >>> 0`.
3. **Jump-count uniforms**: an INDEPENDENT mulberry32 stream seeded `(seed ^ 0x9E3779B9) >>> 0`.
4. **Poisson(λ·dt)**: Knuth — `L = e^{−λdt}`; `k=0, p=1`; do `{k++; p *= uniform()}` while `p > L`; return `k−1`.

Discretization: `dt = tYears/steps`, `sqrtDt = √dt`. Per path (paths simulated sequentially, sharing the two streams; first `keep` paths also record every step):

- **GBM** (log-Euler; exact in law): `S ← S·exp( (r − 0.5σ²)·dt + σ·√dt·z )`, z ~ N(0,1).
- **JUMP** (Merton, martingale-compensated): `κ_J = exp(μ_J + 0.5σ_J²) − 1`; per step, `drift = (r − 0.5σ²)·dt − λ·κ_J·dt`; `N_J ~ Poisson(λ·dt)` from the uniform stream; `dJump = Σ_{j=1..N_J} (μ_J + σ_J·z_j)` with z_j from the SAME normal stream as diffusion; `S ← S·exp(drift + σ√dt·z + dJump)`. (Only applied when `lambda > 0`, otherwise identical to GBM.)
- **HESTON** (full-truncation-style Euler): draw `z1, z2raw`; `z2 = ρ·z1 + √(1−ρ²)·z2raw`; `v⁺ = max(0, v)`; `S ← S·exp( (r − 0.5·v⁺)·dt + √v⁺·√dt·z1 )`; `v ← v⁺ + κ·(θ − v⁺)·dt + ξ·√v⁺·√dt·z2` (note: level term also truncated — see D14). `sigma` is unused in the Heston path itself.

Statistics over the terminal array (Float64Array, ascending sort):
- `mean = Σ S_T / n`; `std = √(Σ(S_T − mean)² / max(1, n−1))` (sample).
- Quantile `q(p) = sorted[clamp(floor(p·(n−1)), 0, n−1)]`; percentiles at 0.05/0.25/0.50/0.75/0.95.
- Loss return `L(price) = (spot − price)/spot` (positive = loss). `var95 = L(q(0.05))`, `var99 = L(q(0.01))`.
- `ES_α = (1/m)·Σ_{i=0..m−1} L(sorted[i])`, `m = max(1, floor(α·n))`; `es95` at α=0.05, `es99` at α=0.01.
- `probUp = #(S_T > spot)/n`; `expectedReturnPct = mean/spot − 1`.
- Histogram: 48 bins over `[q(0.01), q(0.99)]` (span floored at 1); values strictly outside are skipped; bin index `floor((x − lo)/span · 48)` clamped to [0,47]; `edges` has 49 entries.
- `analyticGbmMean = spot·e^{r·tYears}` (validation reference; tests check GBM terminal moments `E[S_T]=S·e^{rT}`, `Var[S_T]=S²e^{2rT}(e^{σ²T}−1)`).

| value | meaning | proposed name |
|---|---|---|
| 0x6D2B79F5 | mulberry32 increment | `MULBERRY32_INC` |
| 61 | mulberry32 mix constant | `MULBERRY32_MIX` |
| 0x9E3779B9 | seed XOR for jump stream | `JUMP_STREAM_SEED_XOR` |
| 4294967296 | 2^32 divisor | `UINT32_RANGE` |
| 1e-12 | Box–Muller u1 floor | `BM_U1_EPS` |
| 1 / 1000 | steps clamp | `MC_MIN_STEPS` / `MC_MAX_STEPS` |
| 1 / 200000 | nPaths clamp | `MC_MIN_PATHS` / `MC_MAX_PATHS` |
| 80 | default retained sample paths | `MC_DEFAULT_SAMPLE_PATHS` |
| 1.5 / 0.3 / −0.6 | Heston default κ / ξ / ρ | `HESTON_DEFAULT_{KAPPA,XI,RHO}` |
| σ² | Heston default θ and v0 | `HESTON_DEFAULT_THETA_V0` |
| 0.05 / 0.01 | VaR/ES tail levels | `VAR_ALPHA_95` / `VAR_ALPHA_99` |
| 48 | histogram bins | `MC_HISTOGRAM_BINS` |
| 0.01 / 0.99 | histogram clip quantiles | `MC_HIST_CLIP_LO/HI` |

Outputs: see `MCResult` — `samplePaths[keep][steps+1]` prices, `terminalMean/Std` $, percentiles $, `var/es` as loss fractions, `probUp` ∈ [0,1], `expectedReturnPct`, histogram, `analyticGbmMean`.

### E16. Forecast read scorer — `scoreRead` (edgeTracker.ts)

Purpose: turn one GEX-outlook forecast (regime + bias + optional target + confidence) into a scored outcome against the realized forward path. Pure/deterministic; regime-correct (sideways = bet price STAYS; directional = bet price MOVES).

Inputs: `snap {ts, ticker, spot (anchor), regime (GexOutlook: 'PINNING'|'GAMMA SQUEEZE'|'SHORT SQUEEZE'|'TREND UP'|'TREND DOWN'|'RANGE'|'NEUTRAL'), bias 'up'|'down'|'sideways', target? $, confidence 0..100, provenance 'live'|'model'}`; `forwardCloses: number[]` (closes strictly AFTER the snapshot, oldest→newest); `opts {moveThreshPct=0.15, pinThreshPct=0.25}` (percent units).

Algorithm:
1. Filter path to finite positives; if empty or `spot ≤ 0` → null.
2. `end = last`, `hi = max(path)`, `lo = min(path)`; `endReturnPct = (end − s0)/s0·100`; `upExcPct = (hi − s0)/s0·100` (can be negative); `dnExcPct = (lo − s0)/s0·100` (≤ 0 typically); `rangePct = (hi − lo)/s0·100`.
3. **Sideways** (`bias === 'sideways'`):
   - `drift = |endReturnPct|`; `nearTargetEnd = target == null OR |end − target|/s0·100 ≤ pinThresh`.
   - `pinned = rangePct ≤ pinThresh AND drift ≤ 0.7·pinThresh AND nearTargetEnd`.
   - `brokeOut = rangePct ≥ 2.2·pinThresh OR drift ≥ 1.8·pinThresh`.
   - `verdict = pinned ? 'hit' : brokeOut ? 'miss' : 'partial'`.
   - `score = clamp(1 − rangePct/pinThresh, −1, 1)`.
   - `maxFavorablePct = rangePct`; `maxAdversePct = max(0, rangePct − pinThresh)`; `reachedTarget = pinned` (field repurposed).
4. **Directional**: `dir = bias==='up' ? +1 : −1`; `dirRet = dir·endReturnPct`;
   `favorablePct = max(0, dir>0 ? upExcPct : −dnExcPct)`; `adversePct = max(0, dir>0 ? −dnExcPct : upExcPct)`;
   `reachedTarget = target != null AND (dir>0 ? (hi ≥ target AND target ≥ s0) : (lo ≤ target AND target ≤ s0))`.
   - Verdict cascade (first match): `dirRet ≥ moveThresh OR reachedTarget` → `'hit'` if `adversePct ≤ favorablePct` else `'partial'`; `dirRet ≤ −moveThresh` → `'miss'`; `|dirRet| < 0.5·moveThresh AND favorablePct < moveThresh` → `'flat'`; else `'partial'`.
   - `score = (dirRet + 0.5·favorablePct − 0.5·adversePct) / (3·moveThresh)`; `+0.15` if `reachedTarget`; clamp to [−1, 1].

| value | meaning | proposed name |
|---|---|---|
| 0.15 | directional move threshold (%) | `MOVE_THRESH_PCT` |
| 0.25 | pin range threshold (%) | `PIN_THRESH_PCT` |
| 0.7 | pin drift multiplier | `PIN_DRIFT_MULT` |
| 2.2 | breakout range multiplier | `BREAKOUT_RANGE_MULT` |
| 1.8 | breakout drift multiplier | `BREAKOUT_DRIFT_MULT` |
| 0.5 | flat-band multiplier of moveThresh | `FLAT_BAND_MULT` |
| 0.5 | favorable/adverse excursion weight | `EXCURSION_WEIGHT` |
| 3 | score normalization (×moveThresh) | `SCORE_NORM_MULT` |
| 0.15 | target-reached score bonus | `TARGET_BONUS` |

Output: `ReadResolution {barsForward, endReturnPct %, maxFavorablePct % ≥0, maxAdversePct % ≥0, reachedTarget, verdict, score ∈ [−1,1]}`.

### E17. Track record summarizer — `summarize` (edgeTracker.ts)

Purpose: roll scored reads into an honest calibrated track record. Caller MUST pre-filter by provenance (live vs model) — never mix.

Algorithm:
1. Credit per verdict: `hit=1, partial=0.5, flat=0, miss=0`.
2. `hitRate = mean(credit)·100`; `cleanHits = #hit`; `misses = #miss`; `avgScore = mean(score)`.
3. `byRegime`: group by `regime`; per group `{n, hitRate = mean(credit)·100}`, sorted by n desc.
4. Calibration buckets on confidence: `[0,40), [40,60), [60,80), [80,101)`; per non-empty bucket `predicted = mean(confidence)`, `realized = mean(credit)·100`.
5. `calibrationError = Σ_b |predicted_b − realized_b|·n_b / Σ_b n_b` (0 if no buckets). Lower = better calibrated.
6. Empty input → all-zero record.

| value | meaning | proposed name |
|---|---|---|
| 1 / 0.5 / 0 / 0 | verdict credit (hit/partial/flat/miss) | `VERDICT_CREDIT_*` |
| [0,40,60,80,101] | confidence bucket edges | `CALIBRATION_BUCKET_EDGES` |

### E18. Calibration journal loop — `calculateCalibrationLoop` (quantSuite.ts)

Purpose: Brier / ECE / reliability / expectancy / performance stats over journaled trades with predicted probabilities, plus self-adjusting setup weights.

Inputs: `trades: {id, ticker, setup string, pop ∈[0,1] predicted win prob, outcome 'WIN'|'LOSS'|'OPEN', pnl? $}[]`. Only `outcome !== 'OPEN'` counted ("completed").

Algorithm:
1. **Brier**: `BS = (1/N)·Σ (pop − O)²`, O = 1 (WIN) / 0 (LOSS). Empty → 0.16 (fabricated default).
2. **Reliability bins** (fixed 5): `[0,0.2), [0.2,0.4), [0.4,0.6), [0.6,0.8), [0.8,1.0)` with centers 0.1/0.3/0.5/0.7/0.9; membership `pop ≥ min && pop < max` (pop = 1.0 falls in NO bin — D8). Per bin `empRate = wins/total` (empty bin → center).
3. **ECE**: `Σ_b (n_b/N)·|empRate_b − center_b|` — compares to bin CENTER, not mean predicted prob (D7). Empty input → 0.08.
4. **Expectancy by setup**: group completed by `setup`; `winR = wins/n`; `avgWin = mean(pnl>0)` (default 1500 if no wins); `avgLoss = |mean(pnl<0)|` (default 1000); `expectancy = round(winR·avgWin − (1−winR)·avgLoss)`; `averagePnl = Σpnl/n`. If NO setups, inject fabricated defaults: `('GEX Mean Reversion', 450, 380, 0.62), ('Magnet Strike Drift', 1100, 950, 0.58), ('Gamma Flip Reversal', −120, −50, 0.45)`.
5. `winRate = wins/N` (empty → 0.65); `averageReturn = Σpnl/N` (empty → 1100).
6. **Max drawdown**: walk completed trades **in array order**, `balance` starting at 100000; track `peak`; `dd = (peak − balance)/peak`; `maxDrawdown = max(dd)·100` (%).
7. **Sharpe**: `meanPnl / stdPnl · √25.2` where stdPnl = sample std (ddof=1) of pnl, `|| 1`; empty-mean default 1100; unreachable fallback 1.75.
8. **Sortino**: downside std = sample std (ddof=1) of NEGATIVE pnls about their own mean (single negative → |value|; none → 1); `meanPnl / downsideStd · √25.2`; fallback 2.25.
9. `calibrationScore = clamp(round((1 − BS)·100), 10, 100)`.
10. **Error divergences**: for each completed trade with `|pop − O| ≥ 0.40`, emit `{id: 'err-'+id, setup, prediction, actual, error (4dp), reason}`; reason by substring of setup, later checks overwrite earlier: default `'Vanna Skew Overrun'`; contains `'GEX'` → `'GEX Flip Boundary Shift'`; contains `'Magnet'` → `'Charm Option Liquidity Decay'`; contains `'Wall'` → `'Gamma Wall Absorption Breakout'`.
11. **Adjusted weightings**: for each of the 4 hardcoded setups `['GEX Mean Reversion','Magnet Strike Drift','Gamma Flip Reversal','Wall Rejection Strike']`: `errorSum = Σ error over that setup's divergences`; `modifier = max(0.15, round2(1.0 − 0.15·errorSum))`.

| value | meaning | proposed name |
|---|---|---|
| 0.16 | Brier fallback (empty) | `BRIER_FALLBACK` |
| 0.2 | reliability bin width | `RELIABILITY_BIN_WIDTH` |
| 0.08 | ECE fallback (empty) | `ECE_FALLBACK` |
| 1500 / 1000 | avgWin / avgLoss fallbacks $ | `EXPECTANCY_DEFAULT_WIN/LOSS` |
| 0.5 | winR fallback (n=0 group) | `EXPECTANCY_DEFAULT_WINRATE` |
| 0.65 / 1100 | winRate / avgReturn fallbacks | `WINRATE_FALLBACK` / `AVGRETURN_FALLBACK` |
| 100000 | drawdown start balance $ | `DD_START_BALANCE` |
| 25.2 | Sharpe/Sortino annualization (√) | `SHARPE_ANNUALIZATION` |
| 1.75 / 2.25 | Sharpe / Sortino fallbacks | `SHARPE_FALLBACK` / `SORTINO_FALLBACK` |
| 10 / 100 | calibrationScore clamp | `CALSCORE_MIN/MAX` |
| 0.40 | divergence error threshold | `ERROR_DIVERGENCE_THRESHOLD` |
| 0.15 | weight decay per unit error | `WEIGHT_ERROR_DECAY` |
| 0.15 | weight modifier floor | `WEIGHT_MODIFIER_FLOOR` |

### E19. Alert rules evaluator — `evaluateAlertRules` (quantSuite.ts)

Purpose: threshold-based alert dispatch on spot, gamma flip, negative GEX, VRP richness, skew stress. Honest underlying values (decimal vol), no fabricated percentiles.

Inputs: `rules {metric 'spot'|'gex_flip'|'gex_negative'|'vrp_high'|'skew_risk', operator 'above'|'below'|'crosses'|'is_negative', thresholdValue?, isActive}[]`, `spot`, `prevSpot`, `deltaGex` (net GEX $), `gammaFlip` (spot level), `varianceRiskPremium` (decimal, IV−RV), `riskReversal25D` (decimal).

Conditions (only `isActive` rules; each fires per evaluation, no edge-detection except crosses):
- `spot`+`above`: `spot > thresholdValue` (guard: thresholdValue truthy — threshold 0 never evaluates) → danger.
- `spot`+`below`: `spot < thresholdValue` → danger.
- `spot`+`crosses`: `prevSpot > 0 AND ((prevSpot ≤ thr AND spot > thr) OR (prevSpot ≥ thr AND spot < thr))` → warning.
- `gex_flip`: `prevSpot > 0 AND ((prevSpot > gammaFlip AND spot ≤ gammaFlip) OR (prevSpot < gammaFlip AND spot ≥ gammaFlip))` → danger.
- `gex_negative`: `deltaGex < 0` → danger.
- `vrp_high`: `varianceRiskPremium·100 ≥ (thresholdValue ?? 5)` — threshold read in VOL POINTS → info.
- `skew_risk`: `|riskReversal25D|·100 ≥ (thresholdValue ?? 3)` → warning; side text: RR < 0 ⇒ "downside puts bid (put skew)" else "upside calls bid (call skew)".

| value | meaning | proposed name |
|---|---|---|
| 5 | default VRP threshold (vol pts) | `VRP_ALERT_DEFAULT_PTS` |
| 3 | default |RR| threshold (vol pts) | `SKEW_ALERT_DEFAULT_PTS` |
| 100 | decimal→vol-pts conversion | `VOL_PTS_PER_DECIMAL` |

---

## State semantics

The rebuild collapses everything to binary ACTIVE/INACTIVE + a continuous score; the continuous quantity behind every discrete state is listed.

| State field | Values | Exact condition | Underlying continuous quantity |
|---|---|---|---|
| `ReadVerdict` (E16, directional) | hit / partial / flat / miss | hit: `(dirRet ≥ 0.15 OR reachedTarget) AND adverse ≤ favorable`; partial: same trigger but adverse > favorable, or none of the other branches; miss: `dirRet ≤ −0.15`; flat: `|dirRet| < 0.075 AND favorable < 0.15` | `score = clamp((dirRet + 0.5·fav − 0.5·adv)/0.45 (+0.15 if target), −1, 1)` — keep this as the continuous score; ACTIVE ⇔ verdict ∈ {hit, partial} |
| `ReadVerdict` (E16, sideways) | hit / partial / miss | hit: `range ≤ 0.25 AND |drift| ≤ 0.175 AND nearTargetEnd`; miss: `range ≥ 0.55 OR |drift| ≥ 0.45`; else partial | `score = clamp(1 − rangePct/0.25, −1, 1)`; continuous driver = realized `rangePct` (and `|endReturnPct|`) |
| Verdict credit (E17) | 1 / 0.5 / 0 / 0 | direct map hit/partial/flat/miss | `resolution.score` (−1..1) is the finer-grained equivalent |
| `LeadLagResult.active` (E13) | true/false | `te > 0.10` bits | `te` (Miller–Madow de-biased transfer entropy, bits) |
| `FisherResult.structuralShift` (E14) | true/false | `divergence > 1.5` | `divergence` (Jeffreys divergence, nats-scaled dimensionless) |
| `isFatTailed` (E3) | true/false | `kurtosis > 1.2` | `kurtosis` (excess kurtosis of RND) |
| Return state (E13 discretize) | 0 down / 1 flat / 2 up | `r < −0.33σ` / in-band / `r > +0.33σ` | the log return r vs `0.33·sd` deadband |
| `maxProfit`/`maxLoss` (E6) | number \| 'unlimited' | 'unlimited' iff `netCallQty > 0` (profit) / `netCallQty < 0` (loss) | `netCallQty = Σ signed call qty` (tail slope /100) |
| `isPeakDecayWindow` (E11) | true/false | `14.0 ≤ h < 16.0` (ET decimal hours) | `decayAccelerationFactor` |
| `AlertDispatch.type` (E19) | info / warning / danger | vrp_high→info; crosses, skew_risk→warning; spot above/below, gex_flip, gex_negative→danger | spot−threshold distance; `deltaGex`; `VRP·100`; `|RR|·100` |
| `outcome` (E18 input) | WIN / LOSS / OPEN | upstream journal state; OPEN excluded from all stats | `pnl`, and `|pop − outcome|` for divergence (threshold 0.40) |
| `MCModel` (E15) | gbm / jump / heston | configuration, not a computed state | n/a |
| `provenance` (E16) | live / model | carried through, never conflated; summaries must be single-provenance | n/a |
| `regime`/`bias` (E16 input, from `terminalRead.GexOutlook`) | PINNING / GAMMA SQUEEZE / SHORT SQUEEZE / TREND UP / TREND DOWN / RANGE / NEUTRAL; up/down/sideways | produced upstream; this cluster only branches on `bias === 'sideways'` vs directional | upstream GEX quantities |

Abstention convention: percentile-type outputs return exactly **50** to mean "insufficient history — no opinion" (E4 rvPercentile, E5 rr/bf percentiles). The rebuild should represent abstention explicitly (null/None) rather than 50.

## Data dependencies

Upstream inputs by engine:
- **Chain** (`ChainContract[]`: strike, iv, type, gamma, openInterest, delta): E3, E5, E10, E12 (→ external `computeSkew`, `computeRiskNeutralDensity`).
- **Candles** (OHLCV + timestamp): E4 (RV suite/cone), E12 regime/microstructure fan-out, E13, E14.
- **Spot** (+ prevSpot for alerts): E3, E5, E6, E7, E8, E9, E10, E15, E19.
- **Other engines' outputs**: E6 requires E3's RND (`rnd` param). E19 requires net GEX + gammaFlip (gexEngine cluster), VRP (E4), RR25 (E5). E12 requires netCharm/netVanna (dealer Greeks cluster) and an options-flow array. E16 requires a `GexOutlook` (terminalRead cluster) plus forward closes (post-hoc candles). E17 requires E16 outputs. E18 requires journaled trades with `pop` (which typically comes from E6).
- **External library functions this cluster calls but does not define** (contract must be preserved): `stdNormalCDF/PDF` (normalDist), `parkinsonVol/garmanKlassVol/yangZhangVol/intervalMinutes/volCone/computeRealizedVol/computeVRP` (realizedVol), `ivAtDelta/computeSkew/percentileRank` (skewAnalytics), `computeBlackScholesPrice` (v11Math — takes DTE in **days**), `computeRiskNeutralDensity` (riskNeutral), `computeDealerClock` (dealerClock), `kellySize(winPct, avgWinPct, avgLossPct, fraction)` (sizing), `classifyRegime/ornsteinUhlenbeck/volCompression/volExpansion/forwardVolMatrix` (regimeEngine), `computeVPIN/computeKylesLambda` (microstructure), `hawkesIntensity/netDeltaAggression` (pointProcess), `formatTime` (timeUtils).

Dependency order (per tick): candles+chain+spot → {E4, E5-external skew, E3/external RND, regime/microstructure/infoTheory} → E12 assembles + updates the 240-deep RR/BF history ring → E6/E7/E8 per-strategy analytics (consume E3 RND, calibrated winPct) → E19 alerts (consume E4 VRP + E5 RR + gexEngine outputs). Asynchronously: E16 scores past reads once forward closes exist → E17 aggregates → E18 closes the journal loop. E15 is standalone (spot, σ, seed).

## Suspected defects

Flag only — do not silently fix; note as-coded vs suspected-correct.

- **D1. Vega units mismatch (E2)** — comment says "divide by 100 to get per 1% vol" but the code returns `S·e^{−qt}·φ(d1)·√t` undivided (per 1.00 vol). Consumers assuming per-1% will be 100× off. Correct form depends on the consumer contract; pick one and document.
- **D2. Short-position P&L wrong (E9)** — `totalCost += entryPrice·|qty|·mult` uses absolute qty while `marketValue` uses signed qty; for a short, `totalProfit = MV − cost = −(current + entry)·|qty|·mult` instead of `(entry − current)·|qty|·mult`. Suspected correct: signed cost basis (`entryPrice·qty·mult`).
- **D3. Skew slope sign (E5)** — `skewSlopeAtm = (put25 − call25)/(0.1·spot)` is labeled "dVol/dK" but under its own strike-spacing assumption (call strike above spot, put below) the derivative is `(call25 − put25)/ΔK`; as-coded value is the negative. Also the 0.1·spot spacing is a fixed guess, not the actual 25Δ strike gap.
- **D4. Shock-matrix baseline hardcodes 30 DTE (E7)** — `originalPrice = bsmPrice(spot, K, 30/365, iv, …)` regardless of each leg's true DTE, so any position not entered at exactly 30 DTE shows phantom baseline P&L; only the (0,0,30) node is guaranteed 0. Suspected correct: price the baseline at the leg's actual entry DTE (or use `entryPrice`).
- **D5. Max-loss scan never reaches S=0 (E6)** — `lowTail = max(0, min(spot−4em, lowest RND strike))` with the RND floor at 0.40·spot; a short-put book's true worst case at S→0 (`Σ strike·qty·100`) is understated. Same truncation understates long-put max profit.
- **D6. POP integral drops the top of the support (E6)** — loop uses `i/samplePointsCount` for `i = 0..199`, so `maxDensityStrike` is never sampled (max fraction 199/200); slight systematic bias against upside mass.
- **D7. Nonstandard ECE (E18)** — ECE compares empirical win rate to the bin **center** instead of the mean predicted probability within the bin; a perfectly calibrated forecaster whose predictions sit at bin edges is penalized. Suspected correct: `|empRate − mean(pop in bin)|`.
- **D8. pop = 1.0 dropped from reliability/ECE (E18)** — bin membership is `pop ≥ min && pop < max` with the last bin ending at 1.0 exclusive; certain predictions (pop = 1.0) are silently excluded (they still count in Brier).
- **D9. √25.2 annualization is unexplained (E18)** — Sharpe/Sortino computed on per-trade dollar PnL (not returns, no risk-free) scaled by √25.2 (≈ 252/10?); the result is not a Sharpe ratio in standard units. Note and re-derive from the actual trade frequency in the rebuild.
- **D10. Nonstandard Sortino (E18)** — downside deviation is the sample std of losing trades about **their own mean** (ddof=1, negatives only), not the root-mean-square of below-target (MAR = 0) deviations over all trades. Suspected correct: `√(Σ min(pnl,0)² / N)`.
- **D11. Drawdown depends on array order + fabricated notional (E18)** — trades are walked in `completed` array order (filter order, not sorted by entry/exit time) from a hardcoded 100,000 balance; result changes if the journal isn't chronologically ordered, and the % scale is arbitrary.
- **D12. Dummy RND moments inconsistent with its own density (E3b)** — returned mean/skew/kurtosis/prob-above/below are hardcoded and do not match the skew-normal nodes returned alongside them; downstream consumers mixing `density` and `mean` get contradictory answers. Whole fallback is fabricated data.
- **D13. Fabricated defaults presented as data (E4/E6/E18)** — RV fallbacks 0.145/0.138/0.142, VRP from those, POP fallback 0.55, Brier 0.16, ECE 0.08, winRate 0.65, avgReturn 1100, expectancy defaults (1500/1000) and the injected 3-setup expectancy table. The rebuild should return explicit "insufficient data" instead.
- **D14. Heston level-term truncation (E15)** — variance update is `v ← v⁺ + κ(θ−v⁺)dt + ξ√v⁺√dt·z2` (v itself replaced by v⁺), an absorption/full-truncation hybrid; Lord et al. full truncation keeps the possibly-negative `v` in the level term and truncates only inside drift/diffusion (`v ← v + κ(θ−v⁺)dt + ξ√v⁺√dt·z2`). Small positive bias in variance near the origin.
- **D15. Timestamp coercion silently changes annualization (E4)** — `Number(isoString) || 0` maps ISO-string candle times to 0, so `intervalMinutes` falls back to 5-minute bars; daily candles with string timestamps get annualized as 5-min data (large RV error). Suspected correct: parse ISO dates.
- **D16. Scale-dependent breakeven dedupe (E6)** — absolute $2 tolerance and $0.1 rounding merge genuinely distinct breakevens on low-priced underlyings; should scale with spot.
- **D17. Alert threshold 0 unreachable + no edge detection (E19)** — `if (r.metric === 'spot' && r.thresholdValue)` skips thresholdValue = 0; 'above'/'below'/'gex_negative' re-fire every evaluation while the condition holds (alert spam), only 'crosses'/'gex_flip' are edge-triggered.
- **D18. pnlPct blow-up on near-zero entry (E8)** — entry floored at $0.01 then used as the pct denominator; a near-free contract shows astronomically large percentages. Consider reporting absolute-only below a premium floor.
- **D19. `reachedTarget` overloaded for sideways reads (E16)** — set to `pinned` rather than any target-touch semantics; consumers reading it as "price reached the target level" get the wrong meaning for sideways reads.
- **D20. Charm/vanna clock timezone (E11)** — labels hardcode "EST" with no DST handling; the 14:00 ramp is meant as US-market ET wall-clock. Cosmetic but should use exchange-timezone-aware times in the rebuild.
- **D21. Dominant GEX strike is per-contract (E10)** — `dominantStrike` picks the single contract with max |gex|, not the strike with max aggregated |call gex + put gex|; a strike with large offsetting call and put OI can beat the reported one.

Not defects (deliberate, keep): seeded deterministic RNG in the MC engine (tests are seeded); flat-smile degenerate fallback in E3 step 3; abstain-with-50 percentiles; single 'Front' GEX node; Kelly win/loss weighting by the same RND measure as POP; spot-scaled dK in B-L; interpolated (never uniform-fallback) density lookup.

## Discarded

- Alert message strings / emoji-toned copy in `evaluateAlertRules` — UI copy, not logic (port the conditions, not the prose).
- `formatTime(new Date())` timestamping inside `evaluateAlertRules` — impure clock glue; inject the timestamp.
- 12-hour "hh:mm AM/PM" label formatting in `generateCharmVannaClock` — presentation.
- `errorDivergences.reason` substring-matched narrative strings (E18 step 10) — decorative labels, not math; keep only the error magnitudes if ported.
- `generateDummyRND` hardcoded "beautiful, realistic" placeholder (E3b) — demo data; rebuild should surface "insufficient chain" instead.
- Injected default expectancy table and all fabricated fallback statistics (D13) — demo filler.
- `calculateVolatilityCone`'s dead `_yzVol` parameter — unused.
- `MCResult.histogram` 48-bin render clipping — chart convenience; recompute client-side if desired.
- Rounding-for-display everywhere (`toFixed`, round to $0.1 / 1dp / 2dp) — presentation concern; the rebuild should keep full precision internally and round at the UI edge (but note E8's best/worst correctly compares unrounded fractions).
- `AlertDispatch.type` color semantics (info/warning/danger) — UI severity mapping.
