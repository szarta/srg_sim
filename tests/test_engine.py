"""Engine tests: turn loop, stops, finish, executor, determinism (§6)."""

from __future__ import annotations

import collections
import json
from dataclasses import replace

import pytest
from srg_sim import effects as fx
from srg_sim.cards import AtkType, Card, PlayOrder, Skill
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


def test_finishes_occur_across_seeds() -> None:
    reasons = collections.Counter(_play(s).result.reason for s in range(30))  # type: ignore[union-attr]
    assert reasons["finish"] > 0


def test_count_out_win_on_empty_deck_and_hand() -> None:
    # A player who must draw on a won turn with both deck and hand empty WINS.
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    eng.state.players["A"].deck.clear()
    eng.state.players["A"].hand.clear()
    assert eng._draw_for_turn("A") is False
    assert eng.result == GameResult("A", "count_out", eng.state.turn_no)


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


# -- stops (text-driven: a card stops only via its parsed Stop effects) -------


def _fresh() -> Engine:
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.state.turn_no = 1  # decks are full (no setup), cards found by number in deck order
    return eng


def _attack(atk: AtkType, order: PlayOrder) -> Card:
    return Card(db_uuid="atk", name="Atk", number=2, atk_type=atk, play_order=order)


def _hand_card(eng: Engine, key: str, number: int) -> Card:
    card = next(c for c in eng.state.players[key].deck if c.number == number)
    eng.state.players[key].hand = [card]
    return card


def test_stop_matches_order_and_type() -> None:
    # Demo card 1 (Strike) stops Grapple *Leads* only.
    eng = _fresh()
    card1 = _hand_card(eng, "B", 1)
    assert card1 in eng._legal_stops("B", "A", _attack(AtkType.GRAPPLE, PlayOrder.LEAD))
    assert card1 not in eng._legal_stops("B", "A", _attack(AtkType.GRAPPLE, PlayOrder.FINISH))
    assert card1 not in eng._legal_stops("B", "A", _attack(AtkType.STRIKE, PlayOrder.LEAD))


def test_card_without_stop_effect_cannot_stop() -> None:
    # Demo card 7 is an incremental-value Lead with no Stop effect.
    eng = _fresh()
    _hand_card(eng, "B", 7)
    assert eng._legal_stops("B", "A", _attack(AtkType.GRAPPLE, PlayOrder.LEAD)) == []


def test_stop_any_covers_every_ordering_of_its_type() -> None:
    # Demo card 25 (Strike) is a stop-any: stops Grapple of any ordering.
    eng = _fresh()
    card25 = _hand_card(eng, "B", 25)
    for order in (PlayOrder.LEAD, PlayOrder.FOLLOWUP, PlayOrder.FINISH):
        assert card25 in eng._legal_stops("B", "A", _attack(AtkType.GRAPPLE, order))
    assert card25 not in eng._legal_stops("B", "A", _attack(AtkType.STRIKE, PlayOrder.FINISH))


def test_skill_stop_gated_by_condition() -> None:
    # Demo card 15 (Submission skill stop) stops Strike iff defender Submission > attacker's.
    eng = _fresh()
    card15 = _hand_card(eng, "B", 15)  # B=Fae Submission 9 vs A=Bull Submission 8 -> online
    strike = _attack(AtkType.STRIKE, PlayOrder.FINISH)
    assert card15 in eng._legal_stops("B", "A", strike)
    # A card in play that lowers Fae's Submission below Bull's flips the stop offline.
    debuff = fx.Effect(
        trigger=fx.Static(),
        actions=(fx.BuffSkill(Skill.SUBMISSION, -3, fx.Who.SELF, fx.Duration.WHILE_IN_PLAY),),
        duration=fx.Duration.WHILE_IN_PLAY,
    )
    eng.state.players["B"].in_play.append(
        Card(
            db_uuid="d",
            name="D",
            number=1,
            atk_type=AtkType.STRIKE,
            play_order=PlayOrder.LEAD,
            effects=(debuff,),
        )
    )
    assert card15 not in eng._legal_stops("B", "A", strike)  # Fae Sub 9-3=6 < Bull 8


def test_see1_stop_needs_opp_type_in_play() -> None:
    # Demo card 19 (Strike see-1) stops Grapple only if the opponent already has a Grapple in play.
    eng = _fresh()
    card19 = _hand_card(eng, "B", 19)
    grapple = _attack(AtkType.GRAPPLE, PlayOrder.FINISH)
    assert card19 not in eng._legal_stops("B", "A", grapple)
    eng.state.players["A"].in_play.append(_attack(AtkType.GRAPPLE, PlayOrder.LEAD))
    assert card19 in eng._legal_stops("B", "A", grapple)


def test_heuristic_actually_plays_stops() -> None:
    # Regression: stop options must be tagged so the heuristic defender uses them
    # (the persistent board exposed a kind-mismatch that made it never stop).
    total = 0
    for seed in range(20):
        eng = _play(seed, HeuristicPolicy(), HeuristicPolicy())
        total += sum(1 for x in eng.state.log.to_lines()[1:] if json.loads(x)["type"] == "stop")
    assert total > 0


# -- persistent board + cross-turn chain (DESIGN.md §6) ----------------------


def test_playable_is_order_only_against_the_board() -> None:
    from srg_sim.engine import _playable

    lead = _attack(AtkType.STRIKE, PlayOrder.LEAD)
    fu = _attack(AtkType.STRIKE, PlayOrder.FOLLOWUP)
    fin = _attack(AtkType.STRIKE, PlayOrder.FINISH)
    assert _playable([], lead)  # a Lead is always playable
    assert _playable([lead], lead)  # you may stack another Lead
    assert not _playable([], fu)  # a Follow Up needs a Lead in play
    assert _playable([lead], fu)
    assert not _playable([lead], fin)  # a Finish needs a Follow Up, not just a Lead
    assert _playable([lead, fu], fin)


def test_resolved_card_persists_in_play_across_the_turn() -> None:
    eng = _fresh()
    eng.state.players["B"].hand = []  # defender cannot stop
    lead = next(c for c in eng.state.players["A"].deck if c.number == 7)  # plain Lead, no stop
    eng.state.players["A"].hand = [lead]
    eng._take_turn_action("A")
    assert lead in eng.state.players["A"].in_play  # board is NOT cleared each turn


def test_breakout_clears_both_boards_and_bumps_crowd_meter() -> None:
    eng = _fresh()
    eng.state.players["A"].in_play = [_attack(AtkType.STRIKE, PlayOrder.LEAD)]
    eng.state.players["B"].in_play = [_attack(AtkType.GRAPPLE, PlayOrder.LEAD)]
    eng._on_broken_out("A")
    assert eng.state.players["A"].in_play == []
    assert eng.state.players["B"].in_play == []
    assert eng.state.crowd_meter == 1


# -- snapshot mid-game -------------------------------------------------------


def test_mid_game_state_snapshot_round_trips() -> None:
    from srg_sim.state import GameState

    eng = _play(9)
    snap = eng.state.to_dict()
    assert GameState.from_dict(snap).to_dict() == snap
