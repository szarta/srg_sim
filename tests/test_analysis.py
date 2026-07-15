"""Tests for the batch match runner (analysis.py) — DESIGN.md §10 (M2, todo #14).

The runner's contract: every game is a pure function of its seed, so a batch is
reproducible and order-independent, and logs are retained only on request.
"""

from __future__ import annotations

import json

import pytest
from srg_sim.analysis import (
    GameOutcome,
    Matchup,
    MatchupReport,
    run_batch,
    run_game,
    seed_range,
    wilson_interval,
)
from srg_sim.engine import GameResult
from srg_sim.policy import HeuristicPolicy, RandomPolicy

from tests.demo_decks import bull_vs_fae

VALID_REASONS = {"finish", "count_out", "disqualification", "pinfall", "turn_cap"}
VALID_ATK = {"Strike", "Grapple", "Submission", "None"}


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


# --- MatchupReport ----------------------------------------------------------


def _report(n: int = 24) -> MatchupReport:
    outcomes = run_batch(_matchup(), seed_range(n), keep_logs=True)
    return MatchupReport.from_outcomes(outcomes)


def test_report_needs_logs() -> None:
    outcomes = run_batch(_matchup(), seed_range(3))  # keep_logs defaults off
    with pytest.raises(ValueError, match="keep_logs=True"):
        MatchupReport.from_outcomes(outcomes)


def test_report_win_and_reason_counts_partition_the_batch() -> None:
    rep = _report()
    assert rep.games == 24
    assert rep.wins["A"] + rep.wins["B"] + rep.wins["draw"] == 24  # every game counted once
    assert sum(rep.reasons.values()) == 24
    assert all(reason in VALID_REASONS for reason in rep.reasons)


def test_report_win_rate_and_wilson_ci_bracket_the_point_estimate() -> None:
    rep = _report()
    for side in ("A", "B"):
        rate = rep.win_rate[side]
        assert rate == pytest.approx(rep.wins[side] / rep.games)
        lo, hi = rep.win_ci[side]
        assert 0.0 <= lo <= rate <= hi <= 1.0


def test_report_finish_types_account_for_every_finish_win() -> None:
    rep = _report()
    # Exactly one finishing card (hence one atk_type) per finish-reason game.
    assert sum(rep.finish_types.values()) == rep.reasons.get("finish", 0)
    assert all(atk in VALID_ATK for atk in rep.finish_types)


def test_report_length_stats_are_ordered() -> None:
    rep = _report()
    lo, mean, hi, med = (rep.length[k] for k in ("min", "mean", "max", "median"))
    assert lo <= mean <= hi
    assert lo <= med <= hi


def test_report_stop_rates_are_non_negative_means() -> None:
    rep = _report()
    assert set(rep.stops) == {"A", "B"}
    assert all(rate >= 0.0 for rate in rep.stops.values())


def test_report_crowd_meter_curve_spans_the_longest_game() -> None:
    rep = _report()
    assert len(rep.crowd_meter_curve) == int(rep.length["max"])
    assert all(value >= 0.0 for value in rep.crowd_meter_curve)


def test_report_to_dict_is_json_serializable_with_list_intervals() -> None:
    rep = _report(8)
    blob = rep.to_dict()
    restored = json.loads(json.dumps(blob))  # round-trips through JSON
    assert restored["games"] == 8
    assert isinstance(restored["win_ci"]["A"], list) and len(restored["win_ci"]["A"]) == 2


def test_report_is_deterministic_for_the_same_batch() -> None:
    assert _report(6).to_dict() == _report(6).to_dict()


# --- wilson_interval --------------------------------------------------------


def test_wilson_interval_empty_sample_is_zero() -> None:
    assert wilson_interval(0, 0) == (0.0, 0.0)


def test_wilson_interval_brackets_the_proportion_and_stays_in_unit_range() -> None:
    lo, hi = wilson_interval(5, 10)
    assert lo < 0.5 < hi
    assert lo >= 0.0 and hi <= 1.0


def test_wilson_interval_clamps_at_the_extremes() -> None:
    assert wilson_interval(0, 10)[0] == 0.0  # no successes -> lower bound clamps to 0
    assert wilson_interval(10, 10)[1] == 1.0  # all successes -> upper bound clamps to 1
    assert wilson_interval(10, 10)[0] < 1.0  # but the interval still has width
