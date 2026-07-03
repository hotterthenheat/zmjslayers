# SkyVision V11 — Mathematical Core

Source files covered (legacy TypeScript, `src/lib/`):

- `v11Math.ts` (1744 LOC) — the flagship V11 quant pipeline: BSM pricing/greeks, market-state sub-scores, dealer positioning, probability calibration, tail risk, liquidity, model trust, KNN retrieval, opportunity quality, decision gate, master orchestrator.
- `skyVisionEngine.ts` (640 LOC) — SkyVision v2.0 contract-intelligence layers 1–7 (contract strength, EMA targets, swings, position health, dynamic exits, master score).
- `skyQuantCore.ts` (420 LOC) — audited quant core: BSM + higher-order greeks, dealer aggregation, gamma flip, barrier-touch probability, BSM inversion, PSS sub-scores, trade management.
- `skyScore.ts` (367 LOC) — V5/V5.1 SkyScore contract ranker (runs after a directional BUY).

Cross-cluster helpers whose exact math this spec inlines because the engines depend on it: `normalize.ts` (normalizers), `technicalEngine.ts::emaLast/emaSeries`, `snapshotStore.ts::SnapshotStore`, `normalDist.ts::stdNormalCDF/stdNormalPDF` (Hart/West double-precision standard-normal CDF/PDF, abs err ~1e-15 — reimplement with any equivalent-accuracy erf-based CDF), `dealerSignals.ts::DEFAULT_DEALER_COUPLING` (interface only; that engine belongs to another cluster).

Notation: `N(x)` = standard normal CDF, `φ(x)` = standard normal PDF, `clamp(v,a,b) = min(max(v,a),b)`, `clamp01(v) = clamp(v,0,1)`. All IVs are annualized decimals (0.15 = 15%). DTE is in calendar days; `T = dte/365` years. Day count is ACT/365 everywhere. Contract multiplier = 100 shares.

---

## Overview

This cluster is the signal brain of the trading terminal. Given (a) OHLCV candles of the underlying, (b) an options chain snapshot (strike, type, OI, IV, bid/ask, greeks), and (c) a spot price, it computes:

1. **Thesis Stability / SystemScore** (0–100) — a weighted blend of price structure, momentum, VWAP alignment, volume expansion, and a dealer proxy.
2. **Dealer positioning** — net/gross GEX, DEX, VEX, charm; call/put walls; the canonical cumulative-GEX gamma-flip price; a Dealer State Index mapped to `dealer01 ∈ [0,1]`; expected move.
3. **Probability machinery** — isotonic (PAV) calibration of a raw win probability against a labeled outcome history, Wilson confidence intervals, ECE/Brier calibration diagnostics.
4. **Risk** — historical VaR/ES tail risk over similar-trade returns, liquidity scoring, model-trust scoring.
5. **The decision** — a hard-gated BUY/WAIT (or HOLD/REDUCE/EXIT when a position is open) plus a 0–100 Opportunity Quality score, price targets with projected option values and probabilities, and an outcome distribution.
6. **Contract-level intelligence** (SkyVision v2) — a 0–100 Contract Strength score from the contract's own time series, EMA target ladders with honest barrier-touch probabilities, swing detection, position health, and five dynamic exit triggers.
7. **Contract ranking** (SkyScore V5.1) — cross-sectional ranking of eligible contracts on a BUY by positioning density, dealer influence, acceleration, EMA-path repriced return, and liquidity, plus a convexity score.
8. **PSS (Position Strength Score)** machinery in `skyQuantCore` — flow/dealer/technical/positioning/vol sub-scores, weighted composite, and a decay-based trade-management state machine.

Everything is deterministic and pure except where explicitly noted (mock KNN database, wall-clock leakage filter, `SnapshotStore` timestamps).

---

## Engines

### E1. Seeded PRNG (`SeededRandom`, v11Math.ts)

**Purpose.** Deterministic random source so mock/KNN data replays identically for a given seed.

**Inputs.** `seed: integer` (any number; used mod 2^32).

**Algorithm.**
1. LCG step: `seed ← (seed * 1664525 + 1013904223) mod 4294967296`; `next() = seed / 4294967296` ∈ [0,1). (Numerical Recipes LCG; all intermediates < 2^53 so exact in f64.)
2. `nextRange(min,max) = min + next()*(max-min)`.
3. `nextNormal(mean,sd)` — Box–Muller with cached spare: if a spare exists return `mean + sd*spare` and clear it. Else `u1 = clamp(next(), 1e-12, 1-1e-12)`, `u2 = next()`, `radius = sqrt(-2·ln u1)`, `spare = radius·cos(2πu2)`, return `mean + sd·(radius·sin(2πu2))`.

| value | where used | semantic meaning | proposed name |
|---|---|---|---|
| 1664525 | LCG | multiplier | LCG_MULT |
| 1013904223 | LCG | increment | LCG_INC |
| 4294967296 | LCG | modulus 2^32 | LCG_MOD |
| 1e-12 | Box-Muller | u1 floor/ceiling offset (avoid log 0) | BOX_MULLER_U1_EPS |

**Outputs.** uniform f64 in [0,1); normal deviates.

---

### E2. Net Option P&L (`computeNetOptionPnL`, v11Math.ts)

**Purpose.** Round-trip P&L net of commissions and slippage for one option position.

**Inputs.** `entryOptionPrice`, `exitOptionPrice` ($/share), `bidAskSpread` ($/share), `contractsCount` (default 1).

**Algorithm.**
1. `grossPnL = (exit − entry) * 100 * contracts`.
2. `commissions = 0.65 * contracts * 2` (per contract per side, two sides).
3. `slippage = bidAskSpread * 100 * contracts` (half spread per side × 2 sides = 1 full spread).
4. `netPnL = grossPnL − commissions − slippage`.
5. Per-share form: `commissionPerShare = 0.65/100`; `halfSpread = spread/2`; `netEntry = entry + halfSpread + commissionPerShare`; `netExit = exit − halfSpread − commissionPerShare`; `netReturnPct = netEntry > 0 ? (netExit − netEntry)/netEntry : 0` (fraction, not %).

| value | where | meaning | name |
|---|---|---|---|
| 0.65 | commissions | $ per contract per side | COMMISSION_PER_CONTRACT |
| 100 | scaling | shares per contract | CONTRACT_MULTIPLIER |
| 2 | commissions | sides per round trip | ROUND_TRIP_SIDES |
| 0.5 (as spread/2) | slippage model | half-spread paid per side | HALF_SPREAD_FRACTION |

**Outputs.** `{grossPnL, commissions, slippage, netPnL ($), netReturnPct (fraction)}`.

---

### E3. Black–Scholes–Merton pricing and greeks — TWO implementations

