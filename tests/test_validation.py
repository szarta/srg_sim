"""Engine validation suite (DESIGN.md §11).

The finish/stop *math* parity lives in ``test_finish.py`` / ``test_stops.py``;
this module validates the assembled engine: a gimmick-free duel is ≈50/50, the
seeded turn roll matches its closed-form expectation, and a game is byte-for-byte
deterministic and replayable under its seed.
"""

from __future__ import annotations

import collections

from tests.demo_decks import bull, fae, make_deck
from srg_sim.engine import Engine
from srg_sim.gamelog import matches
from srg_sim.policy import RandomPolicy

GAMES = 400
ROLLS = 1000


def _mirror_win_counts(games: int) -> collections.Counter[str]:
    wins: collections.Counter[str] = collections.Counter()
    for seed in range(games):
        eng = Engine(
            make_deck("A", bull()), make_deck("B", bull()),
            RandomPolicy(), RandomPolicy(), seed=seed, created="x",
        )
        wins[eng.play().winner] += 1
    return wins


def test_mirror_match_is_roughly_fair() -> None:
    """Identical competitors both sides -> neither seat has a real edge (§11)."""
    wins = _mirror_win_counts(GAMES)
    rate_a = wins["A"] / GAMES
    assert 0.40 <= rate_a <= 0.60, wins


def test_no_draws_in_mirror_batch() -> None:
    """Every game resolves (finish or count-out); the turn cap is never hit."""
    wins = _mirror_win_counts(GAMES)
    assert wins["draw"] == 0, wins


def test_turn_roll_is_fair_against_closed_form() -> None:
    """Both competitors' skills are {5..10}, so a single roll-off is 50/50 by
    symmetry; the Monte-Carlo winner rate converges there (§11)."""
    winners: collections.Counter[str] = collections.Counter()
    for seed in range(ROLLS):
        eng = Engine(
            make_deck("A", bull()), make_deck("B", fae()),
            RandomPolicy(), RandomPolicy(), seed=seed, created="x",
        )
        eng.setup()
        eng.state.turn_no = 1
        winners[eng._roll_off()] += 1
    rate_a = winners["A"] / ROLLS
    assert 0.45 <= rate_a <= 0.55, winners


def test_same_seed_is_byte_identical_and_replays() -> None:
    def run() -> Engine:
        eng = Engine(
            make_deck("A", bull()), make_deck("B", fae()),
            RandomPolicy(), RandomPolicy(), seed=2024, created="2026-07-14",
        )
        eng.play()
        return eng

    first, second = run(), run()
    assert first.state.log.to_lines() == second.state.log.to_lines()
    assert matches(first.state.log, second.state.log)
    assert first.result == second.result
