# Cluster 04 — Vol & Distribution Machinery

Source files (legacy, TypeScript):
`src/lib/normalDist.ts`, `src/lib/rndEngine.ts`, `src/lib/riskNeutral.ts`, `src/lib/ivSurface.ts`,
`src/lib/realizedVol.ts`, `src/lib/crossAsset.ts`, `src/lib/displacementEngine.ts`, `src/lib/regimeEngine.ts`
(plus load-bearing helpers `calculateWilderRSI` / `calculateWilderATR` from `src/lib/v11Math.ts`, transcribed here because regime thresholds depend on their exact form).

Conventions used throughout this spec:
- `ln` = natural log. All vols are **annualized decimals** (0.18 = 18%) unless stated.
- `T` = year fraction (days / 365 unless stated). `r` = continuously-compounded risk-free rate (default **0.05** everywhere). `q` = dividend yield (default **0**).
- `Candle = { timestamp: epoch-ms, open, high, low, close, volume, vwap?, isDisplacement?, displacementType?, displacementScore?, relativeVolume? }`.
- `ChainContract = { strike, type: 'call'|'put', openInterest, iv (annualized decimal), bid, ask, delta, gamma, vega, theta, vanna, charm, volume? }`.
- Sample std `std(a)` in this cluster = `sqrt( Σ(x−mean)² / (len−1) )` (Bessel), returns 0 if `len < 2`. Sample mean is the arithmetic mean, 0 for empty arrays. Exceptions (population divisors) are flagged inline.

## Overview

This cluster is the volatility/distribution core of the terminal:

1. **Normal distribution primitives** — a double-precision Φ(x) (Hart 1968 / Graeme West) that every option price, Greek and tail probability in the platform flows through.
2. **Risk-neutral density (two engines)** — (a) a *synthetic* Breeden–Litzenberger profile built from hard-coded SVI smiles per ticker (`rndEngine`), and (b) a *chain-driven* Breeden–Litzenberger density built from the live IV smile (`riskNeutral`) that yields P(above level), percentiles, expected move, fat-tail score and skew bias.
3. **IV surface model** — front smile is real; longer expiries are the front smile scaled by a Heston forward-variance ATM term factor.
4. **Realized-vol suite** — close-to-close, Parkinson, Garman–Klass, Rogers–Satchell, Yang–Zhang (headline), vol cone percentiles, and the Variance Risk Premium (IV − RV) read.
5. **Cross-asset PCA stat-arb** — PC1 market factor over the index complex via power iteration; residual z-scores flag RICH/CHEAP.
6. **Displacement/liquidity engine** — Wilder-ATR displacement zones, fair-value gaps with a 5-state machine, liquidity sweeps off fractal pivots, BOS/CHoCH market structure, and an ATR/bandwidth vol-regime read.
7. **Regime engine** — Hurst exponent (R/S), Ornstein–Uhlenbeck half-life on a stationarized series, a softmax "HMM-style" 3-state regime classifier, and vol compression/expansion/term-structure flags.

These feed the dealer-flow SSE payload, the regime matrix panel, the RND panel, and the vol dashboard.

---

## Engines

### E1. Standard normal primitives (`normalDist.ts`)

**Purpose.** Single source of truth for Φ, φ, erf. Hart (1968) rational approximation as published by Graeme West ("Better Approximations to Cumulative Normal Functions", Wilmott 2009). ~1e-15 absolute accuracy over the whole line.

**Inputs.** `x: number` (unitless standard-normal argument).

**Algorithm — `stdNormalPDF(x)`:**
`φ(x) = INV_SQRT_2PI * exp(-0.5 * x * x)` with `INV_SQRT_2PI = 0.3989422804014327`.

**Algorithm — `stdNormalCDF(x)`:**
1. `a = |x|`.
2. If `a > 37`: `cumnorm = 0`.
3. Else compute `e = exp(-0.5*a*a)`, then:
   - **Body region** (`a < 7.07106781186547`, i.e. a < 5√2): Horner-evaluate
     ```
     n = ((((( 3.52624965998911e-2*a + 0.700383064443688 )*a
            + 6.37396220353165 )*a + 33.912866078383 )*a
            + 112.079291497871 )*a + 221.213596169931 )*a + 220.206867912376
     d = (((((( 8.83883476483184e-2*a + 1.75566716318264 )*a
            + 16.064177579207 )*a + 86.7807322029461 )*a
            + 296.564248779674 )*a + 637.333633378831 )*a
            + 793.826512519948 )*a + 440.413735824752
     cumnorm = e * n / d
     ```
   - **Tail region** (`a >= 7.07106781186547`): 4-level continued fraction
     ```
     f = a + 0.65
     f = a + 4/f
     f = a + 3/f
     f = a + 2/f
     f = a + 1/f
     cumnorm = e / f / SQRT_2PI        // SQRT_2PI = 2.5066282746310002
     ```
4. Return `x > 0 ? 1 - cumnorm : cumnorm`. (So Φ returns exactly 0/1 beyond ∓37σ.)

**Algorithm — `erf(x)`:** `erf(x) = 2 * stdNormalCDF(x * √2) − 1`.

Note: `v11Math.ts` imports and **re-exports** these same functions — there is exactly one CDF implementation in the codebase; `rndEngine` imports it via `v11Math`, `riskNeutral` directly.

**Constants:**

| value | where used | semantic meaning | proposed name |
|---|---|---|---|
| 0.3989422804014327 | stdNormalPDF | 1/√(2π) | INV_SQRT_2PI |
| 2.5066282746310002 | CDF tail branch | √(2π) | SQRT_2PI |
| 37 | CDF | |x| beyond which Φ saturates to 0/1 | NORM_CDF_SATURATION_SIGMA |
| 7.07106781186547 | CDF | body/tail branch split (5√2) | HART_BRANCH_SPLIT |
| 3.52624965998911e-2, 0.700383064443688, 6.37396220353165, 33.912866078383, 112.079291497871, 221.213596169931, 220.206867912376 | CDF numerator | Hart numerator coefficients (highest→lowest degree) | HART_NUM_COEFFS[0..6] |
| 8.83883476483184e-2, 1.75566716318264, 16.064177579207, 86.7807322029461, 296.564248779674, 637.333633378831, 793.826512519948, 440.413735824752 | CDF denominator | Hart denominator coefficients (highest→lowest degree) | HART_DEN_COEFFS[0..7] |
| 0.65, 4, 3, 2, 1 | CDF tail | continued-fraction ladder constants | HART_CF_CONSTANTS |

**Outputs.** `stdNormalPDF: (0, 0.39894…]`, `stdNormalCDF: [0,1]`, `erf: [−1,1]`.

---

### E2. Synthetic SVI Breeden–Litzenberger profile (`rndEngine.ts`)

**Purpose.** Builds an implied probability density from a *hard-coded* Gatheral SVI smile per ticker (NOT calibrated from the live chain), compares against a lognormal "historical" density, and reports moments / KL divergence / density peak. This whole engine is a model with fixed parameters.

**Inputs.**
- `spot: number` (underlying price, currency units)
- `ticker: string` (keys into parameter tables; unknown → SPX defaults)
- `dteDays: number` (calendar days to expiry)
- `r = 0.05` (rate, decimal)
- `customVols?: number` (optional override for historical vol, annualized decimal)

**Sub-algorithm `calculateRawSVI(k, {a,b,rho,m,sigma})`** — Gatheral raw SVI total variance:
`w(k) = a + b * ( rho*(k−m) + sqrt( max(1e-9, (k−m)² + sigma²) ) )`, `k` = log-moneyness.

**Sub-algorithm `getSviImpliedVol(strike, spot, params, T)`:**
1. If `spot <= 0 || strike <= 0` return `0.20`.
2. `k = ln(strike/spot)`; `w = calculateRawSVI(k, params)`.
3. `iv = sqrt( max(1e-4, w / T) )`.
4. Return `clamp(iv, 0.01, 2.5)`.

**Sub-algorithm `calculateBSCallPrice(spot, strike, T, iv, r=0.05, q=0)`:**
1. If `spot<=0 || strike<=0 || T<=0` return 0.
2. `d1 = ( ln(spot/strike) + (r − q + iv²/2)·T ) / (iv·√T)`; `d2 = d1 − iv·√T`.
3. `price = spot·e^{−qT}·Φ(d1) − strike·e^{−rT}·Φ(d2)`; return `max(0, price)`.

**Hard-coded parameter tables:**

`DEFAULT_SVI_PARAMETERS` (per-ticker `{a, b, rho, m, sigma}`):

| ticker | a | b | rho | m | sigma |
|---|---|---|---|---|---|
| SPX | 0.038 | 0.065 | −0.68 | 0.015 | 0.12 |
| NDX | 0.045 | 0.075 | −0.62 | 0.020 | 0.14 |
| RUT | 0.042 | 0.070 | −0.55 | 0.010 | 0.15 |
| QQQ | 0.046 | 0.078 | −0.60 | 0.022 | 0.14 |
| SPY | 0.039 | 0.066 | −0.67 | 0.016 | 0.13 |

`DEFAULT_HISTORICAL_VOLATILITY`: SPX 0.132, NDX 0.158, RUT 0.175, QQQ 0.155, SPY 0.130; unknown ticker → 0.15.

