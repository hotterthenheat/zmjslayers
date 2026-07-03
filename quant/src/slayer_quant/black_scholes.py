"""Black-Scholes-Merton reference implementation.

This is the *independent oracle* for the Rust kernel: everything routes
through :mod:`scipy.stats.norm` rather than hand-rolled approximations, and
vanna/charm are exposed both analytically and via finite differences so the
golden vectors are not circular.

Units are natural: time in years, rates/vols annualized decimals, theta per
year, vega per unit vol.
"""

from __future__ import annotations

from dataclasses import dataclass
from enum import StrEnum

import numpy as np
from scipy.optimize import brentq
from scipy.stats import norm

#: Lower bound of the implied-vol search bracket (annualized decimal).
IV_LO = 1e-4
#: Upper bound of the implied-vol search bracket: 500% vol is not a market.
IV_HI = 5.0
#: Absolute price tolerance for implied-vol convergence, points.
IV_PRICE_TOL = 1e-10


class Right(StrEnum):
    """Option right."""

    CALL = "CALL"
    PUT = "PUT"


@dataclass(frozen=True)
class BsInputs:
    """BSM pricing inputs. Mirrors ``slayer_quant::black_scholes::BsInputs``."""

    spot: float
    strike: float
    t_years: float
    vol: float
    rate: float
    div_yield: float

    def validate(self) -> None:
        vals = (self.spot, self.strike, self.t_years, self.vol, self.rate, self.div_yield)
        if not all(np.isfinite(vals)):
            raise ValueError("BsInputs out of domain: non-finite input")
        if self.spot <= 0 or self.strike <= 0 or self.t_years < 0 or self.vol < 0:
            raise ValueError("BsInputs out of domain")


def _d1_d2(i: BsInputs) -> tuple[float, float]:
    sqrt_t = np.sqrt(i.t_years)
    denom = i.vol * sqrt_t
    d1 = (np.log(i.spot / i.strike) + (i.rate - i.div_yield + 0.5 * i.vol**2) * i.t_years) / denom
    return d1, d1 - denom


def _deterministic_value(i: BsInputs, right: Right) -> float:
    fwd = i.spot * np.exp((i.rate - i.div_yield) * i.t_years)
    df = np.exp(-i.rate * i.t_years)
    intrinsic = max(fwd - i.strike, 0.0) if right is Right.CALL else max(i.strike - fwd, 0.0)
    return df * intrinsic


def price(i: BsInputs, right: Right) -> float:
    """European BSM price."""
    i.validate()
    if i.t_years == 0.0 or i.vol == 0.0:
        return _deterministic_value(i, right)
    d1, d2 = _d1_d2(i)
    disc_r = np.exp(-i.rate * i.t_years)
    disc_q = np.exp(-i.div_yield * i.t_years)
    if right is Right.CALL:
        return float(i.spot * disc_q * norm.cdf(d1) - i.strike * disc_r * norm.cdf(d2))
    return float(i.strike * disc_r * norm.cdf(-d2) - i.spot * disc_q * norm.cdf(-d1))


@dataclass(frozen=True)
class Greeks:
    """Full greek set, natural units. Mirrors the Rust ``BsGreeks``."""

    delta: float
    gamma: float
    theta: float
    vega: float
    vanna: float
    charm: float


def greeks(i: BsInputs, right: Right) -> Greeks:
    """Analytic BSM greeks."""
    i.validate()
    if i.t_years == 0.0 or i.vol == 0.0:
        fwd = i.spot * np.exp((i.rate - i.div_yield) * i.t_years)
        disc_q = np.exp(-i.div_yield * i.t_years)
        itm = fwd > i.strike if right is Right.CALL else fwd < i.strike
        sign = 1.0 if right is Right.CALL else -1.0
        return Greeks(sign * disc_q if itm else 0.0, 0.0, 0.0, 0.0, 0.0, 0.0)

    d1, d2 = _d1_d2(i)
    sqrt_t = np.sqrt(i.t_years)
    disc_r = np.exp(-i.rate * i.t_years)
    disc_q = np.exp(-i.div_yield * i.t_years)
    pdf_d1 = norm.pdf(d1)

    if right is Right.CALL:
        delta = disc_q * norm.cdf(d1)
        theta = (
            -i.spot * disc_q * pdf_d1 * i.vol / (2 * sqrt_t)
            - i.rate * i.strike * disc_r * norm.cdf(d2)
            + i.div_yield * i.spot * disc_q * norm.cdf(d1)
        )
    else:
        delta = -disc_q * norm.cdf(-d1)
        theta = (
            -i.spot * disc_q * pdf_d1 * i.vol / (2 * sqrt_t)
            + i.rate * i.strike * disc_r * norm.cdf(-d2)
            - i.div_yield * i.spot * disc_q * norm.cdf(-d1)
        )

    gamma = disc_q * pdf_d1 / (i.spot * i.vol * sqrt_t)
    vega = i.spot * disc_q * pdf_d1 * sqrt_t
    vanna = -disc_q * pdf_d1 * d2 / i.vol
    drift_term = (
        pdf_d1
        * (2 * (i.rate - i.div_yield) * i.t_years - d2 * i.vol * sqrt_t)
        / (2 * i.t_years * i.vol * sqrt_t)
    )
    if right is Right.CALL:
        charm = disc_q * (i.div_yield * norm.cdf(d1) - drift_term)
    else:
        charm = disc_q * (-i.div_yield * norm.cdf(-d1) - drift_term)

    return Greeks(
        float(delta), float(gamma), float(theta), float(vega), float(vanna), float(charm)
    )


def implied_vol(market_price: float, i: BsInputs, right: Right) -> float:
    """Implied volatility via Brent on the monotone price-in-vol map."""
    i.validate()
    if i.t_years <= 0.0:
        raise ValueError("implied vol undefined at expiry")
    if not np.isfinite(market_price) or market_price < 0.0:
        raise ValueError("market price must be finite and non-negative")

    def objective(vol: float) -> float:
        return price(BsInputs(i.spot, i.strike, i.t_years, vol, i.rate, i.div_yield), right) - (
            market_price
        )

    lo, hi = objective(IV_LO), objective(IV_HI)
    if lo > IV_PRICE_TOL or hi < -IV_PRICE_TOL:
        raise ValueError("price outside no-arbitrage vol bracket")
    if abs(lo) <= IV_PRICE_TOL:
        return IV_LO
    if abs(hi) <= IV_PRICE_TOL:
        return IV_HI
    return float(brentq(objective, IV_LO, IV_HI, xtol=1e-12))
