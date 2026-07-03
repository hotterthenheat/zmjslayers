//! Technical-indicator kernel over a candle series.
//!
//! Reimplements the legacy SkyVision V11 indicator primitives from
//! `docs/spec/01-skyvision-v11.md`:
//!
//! - **E4** — Wilder RSI and ATR ([`wilder_rsi`], [`wilder_atr`]).
//! - **E5** — fractal pivots and Structure01 ([`fractal_pivots`],
//!   [`structure01`]).
//! - **E6 (support)** — session VWAP and relative volume ([`session_vwap`],
//!   [`rvol`]) consumed by the [`crate::thesis`] composite.
//! - plus a standard EMA ([`ema_series`], [`ema_last`]).
//!
//! Every series estimator makes its warmup region explicit: elements are
//! `Option<f64>` and are `None` until the estimator is seeded, so a caller can
//! never mistake a fill value for a real reading. Where the E7 composite needs
//! the legacy neutral fills ([`RSI_NEUTRAL`], [`ATR_FALLBACK`]) it substitutes
//! them itself, from the named constants below.
//!
//! Kernel purity: no I/O, no clocks; time is only ever the bar timestamps
//! carried in the input candles.
//!
//! ## Deviations from legacy
//! - **D14** — [`rvol`] returns the neutral [`RVOL_NEUTRAL`] (`1.0`) when the
//!   baseline window is empty or has zero total volume, instead of the legacy
//!   `volume / 1 = raw volume`, which saturated `volume01` on tiny histories.
//!   The two legacy RVOL variants (the inline E7 one and `calculateRVOL`)
//!   collapse into this single correct function.
//! - **D16** — [`fractal_pivots`] uses a plateau-tolerant detection rule (see
//!   the function docs): an exact double/triple top or bottom now yields a
//!   pivot (the leftmost bar of the run) instead of the legacy strict
//!   comparison, which suppressed pivots at exactly the most decision-relevant
//!   patterns. Strict single peaks are detected identically to legacy.
//! - **D18** — [`wilder_rsi`] and [`wilder_atr`] both seed after exactly
//!   `period` observations and expose their first reading at index `period`,
//!   removing the legacy one-bar asymmetry (RSI at bar 14, ATR at bar 13). The
//!   degenerate `TR_0 = high_0 − low_0` (a range with no gap component) is
//!   dropped; ATR seeds on the mean of `TR_1..=TR_period`.

use serde::{Deserialize, Serialize};
use slayer_core::Candle;

/// Legacy default Wilder smoothing period (bars), for RSI and ATR.
pub const WILDER_PERIOD_DEFAULT: usize = 14;

/// RSI output scale: RSI ranges over `0..=RSI_SCALE`.
pub const RSI_SCALE: f64 = 100.0;

/// Neutral RSI reading the E7 composite substitutes where the RSI series is
/// still in warmup (legacy fill value).
pub const RSI_NEUTRAL: f64 = 50.0;

/// Fallback ATR the E7 composite substitutes where the ATR series is still in
/// warmup (legacy fill value), in price units.
pub const ATR_FALLBACK: f64 = 0.1;

/// Numerator of the standard EMA smoothing factor `k = EMA_SMOOTHING/(n+1)`.
pub const EMA_SMOOTHING: f64 = 2.0;

/// Divisor of the typical price `(high + low + close)/TYPICAL_PRICE_DIVISOR`
/// used by session VWAP.
pub const TYPICAL_PRICE_DIVISOR: f64 = 3.0;

/// Default number of bars immediately preceding a bar that form its relative
/// volume baseline (E7 constant `RVOL_BASELINE_BARS`).
pub const RVOL_BASELINE_BARS: usize = 20;

/// Neutral relative volume returned when a bar has no usable volume baseline
/// (the D14 fix — never raw volume).
pub const RVOL_NEUTRAL: f64 = 1.0;

/// Default fractal pivot half-window `L`; a pivot needs `2·L+1` bars (E5).
pub const FRACTAL_HALF_WINDOW: usize = 2;

