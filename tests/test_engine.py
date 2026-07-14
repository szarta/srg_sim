"""Engine tests: turn loop, stops, finish, executor, determinism (§6)."""

from __future__ import annotations

import collections
import json
from dataclasses import replace

import pytest
from srg_sim import effects as fx
from srg_sim.cards import AtkType, Card, PlayOrder
from srg_sim.engine import Engine, GameResult, beats
from srg_sim.gamelog import GameLog
from srg_sim.policy import HeuristicPolicy, Policy, RandomPolicy

from tests.demo_decks import bull, bull_vs_fae, fae, make_deck, with_effects

VALID_REASONS = {"finish", "count_out", "disqualification", "pinfall", "turn_cap"}


def _play(seed: int, pa: Policy | None = None, pb: Policy | None = None) -> Engine:
    da, db = bull_vs_fae()
    eng = Engine(
        da, db, pa or RandomPolicy(), pb or RandomPolicy(), seed=seed, created="2026-07-14"
    )
    eng.play()
    return eng


# -- RPS + ordering primitives ----------------------------------------------


def test_rps_beats() -> None:
    assert beats(AtkType.GRAPPLE, AtkType.STRIKE)  # Strike-type stops a Grapple attack
    assert beats(AtkType.SUBMISSION, AtkType.GRAPPLE)
    assert beats(AtkType.STRIKE, AtkType.SUBMISSION)
    assert not beats(AtkType.STRIKE, AtkType.GRAPPLE)  # not symmetric
    assert not beats(AtkType.STRIKE, AtkType.NONE)


# -- a full game terminates with a valid, logged, replayable result ----------


@pytest.mark.parametrize("seed", range(8))
def test_game_reaches_valid_result(seed: int) -> None:
    eng = _play(seed)
    assert eng.result is not None
    assert eng.result.reason in VALID_REASONS
    assert eng.result.winner in {"A", "B", "draw"}
    assert eng.result.turns >= 1


@pytest.mark.parametrize("seed", range(8))
def test_determinism_same_seed_same_log(seed: int) -> None:
    assert _play(seed).state.log.to_lines() == _play(seed).state.log.to_lines()


def test_replay_matches_original() -> None:
    original = _play(11).state.log
    replayed = _play(11).state.log
    from srg_sim.gamelog import matches

    assert matches(original, replayed)


def test_log_round_trips_through_jsonl() -> None:
    lines = _play(4).state.log.to_lines()
    assert GameLog.parse(lines).to_lines() == lines


def test_last_event_is_result() -> None:
    lines = _play(7).state.log.to_lines()
    assert json.loads(lines[-1])["type"] == "result"


def test_header_records_policies_and_deck_refs() -> None:
    eng = _play(1, HeuristicPolicy(), RandomPolicy())
    header = eng.state.log.header
    assert header.players["A"].policy == "heuristic"
    assert header.players["B"].policy == "random"
    assert len(header.players["A"].deck) == 30


def test_heuristic_beats_or_ties_pure_random_over_seeds() -> None:
    # Not a strict guarantee, but the aggressive+defensive heuristic should not
    # lose badly to random over a fixed seed batch.
    wins = collections.Counter()
    for seed in range(40):
        da, db = bull_vs_fae()
        eng = Engine(da, db, HeuristicPolicy(), RandomPolicy(), seed=seed, created="x")
        wins[eng.play().winner] += 1
    assert wins["A"] >= wins["B"]


# -- both finish and count-out occur across seeds ----------------------------


def test_finishes_and_count_outs_both_occur() -> None:
    reasons = collections.Counter(_play(s).result.reason for s in range(60))  # type: ignore[union-attr]
    assert reasons["finish"] > 0
    assert reasons["count_out"] > 0


# -- decision logging policy -------------------------------------------------


def test_decision_events_have_multiple_legal_options() -> None:
    # _decide skips logging forced (single-option) choices, so every logged
    # decision reflects a real branch — the imitation-learning signal (§7).
    for line in _play(3).state.log.to_lines():
        ev = json.loads(line)
        if ev.get("type") == "decision":
            assert len(ev["legal"]) > 1


# -- effect executor ---------------------------------------------------------