**Algorithm — `computeRndProfile`:**
1. `T = max(0.005, dteDays/365)`. SVI params = table[ticker] || table.SPX. `histVol = customVols ?? (table[ticker] || 0.15)`.
2. Strike grid: `rangePct = 0.30`, `numSteps = 120`, `minStrike = spot·0.7`, `maxStrike = spot·1.3`, `ds = (max−min)/120`. Grid has **121 nodes** `K_i = minStrike + i·ds`, i = 0..120.
3. Finite-difference bump `dK = max(0.5, spot·0.0025)` (i.e. 25bp of spot, floored at $0.50 — deliberately spot-scaled to avoid float cancellation in the second difference).
4. For each node `K`:
   a. `ivK = getSviImpliedVol(K)`, `ivUp = getSviImpliedVol(K+dK)`, `ivDn = getSviImpliedVol(K−dK)` (all at same T).
   b. `callMid/Up/Dn = calculateBSCallPrice(spot, K or K±dK, T, respective iv, r)`. **Note: smile-consistent bumping — each bumped call is repriced at its own SVI vol.**
   c. `secondDeriv = (callUp − 2·callMid + callDn) / dK²`.
   d. `impliedDensity = e^{rT} · secondDeriv`; if NaN or negative → 0.
   e. Historical lognormal density (risk-neutral drift form):
      `stdDevT = v·√T`, `logMoneyness = ln(K/spot)`, `drift = (r − v²/2)·T`,
      `exponent = −(logMoneyness − drift)² / (2·stdDevT²)`,
      `denom = K·stdDevT·√(2π)`,
      `historicalDensity = denom > 0 ? e^{exponent}/denom : 0`; NaN/negative → 0.
   f. Accumulate `sumImplied += impliedDensity·ds`, `sumHistoric += historicalDensity·ds`.
5. Normalize: each node's density is divided by its respective sum (if sum > 0, else 0), so `∫f dK = 1` over the truncated grid. Cumulatives are right-Riemann running sums `runCum += normDensity·ds`, clamped `min(1.0, ·)`, stored per node.
6. Moments (rectangle rule over all 121 nodes, weight `ds`):
   `impliedMean = Σ K·f_impl·ds` (if ≤ 0 → spot); `impliedVar = Σ (K−mean)²·f_impl·ds`; `impliedStdDev = sqrt(max(1e-2, impliedVar))`. Same for historical.
7. `gexConcentrationPeak` = strike of the max normalized implied density (init `maxImpliedDens = −1`, peak defaults to spot).
8. KL divergence (implied ‖ historical): `KL = Σ p·ln(p/q)·ds` over nodes where **both** `p > 1e-7` and `q > 1e-7`; final output `max(0, KL)`.
9. `isVolSkewSkewed = |rho| > 0.5`.

**Constants:**

| value | where used | semantic meaning | proposed name |
|---|---|---|---|
| 1e-9 | SVI sqrt guard | floor on (k−m)²+σ² | SVI_SQRT_FLOOR |
| 0.20 | getSviImpliedVol degenerate input | fallback IV | SVI_FALLBACK_IV |
| 1e-4 | getSviImpliedVol | floor on w/T before sqrt (min variance) | SVI_MIN_ANNUAL_VARIANCE |
| 0.01 / 2.5 | getSviImpliedVol | IV clamp bounds | IV_CLAMP_MIN / IV_CLAMP_MAX |
| 0.05 | default r (this and E3) | risk-free rate | DEFAULT_RISK_FREE_RATE |
| 0.005 | computeRndProfile | floor on T (years) ≈ 1.8 days | RND_MIN_T_YEARS |
| 0.30 | grid | ± strike coverage around spot | RND_RANGE_PCT |
| 120 | grid | number of grid steps (121 nodes) | RND_NUM_STEPS |
| 0.5 | dK floor | min FD bump ($) | RND_MIN_BUMP |
| 0.0025 | dK | FD bump as fraction of spot | RND_BUMP_PCT |
| 1e-2 | moment floors | min variance before sqrt (price² units) | RND_MIN_VARIANCE |
| 1e-7 | KL | density floor for inclusion in KL sum | KL_DENSITY_EPS |
| 0.5 | isVolSkewSkewed | |rho| threshold for "skewed" flag | SVI_RHO_SKEW_THRESHOLD |
| 0.15 | histVol fallback | unknown-ticker historical vol | DEFAULT_HIST_VOL_FALLBACK |
| (table) | SVI params ×5 tickers, hist vols ×5 | see tables above | DEFAULT_SVI_PARAMETERS, DEFAULT_HISTORICAL_VOLATILITY |

**Outputs.** `BreedenLitzenbergerAnalysis`: `nodes[121]` (strike, normalized impliedDensity & historicalDensity per unit strike, impliedVol, both cumulatives in [0,1]), `impliedMean`/`historicalMean` (price), `impliedStdDev`/`historicalStdDev` (price, ≥ 0.1), `gexConcentrationPeak` (strike), `entropyDivergence` (nats, ≥ 0), `isVolSkewSkewed` (bool).

---

### E3. Chain-driven risk-neutral density (`riskNeutral.ts`)

**Purpose.** Breeden–Litzenberger on the *real* chain smile: `f(K) = e^{rT}·∂²C/∂K²`, computed by repricing BSM calls on a fine grid from an interpolated IV smile, then differencing. Produces P(above), percentiles, expected move, fat-tail ratio, skew bias, chart density.

**Inputs.** `chain: ChainContract[]` (front expiry, per-contract `iv` annualized decimal), `spot`, `dteDays`, `r = 0.05`. Returns `null` if chain empty or spot ≤ 0 or density mass ≤ 0.

**Sub-algorithm `bsCall(S, K, T, sigma, r=0.05, q=0)`** (no liquidity floor, needed for clean differences):
- If `!(S>0) || !(K>0)` → 0.
- If `!(T>0) || !(sigma>0)` → `max(0, S·e^{−qT} − K·e^{−rT})` (discounted intrinsic).
- Else standard BSM call as in E2 step 2–3 (without the outer `max(0,·)` — the formula result is returned directly).

**Sub-algorithm `buildIvSmile(chain)` → `ivAt(k)`:**
1. Group contracts by strike, keeping only `iv > 0`; per strike take **arithmetic mean of all IVs at that strike** (calls and puts blended together).
2. Sort points by strike. Empty → constant function `0.2`.
3. Interpolator: `k <= firstStrike` → first IV; `k >= lastStrike` → last IV (**flat extrapolation both wings**); else **piecewise-linear** between bracketing strikes: `iv = ivA + (ivB − ivA)·(k − kA)/(kB − kA)`.

**Algorithm — `computeRiskNeutralDensity`:**
1. `T = max(dteDays, 0.25)/365` (DTE floored at 0.25 days). `atmIv = ivAt(spot) || 0.2`.
2. Grid: `lo = spot·0.55`, `hi = spot·1.6`, `N = 220`, `dK = (hi−lo)/220`, nodes `K_i = lo + i·dK`, i = 0..220 (221 nodes). For each: `calls[i] = bsCall(spot, K_i, T, max(0.01, ivAt(K_i)), r)`.
3. Density via central second difference, interior nodes only (endpoints stay 0):
   `density[i] = max(0, e^{rT} · (calls[i+1] − 2·calls[i] + calls[i−1]) / dK²)` for i = 1..N−1.
4. Normalize: `mass = Σ density[i]·dK`; if `!(mass > 0)` return null; `density[i] /= mass`.
5. `cdf(x) = clamp( Σ_{strikes[i] <= x} density[i]·dK , 0, 1)` (loop breaks at first strike > x). `pAbove(x) = 1 − cdf(x)`.
6. `quantile(q)`: first strike where the running mass `Σ density[i]·dK ≥ q`; falls through to the last strike. (Step function — no interpolation.)
7. Moments: `mean = Σ K_i·density[i]·dK`; `variance = Σ (K_i − mean)²·density[i]·dK`; `std = sqrt(max(0, variance))`; `expectedMovePct = std/spot` (0 if spot ≤ 0).
8. Fat tail: `twoSigUp = spot·(1 + 2·expectedMovePct)`, `twoSigDn = spot·(1 − 2·expectedMovePct)`, `rndTail = pAbove(twoSigUp) + cdf(twoSigDn)`, benchmark `lnTail = 0.0455` (Gaussian P(|Z|>2)), `fatTailRatio = rndTail / 0.0455`. Values > 1 read "fatter than lognormal".
9. Skew: `p50 = quantile(0.5)`; `mean < p50·0.997` → `'DOWNSIDE SKEW'`; `mean > p50·1.003` → `'UPSIDE SKEW'`; else `'SYMMETRIC'`.
10. Levels: for m ∈ {0.01, 0.02, 0.03}: `{label:"+m%", price: spot·(1+m), pAbove(price)}` and the −m counterpart.
11. Chart down-sample: `target = 56`, `step = max(1, floor(221/56)) = 3`; take every 3rd node, `k` rounded to 2 decimals, `f` unrounded.
12. Also returned: `forward = round2(spot·e^{rT})`, `pAboveSpot = pAbove(spot)`, percentiles at q ∈ {0.05, 0.1, 0.25, 0.5, 0.75, 0.9, 0.95}.

