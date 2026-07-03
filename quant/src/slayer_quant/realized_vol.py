"""Realized volatility estimator family — reference implementations.

Semantics mirror ``slayer-quant/src/realized_vol.rs`` exactly: annualized
volatility (decimal) from an OHLC candle array and a periods-per-year factor
supplied by the caller.

References: Parkinson (1980), Garman-Klass (1980), Rogers-Satchell (1991),
Yang-Zhang (2000).
"""

from __future__ import annotations

import numpy as np

#: Parkinson variance divisor: 4 ln 2.
PARKINSON_DIVISOR = 4.0 * np.log(2.0)
#: Garman-Klass close-to-open coefficient: 2 ln 2 - 1.
GK_CO_COEFF = 2.0 * np.log(2.0) - 1.0
#: Yang-Zhang k-weight numerator alpha.
YZ_ALPHA = 0.34
#: Yang-Zhang k-weight denominator constant.
YZ_BETA = 1.34
#: Minimum candles for return-based estimators (one return needs two bars,
#: a sample variance needs two returns).
MIN_BARS_RETURNS = 3
#: Expected array rank of an OHLC matrix.
_OHLC_NDIM = 2
#: Columns of an OHLC matrix: open, high, low, close.
_OHLC_COLS = 4


def _validate(ohlc: np.ndarray, min_bars: int) -> np.ndarray:
    ohlc = np.asarray(ohlc, dtype=np.float64)
    if ohlc.ndim != _OHLC_NDIM or ohlc.shape[1] != _OHLC_COLS:
        raise ValueError("expected (n, 4) array of open, high, low, close")
    if len(ohlc) < min_bars:
        raise ValueError(f"insufficient data: needed {min_bars}, got {len(ohlc)}")
    o, h, low, c = ohlc.T
    sane = (
        np.all(np.isfinite(ohlc))
        and np.all(o > 0)
        and np.all(low > 0)
        and np.all(c > 0)
        and np.all(h >= low)
        and np.all(h >= np.maximum(o, c))
        and np.all(low <= np.minimum(o, c))
    )
    if not sane:
        raise ValueError("candle OHLC out of domain")
    return ohlc


def close_to_close(ohlc: np.ndarray, periods_per_year: float) -> float:
    """Annualized sample stdev of close-to-close log returns."""
    ohlc = _validate(ohlc, MIN_BARS_RETURNS)
    returns = np.diff(np.log(ohlc[:, 3]))
    return float(np.sqrt(np.var(returns, ddof=1) * periods_per_year))


def parkinson(ohlc: np.ndarray, periods_per_year: float) -> float:
    """Parkinson (1980) high-low range estimator."""
    ohlc = _validate(ohlc, 1)
    hl = np.log(ohlc[:, 1] / ohlc[:, 2])
    return float(np.sqrt(periods_per_year * np.mean(hl**2) / PARKINSON_DIVISOR))


def garman_klass(ohlc: np.ndarray, periods_per_year: float) -> float:
    """Garman-Klass (1980) OHLC estimator, clamped at zero variance."""
    ohlc = _validate(ohlc, 1)
    hl = np.log(ohlc[:, 1] / ohlc[:, 2])
    co = np.log(ohlc[:, 3] / ohlc[:, 0])
    var = periods_per_year * np.mean(0.5 * hl**2 - GK_CO_COEFF * co**2)
    return float(np.sqrt(max(var, 0.0)))


def _rs_terms(ohlc: np.ndarray) -> np.ndarray:
    o, h, low, c = ohlc.T
    return np.log(h / c) * np.log(h / o) + np.log(low / c) * np.log(low / o)


def rogers_satchell(ohlc: np.ndarray, periods_per_year: float) -> float:
    """Rogers-Satchell (1991) drift-independent estimator."""
    ohlc = _validate(ohlc, 1)
    var = periods_per_year * np.mean(_rs_terms(ohlc))
    return float(np.sqrt(max(var, 0.0)))


def yang_zhang(ohlc: np.ndarray, periods_per_year: float) -> float:
    """Yang-Zhang (2000) estimator with overnight-gap handling."""
    ohlc = _validate(ohlc, MIN_BARS_RETURNS)
    n = len(ohlc) - 1
    overnight = np.log(ohlc[1:, 0] / ohlc[:-1, 3])
    open_close = np.log(ohlc[1:, 3] / ohlc[1:, 0])
    rs_mean = float(np.mean(_rs_terms(ohlc[1:])))
    k = YZ_ALPHA / (YZ_BETA + (n + 1) / (n - 1))
    var = np.var(overnight, ddof=1) + k * np.var(open_close, ddof=1) + (1 - k) * rs_mean
    return float(np.sqrt(max(var * periods_per_year, 0.0)))
