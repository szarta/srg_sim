"""Tests for the seeded RNG wrapper (DESIGN.md §6): determinism + snapshots."""

from __future__ import annotations

import pytest
from srg_sim.cards import Skill
from srg_sim.rng import SKILL_FACES, SeededRNG


def test_skill_faces_are_the_six_skills() -> None:
    assert set(SKILL_FACES) == set(Skill)
    assert len(SKILL_FACES) == 6


def test_same_seed_same_rolls() -> None:
    b = SeededRNG(11)
    c = SeededRNG(11)
    assert [b.roll() for _ in range(20)] == [c.roll() for _ in range(20)]


def test_different_seeds_diverge() -> None:
    # Two long streams from different seeds; near-certain to differ over 30 draws.
    s1 = SeededRNG(1)
    s2 = SeededRNG(2)
    assert [s1.roll() for _ in range(30)] != [s2.roll() for _ in range(30)]


def test_roll_returns_a_skill() -> None:
    assert all(isinstance(SeededRNG(s).roll(), Skill) for s in range(5))


def test_shuffle_is_deterministic_and_in_place() -> None:
    a = list(range(20))
    b = list(range(20))
    SeededRNG(7).shuffle(a)
    SeededRNG(7).shuffle(b)
    assert a == b
    assert a != list(range(20))  # actually permuted
    assert sorted(a) == list(range(20))  # a permutation, nothing lost


def test_reveal_picks_a_member() -> None:
    items = ["x", "y", "z"]
    assert SeededRNG(3).reveal(items) in items


def test_reveal_empty_raises() -> None:
    with pytest.raises(ValueError, match="empty"):
        SeededRNG(0).reveal([])


def test_snapshot_restore_resumes_bit_exact() -> None:
    rng = SeededRNG(99)
    [rng.roll() for _ in range(13)]  # advance the stream
    snap = rng.snapshot()
    restored = SeededRNG.restore(snap)
    assert [rng.roll() for _ in range(25)] == [restored.roll() for _ in range(25)]


def test_snapshot_is_json_friendly() -> None:
    import json

    snap = SeededRNG(5).snapshot()
    assert json.loads(json.dumps(snap)) == snap
    assert snap["seed"] == 5
