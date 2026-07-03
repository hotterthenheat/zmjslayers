"""The committed golden vectors must match what this package generates.

If this test fails, either the reference implementation changed (regenerate
and review the golden diff) or the goldens were edited by hand (revert).
"""

import json
import math
from pathlib import Path

from slayer_quant import golden

GOLDEN_DIR = Path(__file__).resolve().parents[2] / "schema" / "golden"


def _assert_deep_close(a, b, path=""):
    if isinstance(a, dict):
        assert isinstance(b, dict) and a.keys() == b.keys(), path
        for k in a:
            _assert_deep_close(a[k], b[k], f"{path}.{k}")
    elif isinstance(a, list):
        assert isinstance(b, list) and len(a) == len(b), path
        for idx, (x, y) in enumerate(zip(a, b, strict=True)):
            _assert_deep_close(x, y, f"{path}[{idx}]")
    elif isinstance(a, float):
        assert math.isclose(a, float(b), rel_tol=1e-12, abs_tol=1e-12), f"{path}: {a} != {b}"
    else:
        assert a == b, f"{path}: {a} != {b}"


def test_goldens_match_generator():
    generated = golden.generate()
    for name, payload in generated.items():
        committed = json.loads((GOLDEN_DIR / name).read_text())
        _assert_deep_close(payload, committed, name)