There are two BSM stacks; the rebuild should keep ONE (skyQuantCore's is the audited, more complete one) but must know both because downstream consumers differ.

#### E3a. `computeBlackScholesPrice` / `calculateAnalyticGreeks` (v11Math.ts)

**Inputs.** `spot`, `strike` ($), `dteDays` (calendar days), `iv` (annualized decimal), `isCall`, `r = 0.05`, `q = 0`.

**Price algorithm.**
1. `T = max(0.0001, dteDays/365)`; `sigma = max(0.01, iv)`.
2. Guard: if `!(spot > 0) || !(strike > 0)` return `0.05` (also catches NaN).
3. `d1 = (ln(spot/strike) + (r − q + sigma²/2)·T) / (sigma·√T)`; `d2 = d1 − sigma·√T`.
4. Call: `price = S·e^{−qT}·N(d1) − K·e^{−rT}·N(d2)`. Put: `price = K·e^{−rT}·N(−d2) − S·e^{−qT}·N(−d1)`.
5. Return `max(0.05, price)` — **hard $0.05 floor on every price** (see defects D5).

**Greeks algorithm** (same `T`, `sigma`, `d1`, `d2`; `eqT = e^{−qT}`, `nd1 = φ(d1)`):
- `gamma = eqT·φ(d1) / (S·sigma·√T)`
- `vega  = S·eqT·√T·φ(d1)` (per 1.00 vol)
- Call: `delta = eqT·N(d1)`; `theta_annual = −S·eqT·φ(d1)·sigma/(2√T) + q·S·eqT·N(d1) − r·K·e^{−rT}·N(d2)`
- Put: `delta = eqT·(N(d1) − 1)`; `theta_annual = −S·eqT·φ(d1)·sigma/(2√T) − q·S·eqT·N(−d1) + r·K·e^{−rT}·N(−d2)`
- **theta returned DAILY**: `theta = theta_annual / 365`
- `vanna = −eqT·φ(d1)·d2/sigma` (∂Δ/∂σ)
- Charm (∂Δ/∂t, decay convention), annual then **returned DAILY** (`/365`):
  - call: `charmAnnual = q·eqT·N(d1) − eqT·φ(d1)·((r−q)/(sigma·√T) − d2/(2·max(0.0001,T)))`
  - put: `charmAnnual = −q·eqT·N(−d1) − eqT·φ(d1)·((r−q)/(sigma·√T) − d2/(2·max(0.0001,T)))`
  - `charm = charmAnnual/365`
- `speed = −(gamma/max(S,1e-9))·(d1/(sigma·√T) + 1)` (∂³V/∂S³).

| value | where | meaning | name |
|---|---|---|---|
| 0.05 | r default | risk-free rate | DEFAULT_RISK_FREE_RATE |
| 0 | q default | dividend yield | DEFAULT_DIV_YIELD |
| 0.0001 | T floor | min year-fraction | MIN_TAU_YEARS |
| 0.01 | sigma floor | min IV | MIN_SIGMA |
| 0.05 | price floor | min option price ($) | MIN_OPTION_PRICE |
| 365 | theta/charm | per-day conversion | DAYS_PER_YEAR |
| 1e-9 | speed | spot divide guard | SPOT_EPS |

#### E3b. `bsmPrice` / `bsmGreeks` (skyQuantCore.ts) — canonical

**Inputs.** `S, K, tau (years), r, sigma, q = 0, otype ∈ {call, put}`.

**Price.** If `tau ≤ 0 || sigma ≤ 0` return intrinsic `max(0, call ? S−K : K−S)` — no floor otherwise. Else the same BSM formulas as E3a with **no 0.05 floor and no input clamping**.

**Greeks.** If `tau ≤ 0 || sigma ≤ 0 || S ≤ 0 || K ≤ 0` return all-zero greeks. Else with `srt = sigma·√tau`, `dq = e^{−q·tau}`, `dr = e^{−r·tau}`, `pdf = φ(d1)`:
- `gamma = dq·pdf/(S·srt)`; `vega = S·dq·pdf·√tau`; `vanna = −dq·pdf·d2/sigma`; `speed = −(gamma/S)·(d1/srt + 1)`
- `vomma = vega·d1·d2/sigma`; `zomma = gamma·(d1·d2 − 1)/sigma`
- `ultima = (−vega/sigma²)·(d1·d2·(1 − d1·d2) + d1² + d2²)`
- `veta = vega·(q + (r−q)·d1/srt − (1 + d1·d2)/(2·tau))` — ∂Vega/∂t (decay sign convention, i.e. = −∂Vega/∂τ of the textbook form)
- `color = (dq·pdf/(2·S·tau·srt)) · (2q·tau + 1 + ((2(r−q)·tau − d2·srt)/srt)·d1)` — ∂Γ/∂t, same decay convention
- `charmCommon = dq·pdf·(2(r−q)·tau − d2·srt)/(2·tau·srt)`
- call: `delta = dq·N(d1)`; `theta = −S·dq·pdf·sigma/(2√tau) − r·K·dr·N(d2) + q·S·dq·N(d1)` (PER YEAR); `charm = q·dq·N(d1) − charmCommon` (PER YEAR)
- put: `delta = −dq·N(−d1)`; `theta = −S·dq·pdf·sigma/(2√tau) + r·K·dr·N(−d2) − q·S·dq·N(−d1)`; `charm = −q·dq·N(−d1) − charmCommon`

**Unit difference:** E3a returns theta/charm per DAY; E3b per YEAR. Downstream code (chain contracts fed to `computeDealerInventory`) uses E3a's daily charm.

**Outputs.** price ($/share); greeks in standard units as above.

---

### E4. Wilder RSI & ATR (`calculateWilderRSI`, `calculateWilderATR`, v11Math.ts)

**Purpose.** Full-series RSI(14) and ATR(14) with Wilder smoothing.

**Inputs.** `candles: {open,high,low,close,volume,vwap?}[]` (chronological).

**RSI algorithm.**
1. Output array initialized to 50 everywhere. If `len < 15`, return all-50s.
2. `deltas[i] = close[i+1] − close[i]` for i = 0..len−2.
3. Seed: `avgGain = Σ max(deltas[0..13], 0)/14`, `avgLoss = Σ max(−deltas[0..13],0)/14`; `rsi[14] = avgLoss==0 ? 100 : 100 − 100/(1 + avgGain/avgLoss)`.
4. For `i = 15..len−1`: `d = deltas[i−1]`; `avgGain = (avgGain·13 + max(d,0))/14`; `avgLoss = (avgLoss·13 + max(−d,0))/14`; `rsi[i]` per the same formula.

**ATR algorithm.**
1. Output array initialized to 0.1. If `len < 15`, return all-0.1s.
2. `tr[0] = high[0]−low[0]`; `tr[i] = max(high−low, |high−prevClose|, |low−prevClose|)`.
3. Seed `atr[13] = mean(tr[0..13])`; for `i = 14..`: `atr[i] = (atr[i−1]·13 + tr[i])/14`. (NOTE: RSI seeds at index 14, ATR at index 13 — one bar of asymmetry, preserve as-coded.)

| value | where | meaning | name |
|---|---|---|---|
| 14 | both | Wilder period | WILDER_PERIOD |
| 15 | both | min candles required | WILDER_MIN_BARS |
| 50 | RSI default | neutral RSI fill | RSI_NEUTRAL |
| 0.1 | ATR default | fallback ATR fill | ATR_FALLBACK |
| 100 | RSI | scale | RSI_SCALE |

**Outputs.** `rsi[]` 0–100; `atr[]` in price units, same length as input.

---

### E5. Fractal pivots & Structure01 (`calculateFractalPivots`, `computeStructure01`, v11Math.ts)

**Purpose.** Market-structure quality in {0.00, 0.33, 0.66, 1.00} from swing highs/lows.

**Pivot detection.** Window half-width `L = 2`. For each `i ∈ [L, n−L)`: `high[i]` is a pivot high iff `high[i+j] < high[i]` for all `j ∈ [−L,L], j≠0` (**strict**: ties kill the pivot); pivot low symmetric with `low[i+j] > low[i]`. Needs `n ≥ 2L+1 = 5`.

**Structure01 inputs.** `pHighs`, `pLows` (pivot lists), `atr` (current ATR), `dir` (+1 call / −1 put).

**Algorithm.**
1. If fewer than 2 highs or 2 lows → return 0.5.
2. `lastH, prevH` = last two pivot-high prices; `lastL, prevL` likewise. `eps = 0.1·atr`.
3. If `|lastH−prevH| < eps && |lastL−prevL| < eps` → return 0.33 (range-bound).
4. Merge all pivots, sort by index, collapse into an alternating high/low sequence: consecutive same-type pivots keep the more extreme one (higher high / lower low).
5. Leg sizes: default `lastLegSize = lastH − lastL`, `priorLegSize = prevH − prevL`; if the alternating list has ≥ 3 pivots, override with `lastLegSize = p[-1].price − p[-2].price`, `priorLegSize = p[-2].price − p[-3].price`. `shrinkingLeg = |lastLegSize| < |priorLegSize|`.
6. Bullish (`dir > 0`): `HH = lastH > prevH`, `HL = lastL > prevL`; `activePulldown` = the last alternating pivot is a LOW; `hasShrinkingPullback = activePulldown && shrinkingLeg`.
   - `HH && HL && hasShrinkingPullback` → 1.00; `HH || HL` → 0.66; `lastL < prevL && lastH < prevH` → 0.00; else 0.33.
7. Bearish mirror: `LH = lastH < prevH`, `LL = lastL < prevL`, last alternating pivot is a HIGH & shrinking → `LH && LL && shrinking` → 1.00; `LH || LL` → 0.66; both higher → 0.00; else 0.33.

| value | where | meaning | name |
|---|---|---|---|
| 2 | fractal | pivot half-window | FRACTAL_HALF_WINDOW |
| 0.1 | structure | ATR fraction for "equal" pivots | PIVOT_EQ_ATR_FRAC |
| 0.5 | structure | insufficient-pivots neutral | STRUCT_NEUTRAL |
| 0.33 / 0.66 / 1.00 / 0.00 | structure | quality rungs | STRUCT_{RANGE,PARTIAL,FULL,COUNTER} |

**Outputs.** `struct01 ∈ {0, 0.33, 0.5, 0.66, 1}`.

---

### E6. Momentum / VWAP / Volume kernels (v11Math.ts)

**computeMomentum01(momVel, rsiSlope, m0 = 2).** `divPen = (sign(momVel) ≠ sign(rsiSlope) && rsiSlope ≠ 0) ? 0.5 : 1.0`; result `= clamp01(tanh(momVel/m0) · divPen)`. Negative momentum clamps to 0.

**computeVWAP01(close, vwap, atr, dir, d_peak = 0.5, sigma_v = 0.6).** If `atr ≤ 0` (or NaN) return 0. `d = dir·(close − vwap)/atr`; if `d ≤ 0` return 0; else log-normal kernel `exp(−ln²(d/d_peak) / (2·sigma_v²))` — peaks at 1.0 when price sits `0.5·ATR` beyond VWAP in trade direction.

**computeVolume01(rvol, rvol_full = 2).** `clamp01((rvol − 1)/(rvol_full − 1))` — RVOL 1→0, RVOL ≥2→1.

| value | where | meaning | name |
|---|---|---|---|
| 2 | momentum | tanh scale (ATR-units of 10-bar move) | MOMENTUM_TANH_SCALE |
| 0.5 | momentum | divergence penalty multiplier | MOMENTUM_DIVERGENCE_PENALTY |
| 0.5 | vwap kernel | peak distance (ATRs from VWAP) | VWAP_PEAK_ATR |
| 0.6 | vwap kernel | log-space kernel width | VWAP_KERNEL_SIGMA |
| 2 | volume | RVOL for full score | RVOL_FULL |

---

### E7. SystemScore / Thesis Stability composite (`calculateSystemScoreFromCandles`, v11Math.ts)

**Purpose.** The 0–100 Thesis Stability plus ten 0–10 sub-indicator readouts.

**Inputs.** `candles[]`, `dir ∈ {+1,−1}`, `atrVal` (fallback ATR).

**Algorithm** (n = candles.length, `last` = candles[n−1]):
1. Empty input → `{total: 50, all sub-scores 5}`.
2. `rsis = WilderRSI(candles)`, `atrs = WilderATR(candles)`; `currentRSI = rsis[n−1]`; `currentATR = atrs[n−1] || atrVal`.
3. `rsi_5 = n≥6 ? rsis[n−6] : 50`; `close_10 = n≥11 ? candles[n−11].close : candles[0].close`.
4. `rsiSlope = dir·(currentRSI − rsi_5)`; `momVel = dir·(last.close − close_10)/(currentATR || 1)`.
5. RVOL: `rvolLookback = min(20, n−1)`; `rvolSum = Σ_{i=2}^{rvolLookback+1} candles[n−i].volume` (i.e. the `rvolLookback` bars immediately before the live bar); `meanVol = rvolSum/(rvolLookback || 1)`; `rvol = last.volume/(meanVol || 1)`.
6. `struct01 = computeStructure01(fractalPivots(candles), currentATR, dir)`.
7. `mom01 = computeMomentum01(momVel, rsiSlope)`.
8. VWAP block: `currentVWAP = last.vwap || last.close`; `vwap01_kernel = computeVWAP01(last.close, currentVWAP, currentATR, dir)`;
   `vwap_slope = dir·(currentVWAP − (n≥6 ? (candles[n−6].vwap || candles[n−6].close) : currentVWAP))/(currentATR || 1)`;
   `crossedBefore = n≥4 && (dir>0 ? candles[n−2].close ≤ (candles[n−2].vwap||close) : candles[n−2].close ≥ (candles[n−2].vwap||close))`;
   `crossedBackNow = dir>0 ? last.close > currentVWAP : last.close < currentVWAP`;
   `reclaim = (crossedBackNow && crossedBefore) ? 1 : 0`;
   `vwap01_full = clamp01(0.6·vwap01_kernel + 0.25·clamp01((vwap_slope+1)/2) + 0.15·reclaim)`.
9. Volume block: `vol01_base = computeVolume01(rvol)`;
   `prevRVOLSum = n≥6 ? Σ_{idx over candles[n−6..n−2]} calculateRVOL(candles, idx) : 5`; `meanRVOL_5 = prevRVOLSum/5`; `rvol_trend = sign(rvol − meanRVOL_5)`;
   where `calculateRVOL(candles, idx)`: if `idx < 2` return 1.0; baseline = mean volume of `candles[max(0,idx−20) .. idx−1]`; return `volume[idx]/baseline` (1.0 if baseline 0);
   `prevVol = n≥2 ? candles[n−2].volume : 1`; `vol_accel = clamp((last.volume − prevVol)/max_1(prevVol), −1, 1)` with `max_1(x) = x ≤ 0 ? 1 : x`;
   `volume01_full = clamp01(0.6·vol01_base + 0.25·clamp01((rvol_trend+1)/2) + 0.15·clamp01((vol_accel+1)/2))`.
10. `atr_10 = n≥11 ? atrs[n−11] : atrs[0]`; `atr_expansion = clamp01(((currentATR/(atr_10 || 1)) − 1)/0.5)`.
11. Dealer proxy (candles-only): `dealer01 = clamp01(0.5·(1 + dir·(last.close − currentVWAP)/(currentATR || 1)))`.
12. **Thesis Stability** `= clamp(100·(0.25·struct01 + 0.25·mom01 + 0.20·vwap01_full + 0.15·volume01_full + 0.15·dealer01), 1, 100)`.
13. Sub-score mapping (all `Math.round`): `total = round(thesisStability)`; `displacementQuality = structureQuality = struct01·10`; `volumeExpansion = volume01_full·10`; `rsiCascade = mom01·10`; `vwapAlignment = vwap01_full·10`; `liquiditySweep = dealer01·10`; `htfAgreement = thesisStability/10`; `volatilityRegime = atr_expansion·10`; `premiumDiscount = (1−vwap01_full)·10`; `momentumAcceleration = round(clamp(momVel/4, −1, 1)·5 + 5)`.

| value | where | meaning | name |
|---|---|---|---|
| 0.25/0.25/0.20/0.15/0.15 | stability blend | struct/mom/vwap/volume/dealer weights | THESIS_W_* |
| 1, 100 | stability | output clamp bounds | THESIS_MIN/MAX |
| 5 (bars) | rsiSlope | RSI lookback | RSI_SLOPE_LOOKBACK |
| 10 (bars) | momVel | close lookback | MOM_LOOKBACK |
| 20 | RVOL | baseline window | RVOL_BASELINE_BARS |
| 0.6/0.25/0.15 | vwap01_full | kernel/slope/reclaim weights | VWAP_BLEND_* |
| 0.6/0.25/0.15 | volume01_full | base/trend/accel weights | VOLUME_BLEND_* |
| 0.5 | atr_expansion | 50% ATR growth ⇒ full score | ATR_EXPANSION_SCALE |
| 4 | momentumAcceleration | momVel scale | MOM_ACCEL_SCALE |
| 50 / 5 | empty default | neutral total / sub-scores | SCORE_NEUTRAL |

**Outputs.** `SystemScore` = `{total: 1..100 int, ten sub-scores 0..10 int}`.

---

### E8. Dealer Positioning Engine (`computeDealerInventory` v11Math.ts + `netGexStrike`/`netVannaStrike`/`netCharmStrike`/`gammaFlipSpot`/`expectedMove(Pct)` skyQuantCore.ts)

**Purpose.** Aggregate an options chain into dealer exposure metrics, walls, gamma-flip price, a directional dealer score, and expected move.

**Inputs.** `chain: ChainContract[]` (strike, type, openInterest [contracts], iv, bid, ask, delta, gamma, vega, theta, vanna, charm, volume?), `spot` ($), `dir ∈ {+1,−1}`, `dte` days (default 1).

**Per-contract exposures** (sign = +1 call, −1 put):
- `GEX_strike = gamma · OI · 100 · spot² · 0.01 · sign` ($ per 1% spot move)
- `DEX_strike = delta · OI · 100 · spot · sign`
- `VEX_strike = vanna · OI · 100 · spot · 0.01 · sign` ($ per 1% IV move)
- `Charm_strike = charm · OI · 100 · sign` (charm here is DAILY per E3a)

Accumulate `netGex/netDex/netVex/netCharm = Σ signed`, `grossGex/grossDex/grossVex = Σ |signed|`; keep per-strike lists.

**skyQuantCore per-strike pair forms (canonical, used by other engines):**
- `netGexStrike(oiC,gammaC,oiP,gammaP,S) = (oiC·gammaC − oiP·gammaP)·100·S²·0.01`
- `netVannaStrike(...) = (oiC·vannaC − oiP·vannaP)·100·S·0.01`
- `netCharmStrike(oiC,charmC,oiP,charmP,timeDecayFactor=1) = (oiC·charmC − oiP·charmP)·100·timeDecayFactor`

**Gamma flip (`gammaFlipSpot(strikes[], netGexByStrike[])`)** — SqueezeMetrics cumulative convention:
1. Sum GEX per unique strike (call+put rows merge; non-finite strikes skipped; missing gex → 0).
2. Sort strikes ascending; need ≥ 2 unique strikes else return null.
3. Cumulative sum `cums[i]`; scan i: if `cums[i] == 0` return `strike[i]`; if `cums[i]·cums[i+1] < 0` return linear interpolation `x0 − y0·(x1−x0)/(y1−y0)`.
4. No crossing → null (**abstain**).

In `computeDealerInventory`: `gammaFlipConfident = (flip ≠ null)`; fallback `gammaFlip = spot·0.995` when not confident.

**Walls.** For each contract compute `|GEX_strike|`. Call wall = call strike with max |GEX| **at or above spot** (fallback: global max call strike; initial default `spot·1.015`). Put wall = put strike with max |GEX| **at or below spot** (fallback global; default `spot·0.985`). `wallsConfident = maxCallGexAbs > 0 && maxPutGexAbs > 0` (after fallback substitution).

**Dealer State Index.**
- `e_GEX = tanh(3·netGex/(grossGex || 1))`, same for DEX, VEX.
- `DSI = 0.50·dir·e_GEX + 0.30·dir·e_DEX + 0.20·dir·e_VEX`
- `dealer01 = clamp01(0.5·(DSI + 1))`.

**Expected move.** ATM IV = iv of the contract with strike nearest spot (0.15 if chain empty). `expectedMovePct = max(0.0005, atmIV·√(max(dte/365, 0.0001)))` — a FRACTION of spot. (`expectedMove(S,sigma,tau) = S·sigma·√tau` is the $ version.)

| value | where | meaning | name |
|---|---|---|---|
| 100 | exposures | contract multiplier | CONTRACT_MULTIPLIER |
| 0.01 | GEX/VEX | per-1% scaling | PCT_MOVE_SCALE |
| 3 | e_* normalization | tanh steepness on net/gross | EXPOSURE_TANH_K |
| 0.50/0.30/0.20 | DSI | GEX/DEX/VEX weights | DSI_W_GEX/DEX/VEX |
| 0.995 | flip fallback | fallback flip = spot·0.995 | FLIP_FALLBACK_MULT |
| 1.015 / 0.985 | wall defaults | fallback call/put wall | CALL/PUT_WALL_FALLBACK_MULT |
| 0.15 | ATM IV fallback | default IV | DEFAULT_ATM_IV |
| 0.0005 | EM floor | min expected-move fraction | EM_PCT_FLOOR |
| 0.0001 | EM tau floor | min year-fraction | EM_TAU_FLOOR |
| 1 | dte default | assumed DTE (days) | DEFAULT_DTE_DAYS |

**Outputs.** `{netGex, netDex, netVex, netCharm ($-scaled), callWall, putWall, gammaFlipPrice ($), gammaFlipConfident, wallsConfident, grossGex, dealer01 ∈ [0,1], gexStrikes[], dexStrikes[], vexStrikes[], expectedMovePct (fraction)}`.

---

### E9. Mock chain generator (`generateMockOptionsChain`, v11Math.ts)

**Purpose.** Synthesizes a 42-contract chain (21 strikes × call/put) when no live chain is supplied. Deterministic given (spot, ivBase). Needed to reproduce legacy outputs in mock mode.

**Algorithm.**
1. `step = spot > 1000 ? 50 : spot > 150 ? 5 : 1`; `centerStrike = round(spot/step)·step`; `chainDte = 5` days; strikes at `offset = −10..+10`.
2. `strikeDistance = |spot − strike|/spot`.
3. IV smiles: `skewVolCall = ivBase·(1.1 − offset·0.015 + 1.5·strikeDistance²)`; `skewVolPut = ivBase·(1.1 + offset·0.02 + 1.5·strikeDistance²)`.
4. `roundFactor = strike % (5·step) == 0 ? 2.2 : strike % (2·step) == 0 ? 1.35 : 1.0`.
5. `callMoneyness = exp(−(offset−2)²/18)`; `putMoneyness = exp(−(offset+2)²/18)`.
6. `oiCall = round((1500·callMoneyness + 120)·roundFactor)`; `oiPut = round((1800·putMoneyness + 140)·roundFactor)`.
7. Greeks/prices from E3a at `chainDte`, respective skew vol. `bid = max(0.05, bs·0.97)`, `ask = max(0.10, bs·1.03)` (2 dp).
8. `volNear = 0.25 + 0.6·exp(−strikeDistance·12)`; `volume = round(oi·volNear)`.

| value | where | meaning | name |
|---|---|---|---|
| 50 / 5 / 1 | step | strike step by spot tier | MOCK_STRIKE_STEP_* |
| 1000 / 150 | step tiers | spot breakpoints | STRIKE_TIER_HI/MID |
| 5 | chainDte | mock chain DTE days | MOCK_CHAIN_DTE |
| 10 | offsets | strikes each side | MOCK_STRIKES_PER_SIDE |
| 1.1 / 0.015 / 0.02 / 1.5 | smiles | base lift / call skew / put skew / smile curvature | MOCK_SMILE_* |
| 2.2 / 1.35 | roundFactor | OI boost at 5-step / 2-step strikes | ROUND_STRIKE_OI_* |
| ±2 / 18 | moneyness | OI hump center offset / width | OI_HUMP_CENTER/WIDTH |
| 1500 / 120 | call OI | scale / floor | CALL_OI_SCALE/FLOOR |
| 1800 / 140 | put OI | scale / floor | PUT_OI_SCALE/FLOOR |
| 0.97 / 1.03 | bid/ask | ±3% around model price | MOCK_BID/ASK_MULT |
| 0.05 / 0.10 | bid/ask floors | min bid / min ask | MIN_BID/MIN_ASK |
| 0.25 / 0.6 / 12 | volNear | volume-vs-OI base / ATM boost / decay | VOLNEAR_* |

---

### E10. Probability calibration (`calculateWilsonConfidence`, `calibrateIsotonicLoss`, `calculateECE`, `calculateBrierScore`, v11Math.ts)

**Wilson interval** `(p, n, z = 1.96)`: if `n ≤ 0` return [0,0]. `center = (p + z²/(2n)) / (1 + z²/n)`; `half = z·√(p(1−p)/n + z²/(4n²)) / (1 + z²/n)`; return `[clamp01(center−half), clamp01(center+half)]`.

**Isotonic PAV calibration** `(pHat, history: {pred∈[0,1], win∈{0,1}}[])`:
1. Cold start: `history.length < 200` → return `pHat` unchanged.
2. Bucket into 10 bins by `min(9, floor(pred·10))`; per active bucket: `x = mean(pred)`, `y = winRate`, `w = count`.
3. PAV: while any adjacent violation `values[i] > values[i+1]`, pool: weighted-average both `values` and `x` by `w`, sum weights, splice out i+1; restart scan.
4. Interpolate: `pHat ≤ x[0]` → `values[0]`; `pHat ≥ x[last]` → `values[last]`; else linear between bracketing pooled x's (`t = (pHat−x_i)/(x_{i+1}−x_i)`, 0 if denom 0).

**ECE** (10 bins): empty history → 0.05. `ece = Σ_bins (count_b/N)·|meanOutcome_b − meanPred_b|`.

**Brier**: empty → 0.15; else `mean((pred − win)²)`.

| value | where | meaning | name |
|---|---|---|---|
| 1.96 | Wilson | z for 95% CI | WILSON_Z_95 |
| 200 | isotonic | cold-start min samples | CALIBRATION_MIN_SAMPLES |
| 10 | isotonic/ECE | bin count | CALIBRATION_BINS |
| 0.05 | ECE default | empty-history ECE | DEFAULT_ECE |
| 0.15 | Brier default | empty-history Brier | DEFAULT_BRIER |

---

### E11. Percentile & Tail risk (`calculatePercentile`, `computeTailRisk`, v11Math.ts)

**Percentile(values, pct).** Empty → 0. Sort ascending; `index = clamp((clamp(pct,0,100)/100)·(len−1), 0, len−1)`; linear interpolation between `floor` and `ceil` indices.

**computeTailRisk(returns, es_max = 1.00).** `losses = −returns` (fractional). `var95 = P95(losses)`, `var99 = P99(losses)`. `es95 = mean(losses ≥ var95)` (var95 if none), `es99` likewise. `worstOutcome = max(losses)` (1.00 if empty). `tailRiskScore = clamp01(es95/es_max)`.

| value | where | meaning | name |
|---|---|---|---|
| 95 / 99 | VaR | percentile levels | VAR_LEVEL_95/99 |
| 1.00 | es_max | ES for full tail-risk score (=100% loss) | ES_MAX |
| 1.00 | worst default | empty-history worst loss | DEFAULT_WORST_LOSS |

**Outputs.** All in fractional-return units; `tailRiskScore ∈ [0,1]`.

---

### E12. Liquidity score (`computeLiquidityScore`, v11Math.ts)

**Inputs.** `bid, ask` ($), `contractVolume` (contracts), `openInterest` (contracts), `priorMids[]` ($), `stability_max = 0.05` (a VARIANCE, $²).

**Algorithm.**
1. `mid = (bid+ask)/2`; `spreadPercent = mid > 0 ? (ask−bid)/mid : 0.05`; `spreadScore = clamp01(1 − spreadPercent/0.10)`.
2. `volumeScore = clamp01(log10(max(volume,1))/4)` (10,000 contracts → 1.0); `oiScore = clamp01(log10(max(oi,1))/4)`.
3. Sample variance of priorMids (n−1 denominator; 0 if < 2 samples); `quoteStability = clamp01(1 − variance/stability_max)`.
4. `liquidityScore = round(100·(0.40·spreadScore + 0.25·volumeScore + 0.25·oiScore + 0.10·quoteStability))`.

| value | where | meaning | name |
|---|---|---|---|
| 0.10 | spread | max spread% for any credit | SPREAD_MAX_PCT |
| 0.05 | default spreadPercent | when mid ≤ 0 | SPREAD_FALLBACK |
| 4 | volume/oi | log10 scale (1e4 saturates) | LOG_LIQ_SCALE |
| 0.05 | stability_max | variance for zero stability ($²) | QUOTE_VAR_MAX |
| 0.40/0.25/0.25/0.10 | blend | spread/volume/oi/stability weights | LIQ_W_* |

**Outputs.** `liquidityScore` 0–100 int; four sub-scores ∈ [0,1].

---

### E13. Model trust (`computeModelTrust`, v11Math.ts)

**Inputs.** `eceValue ∈ [0,1]`, `predActualDeltas[]` (forecast−actual), `predictionsHistory[]` (recent predicted probabilities), `n_similar` (sample count), `recent100Wins` (win rate of last 100, ∈[0,1]), `forecastScale = 0.15`, `pred_var_max = 0.04`.

**Algorithm.**
1. `meanAbsError = mean(|predActualDeltas|)` (0.12 if empty); `forecastError = clamp01(meanAbsError/forecastScale)`.
2. Sample variance of predictionsHistory (n−1); `predictionStability = clamp01(1 − variance/pred_var_max)`.
3. `sampleStrength = clamp01((log10(max(n_similar,1)) − 1)/2)` — n=10→0, n=1000→1.
4. `recentPerformance = recent100Wins`.
5. `trust = 0.30·(1−ece) + 0.20·(1−forecastError) + 0.20·predictionStability + 0.15·sampleStrength + 0.15·recentPerformance`.
6. Grade: `≥0.80 A; ≥0.60 B; ≥0.40 C; ≥0.20 D; else F`.

| value | where | meaning | name |
|---|---|---|---|
| 0.12 | empty default | mean abs error fallback | DEFAULT_FORECAST_MAE |
| 0.15 | forecastScale | MAE for full error score | FORECAST_ERROR_SCALE |
| 0.04 | pred_var_max | prediction variance ceiling | PRED_VAR_MAX |
| 1, 2 | sampleStrength | log10 offset / span | SAMPLE_LOG_OFFSET/SPAN |
| 0.30/0.20/0.20/0.15/0.15 | trust blend | component weights | TRUST_W_* |
| 0.80/0.60/0.40/0.20 | grades | A/B/C/D cutoffs | GRADE_* |

**Outputs.** `trustScore ∈ [0,1]`, components, letter grade.

---

### E14. KNN similar-trade engine (`computeKNNMatches`, v11Math.ts) — MOCK

**Purpose.** Produces a deterministic pseudo-historical database of "similar trades" (the production data source was never wired in). The rebuild should replace this with a real store, but must document the mock because EV, tail risk, targets and drawdowns all derive from it.

**Algorithm.**
1. Seed: `prng = SeededRandom(ticker.charCodeAt(0) + d_sign(dir)·99)` where `d_sign(dir) = dir ≥ 0 ? 1 : 2`. (Note: only the FIRST character of the ticker matters — see defects.)
2. `databaseSize = 1000`; `baseWin = dir > 0 ? 0.73 : 0.64`.
3. Per row i: `roll = next()`; `win = roll < baseWin`;
   `mae = nextRange(0.01, win ? 0.08 : 0.22)`; `mfe = nextRange(win?0.05:0.01, win?0.35:0.05)`;
   `realReturn = win ? nextRange(0.04, 0.45) : −nextRange(0.05, 0.25)`;
   `matchRating = nextRange(74, 98)`; `holdMins = round(nextRange(15, 180))`;
   ticker from `[asset.ticker, ticker=='SPX'?'SPY':'SPX', 'QQQ','IWM','DIA']` at index `floor(next()·5)`;
   date string `` `2026-0${floor(next()*5)+1}-${floor(next()*25)+1} ${floor(next()*6)+9}:${floor(next()*59)}` `` (months 01–05, days 1–25, hours 9–14, minutes 0–58 — NOT zero-padded);
   `regime = roll > 0.5 ? 'Volatility Expansion' : 'Volatility Compression'`; `maxDrawdown = mae·100` (%), `maxExcursion = mfe·100` (%), `pnlMultiplier = realReturn` (fraction, 4 dp).
4. Leakage gate: keep rows where `new Date(date.replace(' ','T')).getTime()` is a valid number `< Date.now()` (local-time parse of a NON-ISO string — see defects).
5. Sort by `similarityRating` desc; return top `max(30, n_requested)`.

| value | where | meaning | name |
|---|---|---|---|
| 99 | seed | direction seed offset | KNN_SEED_DIR_OFFSET |
| 1000 | db | mock database size | KNN_DB_SIZE |
| 0.73 / 0.64 | baseWin | call / put prior win rate | KNN_BASEWIN_CALL/PUT |
| 0.01–0.08 / 0.01–0.22 | MAE | win / loss MAE range (fraction) | KNN_MAE_* |
| 0.05–0.35 / 0.01–0.05 | MFE | win / loss MFE range | KNN_MFE_* |
| 0.04–0.45 / 0.05–0.25 | return | win / loss return range | KNN_RET_* |
| 74–98 | rating | similarity range | KNN_RATING_RANGE |
| 15–180 | hold | minutes range | KNN_HOLD_RANGE |
| 0.5 | regime | roll threshold expansion/compression | KNN_REGIME_SPLIT |
| 30 | slice | min matches returned | KNN_MIN_MATCHES |

---

### E15. Opportunity Quality (`calculateOpportunityQuality`, v11Math.ts)

`norm_ev = clamp01((ev − 0.0)/(0.30 − 0.0))` (ev fractional; 30% EV saturates).
`score = clamp(25·norm_ev + 20·p_cal + 15·(1 − tailRiskScore) + 15·(liquidityScore/100) + 10·trustScore + 10·sampleStrength + 5·regimeStability, 0, 100)`.

| value | where | meaning | name |
|---|---|---|---|
| 0.0 / 0.30 | EV norm | floor / ceiling (fraction) | EV_FLOOR / EV_CEILING |
| 25/20/15/15/10/10/5 | blend | EV/p/tail/liq/trust/sample/regime points | OQ_W_* (sum 100) |

---

### E16. Decision Gate (`evaluateDecisionGate`, v11Math.ts) + dealer-coupling hook

**Inputs.** `positionOpen: bool`, `ev` (fraction), `p_cal ∈ [0,1]`, `rr` (ratio), `tailRiskScore ∈ [0,1]`, `liquidityScore` 0–100, `n_size` (samples), `trustScore ∈ [0,1]`, `dealer01 ∈ [0,1]`, `thesisStability` 0–100.

**Hard invalidation predicate** (both branches): `thesisStability < 40 || p_cal < 0.50 || ev < 0 || liquidityScore < 30 || trustScore < 0.20`.

**Position OPEN branch:**
1. Hard invalidated → `EXIT` (reason lists each failed clause).
2. Else `thesisStability < 70 || tailRiskScore > 0.70 || dealer01 < 0.50` → `REDUCE`.
3. Else `thesisStability ≥ 70 && ev > 0 && tailRiskScore ≤ 0.70` → `HOLD` ("optimal"), else `HOLD` ("standard"). (Both emit HOLD; only reason text differs.)

**Position CLOSED branch (entry checklist)** — ALL must pass for `BUY`, else `WAIT` with the failed list:
`ev > 0`; `p_cal > 0.62`; `rr > 1.50`; `tailRiskScore ≤ 0.70`; `liquidityScore ≥ 60`; `n_size ≥ 30`; `trustScore ≥ 0.40`; `dealer01 ≥ 0.60`. (NaN rr fails; the failure-list check `passRR2` is the identical `rr > 1.5` predicate.)

**Dealer-coupling hook (V5.1 Phase 3, default OFF).** `DEFAULT_DEALER_COUPLING = {enabled: false, trapHighThreshold: 70, flipNearThreshold: 0.7, stressSizeK: 0.5, sizeFloor: 0.5}`. When enabled, `modulateDecision` (dealerSignals cluster) computes `sizeMultiplier = clamp(1 − 0.5·clamp(dealerStressIndex,0,100)/100, 0.5, 1)` and can veto a BUY when trap score ≥ 70 with flip proximity ≥ 0.7. With the shipped default (`enabled:false`) the multiplier is exactly 1 and the decision is untouched — the rebuild may treat this as dead unless the flag is resurrected.

| value | where | meaning | name |
|---|---|---|---|
| 40 | invalidation | min thesis stability | GATE_STABILITY_FLOOR |
| 0.50 | invalidation | min p_cal | GATE_PCAL_FLOOR |
| 30 | invalidation | min liquidity | GATE_LIQ_FLOOR_OPEN |
| 0.20 | invalidation | min trust | GATE_TRUST_FLOOR_OPEN |
| 70 | REDUCE | thesis stability threshold | GATE_STABILITY_REDUCE |
| 0.70 | REDUCE/BUY | max tail risk | GATE_TAIL_MAX |
| 0.50 | REDUCE | min dealer01 open | GATE_DEALER_REDUCE |
| 0.62 | BUY | min p_cal | GATE_PCAL_BUY |
| 1.50 | BUY | min risk/reward | GATE_RR_MIN |
| 60 | BUY | min liquidity | GATE_LIQ_BUY |
| 30 | BUY | min sample size | GATE_SAMPLE_MIN |
| 0.40 | BUY | min trust | GATE_TRUST_BUY |
| 0.60 | BUY | min dealer01 | GATE_DEALER_BUY |
| 70 / 0.7 / 0.5 / 0.5 | coupling cfg | trap thresh / flip prox / stress K / size floor | DEALER_COUPLING_* |

**Outputs.** `{decision ∈ {BUY,WAIT,HOLD,REDUCE,EXIT}, reason: string}` (+ `sizeMultiplier ∈ [0.5,1]` from the hook).

---

### E17. V11 master pipeline (`calculateV11Metrics` + `calculateV10Metrics`, v11Math.ts)

**Purpose.** Orchestrates E3–E16 into the flagship result blob. Every derived formula here matters for parity.

**Inputs.** `asset {ticker, defaultPrice, volatility, stabilityMax?, forecastScale?}`, `isCall`, `systemScore` (E7), `optionPremiumFloat` ($), `optionStrike?`, `liveChain?`, `liveSpot?`, `dteDays = 1`, `calibrationHistory: {pred, win}[] = []`.

**Algorithm.**
1. `dir = isCall ? 1 : −1`; `spotUsed = liveSpot || asset.defaultPrice`.
2. `step = spotUsed > 1000 ? 100 : spotUsed > 150 ? 5 : 1`; `determinedStrike = optionStrike || round(spotUsed/step)·step + (isCall ? +step : −step)` (one step OTM). **Note step 100 vs the mock chain's 50 for spot > 1000.**
3. `actualChain = liveChain || generateMockOptionsChain(spotUsed, asset.volatility)`; `dealerRes = computeDealerInventory(actualChain, spotUsed, dir, dteDays)`.
4. **Integrity:** start 100 points; `optionPremiumFloat ≤ 0.05` → −25; `asset.volatility ≤ 0.01 || > 4.5` → −20. `score = max(0, pts)`, `isValid = pts ≥ 75`; `greeksConsistency = pts≥90 ? OPTIMAL : pts≥75 ? MARGINAL : CRITICAL`; `chainIntegrity = pts≥80 ? FULLY_INTEGRIFIED : MISALIGNED_GAPS`; `timestampAlignment` is a hardcoded string.
5. **Base rate:** if `calibrationHistory.length ≥ 30`: `baseWinRate = mean(win)`, `sampleSize = length`; else priors `baseWinRate = isCall ? 0.72 : 0.65`, `sampleSize = isCall ? 487 : 404`.
6. `resolvedSimilarTrades = computeKNNMatches(asset, dir, systemScore, sampleSize)`; `matchedReturns = pnlMultiplier[]` (fractions).
7. **Calibrated probability:** `rawWinRate = systemScore.total/100`; `calibratedP = calibrateIsotonicLoss(rawWinRate, calibrationHistory)`; `activeWilInterval = Wilson(calibratedP, sampleSize)` (computed, unused downstream).
8. **EV:** `evSum = mean(matchedReturns)` (`Σ/ (len || 1)`); if NaN → `calibratedP·0.18 + (1−calibratedP)·(−0.08)`.
9. Display stats: winners `r > 0`, losers `r ≤ 0`; `avgGainPct = mean(winners)·100` (18.2 if none); `avgLossPct = |mean(losers)|·100` (7.6 if none).
10. `expectedDrawdownPct = mean(maxDrawdown)` (already in %); `rrRatio = expectedDrawdownPct > 0 ? (evSum·100)/expectedDrawdownPct : 2.5`.
11. `tailRisk = computeTailRisk(matchedReturns, 1.00)`.
12. **Liquidity (fabricated inputs):** `bid = premium·0.98`, `ask = premium·1.02`, volume 14500, OI 18800, `priorMids[i] = premium + sin(i)·0.01` for i = 0..14, `stability_max = asset.stabilityMax || 0.05`.
13. **Trust (partly fabricated):** `predictionsHist[i] = 0.72 + cos(i)·0.02` (30 pts); `eceVal = calculateECE(calibrationHistory)`; `predActualDeltas = matchedReturns.map(r → r − calibratedP)`; `recent100Wins = 0.741`; `forecastScale = asset.forecastScale || 0.15`.
14. `thesisStability = systemScore.total`; `decResult = evaluateDecisionGate(false, evSum, calibratedP, rrRatio, tailRisk.tailRiskScore, liquidity.liquidityScore, sampleSize, modelTrust.trustScore, dealerRes.dealer01, thesisStability)` — **positionOpen is ALWAYS false here.** Dealer coupling per E16 (default no-op; when on, `volExpansionNorm = 0.5` hardcoded and per-contract `speed` recomputed via E3a).
15. `opportunityQuality = calculateOpportunityQuality(evSum, calibratedP, tailRiskScore, liquidityScore, trustScore, sampleStrength, 0.85)` (regimeStability hardcoded 0.85).
16. **Surface (display):** `impliedVolSpread = vol·0.035`; term structure multipliers on asset.volatility: 0DTE 1.25, 1DTE 1.15, 7DTE 1.0, 14DTE 0.97, 30DTE 0.94; `skewCurve = first 5 chain rows` labeled OTM PUT if strike < spot else OTM CALL; `smileSymmetricFactor = 0.12`; `ivRank = round(35 + vol·100)`; `ivPercentile = round(28 + vol·120)`; `expectedMovePct` from E8.
17. **Dealer block:** copies E8 outputs; `dealerPressureIndex = round(dealer01·10)`; text: netGex > 0 → "NET POSITIVE GAMMA (STABILISING FORCE)" else "NET NEGATIVE GAMMA (ACCELERATIVE VECTOR)"; netCharm > 0 → charm-buy-tailwinds text else charm-sell-friction text; vanna text constant.
18. **Entry zone:** `[premium·0.91, premium·0.97]`.
19. **Targets:** return percentiles of matchedReturns: `t1 = P50`, `t2 = P70`, `t3 = P85`, `stretch = P95`. Per-target base probabilities: `p_ti = clamp(calibratedP·mult_i, 0.01, 0.99)` with mults `0.95, 0.78, 0.60, 0.38`; Wilson intervals at `n = sampleSize`; **reported `probability` = Wilson LOWER bound ×100**; `historicalHitRate = round(p_ti·100)`. Underlying prices: `spot·(1 ± expectedMovePct·m)` with `m ∈ {0.35, 0.70, 1.20, 2.00}` (+ for calls, − for puts). `optionValue = premium·(1 + t_pct)`. `expectedTimeMinutes ∈ {14, 28, 45, 95}` hardcoded. `expectedDrawdownPct = var95·100·f`, `f ∈ {0.25, 0.50, 0.65, 0.85}`. `riskReward = t_pct/(var95 || 0.1)`.
20. **Outcome distribution:** raw probs `p_large = calP·0.45` ("Target 2–Stretch", value `avgGainPct·1.6`), `p_mid = calP·0.55` ("Target 1", `avgGainPct·0.9`), `p_flat = (1−calP)·0.65` ("Flat drift", `−avgLossPct·0.5`), `p_stop = (1−calP)·0.35` ("Stop hunt", `−avgLossPct·1.4`); normalize probabilities to sum 100; `contribution = (prob/100)·averageValuePct`.
21. **Feature importances:** weights = each of {structureQuality, rsiCascade, vwapAlignment, volumeExpansion, liquiditySweep} (each `|| 1`) divided by their sum, ×100 rounded.
22. `xgbAdjustPct = (systemScore.total − 75)·0.08` (2 dp).
23. **Valuation:** `optionModelPrice = BSM(spotUsed, determinedStrike, dteDays, asset.volatility, isCall)` (E3a); `premiumSurchargePct = 100·(premium − model)/(model || 1)`; label: `≤ −2.5` UNDERVALUED (isUndervalued true), `≥ +2.5` OVERVALUED, else FAIRLY_PRICED. `fairValue = optionModelPrice`.
24. Scalar constants surfaced: `expectedHoldTimeMinutes = 24`, `expectedSlippagePct = 1.25`.

**`calculateV10Metrics` (compat decomposition).** Given the v11 result: `totalOffsetNeeded = posteriorWinRate − baseWinRate` (both %, 1 dp). Component 0–1 profiles: `d = dealerPressureIndex/10`, `r = rsiCascade/10`, `v = volumeExpansion/10`, `vw = vwapAlignment/10`, `reg = volatilityRegime/10`; deviations `x_dev = x − 0.5`; `sumDevs = Σ|dev| || 1`; offsets `= totalOffsetNeeded · (x_dev/sumDevs || fallback)` with fallbacks dealer 0.15, rsi 0.25, volume 0.15, vwap 0.20 (**the `||` fires when the share is exactly 0 or NaN — see defects**); `regimeOffset = postWin − (baseWin + dealerOffset + rsiOffset + volumeOffset + vwapOffset)` (residual so the sum is exact). Also returns `avgGainPct = 18.2` hardcoded, `avgLossPct = v11.expectedDrawdownPct`.

Pipeline constants not already tabled:

| value | where | meaning | name |
|---|---|---|---|
| 100 / 5 / 1 | strike step | pipeline step by spot tier | PIPELINE_STRIKE_STEP_* |
| 0.05 | integrity | premium floor for penalty | INTEGRITY_PREMIUM_MIN |
| 25 / 20 | integrity | premium / IV penalty points | INTEGRITY_PENALTY_* |
| 0.01 / 4.5 | integrity | IV sanity bounds | IV_MIN/IV_MAX |
| 75 / 90 / 80 | integrity | valid / optimal / chain thresholds | INTEGRITY_THRESH_* |
| 30 | base rate | min real samples | REAL_SAMPLE_MIN |
| 0.72 / 0.65 | priors | call / put base win rate | PRIOR_WINRATE_CALL/PUT |
| 487 / 404 | priors | call / put pseudo sample size | PRIOR_N_CALL/PUT |
| 0.18 / −0.08 | EV fallback | win / loss payoff assumption | EV_FALLBACK_WIN/LOSS |
| 18.2 / 7.6 | display fallback | avg gain / loss % | DEFAULT_AVG_GAIN/LOSS_PCT |
| 2.5 | rr fallback | RR when MAE = 0 | RR_FALLBACK |
| 0.98 / 1.02 | liquidity | synthetic bid/ask multipliers | SYN_BID/ASK_MULT |
| 14500 / 18800 | liquidity | hardcoded volume / OI | SYN_VOLUME / SYN_OI |
| 15 / 0.01 | liquidity | synthetic mids count / sin amplitude | SYN_MIDS_N/AMP |
| 30 / 0.72 / 0.02 | trust | fabricated prediction history len/center/amp | SYN_PRED_* |
| 0.741 | trust | hardcoded recent-100 win rate | SYN_RECENT_WR |
| 0.85 | OQ | hardcoded regime stability | SYN_REGIME_STABILITY |
| 0.5 | coupling | hardcoded volExpansionNorm | SYN_VOL_EXPANSION_NORM |
| 0.035 | surface | IV spread fraction | SURFACE_IV_SPREAD_FRAC |
| 1.25/1.15/1.0/0.97/0.94 | surface | term-structure multipliers | TERM_MULT_* |
| 5 | surface | skew-curve sample rows | SKEW_SAMPLE_N |
| 0.12 | surface | smile symmetric factor | SMILE_SYM_FACTOR |
| 35, 100 / 28, 120 | surface | ivRank / ivPercentile affine coefs | IVRANK_A,B / IVPCT_A,B |
| 0.91 / 0.97 | entry zone | premium multipliers | ENTRY_ZONE_LO/HI |
| 50/70/85/95 | targets | return percentiles T1..stretch | TARGET_PCTL_* |
| 0.95/0.78/0.60/0.38 | targets | probability multipliers | TARGET_PROB_MULT_* |
| 0.01 / 0.99 | targets | probability clamps | TARGET_PROB_MIN/MAX |
| 0.35/0.70/1.20/2.00 | targets | EM multiples for prices | TARGET_EM_MULT_* |
| 14/28/45/95 | targets | expected minutes | TARGET_ETA_MIN_* |
| 0.25/0.50/0.65/0.85 | targets | drawdown fractions of var95 | TARGET_DD_FRAC_* |
| 0.1 | targets | var95 divide fallback | TARGET_VAR95_FALLBACK |
| 0.45/0.55/0.65/0.35 | outcomes | large/mid/flat/stop splits | OUTCOME_SPLIT_* |
| 1.6/0.9/0.5/1.4 | outcomes | value multipliers | OUTCOME_VALUE_MULT_* |
| 75 / 0.08 | xgbAdjust | pivot score / slope | XGB_PIVOT/SLOPE |
| ±2.5 | valuation | surcharge thresholds (%) | VALUATION_THRESH_PCT |
| 24 / 1.25 | scalars | hold minutes / slippage % | DEFAULT_HOLD_MIN / DEFAULT_SLIPPAGE_PCT |
| 0.15/0.25/0.15/0.20 | v10 offsets | fallback weight shares | V10_FALLBACK_SHARE_* |

**Outputs.** `V11MathResult` — see interface at v11Math.ts:1237 (all fields covered above; percentages are ×100 human units, EV as `expectedValuePct` in %, probabilities as % 1 dp).

---

### E18. Contract Strength Score & rotation scanner (`scoreContract`, `rankContractStrengths`, skyVisionEngine.ts)

**Purpose.** 0–100 strength of a single option contract from its own recent time series — the SkyVision v2 Layer-2 primitive.

**Inputs.** `history: ContractSnapshot[]` = `{t, premium, volume, oi, delta (signed), gamma, theta (daily), vega, iv}`; `isCall`.

**Core helper.** `netRel(series) = len < 2 ? 0 : (last − first)/(|first| + 1e-9)` — net relative change over the window.

**Algorithm.**
1. `n < 2` → `{score 50, trend FLAT, confidence round(clamp(n·12, 0, 24)), label "Insufficient data", factors all 0}`.
2. Window `w = last 12 snapshots`. `deltaDir = isCall ? delta : −delta` (put strengthening = delta more negative).
3. Factors (each ∈ [−1,1]): `premium = tanh(3.5·netRel(premium))`; `delta = tanh(3.5·netRel(deltaDir))`; `gamma = tanh(3.0·netRel(gamma))`; `volume = tanh(2.0·netRel(volume))`; `oi = tanh(3.0·netRel(oi))`; `iv = tanh(4.0·netRel(iv))`.
4. `signal = 0.25·premium + 0.2·delta + 0.15·gamma + 0.2·volume + 0.1·oi + 0.1·iv` (weights sum 1).
5. `score = clamp(50 + 50·signal, 0, 100)` (1 dp); `trend = signal > 0.08 ? RISING : signal < −0.08 ? FALLING : FLAT`.
6. Confidence: `agree = signal == 0 ? 0 : (#factors with sign(f) == sign(signal) && |f| > 0.05)/6`; `dataSuff = clamp01((|w|−1)/11)`; `confidence = round(clamp(30 + 50·agree·(0.5 + 0.5·min(1, |signal|·2.5))·dataSuff + 15·dataSuff, 0, 99))`.
7. Label: `≥85 Strong Buy; ≥70 Buy; ≥58 Accumulate; >42 Neutral; ≥30 Weak; else Avoid`.

**Rotation scanner.** Sort by `score` desc, tie-break `confidence` desc; assign `rank = i+1`, `strongest = (i == 0)`.

| value | where | meaning | name |
|---|---|---|---|
| 1e-9 | netRel | denom epsilon | NETREL_EPS |
| 12 | window | snapshots per score | STRENGTH_WINDOW |
| 0.25/0.2/0.2/0.15/0.1/0.1 | weights | premium/delta/volume/gamma/oi/iv | STRENGTH_W_* |
| 3.5/3.5/3.0/2.0/3.0/4.0 | factor tanh | premium/delta/gamma/volume/oi/iv steepness | STRENGTH_K_* |
| 0.08 | trend | rising/falling threshold on signal | TREND_SIGNAL_THRESH |
| 0.05 | agreement | min \|factor\| to count | AGREE_MIN_FACTOR |
| 30 / 50 / 15 | confidence | base / agreement span / data bonus | CONF_BASE/AGREE_SPAN/DATA_BONUS |
| 2.5 | confidence | signal amplification | CONF_SIGNAL_AMP |
| 99 | confidence | ceiling | CONF_MAX |
| 12, 24 | low-data conf | per-sample points, cap | LOWDATA_CONF_* |
| 85/70/58/42/30 | labels | verdict cutoffs | LABEL_* |

**Outputs.** `{score 0–100, trend, confidence 0–99, label, factors, samples}`.

---

### E19. EMA ladder, target stack, premium projection (Layer 3, skyVisionEngine.ts)

**EMA ladder.** `computeEmaLadder(closes)` = EMA 15/20/50/200 of closes via shared `emaLast`; `converged200 = closes.length ≥ 200`. EMA definition (`technicalEngine.emaSeries`): `k = 2/(period+1)`; if `n ≥ period`, seed with SMA of the first `period` values at index `period−1` (indices 0..period−1 all hold the seed), then `out[i] = v[i]·k + out[i−1]·(1−k)`; else running EMA from the first print. `emaLast` = last element (last raw value or 0 if empty).

**Target stack (`buildTargetStack`).** Candidates: EMA15/EMA20 (tier 1), EMA50 (tier 2), EMA200 (tier 3), gamma wall & the in-direction wall — call wall for calls / put wall for puts (tier 4), expected-move high/low (tier 5). Keep only finite prices `> 0` strictly beyond spot in trade direction; sort nearest-first (asc for calls, desc for puts); de-dupe any level within `max(spot·0.0002, 0.01)` of an already-kept one. `distancePct = (level − spot)/spot` (signed, 5 dp).

**Premium projection (`projectTargetPremiums`).**
1. `dte = max(0.02, dteDays)`; `entry = max(0.01, entryPremium ?? BSM(spot, strike, dte, iv, isCall, r=0.05))`; `em = spot·iv·√(dte/365)` ($, 1σ); `tauYears = dte/365`.
2. Per level i: `distEm = em > 0 ? |level − spot|/em : 0`; `elapsedFrac = clamp(distEm², 0, 0.85)` (GBM: time-to-reach ∝ distance²; cap keeps ≥15% of time value); `remDte = max(0.0007, dte·(1 − elapsedFrac))`; `prem = max(0.01, BSM(level, strike, remDte, iv, isCall, r))`.
3. `projectedGainPct = 100·(prem − entry)/entry`; `touchProb = barrierTouchProb(spot, level, iv, tauYears, r, 0, false)` (driftless — E24), 3 dp; `rank = i+1`.

| value | where | meaning | name |
|---|---|---|---|
| 15/20/50/200 | ladder | EMA periods | EMA_PERIODS |
| 200 | convergence | bars for EMA200 trust | EMA200_MIN_BARS |
| 0.0002 / 0.01 | dedupe | relative / absolute gap | TARGET_DEDUPE_REL/ABS |
| 0.02 | dte floor | min DTE days | PROJ_DTE_FLOOR_DAYS |
| 0.01 | premium floor | min entry/projected premium | PROJ_PREMIUM_FLOOR |
| 0.85 | elapsedFrac cap | max time consumed | PROJ_ELAPSED_CAP |
| 0.0007 | remDte floor | min remaining DTE (days ≈ 1 min) | PROJ_REMDTE_FLOOR |
| 0.05 | r default | risk-free | DEFAULT_RISK_FREE_RATE |

**`snapshotFromMarket`** — builds a ContractSnapshot from (t, spot, strike, dteDays, iv, isCall, volume, oi, r=0.05) using E3a price + greeks; rounding: premium 2 dp, delta 4 dp, gamma 6 dp, theta 2 dp, vega 2 dp.

---

### E20. Swing detection & EMA structure score (Layer 4, skyVisionEngine.ts)

**detectSwings inputs.** `isCall`, `emas`, `history` (window = last 12), optional `dealerAligned` override.

**Shared change tests** (require `w.length ≥ 3`): `half = max(2, floor(len/2))`; `deltaAccel = netRel(deltaDir[-half:]) > netRel(deltaDir[:half])`; `volumeUp = netRel(volume) > 0.02`; `premiumExpand = netRel(premium) > 0.02`; `oiBuild = netRel(oi) > 0.01`; `ivUp = netRel(iv) > 0.005`.

**Short-term scalp:** `fastAligned = dir>0 ? ema15 > ema20 : ema15 < ema20`; conditions `[fastAligned, deltaAccel, volumeUp, premiumExpand]`; `strength = round(100·count/4)`; `detected = fastAligned && count ≥ 3`; duration: `strength ≥ 90 → "45-60 min"; ≥ 75 → "30-45 min"; else "5-15 min"` (— when not detected).

**Long-term trend:** `slowAligned = dir>0 ? ema50 > ema200 : ema50 < ema200`; `dealerAligned = params.dealerAligned ?? oiBuild`; conditions `[slowAligned, ivUp, dealerAligned, oiBuild]`; same count/strength/detected rule; duration: `≥90 → "1-2 weeks"; ≥75 → "3-7 days"; else "1-2 days"`.

Direction = BULLISH for calls / BEARISH for puts when detected, else NONE.

**emaStructureScore(spot, emas, isCall).** Conditions (bull; bear mirrored with <): `spot > ema15`, `ema15 > ema20`, `ema20 > ema50`, plus `ema50 > ema200` ONLY if `converged200`. Score = `round(100·passed/total)` (total 3 or 4).

| value | where | meaning | name |
|---|---|---|---|
| 3 | swings | min window for change tests | SWING_MIN_SAMPLES |
| 0.02 / 0.02 / 0.01 / 0.005 | thresholds | volume / premium / OI / IV net-rel | SWING_THRESH_* |
| 3 (of 4) | detection | min passing conditions | SWING_MIN_CONDS |
| 90 / 75 | duration tiers | strength cutoffs | SWING_DURATION_TIER_* |

---

### E21. Position Health (Layer 5, `assessPositionHealth`, skyVisionEngine.ts)

Reuses E18 on the live position's history:
- `score ≥ 70 && trend ≠ FALLING` → Strong / Hold
- `score ≥ 55 && trend ≠ FALLING` → Healthy / Hold
- `score ≥ 40` → Weakening / Reduce
- else → Critical / Exit

Signals (plain text) from factors at ±0.1: premium expanding/stalling/flat; delta strengthening/weakening/steady; volume rising/fading (only if |f| > 0.1); IV expanding/collapsing (only if |f| > 0.1).

| value | where | meaning | name |
|---|---|---|---|
| 70 / 55 / 40 | health tiers | strong/healthy/weakening floors | HEALTH_TIER_* |
| 0.1 | signals | factor threshold for narration | HEALTH_SIGNAL_THRESH |

---

### E22. Dynamic Exit Engine (Layer 6, `evaluateDynamicExits`, skyVisionEngine.ts)

Five independent triggers evaluated per tick over `w = last 12 snapshots`; all firing signals returned sorted by severity desc.

1. **EMA_TARGET** — input flag `emaTargetHit` true → `{SCALE, severity 0.4}` ("take 25%").
2. **STRENGTH_COLLAPSE** — needs `strengthSeries.length ≥ 3`; `peak = max(series)`, `lastS = last`; fire `{EXIT, 0.9}` iff `peak − lastS ≥ 20 && lastS < 62`.
3. **FLOW_REVERSAL** — for calls: `callSweeps < prevCallSweeps && putSweeps > prevPutSweeps` → `{EXIT, 0.85}`; puts mirrored.
4. **GAMMA_WALL** — wall finite `> 0` and `(isCall && spot ≥ wall) || (!isCall && spot ≤ wall)` → `{TAKE_PROFIT, 0.6}`.
5. **IV_CRUSH** — needs `w.length ≥ 3`; `netRel(iv) ≤ −0.04 && netRel(premium) ≤ 0.0` → `{EXIT, 0.7}`.

| value | where | meaning | name |
|---|---|---|---|
| 0.4/0.9/0.85/0.6/0.7 | severities | per-trigger urgency | EXIT_SEVERITY_* |
| 20 / 62 | collapse | min drop from peak / max last score | COLLAPSE_DROP/COLLAPSE_FLOOR |
| −0.04 | IV crush | IV net-rel threshold | IVCRUSH_IV_THRESH |
| 0.0 | IV crush | premium stall threshold | IVCRUSH_PREM_THRESH |

---

### E23. SkyVision Master Score (Layer 7, `computeMasterScore`, skyVisionEngine.ts)

**Inputs.** Seven 0–100 components (each clamped to [0,100]): contractStrength, flowStrength, dealerPositioning, emaStructure, volumeProfile, ivStructure, swingEngine; plus direction/labels passthrough.

**Algorithm.** `score = round(0.25·contractStrength + 0.2·flowStrength + 0.15·dealerPositioning + 0.15·emaStructure + 0.1·volumeProfile + 0.1·ivStructure + 0.05·swingEngine)`. Confidence from dispersion: `sd = population std dev of the 7 components`; `confidence = round(clamp(100 − 1.2·sd, 30, 99))`. `tradeHealth = score ≥ 80 ? Strong : ≥ 60 ? Healthy : ≥ 45 ? Mixed : Weak`.

| value | where | meaning | name |
|---|---|---|---|
| 0.25/0.2/0.15/0.15/0.1/0.1/0.05 | blend | component weights | MASTER_W_* |
| 1.2 | confidence | sd penalty slope | MASTER_CONF_SD_K |
| 30 / 99 | confidence | clamp bounds | MASTER_CONF_MIN/MAX |
| 80 / 60 / 45 | tradeHealth | tier cutoffs | TRADE_HEALTH_* |

---

### E24. Barrier-touch probability & BSM inversion (skyQuantCore.ts §3–4)

**barrierTouchProb(S, K, sigma, tau, r = 0, q = 0, useDrift = false)** — P(underlying touches K before tau), reflection formula:
1. Degenerate (`tau ≤ 0 || sigma ≤ 0 || S ≤ 0 || K ≤ 0`) → 0.
2. `x = ln(K/S)`; `srt = sigma·√tau`; `nu = useDrift ? r − q − sigma²/2 : 0`.
3. `x > 0` (barrier above): `p = N((−x + nu·tau)/srt) + e^{2·nu·x/sigma²}·N((−x − nu·tau)/srt)`.
4. `x < 0` (below): `p = N((x − nu·tau)/srt) + e^{2·nu·x/sigma²}·N((x + nu·tau)/srt)`.
5. `x == 0` → 1. Clamp to [0,1]. (Driftless case collapses to `2·N(−|x|/srt)`.)

**spotForTargetPremium(targetPremium, S, K, tauEval, r, sigma, q, otype)** — bisection inversion of BSM in spot:
`targetPremium ≤ 0` → null. Bracket `[lo, hi] = [1e-6, 5·S]`; `f(s) = bsmPrice(s,...) − target`; if `f(lo)·f(hi) > 0` → null (unreachable). Up to 200 bisection steps; stop when `|f(mid)| < 1e-7` or `hi − lo < 1e-7`; return mid (or bracket midpoint after 200 iters).

**probOptionHitsTarget(entryPremium, targetMult, S, K, tauEntry, r, sigma, q, otype, tauEval?, useDrift = false).** `tauEval` defaults to `tauEntry`. Find `S* = spotForTargetPremium(entry·mult, ...)`; null → `{prob: 0, spotStar: null}`; else `{prob: barrierTouchProb(S, S*, sigma, tauEntry, r, q, useDrift), spotStar: S*}`.

**targetPctViaReprice(S, K, tau, r, sigma, entryPremium, q, otype, dS = 0, dSigma = 0, dt = 0).** `entry ≤ 0` → 0; else `100·(bsmPrice(S+dS, K, max(tau−dt, 0), r, sigma+dSigma, q, otype) − entry)/entry`.

| value | where | meaning | name |
|---|---|---|---|
| 1e-6 | bisection lo | spot lower bracket | INVERT_SPOT_LO |
| 5 | bisection hi | ×S upper bracket | INVERT_SPOT_HI_MULT |
| 200 | bisection | max iterations | INVERT_MAX_ITER |
| 1e-7 | bisection | value & width tolerance | INVERT_TOL |

---

### E25. Strike-chain metrics & normalizers (skyQuantCore.ts §5–6 + normalize.ts)

**nbrsRatio(values, idx, n = 3).** Window `[max(0, idx−n), min(len, idx+n+1))`; `neigh = Σ|values| in window − |values[idx]|`; `cnt = windowCount − 1`; if `cnt ≤ 0 || neigh ≤ 0` → 1; else `|values[idx]| / (neigh/cnt)` — target vs mean of neighbors, target excluded.

**oiVelocity(oiT, oiPrev, dtMinutes) = dtMinutes ? (oiT − oiPrev)/dtMinutes : 0** (contracts/min).
**oiMigration(strikes, oiT, oiPrev) = Σ (oiT_i − oiPrev_i)·strike_i** (strike-weighted OI flow).

**Normalizers (skyQuantCore):** `logisticScore(z, k=4) = 100/(1 + e^{−k·z})`; `ratioScore(x, k=4) = logisticScore(x − 1, k)` (x=1 → 50); `minmaxScore(v, lo, hi) = hi ≤ lo ? 50 : clamp01((v−lo)/(hi−lo))·100`; `percentileScore(v, history)` = right-side empirical CDF ×100 (`count(x ≤ v)/n·100`), 50 if empty.

**Normalizers (normalize.ts, used by SkyScore):** `normSaturate(x, cap) = 100·clamp01(x/cap)` (0 on non-finite/cap ≤ 0); `normLogSaturate(x, cap) = 100·clamp01(ln(max(x,1))/ln(max(cap,1.0000001)))`; `normSignedTanh(d, scale) = 50 + 50·tanh(d/scale)` (50 on non-finite d or zero scale); `percentile(values, p)` linear-interpolated order statistic; `normCrossSection(x, set)` = clip x to [P10(set), P90(set)] then `100·(x − P10)/(P90 − P10)`, degenerate/empty/non-finite → 50; `unit(s) = clamp01(s/100)`.

| value | where | meaning | name |
|---|---|---|---|
| 3 | NBRS | neighbor half-window (strikes) | NBRS_HALF_WINDOW |
| 4 | logistic | default steepness | LOGISTIC_K_DEFAULT |
| 10 / 90 | cross-section | winsorization percentiles | XSEC_P_LO/HI |
| 50 | all | neutral score | SCORE_NEUTRAL_100 |

---

### E26. PSS sub-scores & composite (skyQuantCore.ts §7)

**Config (`DEFAULT_ENGINE_CONFIG`, ALL flagged UNVALIDATED in source):** `wFlow 0.25, wDealer 0.25, wPositioning 0.2, wTechnical 0.15, wVol 0.15` (must sum to 1 within 1e-9 or `computePss` THROWS); `kLogistic 4.0`; `gammaRampMult 1.25, gammaRampCap 100`; `lqPenaltyThreshold 40, lqPenaltyFrac 0.25` (declared, not used in these files); `decaySoft 10, decayModerate 20, decayHard 30` (% drawdown from peak PSS); `pssFloor 70`.

**flowSubscore(netPrem5m, netPrem15m, netPrem30m, totalPrem30m, sweepVolAsk, sweepVolBid, openingAligned).**
`fp = totalPrem30m > 0 ? clamp(50 + 50·(net5 + net15 + net30)/total30, 0, 100) : 50`; `fa = (ask+bid) > 0 ? 100·ask/(ask+bid) : 50`; `of = openingAligned ? 100 : 0`; result `0.4·fp + 0.4·fa + 0.2·of`.

**dealerSubscore(compositeNow, compositeHistory, cfg, gammaRampActive = false).** `base = percentileScore(now, history)`; if ramp active `base = min(100, base·1.25)`.

**technicalSubscore(ema9, ema21, ema50, spot, vwap, highestHigh20, direction).** Bull checks: `[ema9 > ema21 && ema21 > ema50, spot > vwap, spot > ema9, spot > highestHigh20]` (bear mirrored with <); score = `passed/4·100`.

**positioningSubscore(oiVel, oiMig, oiNbrs, density0to100, cfg, oiVelScale = 500, oiMigScale = 1e6, nbrsCap = 20).** `velS = logisticScore(oiVel/500, 4)`; `migS = logisticScore(oiMig/1e6, 4)`; `nbrsS = minmaxScore(nbrs, 0, 20)`; `densS = clamp(density, 0, 100)`; result `0.3·velS + 0.3·migS + 0.2·nbrsS + 0.2·densS`.

**volSubscore(ivNow, iv20dAvg, atrNow, atr5ago, emEff, cfg).** `ivS = iv20dAvg > 0 ? ratioScore(ivNow/iv20dAvg, 4) : 50`; `atrS = atr5ago > 0 ? ratioScore(atrNow/atr5ago, 4) : 50`; `reachS = clamp((1 − clamp01(emEff))·100, 0, 100)` (emEff = fraction of expected move already consumed); result `0.4·ivS + 0.3·atrS + 0.3·reachS`.

**computePss(flow, dealer, positioning, technical, vol, cfg)** = validate weights then plain weighted average → [0,100].

| value | where | meaning | name |
|---|---|---|---|
| 0.25/0.25/0.2/0.15/0.15 | PSS | flow/dealer/positioning/technical/vol weights | PSS_W_* |
| 0.4/0.4/0.2 | flow | premium/aggression/opening weights | FLOW_W_* |
| 1.25 / 100 | dealer ramp | multiplier / cap | GAMMA_RAMP_MULT/CAP |
| 500 / 1e6 / 20 | positioning | oiVel / oiMig scales, NBRS cap | OIVEL_SCALE / OIMIG_SCALE / NBRS_CAP_PSS |
| 0.3/0.3/0.2/0.2 | positioning | vel/mig/nbrs/density weights | POS_W_* |
| 0.4/0.3/0.3 | vol | iv/atr/reach weights | VOL_W_* |
| 4.0 | logistic | kLogistic | PSS_LOGISTIC_K |
| 1e-9 | validation | weight-sum tolerance | WEIGHT_SUM_TOL |

---

### E27. PSS trade management (skyQuantCore.ts §8)

**confidenceDecayPct(pssMax, pssNow) = pssMax > 0 ? 100·(pssMax − pssNow)/pssMax : 0.**

**hardInvalidations(pssNow, netGexNow, netGex0, oiVelNow, oiVel0, flow5mFlipped, priceBrokeStructure, cfg)** → flags:
- `DEALER_FLIP`: `(netGexNow > 0 && netGex0 ≤ 0) || (netGexNow < 0 && netGex0 ≥ 0)` (explicit zero-edge handling).
- `OI_LIQUIDATION`: `oiVelNow < 0 && |oiVelNow| ≥ 0.5·|oiVel0|`.
- `FLOW_REVERSAL`: input boolean. `STRUCTURE_BREAK`: input boolean.
- `PSS_FLOOR`: `pssNow < cfg.pssFloor` (70).

**evaluateTrade(state, pssNow, ...)** — MUTATES `state.maxPss = max(maxPss, pssNow)`; `decay = confidenceDecayPct(maxPss, pssNow)`; any invalidation flag → `HARD_EXIT_CLOSE_ALL`; else `decay ≥ 30` → `HARD_EXIT_CLOSE_ALL` (DECAY_HARD); `≥ 20` → `TRIM_50`; `≥ 10` → `TRIM_25`; else `HOLD`. Returns `{action, reasons[], decayPct}`.

| value | where | meaning | name |
|---|---|---|---|
| 0.5 | OI liquidation | fraction of entry OI velocity | OI_LIQ_FRACTION |
| 10 / 20 / 30 | decay tiers | soft / moderate / hard % | DECAY_SOFT/MODERATE/HARD |
| 70 | floor | min PSS | PSS_FLOOR |

---

### E28. SkyScore V5.1 contract ranker (`rankContracts`, skyScore.ts)

**Purpose.** After a directional BUY, rank the chain's contracts (of the matching type) by expected opportunity. Cross-sectional: several scores are relative to the eligible candidate set C.

**Config defaults (`DEFAULT_V5_CONFIG`):** `minOI 250, minVolume 100, maxSpread 0.12, deltaMin 0.30, deltaMax 0.60; DENSITY_WINDOW 3, DENSITY_BASE 0.5, MIG_SCALE 0.02, OIV_SCALE 5000, NBRS_CAP 12; ACCEL_SIGN +1 (flagged CALIBRATION REQUIRED), dealerWallMagWeight 0.60, dealerAlignWeight 0.40; exposureWeights {gex 0.50, dex 0.30, vex 0.20}; VE_SCALE 1.5, GVEL_SCALE 5e8, VVEL_SCALE 5e7, LOOKBACK_BARS 5; SPOT_VOL_BETA −0.012, SIGMA_FLOOR 0.03, EMA_RET_SCALE 0.5, IV_PEN_K 1.0, IV_PEN_FLOOR 0.5; weights {positioning 0.25, dealer 0.25, acceleration 0.20, emaPath 0.20, liquidity 0.10} (CALIBRATION REQUIRED); convexityWeights {gammaVel 0.45, vannaVel 0.35, speed 0.20}; isExplosiveSky 85, isExplosiveConvexity 80`.

**Inputs.** `direction bullish|bearish`, `spot`, `dteDays`, `chain: RankerContract[]`, `emaTargets {ema5, ema9, ema20, ema50, ema200}`, `dataSource`, `totalChainVolume?`, `expectedMovePct?` (fraction), optional `SnapshotStore`.

**Algorithm.**
1. `targetType = bullish ? 'C' : 'P'`; `dir = ±1`.
2. **Eligibility** per contract (all reasons collected): type mismatch; `bid ≤ 0`; `mid = (bid+ask)/2 ≤ 0`; `oi < 250`; `volume < 100`; `spread = (ask−bid)/mid > 0.12` (spread = 1 when mid ≤ 0); `|delta| ∉ [0.30, 0.60]`. Eligible set C = zero reasons. Ineligible rows are still emitted with all scores 0.
3. **Exposure fallbacks** (sign = C ? +1 : −1): `gex = gexStrike ?? sign·gamma·oi·100·spot²·0.01`; `dex = dexStrike ?? sign·delta·oi·100·spot`; `vex = vexStrike ?? sign·vega·oi·100·spot·0.01` (**vega — see defect D4**).
4. `totalChainVolume` = supplied or `Σ chain volume` (full chain, both types). ATM IV = iv of nearest-spot target-type contract (0.15 fallback); `expectedMovePct` = supplied or `expectedMovePct(atmIv, dteDays/365)` (E8).
5. **Positioning density** (per contract, over target-type OI by strike): window of `±3` strike INDICES (sorted unique strikes; edge strikes just use fewer neighbors); `clusterMass = Σ OI`; `peak = max OI`; `peakShare = peak/max(clusterMass, 1)`; `embeddedness = 1 − peakShare`; `densityRaw = clusterMass·(0.5 + 0.5·embeddedness)`. `densityScore = normCrossSection(densityRaw, {densityRaw over C})`.
6. **Migration:** `volShareNow = volume/max(totalChainVolume, 1)`; with a prior snapshot (5 scans back via `SnapshotStore.prior(key, 5)`, key = `symbol|expiration|strike|type`): `volumeMigration = volShareNow − prev.volShare`; `migrationScore = normSignedTanh(volumeMigration, 0.02)`; no prior → 50 + flag `positioning:no-prior-snapshot`. `positioningScore = 0.60·densityScore + 0.40·migrationScore`.
7. **Dealer influence:** `wallMagScore = normCrossSection(|gex|, {|gex| over C})`; `signed = 0.50·gex + 0.30·dex + 0.20·vex`; `alignScore = normCrossSection(ACCEL_SIGN·dir·signed, {same over C})`; `dealerInfluenceScore = 0.60·wallMagScore + 0.40·alignScore`.
8. **Acceleration:** `ve = rvol ?? 1`; `veScore = normSignedTanh(ve − 1, 1.5)`; with prior: `gammaVelocity = gex − prev.gexStrike`, `gammaVelScore = normSignedTanh(gammaVelocity, 5e8)`; `vexVelScore = normSignedTanh(vex − prev.vexStrike, 5e7)`; no prior → both 50 + flag. `accelerationScore = 0.40·veScore + 0.35·gammaVelScore + 0.25·vexVelScore`.
9. **EMA path:** `targetAhead = nextUnhitEma(spot, direction, emaTargets)` = nearest finite positive EMA strictly beyond spot in direction (null if none → score 50 + flag `emapath:no_target_ahead`). Skew-adjusted reprice: `now = BSM_E3a(spot, strike, dteDays, iv, isCall)`; `movePct = 100·(target − spot)/spot`; `sigmaTarget = max(0.03, iv + (−0.012)·movePct)`; `at = BSM_E3a(target, strike, dteDays, sigmaTarget, isCall)` (**same dteDays — no time decay; source flags `emapath:T_target=T_now`**); `baseReturn = (at − now)/now` (0 if now ≤ 0).
   Reachability §1.2: `reach = min(1, expectedMovePct/max(targetDistancePct, 1e-9))` where `targetDistancePct = |target − spot|/max(spot, 1e-9)` (target at 2·EM → 0.5, 4·EM → 0.25).
   Mispricing §1.3: `premium = iv/max(fairIv, 1e-9) − 1` (fairIv = contract.fairIv ?? atmIv); `misprice = clamp(1 − max(0, premium)·1.0, 0.5, 1)` — only penalizes rich contracts.
   `finalReturn = baseReturn·reach·misprice`; `emaPathScore = normSignedTanh(finalReturn, 0.5)`. Also emit display `emaReturns[k] = 100·repricedReturn(spot→ema_k)` (2 dp) for all five EMAs.
10. **Liquidity §1.4:** `oiScore01 = clamp01(log10(max(oi,1))/4)`; `spreadScore01 = clamp01(1 − spreadPct/0.10)`; `dominanceScore = normCrossSection(volShareNow, {volShare over C})`; `liquidityScore = 100·(0.40·oiScore01 + 0.40·unit(dominanceScore) + 0.20·spreadScore01)`.
11. **SkyScore:** `0.25·positioning + 0.25·dealer + 0.20·acceleration + 0.20·emaPath + 0.10·liquidity` → 0–100.
12. **Convexity (Part 8):** `vannaVelScore = prior ? normSignedTanh(vex − prev.vexStrike, 5e7) : 50` (identical to vexVelScore); `speedLevelScore = normCrossSection((speed || 0)·|targetAhead ?? spot − spot|, {same over C})`; `convexityScore = 100·clamp01(0.45·unit(gammaVelScore) + 0.35·unit(vannaVelScore) + 0.20·unit(speedLevelScore))`. Status: `≥80 Rising Fast; ≥60 Rising; ≥40 Flat; else Falling`. `isExplosive = skyScore ≥ 85 && convexityScore ≥ 80`.
13. Record current snapshot to the store (`{ts: Date.now(), gexStrike, vexStrike, gamma, vanna, oi, volume, volShare, mid}`); sort output eligible-first then skyScore desc. All scores rounded to 2 dp.

**SnapshotStore semantics:** in-memory ring buffer per key; `prior(key, n)` returns the snapshot n entries back from the latest, or null if fewer than n exist (callers MUST then use neutral 50 — never fabricate a delta).

| value | where | meaning | name |
|---|---|---|---|
| 250 / 100 | eligibility | min OI / min volume | ELIG_MIN_OI/VOLUME |
| 0.12 | eligibility | max spread fraction of mid | ELIG_MAX_SPREAD |
| 0.30 / 0.60 | eligibility | \|delta\| band | ELIG_DELTA_MIN/MAX |
| 3 | density | strike-index half-window | DENSITY_WINDOW |
| 0.5 | density | base weight (lone-spike floor) | DENSITY_BASE |
| 0.02 | migration | volume-share tanh scale | MIG_SCALE |
| 5000 | config | OI velocity scale (declared; unused in this file) | OIV_SCALE |
| 12 | config | NBRS cap (declared; unused here) | NBRS_CAP_V5 |
| +1 | dealer align | ACCEL_SIGN (calibration required) | ACCEL_SIGN |
| 0.60 / 0.40 | dealer blend | wall magnitude / alignment | DEALER_W_MAG/ALIGN |
| 0.50/0.30/0.20 | exposures | gex/dex/vex weights | EXPO_W_* |
| 1.5 | acceleration | volume-expansion tanh scale | VE_SCALE |
| 5e8 / 5e7 | acceleration | gamma / vex velocity scales | GVEL_SCALE / VVEL_SCALE |
| 5 | snapshots | lookback bars | LOOKBACK_BARS |
| −0.012 | reprice | spot-vol beta (IV pts per 1% move) | SPOT_VOL_BETA |
| 0.03 | reprice | sigma floor | SIGMA_FLOOR_V5 |
| 0.5 | emaPath | return tanh scale | EMA_RET_SCALE |
| 1.0 / 0.5 | mispricing | penalty slope / floor | IV_PEN_K / IV_PEN_FLOOR |
| 0.25/0.25/0.20/0.20/0.10 | skyScore | blend weights | SKY_W_* |
| 0.40/0.40/0.20 | liquidity | oi/dominance/spread weights | LIQ_V5_W_* |
| 4 / 0.10 | liquidity | log10 OI scale / spread cap | LIQ_LOG_SCALE / LIQ_SPREAD_CAP |
| 0.45/0.35/0.20 | convexity | gammaVel/vannaVel/speed weights | CONVEX_W_* |
| 80 / 60 / 40 | convexity | status cutoffs | CONVEX_STATUS_* |
| 85 / 80 | explosive | skyScore / convexity thresholds | EXPLOSIVE_SKY/CONVEX |
| 0.15 | ATM IV | fallback | DEFAULT_ATM_IV |
| 1e-9 | misc | EPS guard | EPS |

**Outputs.** Per contract: eligibility + reject reasons; five sub-scores and `skyScore` (0–100, 2 dp); `convexityScore` + status; `isExplosive`; raw diagnostics (`positioningDensity`, `volumeMigration` 6 dp, `volumeExpansion`, `gammaVelocity`, `distanceFromSpotPct` 2 dp, `emaReturns` %); flags; data_source passthrough.

---

## State semantics

The rebuild collapses states to binary ACTIVE/INACTIVE + a continuous score; the exact thresholds AND the continuous quantity behind each state are listed so nothing is lost.

| Enum | Values | Exact conditions | Underlying continuous quantity |
|---|---|---|---|
| `DecisionState` (E16) | BUY / WAIT / HOLD / REDUCE / EXIT | Closed position: BUY iff ALL of `ev>0, p_cal>0.62, rr>1.5, tail≤0.70, liq≥60, n≥30, trust≥0.40, dealer01≥0.60`, else WAIT. Open position: EXIT iff `thesis<40 ∨ p_cal<0.50 ∨ ev<0 ∨ liq<30 ∨ trust<0.20`; else REDUCE iff `thesis<70 ∨ tail>0.70 ∨ dealer01<0.50`; else HOLD | the 8-tuple (ev, p_cal, rr, tailRiskScore, liquidityScore, n, trustScore, dealer01) + thesisStability |
| `StrengthTrend` (E18) | RISING / FALLING / FLAT | signal > 0.08 / < −0.08 / else | weighted factor signal ∈ [−1,1] |
| Strength label (E18) | Strong Buy / Buy / Accumulate / Neutral / Weak / Avoid | score ≥85 / ≥70 / ≥58 / >42 / ≥30 / else | contract strength score 0–100 |
| `PositionHealth` (E21) | Strong / Healthy / Weakening / Critical (action Hold/Hold/Reduce/Exit) | score≥70 ∧ trend≠FALLING / score≥55 ∧ trend≠FALLING / score≥40 / else | strength score + trend signal |
| `SwingDirection` (E20) | BULLISH / BEARISH / NONE | detected (alignment cond true ∧ ≥3 of 4 conds) → call=BULLISH, put=BEARISH; else NONE | condition count ×25 = swing strength 0–100 |
| Swing duration (E20) | "45-60 min"/"30-45 min"/"5-15 min" (ST); "1-2 weeks"/"3-7 days"/"1-2 days" (LT) | strength ≥90 / ≥75 / else (per leg) | swing strength |
| `TradeAction` (E27) | HOLD / TRIM_25 / TRIM_50 / HARD_EXIT_CLOSE_ALL | any invalidation flag → HARD_EXIT; decay ≥30 → HARD_EXIT; ≥20 → TRIM_50; ≥10 → TRIM_25; else HOLD | confidenceDecayPct = 100·(maxPSS − PSS)/maxPSS |
| Invalidation flags (E27) | DEALER_FLIP, OI_LIQUIDATION, FLOW_REVERSAL, STRUCTURE_BREAK, PSS_FLOOR | see E27 | netGex sign vs entry; oiVel vs 0.5·\|oiVel0\|; PSS vs 70 |
| `convexityStatus` (E28) | Rising Fast / Rising / Flat / Falling | convexityScore ≥80 / ≥60 / ≥40 / else | convexityScore 0–100 |
| `isExplosive` (E28) | true/false | skyScore ≥85 ∧ convexityScore ≥80 | (skyScore, convexityScore) |
| Trust grade (E13) | A / B / C / D / F | trust ≥0.80 / ≥0.60 / ≥0.40 / ≥0.20 / else | trustScore ∈ [0,1] |
| `valuationLabel` (E17) | UNDERVALUED / FAIRLY_PRICED / OVERVALUED | surcharge ≤ −2.5% / between / ≥ +2.5% | premiumSurchargePct |
| `tradeHealth` (E23) | Strong / Healthy / Mixed / Weak | master score ≥80 / ≥60 / ≥45 / else | master score 0–100 |
| Integrity `greeksConsistency` (E17) | OPTIMAL / MARGINAL / CRITICAL | pts ≥90 / ≥75 / else | integrity points 0–100 |
| Integrity `chainIntegrity` (E17) | FULLY_INTEGRIFIED / MISALIGNED_GAPS | pts ≥80 / else | integrity points |
| `DynamicExitSignal` (E22) | kinds EMA_TARGET/STRENGTH_COLLAPSE/FLOW_REVERSAL/GAMMA_WALL/IV_CRUSH; actions SCALE/TAKE_PROFIT/EXIT | see E22 per-trigger predicates | severity ∈ {0.4,0.6,0.7,0.85,0.9}; collapse drop (peak−last); netRel(iv), netRel(premium) |
| Dealer texts (E17) | NET POSITIVE/NEGATIVE GAMMA; CHARM BUY/SELL | netGex > 0; netCharm > 0 | netGex, netCharm |
| Confidence flags (E8) | gammaFlipConfident, wallsConfident | zero-crossing found; dominant wall found both sides | cumulative GEX profile; max \|GEX\| per side |
| KNN regime (E14) | Volatility Expansion / Compression | roll > 0.5 | uniform roll |
| Structure rungs (E5) | 0.00 / 0.33 / 0.5 / 0.66 / 1.00 | see E5 | pivot relationships in ATR units |

---

## Data dependencies

Dependency order (topological):

1. **Candles (OHLCV + optional vwap)** → E4 (RSI/ATR) → E5 (pivots/structure) → E6 kernels → **E7 SystemScore**. Pure; no chain needed.
2. **Chain snapshot + spot (+ dte)** → E3 greeks (if the feed lacks them) → **E8 dealer inventory** (needs per-contract gamma/delta/vanna/charm, OI, IV). `gammaFlipSpot`/`expectedMovePct` (skyQuantCore) are shared primitives used by E8 and (per source comments) by the external gexEngine.
3. **Labeled outcome history** `{pred, win}` (self-learning store, upstream of this cluster) → E10 calibration, E13 ECE input, E17 base rate. Empty history = documented cold-start priors.
4. **E17 master pipeline** consumes: E7 output (passed in), E8 (from chain or E9 mock), E14 KNN matches (mock; would be the real trade store), E10, E11, E12, E13, E15, E16. `calculateV10Metrics` consumes E17's output.
5. **SkyVision layers**: per-contract time series (`ContractSnapshot[]`, built by `snapshotFromMarket` from spot/strike/dte/iv or from the live feed) → E18 → E21; closes → E19 EMA ladder → E19 target stack (also needs E8 walls + expected-move band) → premium projection (needs E3a + E24 barrierTouchProb); E18 + E19 + E20 + external flow/dealer scores → E23 master score; open-position history + walls + sweep counts → E22.
6. **SkyScore ranker (E28)**: runs only after the directional engine emits BUY. Needs chain (target type), spot, dte, EMA targets (`technicalEngine`), full-chain volume, expected move (from E8 or recomputed), and a `SnapshotStore` with ≥5 prior scans for migration/velocity terms (else neutral 50).
7. **PSS engines (E26/E27)**: need PRE-CLASSIFIED flow inputs (net premium per 5/15/30-min window, sweep volume by side, opening-trade alignment), dealer composite history, EMA 9/21/50 + VWAP + 20-bar high, OI velocity/migration series. Classification is upstream — these files never label trade side.

Wall-clock dependencies (impure): E14's leakage filter reads `Date.now()`; `SnapshotStore.record` stamps `Date.now()`. Everything else is deterministic in its inputs.

---

## Suspected defects

Flagged only — do not fix silently; decide per-item in the rebuild.

- **D1. Engine/timezone-dependent KNN leakage filter** (v11Math.ts:1083–1086). Mock trade dates are non-ISO, non-zero-padded strings (`"2026-05-3 14:7"` → `"2026-05-3T14:7"`). `new Date()` parsing of this is implementation-defined: some engines return Invalid Date (row dropped), others parse as LOCAL time (timezone-dependent comparison vs `Date.now()`). As coded, the entire similar-trades set — hence EV, tail risk, targets — can silently change size or become empty depending on runtime/timezone. Correct form: generate ISO-8601 UTC timestamps and compare in UTC.
- **D2. KNN seed collision** (v11Math.ts:1050). Seed = `ticker.charCodeAt(0) + (dir≥0?1:2)·99` — every ticker sharing a first letter (SPY/SPX/SMH...) gets an identical "historical database". Correct form: hash the full ticker.
- **D3. Unit mismatch in model-trust forecast error** (v11Math.ts:1415). `predActualDeltas = matchedReturns.map(r → r − calibratedP)` subtracts a probability from a fractional RETURN; the MAE of that difference is meaningless. Suspected intent: |predicted win prob − realized win indicator| or |predicted return − realized return|.
- **D4. Two different greeks both called "VEX"** (skyScore.ts:133 vs v11Math.ts:581 / skyQuantCore.ts:125). v11Math/skyQuantCore define VEX from **vanna** (`vanna·OI·100·S·0.01`); skyScore's fallback computes it from **vega** (`vega·OI·100·S·0.01`) while comparing against adapter-supplied vanna-based `vexStrike` in velocity deltas. Whichever an adapter feeds, mixed units flow into `vexVelScore`/`vannaVelScore`. Correct form: one definition (vanna-based) everywhere.
- **D5. $0.05 hard floor inside `computeBlackScholesPrice`** (v11Math.ts:120,123). Every model price — including deep-OTM fair values, mock chain quotes, target projections, and `premiumSurchargePct` denominators — is floored at $0.05, so cheap contracts can never be flagged OVERVALUED against a floored model price, and E19's `max(0.01, price)` guard is dead code (price ≥ 0.05 always). skyQuantCore's `bsmPrice` has no floor — the two disagree for the same inputs.
- **D6. Phantom weights in `calculateV10Metrics`** (v11Math.ts:1720–1723). `(x_dev/sumDevs || fallback)` fires the fallback whenever the share is exactly 0 — a component sitting exactly at its 0.5 neutral gets a phantom 15–25% share of the offset instead of 0. NaN was the intended target of `||`.
- **D7. Strike-step mismatch** (v11Math.ts:1315 vs 685). Pipeline `determinedStrike` uses step 100 for spot > 1000; the mock chain builds strikes on step 50 — the auto-picked strike may not exist in the chain used for dealer metrics/valuation.
- **D8. OI_LIQUIDATION degenerate at oiVel0 = 0** (skyQuantCore.ts:395). `|oiVelNow| ≥ 0.5·|oiVel0|` with `oiVel0 = 0` means ANY negative OI velocity (even −1e-9) triggers a hard exit.
- **D9. ivRank/ivPercentile unbounded** (v11Math.ts:1499–1500). `35 + vol·100` and `28 + vol·120` exceed 100 for IV > 0.65 / 0.60 — not a rank/percentile at all (affine in IV, no history). Correct form: percentile of IV against its own history.
- **D10. Quote-stability variance is scale-dependent** (v11Math.ts:959). Sample variance in $² compared against absolute `stability_max = 0.05` — a $500 SPX option's normal tick noise always scores 0 stability; a $0.30 option always ~1. Correct form: variance of RELATIVE mids.
- **D11. Fabricated analytics presented as computed** (v11Math.ts:1399–1420, 1480, 1465). Liquidity uses hardcoded volume 14500 / OI 18800 and sin-wave mids; trust uses cos-wave prediction history and hardcoded `recent100Wins = 0.741`; `regimeStability = 0.85`; `volExpansionNorm = 0.5`; `expectedSlippagePct = 1.25`; `expectedHoldTimeMinutes = 24`. These flow into gate-relevant scores (liquidityScore, trustScore, opportunityQuality). The rebuild must source them from real data or mark them explicitly as priors.
- **D12. Decision gate's open-position branch is unreachable from the pipeline** (v11Math.ts:1425). `evaluateDecisionGate` is always called with `positionOpen = false`, so HOLD/REDUCE/EXIT logic never executes in V11 (only via direct callers, if any).
- **D13. Target probabilities decoupled from target distances** (v11Math.ts:1534–1588). Prices scale with expected-move multiples (0.35/0.70/1.20/2.00×EM) but probabilities are fixed multipliers of calibratedP (0.95/0.78/0.60/0.38) — internally inconsistent (a nearer target on a high-IV day gets the same probability). Also the headline `probability` is the Wilson LOWER bound, not the point estimate. The honest replacement already exists: E24 `probOptionHitsTarget` / E19 `touchProb`.
- **D14. RVOL divide-by-zero fallout for tiny histories** (v11Math.ts:429–434). With n ≤ 2 the baseline sum is empty → `meanVol = 0` → `rvol = volume/1 =` raw volume (e.g. 50,000), saturating volume01. Guard should return neutral 1.0.
- **D15. `netRel` explodes when the window starts near 0** (skyVisionEngine.ts:69–74). Denominator `|first| + 1e-9`: a gamma/OI series starting at ~0 saturates its factor to ±1 regardless of magnitude, overweighting that factor in E18's signal.
- **D16. Fractal pivot ties suppressed** (v11Math.ts:265,278). Strict `≥`/`≤` neighbor comparisons mean an exact double-top/bottom produces NO pivot on either bar, degrading structure01 exactly at the most decision-relevant patterns.
- **D17. `calibratedP` input/history mismatch risk** (v11Math.ts:1371–1372). The PAV curve is fit on `history.pred` (whatever the store recorded) but queried at `systemScore.total/100`; the calibration is only valid if the store's `pred` was recorded on the same scale — nothing enforces this.
- **D18. Wilder seed index asymmetry** (E4): ATR seeds at bar 13, RSI at bar 14 — one-bar inconsistency vs the canonical Wilder definition (both should seed after `period` deltas). Behavior-preserving port must keep it; a clean rebuild should not.
- **D19. `evaluateTrade` mutates its input `state`** (skyQuantCore.ts:412) — hidden side effect (`maxPss` ratchet) that breaks referential transparency if callers replay ticks.
- **D20. skyScore EMA-path reprices with `T_target = T_now`** (skyScore.ts:169, self-flagged) — no theta decay over the travel time to the target, systematically overstating `baseReturn` for far targets (partially offset by the reach multiplier).

---

## Discarded

Not worth porting; one line each.

- `v11Math.ts` — `DataIntegrityScore.timestampAlignment = 'ALIGNED (0.002s latency)'`: hardcoded UI string, no computation.
- `v11Math.ts` — all `reason`/`decisionReason` string templating, `optimalFillRange` string, `confidenceInterval` string formatting: presentation-layer text.
- `v11Math.ts` — `featureImportances` labels ("Volume01 RVOL Trent Trend" [sic]): display naming, keep only the weight math (E17.21).
- `v11Math.ts` — `dealer.gammaExposureText/charmExposureText/vannaExposureText`: UI verdict strings (the vanna one is a constant regardless of data).
- `v11Math.ts` — `surface.skewCurve` slice-of-first-5-rows and its OTM PUT/CALL labels: cosmetic chart feed, not a real skew computation.
- `v11Math.ts` — `Number(x.toFixed(n))` rounding scattered everywhere: display rounding, standardize at the API boundary in the rebuild.
- `skyVisionEngine.ts` — all `reasons[]`/`signals[]` prose strings in swings, health, exits: narration; the booleans/severities behind them are already specced.
- `skyVisionEngine.ts` — `MasterScore.bestContract/swingType/target` passthrough fields: UI plumbing.
- `skyScore.ts` — `rejectReasons`/`flags` message text: keep the predicates, not the strings; `data_source` passthrough field: plumbing.
- `skyQuantCore.ts` — `lqPenaltyThreshold`/`lqPenaltyFrac` config fields: declared but never referenced in these four files (verify against other clusters before deleting).
- License headers, TypeScript interface re-exports (`export { stdNormalCDF, stdNormalPDF }`), and the `calculateV10Metrics` perf-precompute parameter: build/compat glue.
