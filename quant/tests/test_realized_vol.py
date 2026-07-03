"""Reference-implementation tests for the realized-vol estimators."""

import numpy as np
import pytest

from slayer_quant import realized_vol as rv

ESTIMATORS = [rv.close_to_close, rv.parkinson, rv.garman_klass, rv.rogers_satchell, rv.yang_zhang]


def _gbm_candles(true_vol: float, n_bars: int, seed: int) -> np.ndarray:
    rng = np.random.default_rng(seed)
    intra = 256
    periods = 252.0
    dt = 1.0 / (periods * intra)
    step_vol = true_vol * np.sqrt(dt)
    price = 100.0
    out = []
    for _ in range(n_bars):
        z = rng.standard_normal(intra)
        path = price * np.exp(np.cumsum(step_vol * z - 0.5 * step_vol**2))
        o, c = price, float(path[-1])
        out.append([o, max(o, path.max()), min(o, path.min()), c])
        price = c
    return np.array(out)


def test_all_estimators_recover_gbm_vol():
    candles = _gbm_candles(0.2, 2000, seed=7)
    for est in ESTIMATORS:
        assert est(candles, 252.0) == pytest.approx(0.2, rel=0.08)


def test_flat_series_zero_vol():
    flat = np.full((10, 4), 50.0)
    for est in ESTIMATORS:
        assert est(flat, 252.0) == 0.0


def test_short_series_rejected():
    two = _gbm_candles(0.2, 2, seed=1)
    with pytest.raises(ValueError, match="insufficient"):
        rv.close_to_close(two, 252.0)
    rv.parkinson(two, 252.0)  # range estimators accept short series


def test_malformed_candles_rejected():
    candles = _gbm_candles(0.2, 5, seed=2)
    candles[2, 1] = candles[2, 2] - 1.0  # high < low
    for est in ESTIMATORS:
        with pytest.raises(ValueError, match="domain"):
            est(candles, 252.0)