/// ATR fraction within which two pivots count as "equal" for Structure01
/// range detection (`eps = PIVOT_EQ_ATR_FRAC · atr`).
pub const PIVOT_EQ_ATR_FRAC: f64 = 0.1;

/// Structure01 rung: fewer than two highs or two lows (insufficient structure).
pub const STRUCT_NEUTRAL: f64 = 0.5;

/// Structure01 rung: range-bound / no clean trend.
pub const STRUCT_RANGE: f64 = 0.33;

/// Structure01 rung: partial trend agreement (one of HH/HL, or LH/LL).
pub const STRUCT_PARTIAL: f64 = 0.66;

/// Structure01 rung: full trend with a shrinking pullback in the trade
/// direction.
pub const STRUCT_FULL: f64 = 1.0;

/// Structure01 rung: counter-trend structure against the trade direction.
pub const STRUCT_COUNTER: f64 = 0.0;

/// Which extreme a fractal pivot marks. Descriptive data (not a state): a
/// pivot is a swing high or a swing low.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PivotKind {
    /// A swing high (local resistance).
    High,
    /// A swing low (local support).
    Low,
}

/// A detected fractal pivot: the bar it sits on and the extreme price.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Pivot {
    /// Index of the pivot bar within the candle series.
    pub index: usize,
    /// The pivot price (bar high for a [`PivotKind::High`], low otherwise).
    pub price: f64,
    /// Whether the pivot is a swing high or low.
    pub kind: PivotKind,
}

/// Wilder-smoothed RSI over the candle closes.
///
/// Returns one element per candle. Elements are `None` for the warmup region
/// (indices `< period`) and `Some(rsi)` from index `period` onward, where the
/// seed at index `period` is the classic average of the first `period` gains
/// and losses. Requires `period + 1` candles to produce any reading; a shorter
/// series (or `period == 0`) yields all `None`.
///
/// See module docs (D18) for the seeding-consistency deviation.
#[must_use]
pub fn wilder_rsi(candles: &[Candle], period: usize) -> Vec<Option<f64>> {
    let n = candles.len();
    let mut out = vec![None; n];
    if period == 0 || n <= period {
        return out;
    }
    let period_f = period as f64;

    let (mut gain_sum, mut loss_sum) = (0.0, 0.0);
    for w in candles.windows(2).take(period) {
        let d = w[1].close - w[0].close;
        if d > 0.0 {
            gain_sum += d;
        } else {
            loss_sum += -d;
        }
    }
    let mut avg_gain = gain_sum / period_f;
    let mut avg_loss = loss_sum / period_f;
    if let Some(slot) = out.get_mut(period) {
        *slot = Some(rsi_from(avg_gain, avg_loss));
    }

    for (j, w) in candles.windows(2).enumerate().skip(period) {
        let d = w[1].close - w[0].close;
        avg_gain = (avg_gain * (period_f - 1.0) + d.max(0.0)) / period_f;
        avg_loss = (avg_loss * (period_f - 1.0) + (-d).max(0.0)) / period_f;
        if let Some(slot) = out.get_mut(j + 1) {
            *slot = Some(rsi_from(avg_gain, avg_loss));
        }
    }
    out
}

/// RSI from smoothed average gain/loss. A zero average loss maps to a maximal
/// RSI (`RSI_SCALE`), matching the Wilder definition.
fn rsi_from(avg_gain: f64, avg_loss: f64) -> f64 {
    if avg_loss == 0.0 {
        RSI_SCALE
    } else {
        RSI_SCALE - RSI_SCALE / (1.0 + avg_gain / avg_loss)
    }
}

