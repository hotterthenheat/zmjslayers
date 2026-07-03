"""Reference-implementation tests for the BSM module."""

import numpy as np
import pytest
from scipy.stats import norm

from slayer_quant import black_scholes as bs

ATM = bs.BsInputs(spot=100.0, strike=100.0, t_years=1.0, vol=0.2, rate=0.05, div_yield=0.0)


def test_atm_call_put_known_values():
    assert bs.price(ATM, bs.Right.CALL) == pytest.approx(10.450583572185565, abs=1e-12)
    assert bs.price(ATM, bs.Right.PUT) == pytest.approx(5.573526022256971, abs=1e-12)


def test_put_call_parity_off_atm():
    i = bs.BsInputs(spot=100.0, strike=87.5, t_years=1.0, vol=0.34, rate=0.05, div_yield=0.012)
    c = bs.price(i, bs.Right.CALL)
    p = bs.price(i, bs.Right.PUT)
    parity = i.spot * np.exp(-i.div_yield * i.t_years) - i.strike * np.exp(-i.rate * i.t_years)
    assert c - p == pytest.approx(parity, abs=1e-12)


def test_greeks_against_finite_differences():
    ds, dv, dt = 1e-4, 1e-6, 1e-7
    for right in (bs.Right.CALL, bs.Right.PUT):
        g = bs.greeks(ATM, right)
        up = bs.price(bs.BsInputs(ATM.spot + ds, 100.0, 1.0, 0.2, 0.05, 0.0), right)
        dn = bs.price(bs.BsInputs(ATM.spot - ds, 100.0, 1.0, 0.2, 0.05, 0.0), right)
        mid = bs.price(ATM, right)
        assert g.delta == pytest.approx((up - dn) / (2 * ds), abs=1e-6)
        assert g.gamma == pytest.approx((up - 2 * mid + dn) / ds**2, abs=1e-5)

        vu = bs.price(bs.BsInputs(100.0, 100.0, 1.0, 0.2 + dv, 0.05, 0.0), right)
        vd = bs.price(bs.BsInputs(100.0, 100.0, 1.0, 0.2 - dv, 0.05, 0.0), right)
        assert g.vega == pytest.approx((vu - vd) / (2 * dv), abs=1e-5)

        tu = bs.price(bs.BsInputs(100.0, 100.0, 1.0 - dt, 0.2, 0.05, 0.0), right)
        assert g.theta == pytest.approx((tu - mid) / dt, abs=1e-4)

        gu = bs.greeks(bs.BsInputs(100.0, 100.0, 1.0, 0.2 + dv, 0.05, 0.0), right)
        gd = bs.greeks(bs.BsInputs(100.0, 100.0, 1.0, 0.2 - dv, 0.05, 0.0), right)
        assert g.vanna == pytest.approx((gu.delta - gd.delta) / (2 * dv), abs=1e-5)

        gt = bs.greeks(bs.BsInputs(100.0, 100.0, 1.0 - dt, 0.2, 0.05, 0.0), right)
        assert g.charm == pytest.approx((gt.delta - g.delta) / dt, abs=1e-4)


def test_delta_matches_closed_form():
    d1 = (np.log(1.0) + (0.05 + 0.02) * 1.0) / 0.2
    assert bs.greeks(ATM, bs.Right.CALL).delta == pytest.approx(norm.cdf(d1), abs=1e-12)


def test_implied_vol_roundtrip():
    for vol in (0.08, 0.2, 0.55, 1.4):
        for t in (0.02, 0.25, 1.0):
            i = bs.BsInputs(100.0, 110.0, t, vol, 0.045, 0.0)
            p = bs.price(i, bs.Right.CALL)
            intrinsic = bs.price(bs.BsInputs(100.0, 110.0, t, 0.0, 0.045, 0.0), bs.Right.CALL)
            if p - intrinsic < 1e-6:
                continue  # vega ~ 0: IV unidentifiable
            zero_vol = bs.BsInputs(100.0, 110.0, t, 0.0, 0.045, 0.0)
            assert bs.implied_vol(p, zero_vol, bs.Right.CALL) == pytest.approx(vol, abs=1e-7)


def test_implied_vol_rejects_arbitrage():
    zero_vol = bs.BsInputs(100.0, 100.0, 1.0, 0.0, 0.05, 0.0)
    with pytest.raises(ValueError):
        bs.implied_vol(1000.0, zero_vol, bs.Right.CALL)
    with pytest.raises(ValueError):
        bs.implied_vol(-1.0, zero_vol, bs.Right.CALL)


def test_expired_prices_at_discounted_intrinsic():
    expired = bs.BsInputs(100.0, 90.0, 0.0, 0.2, 0.05, 0.0)
    assert bs.price(expired, bs.Right.CALL) == pytest.approx(10.0, abs=1e-12)
    assert bs.price(expired, bs.Right.PUT) == 0.0