**Helper `probInRange(result, a, b)`:** using the *down-sampled* density, `dk = d[1].k − d[0].k` (1 if fewer than 2 points), `P = clamp( Σ_{a ≤ k ≤ b} f·dk , 0, 1)`.

**Constants:**

| value | where used | semantic meaning | proposed name |
|---|---|---|---|
| 0.25 | T floor | min DTE in days | RN_MIN_DTE_DAYS |
| 0.2 | smile fallbacks | default IV when smile empty/zero | RN_FALLBACK_IV |
| 0.55 / 1.6 | grid | strike range as ×spot (−45% / +60%) | RN_GRID_LO_MULT / RN_GRID_HI_MULT |
| 220 | grid | grid step count (221 nodes) | RN_GRID_STEPS |
| 0.01 | repricing | IV floor into bsCall | RN_MIN_IV |
| 0.0455 | fat tail | Gaussian two-tail mass beyond ±2σ | GAUSSIAN_TWO_SIGMA_TAIL |
| 2 | fat tail | sigma multiple for tail band | FAT_TAIL_SIGMA_MULT |
| 0.997 / 1.003 | skew bias | mean-vs-median ±0.3% dead zone | SKEW_DEAD_ZONE_LO / _HI |
| 0.01, 0.02, 0.03 | levels | reference move sizes (±1/2/3%) | RN_LEVEL_MOVES |
| 56 | downsample | target chart points | RN_CHART_POINTS |
| 0.05 | default r | risk-free rate | DEFAULT_RISK_FREE_RATE |
| {.05,.1,.25,.5,.75,.9,.95} | percentiles | reported quantiles | RN_PERCENTILES |

**Outputs.** `RiskNeutralResult`: `dteDays`, `forward` (price, 2dp), `atmIv` (decimal), `pAboveSpot` [0,1], `levels[6]`, `percentiles` (prices), `expectedMovePct` (decimal fraction of spot, ≥0), `fatTailRatio` (≥0, ~1 = lognormal-like), `skewBias` (enum), `density[≈74]` `{k: price 2dp, f: per-unit-strike density}`.

---

### E4. IV surface model (`ivSurface.ts`)

**Purpose.** The feed only ships front-expiry per-contract IV, so the DTE axis is *modelled*: front row = real smile (reproduced exactly), deeper rows = front smile × an ATM term factor from a mean-reverting (Heston-style) forward-variance term structure. Sticky-moneyness: skew shape constant in K, only the level moves.

**Inputs.** `chain: ChainContract[]` (≥ 4 contracts required), `spot > 0`, `frontDteDays`, optional params `{theta (annualized long-run variance, default 0.18² = 0.0324), kappa (mean-reversion speed per year, default 3), horizonsDays (default [7,14,30,60,90,120]), windowPct (default 0.1)}`. Returns `null` if chain sparse (< 4 usable strikes) or invalid.

**Sub-algorithm `gTerm(v0, theta, kappa, T)`** — expected integrated variance over [0,T] divided by T:
- `T <= 0` → `v0`.
- `x = kappa·T`; `decay = x < 1e-6 ? 1 : (1 − e^{−x})/x`;
- `g = theta + (v0 − theta)·decay`.

**Algorithm — `buildIvSurfaceModel`:**
1. Strike window `[spot·(1−windowPct), spot·(1+windowPct)]`. Collect per-strike lists of finite `iv > 0` inside the window; per-strike front IV = arithmetic mean of the list (call/put blended). Need ≥ 4 distinct strikes else null. Strikes ascending.
2. ATM = strike nearest to spot (min |k − spot|); `atmFront = frontIv[atmIdx]`; `v0 = atmFront²`.
3. `Tfront = max(frontDteDays, 0.5)/365`; `gFront = gTerm(v0, theta, kappa, Tfront) || v0 || 1` (falsy-chain fallback).
4. DTE axis: `horizons = horizonsDays.filter(d > frontDteDays + 0.5)`; `dtes = [round(frontDteDays), ...horizons]` sorted ascending.
5. Per dte `d`: `T = max(d, 0.5)/365`; `f = sqrt( max(1e-8, gTerm(v0,theta,kappa,T)/gFront) )`; `atmTerm = atmFront·f`; row `iv[dteIdx][strikeIdx] = frontIv[strikeIdx]·f`. By construction `factor[0] = 1` (front row exact — up to the `round()` of frontDteDays changing T only for display, since gFront uses the unrounded value; see defects D14).

**Constants:**

| value | where used | semantic meaning | proposed name |
|---|---|---|---|
| 0.0324 (= 0.18²) | theta default | long-run annualized variance | SURFACE_THETA_DEFAULT |
| 3 | kappa default | mean-reversion speed /yr | SURFACE_KAPPA_DEFAULT |
| [7,14,30,60,90,120] | horizons default | modelled DTE axis (days) | SURFACE_HORIZONS_DAYS |
| 0.1 | windowPct default | ±10% strike window | SURFACE_WINDOW_PCT |
| 4 | validity gates | min chain length AND min distinct strikes | SURFACE_MIN_STRIKES |
| 0.5 | T floors + horizon filter | min DTE days; horizon must exceed front by > 0.5d | SURFACE_MIN_DTE_DAYS |
| 1e-6 | gTerm | small-x switch for (1−e^{−x})/x → 1 | GTERM_SMALL_X_EPS |
| 1e-8 | factor | floor on variance ratio before sqrt | SURFACE_RATIO_FLOOR |

**Outputs.** `IvSurfaceModel`: `strikes[]` (ascending), `dtes[]` (days, ascending, `[0]` = front), `iv[dte][strike]` (annualized decimals), `frontIv[]` (== iv[0]), `atmFront`, `atmTerm[]`, `factor[]` (factor[0] == 1), `theta`, `kappa`.

---

### E5. Realized-vol estimator suite (`realizedVol.ts`)

**Purpose.** Five RV estimators from streamed OHLC candles, all annualized. Yang–Zhang is the headline (`primary`).

**Inputs.** `candles: Candle[]` (chronological), `period/lookback` (bars, default 20).

**Shared machinery:**
- `intervalMinutes(candles)`: median of positive finite `(ts[i] − ts[i−1])/60000` diffs; `< 2` candles or no diffs → 5. Median = `diffs[floor(len/2)] || 5` after ascending sort (upper median). If median `< 0.5` or `> 1440` → **5** (guards non-epoch "timestamps" like bar indices).
- `periodsPerYear(m) = (252 · 390) / max(m, 1e-6)` — trading year = 252 days × 390 RTH minutes = 98,280 min.
- `annFactor(candles) = sqrt(periodsPerYear(intervalMinutes(candles)))`.
- `tail(candles, p)`: last `clamp(p, 2, len)` candles.

All estimators multiply the per-bar vol by `annFactor(candles)` (computed on the FULL array, not the tail slice) and clamp negative variances to 0 via `max(0, ·)` before sqrt.

1. **Close-to-close** (`closeToCloseVol`, needs ≥ 3 candles else 0): over `tail(candles, period+1)`, log returns `ln(C_i/C_{i−1})` for consecutive pairs with both closes > 0; needs ≥ 2 returns; **sample variance with mean subtraction, divisor (n−1)**; `σ_ann = sqrt(var)·annFactor`.
2. **Parkinson** (needs ≥ 2 candles): over `tail(candles, period)`, for bars with high>0 and low>0: `sum += ln(H/L)²`; `perBarVar = sum / (4·ln2·n)`; `σ_ann = sqrt(perBarVar)·annFactor`.
3. **Garman–Klass**: same tail; for bars with all of H,L,O,C > 0: `sum += 0.5·ln(H/L)² − (2·ln2 − 1)·ln(C/O)²`; `σ_ann = sqrt(max(0, sum/n))·annFactor`.
4. **Rogers–Satchell** (drift-independent): `sum += ln(H/C)·ln(H/O) + ln(L/C)·ln(L/O)`; `σ_ann = sqrt(max(0, sum/n))·annFactor`.
5. **Yang–Zhang** (needs ≥ 4 candles else falls back to close-to-close): over `tail(candles, period+1)` (n = len−1 pairs), for each pair (bar i, prev i−1) with O,C,H,L of bar and prev close all > 0:
   - overnight `o_i = ln(O_i / C_{i−1})`, open-to-close `c_i = ln(C_i / O_i)`, RS accumulator `rs += ln(H/C)·ln(H/O) + ln(L/C)·ln(L/O)`.
   - `m` = count of accepted pairs (needs ≥ 2). `σ_o² = Var(o)` and `σ_c² = Var(c)` with divisor (m−1) and mean subtraction; `σ_rs² = rs/m` (divisor m, no mean).
   - `k = 0.34 / (1.34 + (m+1)/(m−1))`.
   - `σ_yz² = σ_o² + k·σ_c² + (1−k)·σ_rs²`; `σ_ann = sqrt(max(0, σ_yz²))·annFactor`.

`computeRealizedVol(candles, lookback=20)` returns all five + `primary = yangZhang`, `intervalMinutes`, `lookback`.

**Constants:**