/// Wilder-smoothed Average True Range over the candles, in price units.
///
/// Returns one element per candle: `None` for the warmup region (indices
/// `< period`) and `Some(atr)` from index `period` onward. The true range is
/// `TR_i = max(high_i − low_i, |high_i − close_{i−1}|, |low_i − close_{i−1}|)`
/// for `i ≥ 1`; the seed at index `period` is the mean of `TR_1..=TR_period`
/// and thereafter `atr_i = (atr_{i−1}·(period−1) + TR_i)/period`. Requires
/// `period + 1` candles; a shorter series (or `period == 0`) yields all `None`.
///
/// See module docs (D18): this seeds one bar later than the legacy ATR so it
/// aligns with [`wilder_rsi`].
#[must_use]
pub fn wilder_atr(candles: &[Candle], period: usize) -> Vec<Option<f64>> {
    let n = candles.len();
    let mut out = vec![None; n];
    if period == 0 || n <= period {
        return out;
    }
    let period_f = period as f64;

    let tr = |i: usize| -> f64 {
        let h = candles[i].high;
        let l = candles[i].low;
        let pc = candles[i - 1].close;
        (h - l).max((h - pc).abs()).max((l - pc).abs())
    };

    let mut seed_sum = 0.0;
    for i in 1..=period {
        seed_sum += tr(i);
    }
    let seed = seed_sum / period_f;
    if let Some(slot) = out.get_mut(period) {
        *slot = Some(seed);
    }

    let mut prev = seed;
    for (i, slot) in out.iter_mut().enumerate().skip(period + 1) {
        let atr = (prev * (period_f - 1.0) + tr(i)) / period_f;
        *slot = Some(atr);
        prev = atr;
    }
    out
}

/// Exponential moving average of `values` with standard `2/(period+1)`
/// smoothing.
///
/// Warmup is explicit: elements are `None` before index `period − 1`. The seed
/// at index `period − 1` is the simple mean of the first `period` samples;
/// thereafter `ema_i = k·x_i + (1−k)·ema_{i−1}` with `k = EMA_SMOOTHING/(period+1)`.
/// A series shorter than `period` (or `period == 0`) yields all `None`.
#[must_use]
pub fn ema_series(values: &[f64], period: usize) -> Vec<Option<f64>> {
    let n = values.len();
    let mut out = vec![None; n];
    if period == 0 || n < period {
        return out;
    }
    let period_f = period as f64;
    let k = EMA_SMOOTHING / (period_f + 1.0);
    let seed = values[..period].iter().sum::<f64>() / period_f;
    if let Some(slot) = out.get_mut(period - 1) {
        *slot = Some(seed);
    }
    let mut prev = seed;
    for (i, slot) in out.iter_mut().enumerate().skip(period) {
        let e = k * values[i] + (1.0 - k) * prev;
        *slot = Some(e);
        prev = e;
    }
    out
}

/// Final defined EMA value of `values`, or `None` if the series is too short
/// to seed. Convenience over [`ema_series`].
#[must_use]
pub fn ema_last(values: &[f64], period: usize) -> Option<f64> {
    ema_series(values, period).into_iter().flatten().last()
}

/// Session VWAP: the running volume-weighted average of the typical price
/// `(high + low + close)/3` over the supplied candles, treated as a single
/// session.
///
/// Element `i` is the VWAP through bar `i`. It is `None` while the cumulative
/// volume is still zero — the average is genuinely undefined there and is
/// never faked. Because [`Candle`] carries no provider VWAP field, this is the
/// honest reconstruction the E7 composite consumes; supply real per-bar volume
/// (do not pass a synthetic unit series).
#[must_use]
pub fn session_vwap(candles: &[Candle]) -> Vec<Option<f64>> {
    let mut out = Vec::with_capacity(candles.len());
    let mut cum_pv = 0.0;
    let mut cum_v = 0.0;
    for c in candles {
        let typical = (c.high + c.low + c.close) / TYPICAL_PRICE_DIVISOR;
        cum_pv += typical * c.volume;
        cum_v += c.volume;
        out.push(if cum_v > 0.0 { Some(cum_pv / cum_v) } else { None });
    }
    out
}

