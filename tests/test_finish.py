"""Parity tests for the ported finish/breakout math (DESIGN.md §6, §11).

Golden ``Fraction`` values were computed from the authoritative
``fae_comp/supershow.py`` and are baked in so the port is validated even where
that source is unavailable (e.g. CI). A second, opt-in test re-derives parity
directly against the source when it is importable.
"""

from __future__ import annotations

import importlib.util
import sys
from fractions import Fraction
from pathlib import Path

import pytest
from srg_sim.finish import finish_odds, is_auto_success, stat_breaks_out

BULL = {"Power": 10, "Technique": 6, "Agility": 5, "Strike": 7, "Submission": 8, "Grapple": 9}
FAE = {"Power": 10, "Technique": 7, "Agility": 6, "Strike": 8, "Submission": 9, "Grapple": 5}

# name -> (finisher, defender, kwargs) for finish_odds
CASES: dict[str, tuple[dict[str, int], dict[str, int], dict[str, object]]] = {
    "default": (BULL, FAE, {}),
    "cm1": (BULL, FAE, {"crowd_meter": 1}),
    "cm5": (BULL, FAE, {"crowd_meter": 5}),
    "attempts1": (BULL, FAE, {"breakout_attempts": 1}),
    "penalties_011": (BULL, FAE, {"breakout_penalties": [0, 1, 1]}),
    "reroll": (BULL, FAE, {"allow_reroll": True}),
    "bonus_strike5": (BULL, FAE, {"finish_bonus": {"Strike": 5}}),
    "bonus_all4_cm1": (BULL, FAE, {"finish_bonus": dict.fromkeys(BULL, 4), "crowd_meter": 1}),
    "oppmod_neg1": (BULL, FAE, {"opponent_modifiers": dict.fromkeys(BULL, -1)}),
    "fae_vs_bull_default": (FAE, BULL, {}),
}

# Golden results from fae_comp/supershow.finish_odds.
GOLDEN: dict[str, Fraction] = {
    "default": Fraction(25, 144),
    "cm1": Fraction(49, 144),
    "cm5": Fraction(1205, 1296),
    "attempts1": Fraction(5, 12),
    "penalties_011": Fraction(295, 1296),
    "reroll": Fraction(2183, 7776),
    "bonus_strike5": Fraction(19, 72),
    "bonus_all4_cm1": Fraction(1205, 1296),
    "oppmod_neg1": Fraction(49, 144),
    "fae_vs_bull_default": Fraction(25, 144),
}


@pytest.mark.parametrize("name", list(CASES))
def test_finish_odds_matches_golden(name: str) -> None:
    finisher, defender, kwargs = CASES[name]
    assert finish_odds(finisher, defender, **kwargs) == GOLDEN[name]  # type: ignore[arg-type]


def test_is_auto_success() -> None:
    assert is_auto_success(11, 1)  # >=11 at CM>0
    assert is_auto_success(12, 3)
    assert not is_auto_success(11, 0)  # CM0 has no auto-success
    assert not is_auto_success(10, 5)


def test_stat_breaks_out_cm0_ten_always() -> None:
    # At CM0 a raw 10 breaks out even a finish it "shouldn't", ignoring penalty.
    assert stat_breaks_out(10, 12, 5, 0)
    # ...but a 9 at CM0 follows the normal rule.
    assert not stat_breaks_out(9, 12, 0, 0)


def test_stat_breaks_out_normal_rule() -> None:
    assert stat_breaks_out(9, 9, 0, 1)  # 9 - 0 >= 9
    assert not stat_breaks_out(9, 10, 1, 1)  # 9 - 1 = 8, not >= 10
    assert stat_breaks_out(8, 7, 0, 3)  # 8 >= 7


def test_finish_probability_bounds() -> None:
    # A crushing finish (all-skill +4 at CM1) should be near-certain success.
    p = finish_odds(BULL, FAE, finish_bonus=dict.fromkeys(BULL, 4), crowd_meter=1)
    assert Fraction(9, 10) < p <= 1


# --- opt-in parity directly against fae_comp/supershow.py -------------------

_REFERENCE = Path("/home/brandon/fae_comp/supershow.py")


def _load_reference():  # type: ignore[no-untyped-def]
    if not _REFERENCE.exists():
        pytest.skip(f"reference not available: {_REFERENCE}")
    spec = importlib.util.spec_from_file_location("_ref_supershow", _REFERENCE)
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    sys.modules["_ref_supershow"] = module
    spec.loader.exec_module(module)
    return module


@pytest.mark.parametrize("name", list(CASES))
def test_parity_with_reference(name: str) -> None:
    ref = _load_reference()
    finisher, defender, kwargs = CASES[name]
    ref_result = ref.finish_odds(
        ref.Competitor("F", finisher), ref.Competitor("D", defender), **kwargs
    )
    assert finish_odds(finisher, defender, **kwargs) == ref_result  # type: ignore[arg-type]