| value | where used | semantic meaning | proposed name |
|---|---|---|---|
| 5 | intervalMinutes fallback | default bar interval (min) | DEFAULT_BAR_MINUTES |
| 60000 | intervalMinutes | ms per minute | MS_PER_MINUTE |
| 0.5 / 1440 | interval plausibility band | min/max accepted median interval (min) | INTERVAL_MIN_PLAUSIBLE / INTERVAL_MAX_PLAUSIBLE |
| 252 | periodsPerYear | trading days per year | TRADING_DAYS_PER_YEAR |
| 390 | periodsPerYear | RTH minutes per trading day | RTH_MINUTES_PER_DAY |
| 1e-6 | periodsPerYear | divisor floor | INTERVAL_EPS |
| 20 | default period/lookback | RV window (bars) | RV_DEFAULT_LOOKBACK |
| 4·ln2 | Parkinson | Parkinson denominator coefficient | PARKINSON_DENOM (4·LN2) |
| 0.5, (2·ln2 − 1) | Garman–Klass | GK term coefficients | GK_HL_COEFF, GK_CO_COEFF |
| 0.34, 1.34 | Yang–Zhang | k-weight formula constants | YZ_K_NUM, YZ_K_DEN_BASE |
| 3 / 2 / 4 | min-candle gates | c2c ≥3, park/gk/rs ≥2, yz ≥4 | RV_MIN_CANDLES_* |

**Outputs.** All estimators: annualized decimal vol ≥ 0 (0 = insufficient data).

---

### E6. Volatility cone + Variance Risk Premium (`realizedVol.ts`)

**Purpose.** Where does current RV sit in its own rolling history (per window), and is IV rich or cheap vs RV.

**Algorithm — `quantile(sorted, q)`** (linear interpolation): empty → 0; single → that value; `idx = clamp(q·(len−1), 0, len−1)`, `lo = floor(idx)`, `hi = ceil(idx)`, result `= sorted[lo] + (sorted[hi] − sorted[lo])·(idx − lo)`.

**Algorithm — `volCone(candles, windows=[10,20,30,60])`:** per window `w` (skip if `len < w+3`):
1. Rolling series: for `end = w+1 .. len`, `closeToCloseVol(candles.slice(end−(w+1), end), w)` — one RV value per bar, full history.
2. `valid` = values > 0 (skip window if < 2). `current = series[last]` (may itself be 0 — see defects).
3. Sort valid ascending; `percentile = round( count(valid ≤ current)/len(valid) · 100 )`.
4. Bucket: `{window, current, min, p25 = quantile(.25), median = quantile(.5), p75 = quantile(.75), max, percentile}`.

**Algorithm — `computeVRP(iv, candles, lookback=20)`:**
1. `rv = yangZhangVol(candles, lookback)`; `rvPercentile` from `volCone(candles, [lookback])` (50 if cone empty).
2. If `!(rv > 0)`: return `{iv, rv: 0, vrp: 0, ratio: 0, richness: 'N/A', rvPercentile}` (explicit no-data read).
3. `vrp = iv − rv` (vol points, decimal); `ratio = iv/rv`.
4. `richness`: `ratio >= 1.15` → `'IV RICH'`; `ratio <= 0.9` → `'IV CHEAP'`; else `'NEUTRAL'`.

**Constants:**

| value | where used | semantic meaning | proposed name |
|---|---|---|---|
| [10,20,30,60] | volCone windows | cone lookbacks (bars) | VOL_CONE_WINDOWS |
| w+3 | volCone gate | min candles per window | VOL_CONE_MIN_EXTRA (3) |
| 1.15 | VRP | IV/RV ratio → IV RICH | VRP_RICH_RATIO |
| 0.9 | VRP | IV/RV ratio → IV CHEAP | VRP_CHEAP_RATIO |
| 50 | VRP | rvPercentile fallback | VRP_PERCENTILE_FALLBACK |

**Outputs.** `VolConeBucket[]` (vols annualized decimals, percentile 0–100 int). `VRPResult` `{iv, rv, vrp (decimal points), ratio, richness enum, rvPercentile}`.

---

### E7. Cross-asset PCA residual stat-arb (`crossAsset.ts`)

**Purpose.** PC1 "market factor" across the index complex from standardized returns; flags assets whose idiosyncratic residual z-score exceeds a threshold (RICH/CHEAP).

**Inputs.** `series: Record<ticker, Candle[]>` (assumed time-aligned by recency — no timestamp check), `window = 60` (returns), `zThresh = 2`.

**Algorithm — `pcaResidualZScores`:**
1. Need ≥ 2 tickers else `{}`. Per ticker: log returns of closes (pairs with both > 0), keep last `window`; `minLen` = shortest across tickers; require `minLen ≥ 20` else `{}`.
2. Standardize each ticker's last `minLen` returns: `z = (r − mean(r)) / (std(r) || 1)` (Bessel std).
3. Correlation matrix (n×n): `C[i][j] = ( Σ_k z_i[k]·z_j[k] ) / (minLen − 1)` (symmetric; diagonal = 1 exactly by construction).
4. PC1 via power iteration: `w ← (1/√n, …)`; **60 iterations** of `w ← normalize(C·w)` (L2 norm, `|| 1` guard). No deflation, no convergence test.
5. Factor series: `f[k] = Σ_i w_i · z_i[k]`. Raw factor variance `fVarRaw = (Σ f²)/(minLen−1) || 1`; **floored**: `fVar = max(fVarRaw, 1e-3)` (bounds betas when the factor degenerates).
6. Per ticker i: `cov = (Σ_k z_i[k]·f[k])/(minLen−1)`; `beta = cov/fVar`; residuals `resid[k] = z_i[k] − beta·f[k]`; `rs = std(resid) || 1`; `z = resid[last]/rs` (residual mean not subtracted from the last point; both series are ≈ zero-mean).
7. Output per ticker: `{ z: 2dp, beta: 2dp, active: |z| ≥ zThresh, direction: z ≥ zThresh → 'RICH', z ≤ −zThresh → 'CHEAP', else 'FAIR' }`.

**Constants:**

| value | where used | semantic meaning | proposed name |
|---|---|---|---|
| 60 | window default | returns lookback | PCA_WINDOW |
| 2 | zThresh default | residual z divergence threshold (σ) | PCA_Z_THRESHOLD |
| 20 | minLen gate | min aligned returns | PCA_MIN_RETURNS |
| 60 | power iteration | iteration count | PCA_POWER_ITERS |
| 1e-3 | fVar floor | min factor variance (unit-variance baseline ×0.001) | PCA_FACTOR_VAR_FLOOR |
| 1 | `std || 1` guards | degenerate-std fallback | (guard, not tunable) |

**Outputs.** `Record<ticker, {z (σ, 2dp), beta (2dp, loading on PC1), active: bool, direction enum}>`.

---

### E8. Wilder ATR series + percentile rank (`displacementEngine.ts` helpers)

**`wilderATRSeries(candles, period=14)`** — RMA-smoothed true range, full series:
1. `TR_0 = H_0 − L_0`; `TR_i = max(H−L, |H−prevC|, |L−prevC|)`.
2. If `n < period`: running mean warmup `atrs[i] = (Σ_{j≤i} TR_j)/(i+1)` for all i (keeps ratios finite).
3. Else: `atrs[period−1] = mean(TR_0..TR_{period−1})`; then Wilder recursion `atr = (atr·(period−1) + TR_i)/period`; indices `0..period−2` are **backfilled** with `atrs[period−1]`.

**`percentileRank(series, window)`**: slice = last `window` values; `last` = final value; if len ≤ 1 → 50 (empty series → 50). `rank = ( count(v < last, strict) / (slice.length − 1) ) · 100`. Note: the last element itself is in the slice; strict `<` means an all-equal window ranks 0.

**`fractalPivots(candles, width=2)`**: index i (width ≤ i < n−width) is a pivot high iff `high[i]` strictly exceeds highs of ALL j ∈ [i−width, i+width], j ≠ i (`>=` by any neighbor disqualifies); mirrored for lows. A pivot is only *confirmed* (usable) at bar `i + width`.

**v11Math dependencies (transcribed for fidelity — regime engine uses these, not the local ATR):**
- `calculateWilderRSI(candles)`: array pre-filled with 50; needs ≥ 15 candles. First 14 deltas → `avgGain/avgLoss` simple means; `rsis[14] = avgLoss==0 ? 100 : 100 − 100/(1 + avgGain/avgLoss)`; then Wilder smoothing `avg = (avg·13 + x)/14` per bar. Indices 0..13 stay 50.
- `calculateWilderATR(candles)`: array pre-filled with **0.1**; needs ≥ 15 candles; `atrs[13]` = mean of first 14 TRs; Wilder recursion `(atr·13 + TR)/14` thereafter. Indices 0..12 stay 0.1 (NOT backfilled — differs from `wilderATRSeries`).

**Constants:** period 14 (WILDER_PERIOD), width 2 (PIVOT_WIDTH), warmup fill 50 (RSI) / 0.1 (v11 ATR default), percentile fallback 50.

---

### E9. Displacement zones (`detectDisplacementZones`)

**Purpose.** Flags candles whose body is both large vs ATR and dominant vs range; each becomes an order-block-style zone with a lifecycle state.

**Inputs.** `candles`, opts `{atrMultiple=1.4, bodyDominance=0.55, maxZones=12}`. Needs ≥ 3 candles.