/// Relative volume of bar `idx`: its volume over the mean volume of up to
/// `baseline_bars` bars immediately preceding it (`candles[idx-baseline_bars .. idx]`).
///
/// D14 fix: when that window is empty (e.g. `idx == 0`) or its total volume is
/// zero, the baseline is unusable and this returns the neutral [`RVOL_NEUTRAL`]
/// (`1.0`) rather than the bar's raw volume. Returns [`RVOL_NEUTRAL`] for an
/// out-of-range `idx`.
#[must_use]
pub fn rvol(candles: &[Candle], idx: usize, baseline_bars: usize) -> f64 {
    if idx == 0 || idx >= candles.len() {
        return RVOL_NEUTRAL;
    }
    let start = idx.saturating_sub(baseline_bars);
    let window = &candles[start..idx];
    if window.is_empty() {
        return RVOL_NEUTRAL;
    }
    let sum: f64 = window.iter().map(|c| c.volume).sum();
    if sum <= 0.0 {
        return RVOL_NEUTRAL;
    }
    let mean = sum / window.len() as f64;
    candles[idx].volume / mean
}

/// Detect fractal swing pivots with a half-window of `half_window` bars.
///
/// Pivots are returned in ascending bar order. A candle series shorter than
/// `2·half_window + 1` (or `half_window == 0`) yields no pivots.
///
/// **Plateau-tolerant rule (D16 fix).** Bar `i ∈ [L, n−L)` (with `L =
/// half_window`) is a **pivot high** iff
/// 1. `high[i] ≥ high[k]` for every `k ∈ [i−L, i+L]` (weak dominance — an equal
///    neighbour no longer disqualifies the pivot, which is the defect fix), and
/// 2. `high[i] > high[k]` for every `k ∈ [i−L, i)` (strictly above every bar to
///    its left inside the window).
///
/// Condition (2) breaks plateau ties deterministically: on a run of equal highs
/// only the leftmost bar satisfies it, so a double/triple top yields exactly one
/// pivot high instead of the legacy zero. A strict single peak (all neighbours
/// strictly lower) satisfies both conditions, so non-tie behaviour is identical
/// to the legacy strict comparison. Pivot lows are the mirror image (weak `≤`
/// across the window, strict `<` to the left).
#[must_use]
pub fn fractal_pivots(candles: &[Candle], half_window: usize) -> Vec<Pivot> {
    let n = candles.len();
    let mut out = Vec::new();
    if half_window == 0 || n <= 2 * half_window {
        return out;
    }
    let l = half_window;
    for i in l..(n - l) {
        let hi = candles[i].high;
        let weak_high = (i - l..=i + l).all(|k| candles[k].high <= hi);
        let strict_left_high = (i - l..i).all(|k| candles[k].high < hi);
        if weak_high && strict_left_high {
            out.push(Pivot { index: i, price: hi, kind: PivotKind::High });
        }

        let lo = candles[i].low;
        let weak_low = (i - l..=i + l).all(|k| candles[k].low >= lo);
        let strict_left_low = (i - l..i).all(|k| candles[k].low > lo);
        if weak_low && strict_left_low {
            out.push(Pivot { index: i, price: lo, kind: PivotKind::Low });
        }
    }
    out
}

/// Collapse a bar-ordered pivot list into an alternating high/low sequence,
/// keeping the more extreme of any run of same-kind pivots (higher high /
/// lower low).
fn collapse_alternating(pivots: &[Pivot]) -> Vec<Pivot> {
    let mut out: Vec<Pivot> = Vec::new();
    for &p in pivots {
        if let Some(top) = out.last_mut()
            && top.kind == p.kind
        {
            let more_extreme = match p.kind {
                PivotKind::High => p.price > top.price,
                PivotKind::Low => p.price < top.price,
            };
            if more_extreme {
                *top = p;
            }
            continue;
        }
        out.push(p);
    }
    out
}

