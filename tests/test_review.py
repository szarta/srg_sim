"""Tests for the post-game review substrate (DESIGN.md §7/§8, todo #42).

Exercises :class:`~srg_sim.policy.ReplayPolicy` and
:func:`~srg_sim.review.reconstruct_with_decks` on synthetic decks (no card DB):
a recorded match replays byte-identically, and every decision reconstructs both
the redacted player-view and the full oracle truth.
"""

from __future__ import annotations

import json

import pytest
from srg_sim.engine import Engine
from srg_sim.gamelog import Decision, GameLog
from srg_sim.policy import HeuristicPolicy, Policy, ReplayExhausted, ReplayPolicy, SmartPasser
from srg_sim.review import reconstruct_with_decks, records_to_ndjson

from tests.demo_decks import bull_vs_fae


def _record(seed: int, pa: Policy | None = None, pb: Policy | None = None) -> GameLog:
    da, db = bull_vs_fae()
    eng = Engine(
        da, db, pa or HeuristicPolicy(), pb or SmartPasser(), seed=seed, created="2026-07-15"
    )
    eng.play()
    assert eng.state.log is not None
    return eng.state.log


def _decisions_by_player(log: GameLog) -> dict[str, list]:
    by: dict[str, list] = {"A": [], "B": []}
    for event in log.events:
        if isinstance(event, Decision):
            by[event.player].append(event.chosen)
    return by


# -- ReplayPolicy: recorded decisions reproduce the match byte-for-byte -------


@pytest.mark.parametrize("seed", range(6))
def test_replay_policy_reproduces_log_byte_identically(seed: int) -> None:
    log = _record(seed)
    by = _decisions_by_player(log)
    names = {k: log.header.players[k].policy for k in ("A", "B")}
    da, db = bull_vs_fae()
    eng = Engine(
        da,
        db,
        ReplayPolicy(by["A"], name=names["A"]),
        ReplayPolicy(by["B"], name=names["B"]),
        seed=log.header.seed,
        created=log.header.created,
        kind=log.header.kind,
    )
    eng.play()
    assert eng.state.log is not None
    assert eng.state.log.to_lines() == log.to_lines()


def test_replay_policy_raises_when_exhausted() -> None:
    policy = ReplayPolicy([{"kind": "pass"}])
    engine = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=0)
    legal = [{"kind": "pass"}, {"kind": "x"}]
    assert policy.choose("turn_action", legal, engine.state, "A") == {"kind": "pass"}
    with pytest.raises(ReplayExhausted):
        policy.choose("turn_action", legal, engine.state, "A")


# -- reconstruct: both views recovered at every decision ---------------------


@pytest.mark.parametrize("seed", range(6))
def test_reconstruct_captures_one_record_per_decision(seed: int) -> None:
    log = _record(seed)
    recon = reconstruct_with_decks(log, dict(zip(("A", "B"), bull_vs_fae(), strict=True)))
    n_decisions = sum(1 for e in log.events if isinstance(e, Decision))
    assert len(recon.records) == n_decisions
    assert recon.result.winner == next(
        e.winner for e in reversed(log.events) if getattr(e, "TYPE", "") == "result"
    )


def test_reconstruct_player_view_hides_opponent_hand_and_deck_order() -> None:
    log = _record(1)
    recon = reconstruct_with_decks(log, dict(zip(("A", "B"), bull_vs_fae(), strict=True)))
    assert recon.records, "expected at least one decision"
    for rec in recon.records:
        opp = "B" if rec.player == "A" else "A"
        opp_view = rec.player_view["players"][opp]
        # Opponent hand is a count, never card identities; no deck order anywhere.
        assert "hand" not in opp_view and "hand_size" in opp_view
        assert "deck" not in opp_view and "deck_size" in opp_view
        assert "deck" not in rec.player_view["players"][rec.player]  # own deck order hidden too


def test_reconstruct_oracle_has_full_hidden_state() -> None:
    log = _record(1)
    recon = reconstruct_with_decks(log, dict(zip(("A", "B"), bull_vs_fae(), strict=True)))
    rec = recon.records[0]
    for key in ("A", "B"):
        oracle_player = rec.oracle["players"][key]
        assert "hand" in oracle_player and "deck" in oracle_player  # full zones
    assert "rng" in rec.oracle  # oracle snapshot is resumable


def test_for_player_filters_to_one_side() -> None:
    log = _record(2)
    recon = reconstruct_with_decks(log, dict(zip(("A", "B"), bull_vs_fae(), strict=True)))
    just_a = recon.for_player("A")
    assert just_a == [r for r in recon.records if r.player == "A"]
    assert all(r.player == "A" for r in just_a)


def test_records_to_ndjson_is_one_json_object_per_line() -> None:
    log = _record(3)
    recon = reconstruct_with_decks(log, dict(zip(("A", "B"), bull_vs_fae(), strict=True)))
    text = records_to_ndjson(recon.records)
    lines = text.splitlines()
    assert len(lines) == len(recon.records)
    for line, rec in zip(lines, recon.records, strict=True):
        obj = json.loads(line)
        assert obj["turn"] == rec.turn and obj["point"] == rec.point
        assert obj["player"] == rec.player
        assert "player_view" in obj and "oracle" in obj