**Algorithm:**
1. `atrs = wilderATRSeries(candles)` (period 14).
2. For i = 1..n−1: `body = |C−O|`; `range = max(H−L, 1e-9)`; `dominance = body/range`; `force = body/(atr_i || 1e-9)`.
3. Displacement iff `force >= 1.4 && dominance >= 0.55`. Then:
   - `type = C >= O ? 'bullish' : 'bearish'`; zone `[bottom, top] = [min(O,C), max(O,C)]`; `equilibrium = (top+bottom)/2`.
   - `score = round( min(force/3, 1)·60 + dominance·25 + min((relativeVolume ?? 1)/3, 1)·15 )` — range 0..100.
   - `flags[i] = true`; push zone with `state: 'ACTIVE'`, `atrMultiple = force` (2dp), `bodyDominance = dominance` (2dp).
4. State resolution — forward walk per zone from `createdAtIdx+1`:
   - `touched = (low_j <= top && high_j >= bottom)` (range overlap). First touch while ACTIVE → `MITIGATED` (records `mitigatedAtIdx`; loop continues).
   - Bullish zone: `close_j < bottom` → `INVALIDATED` (records idx, **break**). Bearish: `close_j > top` → `INVALIDATED`, break.
5. Return last `maxZones` (12) zones by creation order (states already resolved on the full set) + per-candle `flags` array.

**Constants:**

| value | where used | meaning | name |
|---|---|---|---|
| 1.4 | detection | min body/ATR "force" | DISPLACEMENT_ATR_MULTIPLE |
| 0.55 | detection | min body/range dominance | DISPLACEMENT_BODY_DOMINANCE |
| 12 | output cap | max zones kept | DISPLACEMENT_MAX_ZONES |
| 3 | score | force saturation (force/3 capped at 1) | SCORE_FORCE_SAT |
| 60 / 25 / 15 | score | weights: force / dominance / rel-volume | SCORE_W_FORCE / _DOM / _RVOL |
| 3 | score | relative-volume saturation | SCORE_RVOL_SAT |
| 1e-9 | guards | zero-range & zero-ATR guards | RANGE_EPS |
| 1 | rvol default | relativeVolume fallback | RVOL_DEFAULT |

**Outputs.** `zones[]` (`type`, `top/bottom/equilibrium` prices, `atrMultiple`, `bodyDominance`, `score` 0–100 int, `state`, `mitigatedAtIdx?`, `invalidatedAtIdx?`), `flags: boolean[n]`.

---

### E10. Fair-value gaps with state machine (`detectFairValueGaps`)

**Purpose.** 3-candle imbalance gaps (ICT FVG) with a 5-state lifecycle.

**Inputs.** `candles`, `minBodyRatio = 0.4`.

**Detection** (for i = 2..n−1 with `a = c[i−2]`, `b = c[i−1]`, `c = c[i]`):
- Middle candle must be impulsive: `|b.close − b.open| / max(b.high − b.low, 1e-9) >= 0.4` **OR** `b.isDisplacement === true`.
- Bullish FVG iff `c.low > a.high`: `top = c.low`, `bottom = a.high`.
- Bearish FVG iff `c.high < a.low`: `top = a.low`, `bottom = c.high`.
- `equilibrium = (top + bottom)/2`; initial `state = 'ARMED'`.

**State machine** — forward walk per FVG from `createdAtIdx+1`; skip candles with no overlap (`!(low <= top && high >= bottom)`); on first overlap set `testedAtIdx` if unset. Then (bullish case; bearish is the mirror with close/high vs top):
1. `close < bottom` → `INVALIDATED` (idx recorded, break).
2. else `low <= bottom` (wick reaches the far edge = fully filled) → `COMPLETED` (idx recorded, break).
3. else `state = low < equilibrium ? 'TESTED' : 'HELD'` — **no break**; state keeps being overwritten by later touches (last touch wins); `heldAtIdx` records the most recent HELD assignment.

Bearish mirror: INVALIDATED on `close > top`; COMPLETED on `high >= top`; TESTED when `high > equilibrium`, else HELD.

**Constants:** 0.4 = FVG_MIN_BODY_RATIO; 1e-9 range guard. Composite caller keeps last **14** FVGs (FVG_DISPLAY_CAP).

**Outputs.** `FairValueGap[]`: `{type, top, bottom, equilibrium, state ∈ {ARMED, TESTED, HELD, INVALIDATED, COMPLETED}, createdAtIdx, testedAtIdx?, heldAtIdx?, invalidatedAtIdx?, completedAtIdx?}`.

---

### E11. Liquidity sweeps (`detectLiquiditySweeps`)

**Purpose.** Wick-through-and-reject events at confirmed fractal pivot levels.

**Inputs.** `candles`, `pivotWidth = 2`. Needs `n >= pivotWidth·2 + 2`.

**Algorithm:**
1. `{highs, lows} = fractalPivots(candles, 2)`; `globalHigh/globalLow` = max high / min low over ALL candles.
2. For i = pivotWidth+1 .. n−1: `lastHigh` = most recent pivot high with `index + width <= i` (confirmed); same for `lastLow`. (Implemented as reverse-scan per candle.)
3. Sweep high (bearish): `high_i > lastHigh.price && close_i < lastHigh.price`. Label: `'External Liquidity Grab'` if `|lastHigh.price − globalHigh| < 1e-9` else `'Liquidity Sweep High'`.
4. Sweep low (bullish): `low_i < lastLow.price && close_i > lastLow.price`. `spike = (lastLow.price − low_i) / max(high_i − low_i, 1e-9)`. Label: external grab if pivot == globalLow (1e-9 tol), else `'Stop Run'` if `spike > 0.5`, else `'Liquidity Sweep Low'`.
5. Dedup pass: drop event when the **immediately preceding event in the output array** has same price, same type, and `candleIdx` within 1.

**Constants:** pivotWidth 2; 1e-9 price tolerance (GLOBAL_LEVEL_EPS); 0.5 spike ratio (STOP_RUN_SPIKE_RATIO); dedup gap 1 candle; composite caller keeps last 14.

**Outputs.** `LiquidityEvent[]`: `{label enum, price (pivot level), candleIdx, type: 'bearish' (high sweep) | 'bullish' (low sweep)}`.

---

### E12. Market structure BOS/CHoCH (`analyzeMarketStructure`)

**Purpose.** Stateful break-of-structure / change-of-character detection over confirmed pivots + premium/discount read.

**Inputs.** `candles`, `pivotWidth = 2`.

**Algorithm:**
1. Confirmed-pivot pointers advance with i: pivot usable when `index + width <= i`. `lastHigh/lastLow` = most recent confirmed pivots.
2. For each bar close: if `close > lastHigh.price` AND that exact price hasn't been broken before (`lastBrokenHigh !== price`): emit event `{kind: trend === 'bearish' ? 'CHoCH' : 'BOS', direction: 'bullish', price}`; set `trend = 'bullish'`; remember `lastBrokenHigh`. Mirror for lows (`close < lastLow.price`, CHoCH iff trend was 'bullish', sets trend 'bearish'). Both can fire on one bar (up first, then down).
3. Range read: `rangeHigh/rangeLow` = LAST pivot high/low prices (not extremes; null if none). `equilibrium = (rangeHigh + rangeLow)/2`. `band = (rangeHigh − rangeLow)·0.1`. `pricePosition`: `|close − equilibrium| <= band` → `'EQUILIBRIUM'`; `close > equilibrium` → `'PREMIUM'`; else `'DISCOUNT'`.
4. Events truncated to last **14**. Trend starts `'neutral'`.

**Constants:** pivotWidth 2; 0.1 equilibrium band fraction (EQUILIBRIUM_BAND_FRAC); 14 event cap (STRUCTURE_EVENT_CAP).

**Outputs.** `{events[≤14] {kind BOS|CHoCH, direction, price, candleIdx, timestamp}, trend ∈ {bullish, bearish, neutral}, rangeHigh, rangeLow, equilibrium, pricePosition ∈ {PREMIUM, DISCOUNT, EQUILIBRIUM, null}}`.

---

### E13. Volatility engine — ATR/bandwidth regime (`computeVolatilityEngine`)

**Purpose.** COMPRESSION/NEUTRAL/EXPANSION regime + squeeze flag + 0–100 "energy" from real percentiles.

**Inputs.** `candles`, `lookback = 100`. `n < 5` → neutral defaults `{regime NEUTRAL, atrPercentile 50, bandwidthPercentile 50, squeeze false, energy 50, atr 0, atrSlope 1}`.

**Algorithm:**
1. `atrs = wilderATRSeries(candles)`; `atrPct = percentileRank(atrs, 100)`; `atr = atrs[n−1]`; `atrPrev = atrs[max(0, n−11)] || atr` (10 bars back); `atrSlope = atrPrev > 0 ? atr/atrPrev : 1`.
2. Bollinger bandwidth series: `period = min(20, n)`; for each i from period−1: window closes, `mean`, `sd = sqrt( Σ(x−mean)²/period )` (**population divisor**), `bw = mean > 0 ? 4·sd/mean : 0`. `bwPct = percentileRank(bw, 100)`.
3. `regime = atrPct >= 70 ? 'EXPANSION' : atrPct <= 30 ? 'COMPRESSION' : 'NEUTRAL'`.
4. `squeeze = bwPct <= 15`.
5. `rvol = min((relativeVolume_last ?? 1)/3, 1)`; `slopeNorm = clamp((atrSlope − 0.8)/0.8, 0, 1)`;
   `energy = clamp( round(0.45·atrPct + 0.35·slopeNorm·100 + 0.20·rvol·100), 0, 100)`.

