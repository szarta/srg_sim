"""Tests for the batch match runner (analysis.py) — DESIGN.md §10 (M2, todo #14).

The runner's contract: every game is a pure function of its seed, so a batch is
reproducible and order-independent, and logs are retained only on request.
"""

from __future__ import annotations

import pytest
from srg_sim.analysis import GameOutcome, Matchup, run_batch, run_game, seed_range
from srg_sim.engine import GameResult
from srg_sim.policy import HeuristicPolicy, RandomPolicy

from tests.demo_decks import bull_vs_fae

VALID_REASONS = {"finish", "count_out", "disqualification", "pinfall", "turn_cap"}


def _matchup(**kw: object) -> Matchup:
    da, db = bull_vs_fae()
    return Matchup(deck_a=da, deck_b=db, **kw)  # type: ignore[arg-type]


# --- run_game ---------------------------------------------------------------


def test_run_game_returns_a_valid_tagged_outcome() -> None:
    outcome = run_game(_matchup(), seed=7)
    assert isinstance(outcome, GameOutcome)
    assert outcome.seed == 7
    assert isinstance(outcome.result, GameResult)
    assert outcome.result.reason in VALID_REASONS
    assert outcome.result.winner in {"A", "B", "draw"}
    assert outcome.log is None  # not kept by default


def test_run_game_is_pure_in_the_seed() -> None:
    first = run_game(_matchup(), seed=42).result
    second = run_game(_matchup(), seed=42).result
    assert first == second  # GameResult is a frozen dataclass -> value equality


def test_run_game_keeps_a_replayable_log_when_asked() -> None:
    outcome = run_game(_matchup(created="x"), seed=9, keep_log=True)
    log = outcome.log
    assert log is not None
    assert log.header.seed == 9
    assert log.header.kind == "sim"
    assert log.events[-1].TYPE == "result"


def test_kept_log_matches_a_fresh_replay_of_the_same_seed() -> None:
    from srg_sim.gamelog import matches

    a = run_game(_matchup(), seed=3, keep_log=True).log
    b = run_game(_matchup(), seed=3, keep_log=True).log
    assert a is not None and b is not None
    assert matches(a, b)  # byte-identical stream from the same seed


# --- run_batch --------------------------------------------------------------


def test_run_batch_produces_one_outcome_per_seed_in_order() -> None:
    seeds = [5, 6, 7, 8]
    outcomes = run_batch(_matchup(), seeds)
    assert [o.seed for o in outcomes] == seeds
    assert all(o.result.reason in VALID_REASONS for o in outcomes)


def test_run_batch_is_order_independent() -> None:
    # Each seed's outcome is the same whether run alone or inside any batch order.
    matchup = _matchup()
    alone = {s: run_game(matchup, s).result for s in (1, 2, 3)}
    for order in ([3, 1, 2], [2, 3, 1]):
        for outcome in run_batch(matchup, order):
            assert outcome.result == alone[outcome.seed]


def test_run_batch_keep_logs_toggles_every_log() -> None:
    without = run_batch(_matchup(), seed_range(3))
    assert all(o.log is None for o in without)
    with_logs = run_batch(_matchup(), seed_range(3), keep_logs=True)
    assert all(o.log is not None for o in with_logs)


def test_run_batch_over_seed_range_covers_the_whole_range() -> None:
    outcomes = run_batch(_matchup(), seed_range(10, start=100))
    assert [o.seed for o in outcomes] == list(range(100, 110))


# --- policies + matchup defaults --------------------------------------------


def test_matchup_defaults_to_heuristic_policies() -> None:
    matchup = _matchup()
    assert matchup.policy_a is HeuristicPolicy and matchup.policy_b is HeuristicPolicy


def test_distinct_policy_factories_each_play_a_full_game() -> None:
    # A fresh policy pair per game; a mixed matchup still runs to a valid result.
    matchup = _matchup(policy_a=RandomPolicy, policy_b=HeuristicPolicy)
    outcome = run_game(matchup, seed=4)
    assert outcome.result.reason in VALID_REASONS


# --- seed_range -------------------------------------------------------------


def test_seed_range_is_contiguous_from_start() -> None:
    assert list(seed_range(4)) == [0, 1, 2, 3]
    assert list(seed_range(3, start=50)) == [50, 51, 52]


def test_seed_range_zero_is_empty() -> None:
    assert list(seed_range(0)) == []


def test_seed_range_rejects_negative_count() -> None:
    with pytest.raises(ValueError, match="non-negative"):
        seed_range(-1)