def test_modify_roll_effect_emits_audit_and_shifts_a_roll() -> None:
    mod = fx.Effect(
        trigger=fx.OnWinTurn(),
        actions=(fx.ModifyRoll(who=fx.Who.SELF, delta=1, when=fx.RollWhen.NEXT),),
        raw_clause="+1 next roll",
        source=fx.EffectSource.GIMMICK,
    )
    eng = Engine(
        make_deck("A", with_effects(bull(), (mod,))),
        make_deck("B", fae()),
        HeuristicPolicy(),
        HeuristicPolicy(),
        seed=5,
        created="x",
    )
    eng.play()
    events = [json.loads(x) for x in eng.state.log.to_lines()[1:]]
    assert any(e["type"] == "effect" and e["action"] == "ModifyRoll" for e in events)
    assert any(e["type"] == "roll" and e["mods"] for e in events)


def test_draw_effect_logs_a_draw_not_an_effect_event() -> None:
    draw = fx.Effect(
        trigger=fx.OnWinTurn(),
        actions=(fx.Draw(n=1),),
        raw_clause="draw on win",
        source=fx.EffectSource.GIMMICK,
    )
    eng = Engine(
        make_deck("A", with_effects(bull(), (draw,))),
        make_deck("B", fae()),
        HeuristicPolicy(),
        HeuristicPolicy(),
        seed=2,
        created="x",
    )
    eng.play()
    types = collections.Counter(json.loads(x)["type"] for x in eng.state.log.to_lines()[1:])
    assert types["draw"] > 0  # Draw is logged as its concrete event, not `effect`


def test_unsupported_action_is_logged_never_dropped() -> None:
    weird = fx.Effect(
        trigger=fx.OnWinTurn(),
        actions=(fx.Unsupported(raw_text="do something odd", reason="no grammar"),),
        raw_clause="odd",
        source=fx.EffectSource.GIMMICK,
    )
    eng = Engine(
        make_deck("A", with_effects(bull(), (weird,))),
        make_deck("B", fae()),
        HeuristicPolicy(),
        HeuristicPolicy(),
        seed=3,
        created="x",
    )
    eng.play()
    types = collections.Counter(json.loads(x)["type"] for x in eng.state.log.to_lines()[1:])
    assert types["unsupported"] > 0


def test_start_of_match_crowd_effect_fires_at_setup() -> None:
    entrance_eff = fx.Effect(
        trigger=fx.StartOfMatch(),
        actions=(fx.CrowdMeter(delta=1),),
        raw_clause="start at CM1",
        source=fx.EffectSource.ENTRANCE,
    )
    da, db = bull_vs_fae()
    da = replace(da, entrance=replace(da.entrance, effects=(entrance_eff,)))
    eng = Engine(da, db, HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    assert eng.state.crowd_meter == 1


# -- LoseBy win conditions ---------------------------------------------------


def test_lose_by_disqualification_when_a_card_is_stopped() -> None:
    dq = fx.Effect(
        trigger=fx.OnStop(dir=fx.Direction.YOURS),
        actions=(fx.LoseBy(kind=fx.LoseKind.DISQUALIFICATION, who=fx.Who.SELF),),
        raw_clause="if stopped, DQ",
        source=fx.EffectSource.CARD,
    )
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    eng.state.turn_no = 1
    a_deck = eng.state.players["A"].deck
    attack = replace(a_deck[0], atk_type=AtkType.STRIKE, play_order=PlayOrder.LEAD, effects=(dq,))
    stopper = replace(eng.state.players["B"].deck[0], atk_type=AtkType.SUBMISSION)
    eng._apply_stop("A", "B", attack, stopper)
    assert eng.result == GameResult("B", "disqualification", 1)


# -- stops -------------------------------------------------------------------


def test_skill_stop_requires_online() -> None:
    # Card 15 (Submission-type skill stop) stops a Strike attack only when online.
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    defender = eng.state.players["B"]
    card15 = next(c for c in defender.deck if c.number == 15)
    defender.hand = [card15]
    strike_attack: Card = replace(defender.deck[0], atk_type=AtkType.STRIKE)
    legal = eng._legal_stops("B", "A", strike_attack)
    # Fae's Technique 7 / Submission 9 vs Bull: whether online is matchup-decided;
    # the point is the gate is *consulted* — a non-skill-stop Submission card is
    # unconditionally legal, card 15 only if evaluate_stop says online.
    from srg_sim.stops import evaluate_stop

    online = evaluate_stop(
        eng.state.effective_stats("B"), "Strike", eng.state.effective_stats("A")
    )["online"]
    assert (card15 in legal) == online


# -- snapshot mid-game -------------------------------------------------------


def test_mid_game_state_snapshot_round_trips() -> None:
    from srg_sim.state import GameState

    eng = _play(9)
    snap = eng.state.to_dict()
    assert GameState.from_dict(snap).to_dict() == snap