**Constants:**

| value | meaning | name |
|---|---|---|
| 100 | percentile lookback (bars) | VOL_ENGINE_LOOKBACK |
| 5 | min candles | VOL_ENGINE_MIN_CANDLES |
| 11 | ATR slope offset (n−11 ⇒ 10 bars back) | ATR_SLOPE_LOOKBACK |
| 20 | Bollinger period cap | BB_PERIOD |
| 4 | bandwidth multiplier (±2σ width / mean) | BB_WIDTH_MULT |
| 70 / 30 | ATR percentile regime thresholds | EXPANSION_PCTILE / COMPRESSION_PCTILE |
| 15 | bandwidth percentile squeeze threshold | SQUEEZE_PCTILE |
| 0.45 / 0.35 / 0.20 | energy weights (atrPct / slope / rvol) | ENERGY_W_ATR / _SLOPE / _RVOL |
| 0.8 / 0.8 | slope normalization: (slope − 0.8)/0.8 | SLOPE_NORM_OFFSET / SLOPE_NORM_SCALE |
| 3 | rvol saturation divisor | ENERGY_RVOL_SAT |

**Outputs.** `{regime enum, atrPercentile (0–100, 1dp), bandwidthPercentile (1dp), squeeze bool, energy int 0–100, atr (price units, 6dp), atrSlope (ratio, 3dp)}`.

`computeDisplacementIntelligence(candles)` = one-call composite: `{zones, fvgs (last 14), sweeps (last 14), structure, volatility, displacementFlags}`.

---

### E14. Hurst exponent — R/S analysis (`regimeEngine.ts`)

**Purpose.** Trend persistence: H > 0.5 trending, < 0.5 mean-reverting.

**Inputs.** `series: number[]` (prices). Uses log returns; `N = returns count`; `N < 32` → 0.5.

**Algorithm:**
1. Window ladder: `n = 8`, then `n = floor(n·1.6)` while `n <= floor(N/2)` (8, 12, 19, 30, 48, 76, …).
2. Per window size n: split returns into `floor(N/n)` consecutive non-overlapping chunks. Per chunk: demean (`x − mean`), cumulative sum, `R = max(cum) − min(cum)`, `S = std(chunk)` (Bessel). Accept chunk iff `S > 1e-12 && R > 0`. Window's R/S = mean of accepted chunks' R/S (window skipped if none).
3. OLS slope of `ln(R/S)` on `ln(n)` across windows (need ≥ 2 points else 0.5): `H = Σ(x−x̄)(y−ȳ) / Σ(x−x̄)²` (0.5 if denominator ≤ 0).
4. Clamp `H` to [0, 1].

**Constants:** 32 min returns (HURST_MIN_RETURNS), 8 first window (HURST_BASE_WINDOW), 1.6 ladder growth (HURST_WINDOW_GROWTH), N/2 max window, 1e-12 std guard, 0.5 fallback.

**Outputs.** `H ∈ [0,1]`.

---

### E15. Ornstein–Uhlenbeck half-life (`ornsteinUhlenbeck`)

**Purpose.** Mean-reversion speed/half-life from an AR(1) on a *stationarized* series (deviation from trailing SMA rather than raw level — strips local drift).

**Inputs.** `series: number[]` (prices), `intervalMin = 5` (bar minutes), `meanWindow = 20` (bars). Filter to `x > 0 && finite`; `< 20` points → `{theta 0, mu mean(px), halfLife ∞, meanReverting false}`.

**Algorithm:**
1. `w = clamp(meanWindow, 2, len−1)`. Trailing (causal) rolling mean `m_t` = mean of `px[max(0, t−w+1) .. t]`.
2. Regression samples for t = 1..len−1: regressor `dev_t = px[t−1] − m_{t−1}`; response `dx_t = px[t] − px[t−1]`.
3. OLS with intercept: `b = Σ(dev−d̄)(dx−dx̄) / Σ(dev−d̄)²` (0 if denominator ≤ 0); `a = dx̄ − b·d̄`.
4. `θ = (0 < 1+b < 1) ? −ln(1+b) : (b < 0 ? −b : 0)` — i.e. proper OU speed for −1 < b < 0; linear fallback `−b` for b ≤ −1; 0 for b ≥ 0.
5. Equilibrium price: `mu = b ≠ 0 ? rollMean(last) − a/b : rollMean(last)` (from `a + b·dev* = 0` ⇒ `dev* = −a/b`).
6. `halfLifeBars = θ > 1e-9 ? ln(2)/θ : ∞`; `halfLifeMinutes = halfLifeBars · (intervalMin valid ? intervalMin : 5)`.
7. `meanReverting = b < −1e-4`.
8. Rounding on output: θ 5dp, mu 2dp, half-lives 1dp (∞ preserved).

**Constants:** 20 min points (OU_MIN_POINTS), 20 mean window (OU_MEAN_WINDOW), 5 default interval minutes, 1e-9 θ floor for half-life, −1e-4 mean-reversion b threshold (OU_B_THRESHOLD).

**Outputs.** `{theta: per-bar speed ≥ 0, mu: price, halfLifeBars, halfLifeMinutes, meanReverting: bool}`.

---

### E16. Regime classifier — softmax over Gaussian features (`classifyRegime`)

**Purpose.** Keyless "HMM-style" 3-state classifier: TREND_EXPANSION / MEAN_REVERSION / TAIL_RISK from realized-vol level, kurtosis, and Hurst.

**Inputs.** `candles`. Uses positive-finite closes; log returns over ALL of them.

**Algorithm:**
1. `hurst = hurstExponent(closes)`. If `< 10` returns: state MEAN_REVERSION, posteriors {0.33, 0.34, 0.33}, transitionProb 34.
2. `recent = last 30 returns`; `vol = std(recent)`; `m = mean(recent)`; `kurt = vol > 0 ? mean( ((r−m)/vol)^4 ) : 3` (raw kurtosis; Gaussian ≈ 3).
3. `volBaseline = std(ALL returns)` (variable misnamed `volPctile` in code); `volRatio = volBaseline > 0 ? vol/volBaseline : 1`.
4. Energies (unnormalized, each term floored at 0):
   - `trendE  = max(0, (hurst − 0.5)·6) + max(0, (volRatio − 1)·1.2)`
   - `revertE = max(0, (0.5 − hurst)·6) + max(0, (1 − volRatio)·1.5) + 0.4`
   - `tailE   = max(0, (kurt − 4)·0.5)  + max(0, (volRatio − 1.6)·2)`
5. Softmax: `posterior_s = e^{E_s} / Σ e^{E}` (sum guarded `|| 1`).
6. `state = argmax posterior` (ties: first in object order TREND_EXPANSION, MEAN_REVERSION, TAIL_RISK; init favors MEAN_REVERSION only if all ≤ −1, which cannot happen). `transitionProb = round(maxPosterior·100)` — despite the name this is the **dominant-state confidence**, not a transition probability.

**Constants:**

| value | meaning | name |
|---|---|---|
| 10 | min returns | REGIME_MIN_RETURNS |
| 30 | recent-vol window (returns) | REGIME_RECENT_WINDOW |
| 6 | Hurst energy slope (both trend & revert) | REGIME_HURST_GAIN |
| 1.2 | trend vol-ratio gain | REGIME_TREND_VOL_GAIN |
| 1.5 | revert vol-ratio gain | REGIME_REVERT_VOL_GAIN |
| 0.4 | revert base energy (prior) | REGIME_REVERT_PRIOR |
| 4 | kurtosis threshold for tail energy | REGIME_KURT_THRESHOLD |
| 0.5 | tail kurtosis gain | REGIME_TAIL_KURT_GAIN |
| 1.6 | vol-ratio threshold for tail | REGIME_TAIL_VOL_THRESHOLD |
| 2 | tail vol-ratio gain | REGIME_TAIL_VOL_GAIN |
| 3 | kurtosis fallback (Gaussian) | GAUSSIAN_KURTOSIS |
| 0.33/0.34/0.33, 34 | insufficient-data posteriors/confidence | REGIME_FLAT_PRIORS |

**Outputs.** `{state enum, posteriors: 3 probs summing to 1, transitionProb int 0–100, hurst [0,1]}`.

---

### E17. Vol compression / expansion / RV term-structure flags (`regimeEngine.ts`)

All three return `VolRegime = {active: bool, score: number (0..1, 2dp), detail: string}`.

**`volCompression(candles)`** — EMA pinch + flat RSI:
1. Needs ≥ 60 closes else inactive/0. EMAs: `ema(px, p)` with `k = 2/(p+1)`, seeded with `px[0]`, for p ∈ {8, 21, 50, 200}.
2. `spread_t = std([e8_t, e21_t, e50_t, e200_t]) / (px_t || 1)` (Bessel std of the 4 EMA values; normalized dispersion).
3. History: `spread_j` for j from `max(50, len−120)` to last index → ascending sort → `rank = count(v <= spread_last)/count` (0.5 if empty). Note `<=` counting includes self ⇒ rank > 0 always.
4. `rsi = last of calculateWilderRSI(candles)` (50 if empty); `rsiFlat = |rsi − 50| < 10`.
5. `active = rank <= 0.2 && rsiFlat`; `score = (1 − rank) · (rsiFlat ? 1 : 0.5)`.

