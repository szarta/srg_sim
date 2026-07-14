"""Coverage/parity tests for the ported skill-stop logic (DESIGN.md §6, §11).

Golden facts come from ``fae_comp/skill_stops.py``; an opt-in test re-derives
parity directly against that source when importable.
"""

from __future__ import annotations

import importlib.util
import sys
from fractions import Fraction
from pathlib import Path

import pytest
from srg_sim.stops import STOP_CARDS, evaluate_stop

BULL = {"Power": 10, "Technique": 6, "Agility": 5, "Strike": 7, "Submission": 8, "Grapple": 9}
FAE = {"Power": 10, "Agility": 6, "Strike": 8, "Submission": 9, "Grapple": 5, "Technique": 7}


def test_stop_cards_partition_the_six_skills() -> None:
    paired = [skill for _, pair in STOP_CARDS.values() for skill in pair]
    assert sorted(paired) == ["Agility", "Grapple", "Power", "Strike", "Submission", "Technique"]


def test_bull_vs_fae_only_submission_online() -> None:
    assert not evaluate_stop(BULL, "Strike", FAE)["online"]
    assert not evaluate_stop(BULL, "Grapple", FAE)["online"]
    sub = evaluate_stop(BULL, "Submission", FAE)
    assert sub["online"]
    assert sub["card"] == 14
    assert sub["pair"] == ("Power", "Grapple")
    assert len(sub["reasons"]) == 3  # beat-opp Grapple + equal-8 + Colossal


def test_colossal_smash_always_on_without_opponent() -> None:
    # Power 10 & Grapple 9 -> stop-Submission is guaranteed, matchup-proof.
    result = evaluate_stop(BULL, "Submission", opponent=None)
    assert result["online"]
    assert any("Colossal Smash" in r for r in result["reasons"])


def test_colossal_smash_offline_when_stats_wrong() -> None:
    weak = {**BULL, "Grapple": 8}  # Grapple 8, not 9 -> no Colossal
    result = evaluate_stop(weak, "Submission", opponent=None)
    assert any("Colossal Smash: needs Power 10 & Grapple 9" in n for n in result["offline_notes"])


def test_fae_vs_bull_coverage() -> None:
    assert evaluate_stop(FAE, "Strike", BULL)["online"]
    assert evaluate_stop(FAE, "Grapple", BULL)["online"]
    assert not evaluate_stop(FAE, "Submission", BULL)["online"]


def test_beat_opponent_is_strict() -> None:
    # Equal keyed skills do NOT bring a beat-opp stop online (strict >).
    even = {"Power": 10, "Technique": 8, "Agility": 7, "Strike": 6, "Submission": 5, "Grapple": 9}
    # card 13 (Grapple finish) keys on Strike/Agility; make them tie the opponent.
    opp = {**even}
    result = evaluate_stop(even, "Grapple", opp)
    assert not any("beat-opp" in r for r in result["reasons"])


def test_random_online_prob() -> None:
    # best beat key for Submission stop is Power 10 -> (10-5)/6.
    result = evaluate_stop(BULL, "Submission", opponent=None)
    assert result["best_beat_key"] == ("Power", 10)
    assert result["random_online_prob"] == Fraction(5, 6)


# --- opt-in parity directly against fae_comp/skill_stops.py -----------------

_REFERENCE = Path("/home/brandon/fae_comp/skill_stops.py")


def _load_reference():  # type: ignore[no-untyped-def]
    if not _REFERENCE.exists():
        pytest.skip(f"reference not available: {_REFERENCE}")
    spec = importlib.util.spec_from_file_location("_ref_skill_stops", _REFERENCE)
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    sys.modules["_ref_skill_stops"] = module
    spec.loader.exec_module(module)
    return module


@pytest.mark.parametrize("finish_type", ["Strike", "Grapple", "Submission"])
@pytest.mark.parametrize(("defender", "opponent"), [(BULL, FAE), (FAE, BULL), (BULL, None)])
def test_parity_with_reference(
    finish_type: str, defender: dict[str, int], opponent: dict[str, int] | None
) -> None:
    ref = _load_reference()
    ours = evaluate_stop(defender, finish_type, opponent)
    theirs = ref.evaluate_stop(defender, finish_type, opponent)
    assert ours["online"] == theirs["online"]
    assert ours["reasons"] == theirs["reasons"]
    assert ours["offline_notes"] == theirs["offline_notes"]
    assert ours["random_online_prob"] == theirs["random_online_prob"]