/// Market-structure quality in `{0.0, 0.33, 0.5, 0.66, 1.0}` from swing pivots
/// (E5 `computeStructure01`).
///
/// `atr` is the current ATR (sets the "equal pivots" tolerance) and `dir` is
/// the trade direction (`> 0` long, `< 0` short; only its sign matters). The
/// pivot slice need not be pre-sorted. Returns [`STRUCT_NEUTRAL`] when there
/// are fewer than two highs or two lows.
#[must_use]
pub fn structure01(pivots: &[Pivot], atr: f64, dir: f64) -> f64 {
    let mut sorted = pivots.to_vec();
    sorted.sort_by_key(|p| p.index);

    let highs: Vec<f64> =
        sorted.iter().filter(|p| p.kind == PivotKind::High).map(|p| p.price).collect();
    let lows: Vec<f64> =
        sorted.iter().filter(|p| p.kind == PivotKind::Low).map(|p| p.price).collect();
    if highs.len() < 2 || lows.len() < 2 {
        return STRUCT_NEUTRAL;
    }
    let last_h = highs[highs.len() - 1];
    let prev_h = highs[highs.len() - 2];
    let last_l = lows[lows.len() - 1];
    let prev_l = lows[lows.len() - 2];

    let eps = PIVOT_EQ_ATR_FRAC * atr;
    if (last_h - prev_h).abs() < eps && (last_l - prev_l).abs() < eps {
        return STRUCT_RANGE;
    }

    let alt = collapse_alternating(&sorted);
    let (mut last_leg, mut prior_leg) = (last_h - last_l, prev_h - prev_l);
    if alt.len() >= 3 {
        let m = alt.len();
        last_leg = alt[m - 1].price - alt[m - 2].price;
        prior_leg = alt[m - 2].price - alt[m - 3].price;
    }
    let shrinking = last_leg.abs() < prior_leg.abs();
    let last_kind = alt.last().map(|p| p.kind);

    if dir > 0.0 {
        let hh = last_h > prev_h;
        let hl = last_l > prev_l;
        let has_shrinking_pullback = last_kind == Some(PivotKind::Low) && shrinking;
        if hh && hl && has_shrinking_pullback {
            STRUCT_FULL
        } else if hh || hl {
            STRUCT_PARTIAL
        } else if last_l < prev_l && last_h < prev_h {
            STRUCT_COUNTER
        } else {
            STRUCT_RANGE
        }
    } else {
        let lh = last_h < prev_h;
        let ll = last_l < prev_l;
        let has_shrinking_rally = last_kind == Some(PivotKind::High) && shrinking;
        if lh && ll && has_shrinking_rally {
            STRUCT_FULL
        } else if lh || ll {
            STRUCT_PARTIAL
        } else if last_l > prev_l && last_h > prev_h {
            STRUCT_COUNTER
        } else {
            STRUCT_RANGE
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use slayer_core::TsMillis;

    const EPS: f64 = 1e-9;

    fn candle(o: f64, h: f64, l: f64, c: f64, v: f64) -> Candle {
        Candle { ts: TsMillis(0), open: o, high: h, low: l, close: c, volume: v }
    }

    /// A flat OHLC bar at price `p` with the given range half-width and volume.
    fn bar(p: f64, half: f64, v: f64) -> Candle {
        candle(p, p + half, p - half, p, v)
    }

    fn rising(n: usize) -> Vec<Candle> {
        (0..n).map(|i| bar(100.0 + i as f64, 0.5, 1000.0 + i as f64)).collect()
    }

    #[test]
    fn rsi_warmup_is_none_then_seeds_at_period() {
        let c = rising(20);
        let rsi = wilder_rsi(&c, WILDER_PERIOD_DEFAULT);
        assert_eq!(rsi.len(), 20);
        for r in rsi.iter().take(WILDER_PERIOD_DEFAULT) {
            assert!(r.is_none());
        }
        assert!(rsi[WILDER_PERIOD_DEFAULT].is_some());
    }

    #[test]
    fn rsi_saturates_on_monotonic_series() {
        let up = rising(20);
        let rsi_up = wilder_rsi(&up, WILDER_PERIOD_DEFAULT);
        assert!((rsi_up[19].unwrap() - 100.0).abs() < EPS);

        let down: Vec<Candle> =
            (0..20).map(|i| bar(200.0 - i as f64, 0.5, 1000.0)).collect();
        let rsi_dn = wilder_rsi(&down, WILDER_PERIOD_DEFAULT);
        assert!(rsi_dn[19].unwrap().abs() < EPS);
    }

    #[test]
    fn rsi_short_series_all_none() {
        let c = rising(WILDER_PERIOD_DEFAULT); // exactly period, one short of the seed
        assert!(wilder_rsi(&c, WILDER_PERIOD_DEFAULT).iter().all(Option::is_none));
        assert!(wilder_rsi(&rising(5), 0).iter().all(Option::is_none));
    }

    #[test]
    fn atr_constant_range_converges_to_range() {
        // Every bar high-low = 2, closes equal => TR = 2 everywhere.
        let c: Vec<Candle> = (0..20).map(|_| bar(100.0, 1.0, 1000.0)).collect();
        let atr = wilder_atr(&c, WILDER_PERIOD_DEFAULT);
        assert!((atr[19].unwrap() - 2.0).abs() < EPS);
    }

    #[test]
    fn rsi_and_atr_seed_at_the_same_index_d18() {
        // The whole point of the D18 fix: first defined reading is co-located.
        let c = rising(30);
        let rsi = wilder_rsi(&c, WILDER_PERIOD_DEFAULT);
        let atr = wilder_atr(&c, WILDER_PERIOD_DEFAULT);
        let first_rsi = rsi.iter().position(Option::is_some).unwrap();
        let first_atr = atr.iter().position(Option::is_some).unwrap();
        assert_eq!(first_rsi, WILDER_PERIOD_DEFAULT);
        assert_eq!(first_atr, WILDER_PERIOD_DEFAULT);
    }

    #[test]
    fn ema_constant_series_is_constant() {
        let v = vec![7.0; 10];
        let e = ema_series(&v, 4);
        assert!(e[..3].iter().all(Option::is_none));
        assert!((e[3].unwrap() - 7.0).abs() < EPS);
        assert!((ema_last(&v, 4).unwrap() - 7.0).abs() < EPS);
        assert!(ema_last(&[1.0, 2.0], 4).is_none());
    }

    #[test]
    fn session_vwap_matches_typical_and_abstains_without_volume() {
        // Constant typical price with volume => VWAP == typical price.
        let c: Vec<Candle> = (0..5).map(|_| bar(50.0, 1.0, 100.0)).collect();
        let v = session_vwap(&c);
        assert!((v[4].unwrap() - 50.0).abs() < EPS);

        let zero: Vec<Candle> = (0..3).map(|_| bar(50.0, 1.0, 0.0)).collect();
        assert!(session_vwap(&zero).iter().all(Option::is_none));
    }

    #[test]
    fn rvol_neutral_when_baseline_unusable_d14() {
        let c: Vec<Candle> = (0..4).map(|i| bar(100.0, 0.5, [10.0, 10.0, 10.0, 30.0][i])).collect();
        // idx 0 has no baseline => neutral, not raw volume.
        assert!((rvol(&c, 0, RVOL_BASELINE_BARS) - RVOL_NEUTRAL).abs() < EPS);
        // idx 3 baseline mean = 10 => rvol = 3.
        assert!((rvol(&c, 3, RVOL_BASELINE_BARS) - 3.0).abs() < EPS);
        // Zero-volume baseline => neutral, not division blow-up.
        let z: Vec<Candle> = (0..4).map(|_| bar(100.0, 0.5, 0.0)).collect();
        assert!((rvol(&z, 3, RVOL_BASELINE_BARS) - RVOL_NEUTRAL).abs() < EPS);
    }

    #[test]
    fn fractal_double_top_yields_one_pivot_d16() {
        // Highs: low ... TOP TOP ... low, with an exact double top.
        let highs = [1.0, 2.0, 5.0, 5.0, 2.0, 1.0, 0.5];
        let c: Vec<Candle> =
            highs.iter().map(|&h| candle(h, h, h - 3.0, h - 1.0, 100.0)).collect();
        let pivots = fractal_pivots(&c, FRACTAL_HALF_WINDOW);
        let tops: Vec<&Pivot> =
            pivots.iter().filter(|p| p.kind == PivotKind::High).collect();
        assert_eq!(tops.len(), 1, "double top must produce exactly one pivot");
        assert_eq!(tops[0].index, 2, "leftmost bar of the plateau");
        assert!((tops[0].price - 5.0).abs() < EPS);
    }

    #[test]
    fn fractal_strict_peak_detected_like_legacy() {
        let highs = [1.0, 2.0, 3.0, 9.0, 3.0, 2.0, 1.0];
        let c: Vec<Candle> =
            highs.iter().map(|&h| candle(h, h, h - 3.0, h - 1.0, 100.0)).collect();
        let pivots = fractal_pivots(&c, FRACTAL_HALF_WINDOW);
        let tops: Vec<&Pivot> =
            pivots.iter().filter(|p| p.kind == PivotKind::High).collect();
        assert_eq!(tops.len(), 1);
        assert_eq!(tops[0].index, 3);
    }

    #[test]
    fn structure01_neutral_without_enough_pivots() {
        let p =
            [Pivot { index: 0, price: 10.0, kind: PivotKind::High }];
        assert!((structure01(&p, 1.0, 1.0) - STRUCT_NEUTRAL).abs() < EPS);
    }

    #[test]
    fn structure01_bullish_higher_highs_and_lows() {
        // HH and HL, last pivot a low (pullback), shrinking last leg.
        let p = vec![
            Pivot { index: 0, price: 10.0, kind: PivotKind::Low },
            Pivot { index: 1, price: 20.0, kind: PivotKind::High },
            Pivot { index: 2, price: 14.0, kind: PivotKind::Low },
            Pivot { index: 3, price: 25.0, kind: PivotKind::High },
            Pivot { index: 4, price: 22.0, kind: PivotKind::Low },
        ];
        // last_h 25 > prev_h 20 (HH), last_l 22 > prev_l 14 (HL),
        // last alt pivot is a Low, last leg |22-25|=3 < prior |25-14|=11.
        assert!((structure01(&p, 1.0, 1.0) - STRUCT_FULL).abs() < EPS);
    }

    #[test]
    fn structure01_counter_trend_is_zero() {
        // Bullish dir but lower highs and lower lows => counter.
        let p = vec![
            Pivot { index: 0, price: 30.0, kind: PivotKind::High },
            Pivot { index: 1, price: 20.0, kind: PivotKind::Low },
            Pivot { index: 2, price: 25.0, kind: PivotKind::High },
            Pivot { index: 3, price: 15.0, kind: PivotKind::Low },
        ];
        // last_h 25 < prev_h 30, last_l 15 < prev_l 20 => COUNTER for dir>0.
        assert!((structure01(&p, 1.0, 1.0) - STRUCT_COUNTER).abs() < EPS);
    }

    #[test]
    fn structure01_range_bound_when_pivots_equal() {
        let p = vec![
            Pivot { index: 0, price: 20.0, kind: PivotKind::High },
            Pivot { index: 1, price: 10.0, kind: PivotKind::Low },
            Pivot { index: 2, price: 20.02, kind: PivotKind::High },
            Pivot { index: 3, price: 10.02, kind: PivotKind::Low },
        ];
        // Both last/prev pairs within 0.1*atr (atr=1 => eps=0.1).
        assert!((structure01(&p, 1.0, 1.0) - STRUCT_RANGE).abs() < EPS);
    }

    #[test]
    fn pivot_kind_wire_format_is_screaming_snake() {
        assert_eq!(serde_json::to_string(&PivotKind::High).unwrap(), "\"HIGH\"");
        assert_eq!(serde_json::to_string(&PivotKind::Low).unwrap(), "\"LOW\"");
    }
}