**`volExpansion(candles)`** — ATR expansion + persistence:
1. Needs ≥ 30 candles; `atr = calculateWilderATR(candles)` (v11Math version), needs ≥ 20 values.
2. `cur = atr[last]`; `base = mean(atr[len−20 .. len−2])` (19 values, excludes current); `ratio = base > 0 ? cur/base : 1`.
3. `hurst = hurstExponent(closes)`; `rvShort = closeToCloseVol(candles, 10)`; `rvLong = closeToCloseVol(candles, 40)`; `volRising = rvLong > 0 ? rvShort/rvLong : 1`.
4. `active = ratio > 1.25 && (hurst > 0.5 || volRising > 1.2)`; `score = clamp((ratio − 1)·1.5, 0, 1)`.

**`forwardVolMatrix(candles)`** — trailing RV term-structure inversion (name is legacy; it is NOT forward/implied vol):
1. Needs ≥ 50 candles. `rvNear = closeToCloseVol(candles, 10)`; `rvFar = closeToCloseVol(candles, 40)`; `rvRatio = rvFar > 0 ? rvNear/rvFar : 1`.
2. `active = rvRatio > 1.2`; `score = clamp((rvRatio − 1)·2, 0, 1)`.

**Constants:**

| value | where | meaning | name |
|---|---|---|---|
| 60 | compression gate | min closes | COMPRESSION_MIN_BARS |
| 8, 21, 50, 200 | compression | EMA periods | EMA_PERIODS |
| 120 / 50 | compression history | spread baseline window / earliest start index | SPREAD_HIST_WINDOW / SPREAD_HIST_MIN_START |
| 0.2 | compression | spread percentile threshold | COMPRESSION_RANK_THRESHOLD |
| 10 | compression | RSI flat band half-width around 50 | RSI_FLAT_BAND |
| 0.5 | compression score | penalty multiplier when RSI not flat | COMPRESSION_RSI_PENALTY |
| 30 / 20 | expansion gates | min candles / min ATR values | EXPANSION_MIN_BARS / _MIN_ATR |
| 20 (window), −1 (exclusion) | expansion | ATR baseline = mean of slice(−20,−1) | ATR_BASE_WINDOW |
| 1.25 | expansion | ATR ratio threshold | EXPANSION_ATR_RATIO |
| 0.5 | expansion | Hurst persistence threshold | EXPANSION_HURST_THRESHOLD |
| 1.2 | expansion + fvm | RV rising / near-vs-far ratio threshold | RV_RATIO_THRESHOLD |
| 1.5 / 2 | scores | score gains: (ratio−1)·gain | EXPANSION_SCORE_GAIN / FVM_SCORE_GAIN |
| 10 / 40 | RV windows | near / far close-to-close windows (bars) | RV_NEAR_WINDOW / RV_FAR_WINDOW |
| 50 | fvm gate | min candles | FVM_MIN_BARS |

---

## State semantics

The rebuild collapses states to binary ACTIVE/INACTIVE + a continuous score, so each enum below lists (a) exact legacy conditions and (b) the continuous quantity being thresholded.

1. **`ZoneState` ∈ {ACTIVE, MITIGATED, INVALIDATED}** (E9). ACTIVE at creation. MITIGATED on first bar j > created with `low_j <= top && high_j >= bottom` (any overlap). INVALIDATED (terminal) on `close < bottom` (bullish) / `close > top` (bearish). Continuous quantities: creation strength = `force = body/ATR` (threshold 1.4) and `dominance = body/range` (threshold 0.55), summarized in `score` (0–100); lifecycle depth = signed distance of close beyond the invalidation edge, and overlap depth for mitigation. Binary mapping: ACTIVE→ACTIVE with score; MITIGATED/INVALIDATED→INACTIVE (keep `score` and the invalidation distance as the continuous residue).
2. **`FVGState` ∈ {ARMED, TESTED, HELD, INVALIDATED, COMPLETED}** (E10). ARMED until first overlap. On each overlapping bar (bullish): INVALIDATED iff `close < bottom` (terminal); COMPLETED iff `low <= bottom` (terminal, gap fully filled by wick); else TESTED iff `low < equilibrium` (penetrated past midpoint), HELD otherwise (touched but held above midpoint). Bearish mirrors with high/top. TESTED/HELD are re-evaluated on every subsequent touch (last write wins). **Continuous quantity: fill fraction** — bullish `fill = (top − min(low_touched…)) / (top − bottom)` ∈ [0, 1+]; TESTED ⇔ fill > 0.5, COMPLETED ⇔ fill ≥ 1, INVALIDATED ⇔ close beyond fill = 1 boundary.
3. **`VolRegime` (displacement) ∈ {COMPRESSION, NEUTRAL, EXPANSION}** (E13). Thresholds on `atrPercentile` (continuous 0–100): ≥ 70 EXPANSION, ≤ 30 COMPRESSION, else NEUTRAL. `squeeze` boolean thresholds `bandwidthPercentile <= 15`. Continuous: atrPercentile, bandwidthPercentile, energy (0–100 composite).
4. **`RegimeState` ∈ {TREND_EXPANSION, MEAN_REVERSION, TAIL_RISK}** (E16). Argmax of softmax posteriors; continuous quantities are the three posteriors (and upstream: hurst, volRatio, kurt). `transitionProb` = max posterior ×100.
5. **VRP `richness` ∈ {IV RICH, NEUTRAL, IV CHEAP, N/A}** (E6). Continuous: `ratio = iv/rv`. IV RICH ⇔ ratio ≥ 1.15; IV CHEAP ⇔ ratio ≤ 0.9; N/A ⇔ rv ≤ 0 (no data); else NEUTRAL.
6. **`skewBias` ∈ {DOWNSIDE SKEW, UPSIDE SKEW, SYMMETRIC}** (E3). Continuous: `mean/p50 − 1`. DOWNSIDE ⇔ < −0.003; UPSIDE ⇔ > +0.003; else SYMMETRIC.
7. **PCA `direction` ∈ {RICH, CHEAP, FAIR}** + `active` (E7). Continuous: residual z. RICH ⇔ z ≥ 2; CHEAP ⇔ z ≤ −2; active ⇔ |z| ≥ 2.
8. **Structure `trend` ∈ {bullish, bearish, neutral}** (E12): set by last structure break; neutral before any. **`pricePosition` ∈ {PREMIUM, DISCOUNT, EQUILIBRIUM, null}**: continuous quantity `(close − equilibrium)/(rangeHigh − rangeLow)`; EQUILIBRIUM ⇔ |·| ≤ 0.1; PREMIUM ⇔ > 0; DISCOUNT ⇔ < 0; null when either pivot missing. Event `kind` BOS vs CHoCH: CHoCH iff the break direction opposes the prevailing trend.
9. **Liquidity `label`** (E11): External Liquidity Grab ⇔ swept pivot equals the global extreme (1e-9); Stop Run ⇔ low-sweep spike ratio > 0.5; else Liquidity Sweep High/Low. Continuous: spike ratio.
10. **Regime-engine `VolRegime {active, score}`** (E17): compression active ⇔ spread-rank ≤ 0.2 AND |RSI−50| < 10 (continuous: rank, RSI distance; score = (1−rank)·penalty); expansion active ⇔ ATR ratio > 1.25 AND (H > 0.5 OR RV ratio > 1.2) (continuous: ratio; score = (ratio−1)·1.5 clamped); forwardVolMatrix active ⇔ near/far RV > 1.2 (score = (ratio−1)·2 clamped).
11. **`meanReverting` bool** (E15): b < −1e-4 (continuous: b, or θ / half-life).
12. **`isVolSkewSkewed` bool** (E2): |SVI rho| > 0.5 (continuous: rho).

---

## Data dependencies

Dependency order (build bottom-up):

