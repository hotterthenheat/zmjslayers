"""Golden-vector generator.

Emits the cross-language test fixtures in ``schema/golden/``. The Rust kernel
and this Python package both assert against these files; regenerating them is
an explicit, reviewed act:

    python -m slayer_quant.golden <repo-root>/schema/golden

Determinism: the candle fixture uses a fixed PCG64 seed; nothing here reads a
clock.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

import numpy as np
from scipy.stats import norm

from . import black_scholes as bs
from . import realized_vol as rv

#: Seed for the GBM candle fixture. Never change casually: goldens are
#: committed artifacts and churn must be reviewed.
CANDLE_SEED = 20260703
#: Bars in the candle fixture.
CANDLE_BARS = 500
#: Intra-bar monitoring steps for high/low construction.
CANDLE_INTRA_STEPS = 64
#: Annualization used by the realized-vol fixture (daily bars).
CANDLE_PERIODS_PER_YEAR = 252.0
#: True annualized vol of the simulated GBM.
CANDLE_TRUE_VOL = 0.2


def norm_dist_cases() -> dict:
    """Standard normal CDF/PPF pairs from scipy."""
    xs = [-8.0, -3.0, -1.0, -0.35, 0.0, 0.15, 0.35, 1.0, 1.96, 2.5758, 6.0]
    ps = [1e-10, 1e-4, 0.025, 0.1, 0.25, 0.5, 0.75, 0.9, 0.975, 0.999, 1 - 1e-10]
    return {
        "cdf": [{"x": x, "value": float(norm.cdf(x))} for x in xs],
        "ppf": [{"p": p, "value": float(norm.ppf(p))} for p in ps],
    }


def black_scholes_cases() -> list[dict]:
    """BSM price + greeks over a strike/vol/tenor grid, both rights."""
    out: list[dict] = []
    for strike in (60.0, 85.0, 100.0, 120.0, 160.0):
        for vol in (0.08, 0.2, 0.55):
            for t in (0.02, 0.25, 1.0):
                for right in (bs.Right.CALL, bs.Right.PUT):
                    i = bs.BsInputs(
                        spot=100.0,
                        strike=strike,
                        t_years=t,
                        vol=vol,
                        rate=0.045,
                        div_yield=0.013,
                    )
                    g = bs.greeks(i, right)
                    out.append(
                        {
                            "spot": i.spot,
                            "strike": i.strike,
                            "t_years": i.t_years,
                            "vol": i.vol,
                            "rate": i.rate,
                            "div_yield": i.div_yield,
                            "right": right.value,
                            "price": bs.price(i, right),
                            "delta": g.delta,
                            "gamma": g.gamma,
                            "theta": g.theta,
                            "vega": g.vega,
                            "vanna": g.vanna,
                            "charm": g.charm,
                        }
                    )
    return out


def candle_fixture() -> dict:
    """Deterministic GBM candles and the estimator outputs over them."""
    rng = np.random.default_rng(CANDLE_SEED)
    dt = 1.0 / (CANDLE_PERIODS_PER_YEAR * CANDLE_INTRA_STEPS)
    step_vol = CANDLE_TRUE_VOL * np.sqrt(dt)
    price = 100.0
    candles = []
    for _ in range(CANDLE_BARS):
        z = rng.standard_normal(CANDLE_INTRA_STEPS)
        path = price * np.exp(np.cumsum(step_vol * z - 0.5 * step_vol**2))
        o, c = price, float(path[-1])
        h = float(max(o, path.max()))
        low = float(min(o, path.min()))
        candles.append([o, h, low, c])
        price = c
    ohlc = np.array(candles)
    p = CANDLE_PERIODS_PER_YEAR
    return {
        "seed": CANDLE_SEED,
        "true_vol": CANDLE_TRUE_VOL,
        "periods_per_year": p,
        "candles": candles,
        "estimates": {
            "close_to_close": rv.close_to_close(ohlc, p),
            "parkinson": rv.parkinson(ohlc, p),
            "garman_klass": rv.garman_klass(ohlc, p),
            "rogers_satchell": rv.rogers_satchell(ohlc, p),
            "yang_zhang": rv.yang_zhang(ohlc, p),
        },
    }


def generate() -> dict[str, object]:
    """All golden payloads, keyed by output filename."""
    return {
        "norm_dist.json": norm_dist_cases(),
        "black_scholes.json": black_scholes_cases(),
        "realized_vol.json": candle_fixture(),
    }


def main(out_dir: str) -> None:
    """Write all golden files to ``out_dir``."""
    target = Path(out_dir)
    target.mkdir(parents=True, exist_ok=True)
    for name, payload in generate().items():
        path = target / name
        path.write_text(json.dumps(payload, indent=1) + "\n")
        print(f"wrote {path}")


if __name__ == "__main__":
    main(sys.argv[1])