1. **`normalDist`** — pure math, no inputs. Everything Black-Scholes depends on it (`v11Math` re-exports it; E2 and E3 both resolve to this single Φ).
2. **Candle feed** (OHLCV + optional `relativeVolume`, `isDisplacement`) → E5/E6 (realized vol, cone, VRP), E7 (multi-ticker closes), E8–E13 (displacement suite), E14–E17 (regime suite).
3. **Option chain (`ChainContract[]`, front expiry)** + **spot** + **dteDays** → E3 (risk-neutral density) and E4 (IV surface). Both blend call+put IV per strike; neither uses OI/greeks from the chain.
4. **E2 (synthetic RND)** needs only spot/ticker/dte — no chain, no candles (hard-coded SVI + hist vol tables).
5. **`v11Math.calculateWilderRSI` / `calculateWilderATR`** (candles) → E17 (`volCompression` uses RSI; `volExpansion` uses the v11 ATR, NOT the displacement engine's `wilderATRSeries` — the two ATRs differ in warmup behavior: 0.1 prefill vs running-mean/backfill).
6. **E5 `closeToCloseVol`** → E17 (`volExpansion`, `forwardVolMatrix`). **E5 `intervalMinutes`** should feed E15's `intervalMin` (callers may pass it; default 5).
7. **E6 `computeVRP`** consumes an IV from upstream (e.g. E3's `atmIv` or the chain ATM) + candles.
8. Composite: `computeDisplacementIntelligence` = E9+E10+E11+E12+E13 in one call for the dealer-flow SSE payload.

No engine in this cluster consumes another engine's *output* except as listed (RSI/ATR/c2c-vol helpers); RND (E3) and surface (E4) are independent consumers of the same chain snapshot.

---

## Suspected defects

Flagged, not fixed. As-coded behavior first, suspected correct form second.

- **D1 (E5, high impact): daily-bar annualization is wrong.** `intervalMinutes` accepts a 1440-min median (daily bars pass the `[0.5, 1440]` plausibility band) but `periodsPerYear = 252·390/1440 ≈ 68.3` instead of 252 — daily RV is understated by √(252/68.3) ≈ 1.92×. The 390-min RTH day is also wrong for 24h assets. Correct form: interval-class-aware mapping (daily → 252/yr; intraday RTH → 252·390/m; 24/7 → 365·1440/m).
- **D2 (E3): fat-tail ratio is drift/skew-contaminated.** Tail bands `spot·(1 ± 2·expectedMovePct)` are centered on **spot**, but the RND mean is ≈ the forward (`spot·e^{rT}`); the benchmark 0.0455 is the Gaussian two-tail mass about its **own mean**. With r = 5% and long DTE the up-band picks up drift mass, inflating `fatTailRatio` even for a pure lognormal. Correct: center bands on the RND mean (or forward) and/or benchmark against an actual same-vol lognormal's tail mass over the same bands.
- **D3 (E3): grid truncation + renormalization.** Density lives on `[0.55·S, 1.6·S]`; mass outside (asymmetric −45%/+60%) is silently redistributed by the normalization, biasing percentiles, std, and tail metrics for high-IV/long-DTE inputs. Endpoint densities are also fixed at 0 (no one-sided difference). Correct: widen the grid with IV/T (e.g. ±6·σ√T in log space) or account for truncated mass explicitly.
- **D4 (E2): IV clamps kink the call curve.** `max(1e-4, w/T)` and `clamp(iv, 0.01, 2.5)` create derivative discontinuities in C(K); the BL second difference turns those into spurious density spikes/zeros where a clamp activates. Correct: clamp the SVI *parameters* to a valid (arbitrage-free) region instead of clamping outputs.
- **D5 (E2): variance floors distort moments.** `impliedStdDev = sqrt(max(1e-2, var))` imposes a 0.1-price-unit std floor — irrelevant for SPX, badly wrong for low-priced underlyings. Same for historical. Correct: floor at 0 (or an epsilon scaled to spot²).
- **D6 (E2): the engine is synthetic end-to-end.** SVI parameters and "historical" vols are hard-coded per ticker; nothing is calibrated from live data, despite the "no mock math" comment. The output should not be presented alongside chain-derived numbers as market-implied. (Port decision: keep as demo/fallback or replace with a calibrated SVI fit.)
- **D7 (E16): `volRatio` baseline overlaps the numerator.** Baseline `std(all returns)` includes the most recent 30 returns being compared against it, compressing the ratio toward 1 for short histories (for len=30, ratio ≡ 1). Correct: baseline over returns *excluding* the recent window.
- **D8 (E16): `transitionProb` misnomer.** It is `round(max posterior · 100)` — a state-confidence, not a transition probability. Rename in the rebuild.
- **D9 (E14): Hurst has no small-sample correction.** Plain R/S without Anis–Lloyd/Peters correction biases H upward for short series; the coarse `×1.6` window ladder with rectangle chunking adds noise. Behavior to preserve if bit-compatibility matters; otherwise apply the correction.
- **D10 (E17): EMA-200 on 60 bars.** `volCompression` runs with as few as 60 closes; `ema(px, 200)` is then dominated by its seed (px[0]) — the "pinch" partially measures seed decay, not real convergence. Correct: require len ≳ period (or use SMA fallback / bias-corrected EMA).
- **D11 (percentile conventions are inconsistent).** `percentileRank` (E8) counts strict `<` over the window and divides by `len−1` (self included in window; all-equal → 0); `volCone` (E6) counts `<=` over *valid-only* values and divides by len (all-equal → 100); `volCompression` counts `<=` including self (rank never 0). Same word, three semantics — unify in the rebuild and note flag thresholds (30/70/15/0.2) were tuned against these specific conventions.
- **D12 (E6): `volCone.current` can be a filtered-out zero.** `current = series[last]` is taken from the unfiltered series while the distribution uses `valid` (> 0) only; a zero current yields percentile 0 silently instead of N/A.
- **D13 (E11): sweep dedup only checks the adjacent event.** The dedup filter compares each event to the *immediately preceding* one in a mixed high/low array; identical duplicates separated by an opposite-side event survive. Also the per-candle `[...highs].reverse().find(...)` scan is O(n·p) (performance hazard on long series).
- **D14 (E4): front-row DTE rounding.** `dtes[0] = round(frontDteDays)` while `gFront` is computed with the *unrounded* `frontDteDays`; the front factor is exactly 1, so the displayed front DTE can disagree with the T used (cosmetic, but the axis lies by up to 0.5 day). Also `gFront || v0 || 1` treats a legitimate `gFront = 0` (impossible for positive v0/theta, but the falsy-chain is fragile) via fallback.
- **D15 (E15): θ fallback is discontinuous at b = −1.** For b → −1⁺, θ = −ln(1+b) → +∞; at b ≤ −1, θ jumps to −b (≈1). Half-life collapses from 0 to ln2. Rare (over-shooting AR(1)), but the discontinuity is unphysical; consider capping θ instead.
- **D16 (E5): Yang–Zhang pair skipping straddles gaps.** Invalid bars are skipped but the pair loop always uses `c[i−1]` as prev — if the invalid bar is the previous one and its close happens to be > 0 (e.g., 0-high junk), overnight returns can span or include junk bars. Correct: track last *accepted* bar as prev.
- **D17 (E3/E2): quadrature details.** Rectangle-rule sums weight all `numSteps+1` nodes by `ds` (integrates over range+ds, ~0.8% overweight in E2 — mostly cancelled by normalization); E3's `quantile` is a step function with ~0.48%-of-spot granularity and its CDF counts the node at `x` as below-or-equal. Preserve or document the convention.
- **D18 (E7): time alignment is assumed, not checked.** Cross-asset returns are aligned by *array recency* (`slice(-minLen)`), not timestamps; a stale feed for one ticker misaligns the whole correlation matrix and fabricates residuals. Correct: align on timestamps and require overlap.
- **D19 (E7): PC1 sign and degeneracy.** Power iteration (60 fixed iterations, no convergence check) returns an arbitrary eigenvector sign; z-scores are invariant (beta·f unchanged) but the reported `beta` sign can flip run-to-run for near-degenerate correlation structures. The `fVar` floor (1e-3) bounds but does not remove the instability.
- **D20 (E10): TESTED/HELD churn.** Non-terminal FVG states are overwritten on every subsequent touch (last touch wins) and `heldAtIdx` records only the latest HELD; a gap that tested deep then recovered reads HELD. If the rebuild's continuous fill-fraction (max penetration) is used, compute it as a running max, which the legacy code does *not* do.
- **D21 (E13): `atrSlope` is a 10-bar ratio, not a slope**; `atrPrev = atrs[n−11] || atr` silently substitutes the current ATR when the lookup is falsy (index 0 of a warmup array can be 0 → slope forced to cur/cur = 1 via `|| atr`... actually `atrPrev = 0` → `|| atr` replaces it, then ratio = 1). Naming + warmup artifact.
- **D22 (E12): pivot price collision.** `lastBrokenHigh !== lastHigh.price` uses exact float equality on *price*: two distinct pivots at the identical price can't both break (second break is swallowed); conversely a re-test of the same pivot correctly fires once. Use pivot identity (index), not price.

---

## Discarded

- `Number(x.toFixed(n))` display roundings sprinkled through outputs (E3 forward/density k, E7 z/beta, E9 force/dominance, E13, E15, E17 scores/details) — presentation, not math; rebuild should return full precision and round at the UI.
- `detail` strings in E17 (`"EMA pinch pctile 12 · RSI 48"`) — UI copy.
- `id` string builders (`dz-…`, `fvg-…`, `liq-…`, `st-…`) — replace with structured IDs.
- SSE payload down-sampling target 56 in E3 and the `.slice(-14)` display caps in `computeDisplacementIntelligence` — transport/UI concerns (keep the *math* of `probInRange` but run it on the full grid, not the downsample).
- License headers / long prose comments — documentation only.
- `RndNode.cumulativeImplied/cumulativeHistorical` initial zeros before normalization pass — implementation scaffolding.
- Duplicate BSM call implementations (E2 `calculateBSCallPrice` clamps ≥ 0 + returns on T≤0; E3 `bsCall` returns discounted intrinsic on T≤0/σ≤0) — port ONE canonical `bs_call(S,K,T,σ,r,q)` with the E3 degenerate-input semantics (they matter for the density) and E2's non-negativity only where a price is displayed.
