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

from tests.demo_decks import bull, bull_vs_fae, fae, make_deck, vanilla, with_effects

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


def test_opponent_draw_action_draws_for_the_opponent() -> None:
    # Draw(who=OPP) moves cards to the OTHER player's hand (#27 "your opponent draws N").
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    before = len(eng.state.players["B"].hand)
    eng._act_draw(fx.Draw(n=2, who=fx.Who.OPP), "A")
    assert len(eng.state.players["B"].hand) == before + 2


def test_suppress_opponent_draw_voids_only_the_opponents_effect_draw() -> None:
    # Sami "The Draw": "your opponent does not draw for your card effects." A's
    # Draw(who=OPP) is voided; A's own draws and the raw amount are unaffected.
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    # Baseline: no flag -> Draw(who=OPP) gives B two cards.
    before = len(eng.state.players["B"].hand)
    eng._act_draw(fx.Draw(n=2, who=fx.Who.OPP), "A")
    assert len(eng.state.players["B"].hand) == before + 2
    # Give A the SuppressOpponentDraw flag -> A's Draw(who=OPP) is voided.
    flag = fx.Effect(
        trigger=fx.Static(),
        actions=(fx.SuppressOpponentDraw(),),
        source=fx.EffectSource.GIMMICK,
    )
    eng.state.players["A"].competitor = replace(eng.state.players["A"].competitor, effects=(flag,))
    b2 = len(eng.state.players["B"].hand)
    eng._act_draw(fx.Draw(n=2, who=fx.Who.OPP), "A")
    assert len(eng.state.players["B"].hand) == b2  # suppressed
    # A's OWN draw is unaffected, and B drawing for B's own effect is unaffected.
    a2 = len(eng.state.players["A"].hand)
    eng._act_draw(fx.Draw(n=1, who=fx.Who.SELF), "A")
    assert len(eng.state.players["A"].hand) == a2 + 1
    a3 = len(eng.state.players["A"].hand)
    eng._act_draw(fx.Draw(n=1, who=fx.Who.OPP), "B")  # B's effect makes A draw — allowed
    assert len(eng.state.players["A"].hand) == a3 + 1


def test_reveal_for_draw_draws_only_when_a_stop_is_revealed() -> None:
    # Bartholomew Hooke: reveal 1 from the opponent's hand; if it is a stop, draw 2.
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    stop_eff = fx.Effect(trigger=fx.OnPlay(), actions=(fx.Stop(),))
    stop = Card(
        db_uuid="stop", name="Stopper", number=13, atk_type=AtkType.STRIKE,
        play_order=PlayOrder.FOLLOWUP, effects=(stop_eff,),
    )
    eng.state.players["B"].hand = [stop]
    before = len(eng.state.players["A"].hand)
    eng._act_reveal_for_draw(fx.RevealForDraw(who=fx.Who.OPP, count=1, draw=2), "A")
    assert len(eng.state.players["A"].hand) == before + 2  # revealed a stop -> draw 2
    assert stop in eng.state.players["B"].hand  # a reveal does not remove the card
    # A non-stop reveal draws nothing.
    plain = Card(db_uuid="ns", name="Plain", number=1, atk_type=AtkType.STRIKE, play_order=PlayOrder.LEAD)
    eng.state.players["B"].hand = [plain]
    a2 = len(eng.state.players["A"].hand)
    eng._act_reveal_for_draw(fx.RevealForDraw(who=fx.Who.OPP, count=1, draw=2), "A")
    assert len(eng.state.players["A"].hand) == a2


def test_on_breakout_who_opp_recurs_spotlights_only_when_opponent_breaks_out() -> None:
    # "If your opponent breaks out, shuffle up to 3 Spotlight cards from your discard
    # into your deck." OnBreakout(who=OPP) fires for A only when B (the opponent) broke
    # out; the recur runs while A's card is still in play (before the board clears).
    def fresh() -> Engine:
        eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
        eng.setup()
        recur = Card(
            db_uuid="r", name="R", number=1, atk_type=AtkType.STRIKE, play_order=PlayOrder.LEAD,
            effects=(
                fx.Effect(
                    trigger=fx.OnBreakout(who=fx.Who.OPP),
                    actions=tuple(
                        fx.ShuffleIntoDeck(selector=fx.CardFilter(tag="Spotlight")) for _ in range(3)
                    ),
                ),
            ),
        )
        eng.state.players["A"].in_play = [recur]
        eng.state.players["A"].deck = []
        sp = [Card(db_uuid=f"s{i}", name="S", number=1, atk_type=AtkType.STRIKE,
                   play_order=PlayOrder.LEAD, tags=("Spotlight",)) for i in range(2)]
        plain = Card(db_uuid="p", name="P", number=2, atk_type=AtkType.GRAPPLE, play_order=PlayOrder.LEAD)
        eng.state.players["A"].discard = [*sp, plain]
        return eng

    # A finished; B (the opponent) broke out -> recur fires: 2 spotlights leave A's discard.
    eng = fresh()
    eng._on_broken_out("A")
    assert sum(1 for c in eng.state.players["A"].discard if "Spotlight" in c.tags) == 0
    assert sum(1 for c in eng.state.players["A"].deck if "Spotlight" in c.tags) == 2
    assert any(c.db_uuid == "p" for c in eng.state.players["A"].discard)  # plain card stays
    # B finished; A broke out -> A's OnBreakout(who=OPP) does NOT fire.
    eng = fresh()
    eng._on_broken_out("B")
    assert sum(1 for c in eng.state.players["A"].discard if "Spotlight" in c.tags) == 2


def test_finish_roll_bonus_per_spotlight_in_opponent_discard() -> None:
    # "Your Finish rolls are +1 for each Spotlight in your opponent's discard pile":
    # FinishRollBonus.per counts per_who's cards in per_zone matching the filter.
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    made = Card(
        db_uuid="m", name="M", number=1, atk_type=AtkType.STRIKE, play_order=PlayOrder.LEAD,
        effects=(
            fx.Effect(
                trigger=fx.Static(),
                actions=(fx.FinishRollBonus(
                    delta=1, per=fx.CardFilter(tag="Spotlight"),
                    per_who=fx.Who.OPP, per_zone=fx.CountZone.DISCARD,
                ),),
            ),
        ),
    )
    eng.state.players["A"].in_play = [made]

    def spot(i: int) -> Card:
        return Card(db_uuid=f"s{i}", name="S", number=1, atk_type=AtkType.STRIKE,
                    play_order=PlayOrder.LEAD, tags=("Spotlight",))

    plain = Card(db_uuid="p", name="P", number=1, atk_type=AtkType.STRIKE, play_order=PlayOrder.LEAD)
    eng.state.players["B"].discard = [spot(1), spot(2), plain]  # 2 spotlights in OPP discard
    assert eng._finish_roll_bonus("A", Skill.POWER) == 2  # any skill; +1 per spotlight
    eng.state.players["B"].discard = []
    assert eng._finish_roll_bonus("A", Skill.POWER) == 0
    # per_who=OPP: spotlights in A's OWN discard must not count.
    eng.state.players["A"].discard = [spot(1), spot(2)]
    assert eng._finish_roll_bonus("A", Skill.POWER) == 0


def test_spotlight_count_gates_a_conditional_finish_bonus() -> None:
    # "If you have 3+ Spotlight cards in play, +2 to Technique": HasInPlay over the
    # synthetic Spotlight tag gates a FinishRollBonus. Pure override, no new node.
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    cond = fx.HasInPlay(
        who=fx.Who.SELF, filter=fx.CardFilter(tag="Spotlight"), count=3, cmp=fx.Comparator.GE
    )

    def spot(i: int) -> Card:
        return Card(db_uuid=f"s{i}", name=f"S{i}", number=1, atk_type=AtkType.STRIKE,
                    play_order=PlayOrder.LEAD, tags=("Spotlight",))

    made = Card(
        db_uuid="m", name="M", number=1, atk_type=AtkType.STRIKE, play_order=PlayOrder.LEAD,
        effects=(
            fx.Effect(
                trigger=fx.Static(), condition=cond,
                actions=(fx.FinishRollBonus(delta=2, when_skill=Skill.TECHNIQUE),),
            ),
        ),
    )
    eng.state.players["A"].in_play = [spot(1), spot(2), spot(3), made]
    assert eng._finish_roll_bonus("A", Skill.TECHNIQUE) == 2  # 3 spotlights -> +2
    assert eng._finish_roll_bonus("A", Skill.POWER) == 0  # wrong finish skill
    eng.state.players["A"].in_play = [spot(1), made]
    assert eng._finish_roll_bonus("A", Skill.TECHNIQUE) == 0  # only 1 spotlight -> no bonus


def test_while_in_discard_blank_only_active_from_the_discard_pile() -> None:
    # "When this card is in your discard pile, your opponent's Spotlights are blank":
    # a WHILE_IN_DISCARD BlankText is active only while its card sits in the discard.
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    blanker = Card(
        db_uuid="bk", name="Blanker", number=1, atk_type=AtkType.STRIKE, play_order=PlayOrder.LEAD,
        effects=(
            fx.Effect(
                trigger=fx.Static(),
                actions=(fx.BlankText(selector=fx.CardFilter(tag="Spotlight"), who=fx.Who.OPP),),
                duration=fx.Duration.WHILE_IN_DISCARD,
            ),
        ),
    )
    spot = Card(db_uuid="sp", name="Spot", number=2, atk_type=AtkType.GRAPPLE,
                play_order=PlayOrder.LEAD, tags=("Spotlight",))
    # In A's discard -> B's Spotlight is blanked.
    eng.state.players["A"].discard = [blanker]
    eng.state.players["A"].in_play = []
    assert eng.state.is_text_blanked(spot, "B")
    # Same card in A's play (WHILE_IN_DISCARD) -> inert, no blank.
    eng.state.players["A"].discard = []
    eng.state.players["A"].in_play = [blanker]
    assert not eng.state.is_text_blanked(spot, "B")


def test_blank_text_blanks_opponent_spotlights() -> None:
    # "Your opponent's Spotlights are blank": while A holds the declaring card in play,
    # B's Spotlight cards fire no effects and cannot stop.
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    blanker = Card(
        db_uuid="bk", name="Blanker", number=1, atk_type=AtkType.STRIKE, play_order=PlayOrder.LEAD,
        effects=(
            fx.Effect(
                trigger=fx.Static(),
                actions=(fx.BlankText(selector=fx.CardFilter(tag="Spotlight"), who=fx.Who.OPP),),
            ),
        ),
    )
    eng.state.players["A"].in_play = [blanker]
    spot_draw = Card(
        db_uuid="sd", name="Spot", number=2, atk_type=AtkType.GRAPPLE, play_order=PlayOrder.LEAD,
        tags=("Spotlight",),
        effects=(fx.Effect(trigger=fx.OnPlay(), actions=(fx.Draw(n=2),)),),
    )
    # B owns the spotlight card -> blanked from A's declaration.
    assert eng.state.is_text_blanked(spot_draw, "B")
    plain = Card(db_uuid="pl", name="Plain", number=2, atk_type=AtkType.GRAPPLE, play_order=PlayOrder.LEAD)
    assert not eng.state.is_text_blanked(plain, "B")
    # A's OWN spotlight card is not blanked (the declaration targets the opponent).
    assert not eng.state.is_text_blanked(spot_draw, "A")
    # Playing the blanked card fires none of its text (no Draw 2).
    eng.state.players["A"].hand = []  # A won't stop B
    before = len(eng.state.players["B"].hand)
    eng._resolve_play("B", "A", spot_draw)
    assert len(eng.state.players["B"].hand) == before  # OnPlay Draw was blanked
    # A blanked Spotlight stop card cannot stop.
    spot_stop = Card(
        db_uuid="sst", name="SpotStop", number=1, atk_type=AtkType.STRIKE, play_order=PlayOrder.LEAD,
        tags=("Spotlight",),
        effects=(fx.Effect(trigger=fx.OnPlay(), actions=(fx.Stop(atk_type=AtkType.GRAPPLE),)),),
    )
    attack = Card(db_uuid="at", name="Atk", number=2, atk_type=AtkType.GRAPPLE, play_order=PlayOrder.LEAD)
    # B owns the stop card; A's declaration blanks it -> cannot stop.
    assert not eng._card_can_stop("B", spot_stop, attack)


def test_stop_requires_tag_gates_on_the_attacker_being_a_spotlight() -> None:
    # "Stop any Grapple with a Spotlight": the stop is legal only vs a Grapple that
    # carries the Spotlight tag (StopRequiresTag marker paired with the Stop).
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    stopper = Card(
        db_uuid="st", name="Stopper", number=1, atk_type=AtkType.STRIKE, play_order=PlayOrder.LEAD,
        effects=(
            fx.Effect(
                trigger=fx.OnPlay(),
                actions=(fx.Stop(atk_type=AtkType.GRAPPLE), fx.StopRequiresTag(tag="Spotlight")),
            ),
        ),
    )
    spot_grapple = Card(db_uuid="sg", name="G", number=2, atk_type=AtkType.GRAPPLE,
                        play_order=PlayOrder.LEAD, tags=("Spotlight",))
    plain_grapple = Card(db_uuid="pg", name="G2", number=2, atk_type=AtkType.GRAPPLE,
                         play_order=PlayOrder.LEAD)
    spot_strike = Card(db_uuid="ss", name="S", number=1, atk_type=AtkType.STRIKE,
                       play_order=PlayOrder.LEAD, tags=("Spotlight",))
    assert eng._card_can_stop("B", stopper, spot_grapple)  # Grapple + Spotlight -> stoppable
    assert not eng._card_can_stop("B", stopper, plain_grapple)  # Grapple, no Spotlight -> not
    assert not eng._card_can_stop("B", stopper, spot_strike)  # Spotlight but wrong type -> not


def test_spotlight_tag_injected_at_load() -> None:
    # The DB `spotlight: true` flag folds into a synthetic "Spotlight" tag so gimmicks
    # match it via CardFilter(tag="Spotlight") — no Effect-IR change.
    from srg_sim.loader import _build_card

    rec = {"db_uuid": "s", "name": "Cuddle Time", "deck_card_number": 1, "spotlight": True}
    assert "Spotlight" in _build_card(rec).tags
    plain = {"db_uuid": "p", "name": "Plain", "deck_card_number": 2, "tags": ["Old School"]}
    assert "Spotlight" not in _build_card(plain).tags


def test_search_for_a_spotlight_card_finds_it_by_tag() -> None:
    # I Was Made for the Spotlight: "Search your deck for a Spotlight card, add to hand."
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    spot = Card(
        db_uuid="sp", name="Restoration Potion", number=1, atk_type=AtkType.STRIKE,
        play_order=PlayOrder.LEAD, tags=("Spotlight",),
    )
    plain = Card(
        db_uuid="pl", name="Plain", number=2, atk_type=AtkType.GRAPPLE, play_order=PlayOrder.LEAD
    )
    eng.state.players["A"].deck = [plain, spot]
    made = Card(
        db_uuid="made", name="I Was Made for the Spotlight", number=7, atk_type=AtkType.STRIKE,
        play_order=PlayOrder.LEAD,
        effects=(
            fx.Effect(
                trigger=fx.OnPlay(),
                actions=(fx.Search(filter=fx.CardFilter(tag="Spotlight"), dest=fx.Dest.HAND),),
            ),
        ),
    )
    eng.state.players["B"].hand = []  # no stop
    eng._resolve_play("A", "B", made)
    assert spot in eng.state.players["A"].hand  # the Spotlight card was searched to hand
    assert spot not in eng.state.players["A"].deck


def test_add_text_injects_effects_into_matching_named_cards() -> None:
    # El Super Santa: cards with "Super" in the name gain the added text "Draw 2".
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    added = fx.Effect(trigger=fx.OnPlay(), actions=(fx.Draw(n=2),))
    gimmick = fx.Effect(
        trigger=fx.Static(),
        actions=(fx.AddText(name_contains=("Super",), effects=(added,)),),
        source=fx.EffectSource.GIMMICK,
    )
    eng.state.players["A"].competitor = replace(
        eng.state.players["A"].competitor, effects=(gimmick,)
    )
    supercard = Card(
        db_uuid="sc", name="Super Kick", number=1, atk_type=AtkType.STRIKE, play_order=PlayOrder.LEAD
    )
    plain = Card(
        db_uuid="pc", name="Kick", number=2, atk_type=AtkType.GRAPPLE, play_order=PlayOrder.LEAD
    )
    # The gimmick grants the added effect only to the Super-named card.
    assert eng._injected_text("A", supercard) == [added]
    assert eng._injected_text("A", plain) == []
    # Play resolution runs the injected OnPlay Draw 2 (B holds no stop, so it lands).
    eng.state.players["B"].hand = []
    before = len(eng.state.players["A"].hand)
    eng._resolve_play("A", "B", supercard)
    assert len(eng.state.players["A"].hand) == before + 2


def _static_blank_opp(source: fx.EffectSource, condition: fx.Condition) -> fx.Effect:
    return fx.Effect(
        trigger=fx.Static(),
        condition=condition,
        actions=(fx.BlankGimmick(who=fx.Who.OPP, duration=fx.Duration.WHILE_IN_PLAY),),
        source=source,
    )


def test_gimmick_sourced_conditional_blank() -> None:
    # A gimmick-sourced Static conditional BlankGimmick blanks the opponent (GM
    # Calace V2 / Mr. Snap V1 shape) — but only while its count condition holds AND
    # the owner's own gimmick is still active.
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    for k in ("A", "B"):
        eng.state.players[k].competitor = replace(eng.state.players[k].competitor, effects=())
        eng.state.players[k].entrance = replace(eng.state.players[k].entrance, effects=())
        eng.state.players[k].in_play = []

    has_two_bars = fx.HasInPlay(
        who=fx.Who.SELF,
        filter=fx.CardFilter(name_contains=("Bar",)),
        count=2,
        cmp=fx.Comparator.GE,
    )
    eng.state.players["A"].competitor = replace(
        eng.state.players["A"].competitor,
        effects=(_static_blank_opp(fx.EffectSource.GIMMICK, has_two_bars),),
    )

    def card(nm: str, i: int) -> Card:
        return Card(db_uuid=f"c{i}", name=nm, number=1, atk_type=AtkType.STRIKE, play_order=PlayOrder.LEAD)

    # 1 matching card -> below threshold -> not blanked.
    eng.state.players["A"].in_play = [card("Crowbar", 1)]
    assert not eng.state.is_gimmick_blanked("B")
    # 2 matching -> the count holds -> B is blanked.
    eng.state.players["A"].in_play = [card("Crowbar", 1), card("Sidebar", 2)]
    assert eng.state.is_gimmick_blanked("B")
    # B's entrance unconditionally blanks A: A's gimmick goes inactive, so it can no
    # longer blank B (the "only while your own gimmick is active" gate; guard-bounded).
    eng.state.players["B"].entrance = replace(
        eng.state.players["B"].entrance,
        effects=(_static_blank_opp(fx.EffectSource.ENTRANCE, fx.Always()),),
    )
    assert eng.state.is_gimmick_blanked("A")
    assert not eng.state.is_gimmick_blanked("B")


def _breakout_mod(delta: int, attempts: int | None, condition: fx.Condition | None = None) -> fx.Effect:
    """A Static gimmick effect wrapping one BreakoutModifier, gated by `condition`."""
    return fx.Effect(
        trigger=fx.Static(),
        condition=condition or fx.Always(),
        actions=(fx.BreakoutModifier(delta=delta, attempts=attempts),),
        source=fx.EffectSource.GIMMICK,
    )


def _with_gimmicks(eng: Engine, key: str, *effs: fx.Effect) -> None:
    eng.state.players[key].competitor = replace(
        eng.state.players[key].competitor, effects=tuple(effs)
    )


def test_breakout_modifier_attempts_gate_selects_the_nth_roll() -> None:
    # El Super Hombre V1: "Your 3rd breakout roll each turn is +2." Only the 3rd
    # attempt sees it; a flat modifier (attempts None) applies to every attempt and
    # stacks. A false condition / a modifier on the other side never leaks in.
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    _with_gimmicks(eng, "A", _breakout_mod(2, 3))
    assert eng._breakout_bonus("A", 1) == 0
    assert eng._breakout_bonus("A", 2) == 0
    assert eng._breakout_bonus("A", 3) == 2

    _with_gimmicks(eng, "A", _breakout_mod(1, None), _breakout_mod(2, 3))
    assert eng._breakout_bonus("A", 1) == 1
    assert eng._breakout_bonus("A", 3) == 3

    gated = _breakout_mod(2, None, fx.CrowdMeterCompare(cmp=fx.Comparator.GE, value=5))
    _with_gimmicks(eng, "A", gated)
    _with_gimmicks(eng, "B", _breakout_mod(4, None))
    assert eng._breakout_bonus("A", 1) == 0  # crowd meter is 0, condition false
    assert eng._breakout_bonus("B", 1) == 4  # B's own modifier, not A's

    _with_gimmicks(eng, "A", _breakout_mod(2, 3))
    eng.state.players["A"].gimmick_blanked = True
    assert eng._breakout_bonus("A", 3) == 0  # blanked gimmick contributes nothing


def test_breakout_roll_honors_the_modifier() -> None:
    # Defender stats are all 5, so a finish of 8 is unbreakable (5 < 8) unaided; a flat
    # +5 breakout modifier lifts every roll to 10 and breaks out at once. Drives the
    # real _breakout() roll, proving the bonus reaches stat_breaks_out as a -penalty.
    from srg_sim.cards import Stats

    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    flat = Stats(power=5, agility=5, technique=5, submission=5, grapple=5, strike=5)
    eng.state.players["A"].competitor = replace(eng.state.players["A"].competitor, stats=flat)
    assert not eng._breakout("A", 8)
    _with_gimmicks(eng, "A", _breakout_mod(5, None))
    assert eng._breakout("A", 8)
    last = eng.state.log.events[-1]
    assert last.rolls[0].penalty == -5


def test_on_stop_order_gates_on_the_stopped_cards_play_order() -> None:
    # La Fenix (Super Lucha): OnStop{dir=YOURS, order=Finish} fires only when the
    # STOPPED card is a Finish — tutoring a Finish from the deck to hand.
    tutor = fx.Search(
        filter=fx.CardFilter(play_order=PlayOrder.FINISH), dest=fx.Dest.HAND, count=1
    )
    gimmick = fx.Effect(
        trigger=fx.OnStop(dir=fx.Direction.YOURS, order=PlayOrder.FINISH),
        actions=(tutor,),
        source=fx.EffectSource.GIMMICK,
    )

    def card(u: str, order: PlayOrder) -> Card:
        return Card(db_uuid=u, name=u, number=1, atk_type=AtkType.STRIKE, play_order=order)

    def fresh() -> Engine:
        eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
        eng.state.players["A"].competitor = replace(
            eng.state.players["A"].competitor, effects=(gimmick,)
        )
        eng.state.players["A"].deck = [card("tutor-finish", PlayOrder.FINISH), card("l", PlayOrder.LEAD)]
        eng.state.players["A"].hand = []
        return eng

    def tutored(eng: Engine) -> bool:
        return any(c.db_uuid == "tutor-finish" for c in eng.state.players["A"].hand)

    # A's Finish is stopped -> the gate matches -> tutor fires.
    eng = fresh()
    eng._apply_stop("A", "B", card("my-finish", PlayOrder.FINISH), card("stop", PlayOrder.LEAD))
    assert tutored(eng)
    # A's Lead is stopped -> order=Finish gate stays inert.
    eng = fresh()
    eng._apply_stop("A", "B", card("my-lead", PlayOrder.LEAD), card("stop", PlayOrder.LEAD))
    assert not tutored(eng)


class _PickChoice(HeuristicPolicy):
    def __init__(self, pick: int) -> None:
        super().__init__()
        self.pick = pick

    def choose(self, point, legal, state, key):  # type: ignore[no-untyped-def]
        if point == "choice":
            return next((o for o in legal if o["index"] == self.pick), legal[0])
        return super().choose(point, legal, state, key)


def _eshv3_engine(pick: int) -> Engine:
    eng = Engine(*bull_vs_fae(), _PickChoice(pick), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    gimmick = fx.Effect(
        trigger=fx.OnRollBoost(skill=Skill.AGILITY, delta=0, on_bump=False),
        actions=(
            fx.Choice(options=(
                fx.ChoiceOption(label="draw", actions=(fx.Draw(n=1),)),
                fx.ChoiceOption(label="boost", actions=(fx.RollBoost(1),)),
            )),
        ),
        source=fx.EffectSource.GIMMICK,
    )
    eng.state.players["A"].competitor = replace(eng.state.players["A"].competitor, effects=(gimmick,))
    eng.state.players["A"].deck = [
        Card(db_uuid="d", name="d", number=1, atk_type=AtkType.STRIKE, play_order=PlayOrder.LEAD)
    ]
    return eng


def test_eshv3_boost_branch_adds_one_to_the_roll() -> None:
    eng = _eshv3_engine(pick=1)  # "boost"
    before = len(eng.state.players["A"].hand)
    v = eng._offer_roll_boost("A", Skill.AGILITY, 7, on_bump=False)
    assert v == 8
    assert len(eng.state.players["A"].hand) == before  # no draw


def test_eshv3_draw_branch_leaves_the_roll_and_draws() -> None:
    eng = _eshv3_engine(pick=0)  # "draw"
    before = len(eng.state.players["A"].hand)
    v = eng._offer_roll_boost("A", Skill.AGILITY, 7, on_bump=False)
    assert v == 7
    assert len(eng.state.players["A"].hand) == before + 1


def test_eshv3_does_not_fire_on_a_non_agility_roll() -> None:
    eng = _eshv3_engine(pick=1)
    assert eng._offer_roll_boost("A", Skill.POWER, 7, on_bump=False) == 7


def _pedro_engine(ent_name: str, declare: bool = True) -> Engine:
    from srg_sim.cards import EntranceCard
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    ent = EntranceCard(
        db_uuid="ent", name=ent_name,
        effects=(
            fx.Effect(
                trigger=fx.Static(),
                actions=(fx.Draw(n=1), fx.ModifyRoll(who=fx.Who.SELF, delta=1, when=fx.RollWhen.NEXT)),
            ),
        ),
    )
    eng.state.players["A"].entrance = ent
    if declare:
        decl = fx.Effect(
            trigger=fx.Static(),
            actions=(fx.ScaleEntranceNumbers(name_contains=("Training with",), factor=3),),
            source=fx.EffectSource.GIMMICK,
        )
        eng.state.players["A"].competitor = replace(eng.state.players["A"].competitor, effects=(decl,))
    return eng


def _entrance_numbers(eng: Engine) -> tuple[int, int]:
    n = d = 0
    for eff in eng._standing_effects("A"):
        for a in eff.actions:
            if isinstance(a, fx.Draw):
                n = a.n
            elif isinstance(a, fx.ModifyRoll):
                d = a.delta
    return n, d


def test_pedro_scales_a_matching_entrance() -> None:
    eng = _pedro_engine("Power Training with Rock Newman")
    assert _entrance_numbers(eng) == (3, 3)  # draw 1 -> 3, +1 roll -> +3


def test_pedro_does_not_scale_a_non_matching_entrance() -> None:
    eng = _pedro_engine("Some Other Entrance")
    assert _entrance_numbers(eng) == (1, 1)


def test_pedro_blanked_gimmick_stops_scaling() -> None:
    eng = _pedro_engine("Power Training with Rock Newman")
    assert _entrance_numbers(eng) == (3, 3)
    eng.state.players["A"].gimmick_blanked = True
    assert _entrance_numbers(eng) == (1, 1)


def _lead(uuid: str) -> Card:
    return Card(db_uuid=uuid, name=uuid, number=1, atk_type=AtkType.STRIKE, play_order=PlayOrder.LEAD)


def test_shuffle_hand_draw_reveal_one_shuffles_a_single_card() -> None:
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    eng.state.players["A"].hand = [_lead("h1"), _lead("h2")]
    eng.state.players["A"].deck = [_lead("d1"), _lead("d2")]
    eng._act_shuffle_hand_draw(fx.ShuffleHandDraw(who=fx.Who.SELF, count=1, hand_count=1), "A")
    a = eng.state.players["A"]
    # hand size 2 proves exactly 1 was shed (a whole-hand shuffle would leave 1 = draw only).
    assert len(a.hand) == 2  # shed 1 + drew 1
    assert len(a.hand) + len(a.deck) == 4  # no cards lost


def test_shuffle_hand_draw_whole_hand_when_hand_count_none() -> None:
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    eng.state.players["A"].hand = [_lead("h1"), _lead("h2"), _lead("h3")]
    eng.state.players["A"].deck = [_lead("d1")]
    eng._act_shuffle_hand_draw(fx.ShuffleHandDraw(who=fx.Who.SELF, count=2), "A")
    a = eng.state.players["A"]
    assert len(a.hand) == 2
    assert len(a.hand) + len(a.deck) == 4


def test_during_opponent_turn_fires_for_the_non_active_player() -> None:
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    gimmick = fx.Effect(
        trigger=fx.DuringOpponentTurn(), actions=(fx.Draw(n=1),), source=fx.EffectSource.GIMMICK
    )
    eng.state.players["A"].competitor = replace(eng.state.players["A"].competitor, effects=(gimmick,))
    eng.state.players["A"].hand = []
    eng.state.players["A"].deck = [_lead("d1")]
    eng._run_opponent_turn("A")
    assert len(eng.state.players["A"].hand) == 1


def _inplay(uuid: str, order: PlayOrder) -> Card:
    return Card(db_uuid=uuid, name=uuid, number=1, atk_type=AtkType.STRIKE, play_order=order)


def test_candyman_discards_own_then_opponents_same_order() -> None:
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    eng.state.players["A"].in_play = [_inplay("a-fu", PlayOrder.FOLLOWUP)]
    eng.state.players["B"].in_play = [_inplay("b-fu", PlayOrder.FOLLOWUP), _inplay("b-lead", PlayOrder.LEAD)]
    eng._act_discard_in_play_match(fx.DiscardInPlayMatch(), "A")
    assert not eng.state.players["A"].in_play
    assert any(c.db_uuid == "a-fu" for c in eng.state.players["A"].discard)
    assert any(c.db_uuid == "b-lead" for c in eng.state.players["B"].in_play)  # Lead kept
    assert not any(c.db_uuid == "b-fu" for c in eng.state.players["B"].in_play)  # Follow Up discarded


def test_candyman_no_matching_opponent_card_discards_only_own() -> None:
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    eng.state.players["A"].in_play = [_inplay("a-fin", PlayOrder.FINISH)]
    eng.state.players["B"].in_play = [_inplay("b-lead", PlayOrder.LEAD)]
    eng._act_discard_in_play_match(fx.DiscardInPlayMatch(), "A")
    assert not eng.state.players["A"].in_play
    assert len(eng.state.players["B"].in_play) == 1  # no Finish to match


def test_candyman_no_own_in_play_is_a_noop() -> None:
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    eng.state.players["A"].in_play = []
    eng.state.players["B"].in_play = [_inplay("b-fu", PlayOrder.FOLLOWUP)]
    eng._act_discard_in_play_match(fx.DiscardInPlayMatch(), "A")
    assert len(eng.state.players["B"].in_play) == 1  # nothing to trade


def test_bump_replace_makes_the_opponent_discard_instead_of_drawing() -> None:
    # Mack-a-Tack (A) declares BumpDrawReplace: on a bump, B discards 1 instead of drawing.
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    gimmick = fx.Effect(
        trigger=fx.Static(), actions=(fx.BumpDrawReplace(),), source=fx.EffectSource.GIMMICK
    )
    eng.state.players["A"].competitor = replace(eng.state.players["A"].competitor, effects=(gimmick,))
    for k in ("A", "B"):
        eng.state.players[k].hand = [
            Card(db_uuid=f"{k}1", name="c", number=1, atk_type=AtkType.STRIKE, play_order=PlayOrder.LEAD)
        ]
        eng.state.players[k].deck = [
            Card(db_uuid=f"{k}d", name="d", number=1, atk_type=AtkType.STRIKE, play_order=PlayOrder.LEAD)
        ]
    eng._bump_draw("B")  # A declares -> B discards 1, does not draw
    assert len(eng.state.players["B"].hand) == 0
    assert len(eng.state.players["B"].deck) == 1
    eng._bump_draw("A")  # B declares nothing -> A draws
    assert len(eng.state.players["A"].hand) == 2
    assert len(eng.state.players["A"].deck) == 0


def test_bumped_last_turn_roll_condition_reads_the_flag() -> None:
    from srg_sim import conditions
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    assert not conditions.holds(fx.BumpedLastTurnRoll(), eng.state, "A")
    eng.state.last_turn_bumped = True
    assert conditions.holds(fx.BumpedLastTurnRoll(), eng.state, "A")


def _glw_engine() -> Engine:
    # General Lee Wong V2: OnRolledAll{P,A,T} -> Draw 3 + next roll +2.
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    gimmick = fx.Effect(
        trigger=fx.OnRolledAll(skills=(Skill.POWER, Skill.AGILITY, Skill.TECHNIQUE), who=fx.Who.SELF),
        actions=(fx.Draw(n=3), fx.ModifyRoll(who=fx.Who.SELF, delta=2, when=fx.RollWhen.NEXT)),
        source=fx.EffectSource.GIMMICK,
    )
    eng.state.players["A"].competitor = replace(eng.state.players["A"].competitor, effects=(gimmick,))
    eng.state.players["A"].hand = []
    eng.state.players["A"].deck = [
        Card(db_uuid=f"d{i}", name=f"d{i}", number=1, atk_type=AtkType.STRIKE, play_order=PlayOrder.LEAD)
        for i in range(6)
    ]
    return eng


def _roll_glw(eng: Engine, skill: Skill) -> None:
    from srg_sim import conditions
    eng._roll_ctx["A"] = conditions.RollContext(skill=skill, gap=0, value=5, opp_skill=Skill.POWER)
    eng._run_on_rolled_all("A")


def test_on_rolled_all_fires_after_the_set_then_resets() -> None:
    eng = _glw_engine()
    _roll_glw(eng, Skill.POWER)
    _roll_glw(eng, Skill.AGILITY)
    assert len(eng.state.players["A"].hand) == 0  # incomplete
    _roll_glw(eng, Skill.POWER)  # idempotent
    assert len(eng.state.players["A"].hand) == 0
    _roll_glw(eng, Skill.TECHNIQUE)  # completes {P, A, T}
    assert len(eng.state.players["A"].hand) == 3  # drew 3
    assert eng.state.players["A"].pending_roll_mods["next"] == 2  # next roll +2
    _roll_glw(eng, Skill.TECHNIQUE)  # accumulator reset -> no re-fire
    assert len(eng.state.players["A"].hand) == 3


def test_on_rolled_all_ignores_non_required_skills() -> None:
    eng = _glw_engine()
    for s in (Skill.SUBMISSION, Skill.GRAPPLE, Skill.POWER, Skill.AGILITY, Skill.TECHNIQUE):
        _roll_glw(eng, s)
    assert len(eng.state.players["A"].hand) == 3  # only P/A/T count


def _hyde_engine() -> Engine:
    # Mr. Hyde: a Static once-per-turn optional self re-roll costing an in-play "Potion".
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    gimmick = fx.Effect(
        trigger=fx.Static(),
        actions=(
            fx.Reroll(
                who=fx.Who.SELF, once=True, choose=False, when=fx.RollWhen.THIS,
                cost=fx.CardFilter(name_contains=("Potion",)),
            ),
        ),
        source=fx.EffectSource.GIMMICK,
        optional=True,
        frequency=fx.FrequencyGuard(kind=fx.Frequency.ONCE_PER_TURN),
    )
    eng.state.players["A"].competitor = replace(eng.state.players["A"].competitor, effects=(gimmick,))
    return eng


def _potion(uuid: str) -> Card:
    return Card(db_uuid=uuid, name="Health Potion", number=1, atk_type=AtkType.STRIKE,
                play_order=PlayOrder.LEAD)


def test_costed_reroll_fires_and_shuffles_the_potion_away() -> None:
    from srg_sim import conditions
    eng = _hyde_engine()
    eng.state.players["A"].in_play = [_potion("p1")]
    ctx = conditions.RollContext(skill=Skill.POWER, gap=0, value=5, opp_skill=Skill.POWER)
    target = eng._offer_reroll("A", ctx, ctx)
    assert target == "A"  # offered and taken
    a = eng.state.players["A"]
    assert not a.in_play  # the Potion left play
    assert any(c.db_uuid == "p1" for c in a.deck)  # shuffled into the deck


def test_costed_reroll_is_not_offered_without_the_potion() -> None:
    from srg_sim import conditions
    eng = _hyde_engine()
    eng.state.players["A"].in_play = [
        Card(db_uuid="x", name="Chair", number=1, atk_type=AtkType.STRIKE, play_order=PlayOrder.LEAD)
    ]
    ctx = conditions.RollContext(skill=Skill.POWER, gap=0, value=5, opp_skill=Skill.POWER)
    assert eng._offer_reroll("A", ctx, ctx) is None
    assert len(eng.state.players["A"].in_play) == 1  # untouched


def test_burying_the_opponents_discard_does_not_crash() -> None:
    # Regression: the heuristic _at_bury used to look the chosen card up in the ACTOR's
    # discard, failing when burying the OPPONENT's. A buries 1 from B's discard; the
    # card must move to B's deck bottom (the Finish, most recyclable).
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    eng.state.players["B"].discard = [
        Card(db_uuid="b-fin", name="F", number=20, atk_type=AtkType.STRIKE, play_order=PlayOrder.FINISH),
        Card(db_uuid="b-lead", name="L", number=1, atk_type=AtkType.STRIKE, play_order=PlayOrder.LEAD),
    ]
    bury = fx.Bury(
        selector=fx.CardFilter(), count=1, who=fx.Who.OPP, random=False,
        source=fx.BuryFrom.DISCARD, choose=False,
    )
    eng._act_bury(bury, "A")  # must not raise
    b = eng.state.players["B"]
    assert len(b.discard) == 1
    assert any(c.db_uuid == "b-fin" for c in b.deck)


def test_on_finish_roll_fires_only_on_the_gated_skill() -> None:
    # The Man from I.T.: OnFinishRoll(Technique) fires on a Technique finish roll only.
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    gimmick = fx.Effect(
        trigger=fx.OnFinishRoll(skill=Skill.TECHNIQUE, who=fx.Who.SELF),
        actions=(fx.Draw(n=1),),
        source=fx.EffectSource.GIMMICK,
    )
    eng.state.players["A"].competitor = replace(eng.state.players["A"].competitor, effects=(gimmick,))
    eng.state.players["A"].deck = [
        Card(db_uuid="d", name="D", number=1, atk_type=AtkType.STRIKE, play_order=PlayOrder.LEAD)
    ]
    before = len(eng.state.players["A"].hand)
    eng._run_on_finish_roll("A", Skill.POWER, 20)  # wrong skill -> no fire
    assert len(eng.state.players["A"].hand) == before
    eng._run_on_finish_roll("A", Skill.TECHNIQUE, 20)  # matches -> draw 1
    assert len(eng.state.players["A"].hand) == before + 1


def test_choose_hand_bury_lets_the_attacker_bury_the_opponents_best() -> None:
    # A buries 1 of B's Follow Up / Finish hand cards, choosing (sabotage = the Finish).
    # B's Lead is out of the filter and stays.
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    eng.state.players["B"].hand = [
        Card(db_uuid="b-lead", name="L", number=1, atk_type=AtkType.STRIKE, play_order=PlayOrder.LEAD),
        Card(db_uuid="b-fu", name="F", number=2, atk_type=AtkType.STRIKE, play_order=PlayOrder.FOLLOWUP),
        Card(db_uuid="b-fin", name="X", number=3, atk_type=AtkType.STRIKE, play_order=PlayOrder.FINISH),
    ]
    bury = fx.Bury(
        selector=fx.CardFilter(play_orders=(PlayOrder.FOLLOWUP, PlayOrder.FINISH)),
        count=1, who=fx.Who.OPP, random=False, source=fx.BuryFrom.HAND, choose=True,
    )
    eng._act_bury(bury, "A")
    b = eng.state.players["B"]
    assert any(c.db_uuid == "b-lead" for c in b.hand)  # Lead untouched
    assert any(c.db_uuid == "b-fu" for c in b.hand)
    assert not any(c.db_uuid == "b-fin" for c in b.hand)  # Finish buried
    assert any(c.db_uuid == "b-fin" for c in b.deck)


def test_stop_counts_order_as_lets_a_followup_stop_catch_a_finish() -> None:
    # Jokerfish V2: "your opponent's Finishes are also Follow Ups for your Stop cards."
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    followup_stop = Card(
        db_uuid="fs", name="FollowStop", number=1, atk_type=AtkType.STRIKE, play_order=PlayOrder.LEAD,
        effects=(fx.Effect(trigger=fx.Static(), actions=(fx.Stop(order=PlayOrder.FOLLOWUP),)),),
    )
    finish = Card(db_uuid="fin", name="Fin", number=1, atk_type=AtkType.STRIKE, play_order=PlayOrder.FINISH)
    lead = Card(db_uuid="ld", name="Ld", number=1, atk_type=AtkType.STRIKE, play_order=PlayOrder.LEAD)
    # Without the declaration a Follow-Up stop cannot stop a Finish.
    assert not eng._card_can_stop("A", followup_stop, finish)
    eng.state.players["A"].competitor = replace(
        eng.state.players["A"].competitor,
        effects=(
            fx.Effect(
                trigger=fx.Static(),
                actions=(fx.StopCountsOrderAs(PlayOrder.FINISH, PlayOrder.FOLLOWUP),),
                source=fx.EffectSource.GIMMICK,
            ),
        ),
    )
    assert eng._card_can_stop("A", followup_stop, finish)  # reframe applies
    assert not eng._card_can_stop("A", followup_stop, lead)  # only Finish->Follow Up


def test_suppress_stop_disables_only_cards_in_the_number_range() -> None:
    # Jokerfish V2: "your cards #19-21 cannot stop cards."
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()

    def stopper(number: int) -> Card:
        return Card(
            db_uuid=f"s{number}", name="S", number=number, atk_type=AtkType.STRIKE,
            play_order=PlayOrder.LEAD,
            effects=(fx.Effect(trigger=fx.Static(), actions=(fx.Stop(order=PlayOrder.LEAD),)),),
        )

    lead = Card(db_uuid="ld", name="Ld", number=1, atk_type=AtkType.STRIKE, play_order=PlayOrder.LEAD)
    # Baseline: both stop.
    assert eng._card_can_stop("A", stopper(20), lead)
    eng.state.players["A"].competitor = replace(
        eng.state.players["A"].competitor,
        effects=(
            fx.Effect(
                trigger=fx.Static(),
                actions=(fx.SuppressStop(19, 21),),
                source=fx.EffectSource.GIMMICK,
            ),
        ),
    )
    assert not eng._card_can_stop("A", stopper(20), lead)  # in range -> disabled
    assert eng._card_can_stop("A", stopper(18), lead)  # outside range -> unaffected


def test_on_shuffle_draws_only_on_an_opponents_effect_shuffle() -> None:
    # Memes Dealer V2 on A: OnShuffle{who=OPP} -> Draw 2. A draws when B's deck is
    # shuffled by an effect, not on A's own shuffle nor the match-start setup shuffle.
    gimmick = fx.Effect(
        trigger=fx.OnShuffle(who=fx.Who.OPP),
        actions=(fx.Draw(n=2),),
        source=fx.EffectSource.GIMMICK,
    )

    def card(u: str) -> Card:
        return Card(db_uuid=u, name=u, number=1, atk_type=AtkType.STRIKE, play_order=PlayOrder.LEAD)

    def fresh() -> Engine:
        eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
        eng.state.players["A"].competitor = replace(
            eng.state.players["A"].competitor, effects=(gimmick,)
        )
        for k in ("A", "B"):
            eng.state.players[k].deck = [card(f"{k}{i}") for i in range(10)]
            eng.state.players[k].hand = []
        return eng

    # B shuffles their own deck via an effect -> A (opponent) draws 2.
    eng = fresh()
    eng._act_shuffle_deck(fx.ShuffleDeck(who=fx.Who.SELF), "B")
    assert len(eng.state.players["A"].hand) == 2
    # A shuffles their OWN deck -> who=OPP does not fire.
    eng = fresh()
    eng._act_shuffle_deck(fx.ShuffleDeck(who=fx.Who.SELF), "A")
    assert len(eng.state.players["A"].hand) == 0
    # The match-start setup shuffle bypasses OnShuffle: only the opening hand is drawn.
    eng = fresh()
    eng.setup()
    from srg_sim.engine import OPENING_HAND

    assert len(eng.state.players["A"].hand) == OPENING_HAND


def test_on_discard_move_fires_once_per_effect_driven_exit() -> None:
    # Brumeister V2 on A: OnDiscardMove{who=OPP} -> RemoveFromPlay{OPP, 1}. A discards
    # one of B's in-play cards whenever an effect pulls cards out of B's discard pile.
    gimmick = fx.Effect(
        trigger=fx.OnDiscardMove(who=fx.Who.OPP),
        actions=(fx.RemoveFromPlay(selector=fx.CardFilter(), who=fx.Who.OPP, count=1),),
        source=fx.EffectSource.GIMMICK,
    )

    def card(u: str) -> Card:
        return Card(db_uuid=u, name=u, number=1, atk_type=AtkType.STRIKE, play_order=PlayOrder.LEAD)

    def fresh() -> Engine:
        eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
        eng.state.players["A"].competitor = replace(
            eng.state.players["A"].competitor, effects=(gimmick,)
        )
        # Stock every zone the discard-exit paths read: a pile to pull from, a board to
        # be punished, and a hand so the hand/discard swap is not a no-op.
        for k in ("A", "B"):
            eng.state.players[k].deck = [card(f"{k}n{i}") for i in range(10)]
            eng.state.players[k].discard = [card(f"{k}d{i}") for i in range(3)]
            eng.state.players[k].in_play = [card(f"{k}p{i}") for i in range(3)]
            eng.state.players[k].hand = [card(f"{k}h{i}") for i in range(3)]
        return eng

    def board(eng: Engine, k: str) -> int:
        return len(eng.state.players[k].in_play)

    # B pulls a card out of their own discard -> A discards one of B's in-play cards.
    eng = fresh()
    eng._act_add_from_discard(fx.AddFromDiscard(filter=fx.CardFilter()), "B")
    assert board(eng, "B") == 2
    assert board(eng, "A") == 3  # A's own board is untouched
    # A pulling from their OWN pile must not fire A's who=OPP trigger.
    eng = fresh()
    eng._act_add_from_discard(fx.AddFromDiscard(filter=fx.CardFilter()), "A")
    assert board(eng, "B") == 3
    # Every other effect-driven exit from B's pile counts as a "move" too.
    eng = fresh()
    eng._act_shuffle_into_deck(fx.ShuffleIntoDeck(selector=fx.CardFilter()), "B")
    assert board(eng, "B") == 2
    eng = fresh()
    eng._act_swap_hand_discard(fx.SwapHandDiscard(), "B")
    assert board(eng, "B") == 2
    # "Moves ANY NUMBER of cards": a 2-card recur is still a single trigger.
    eng = fresh()
    eng._act_recur_to_deck_top(fx.RecurToDeckTop(selector=fx.CardFilter(), count=2), "B")
    assert board(eng, "B") == 2
    # The mechanical pass-and-recycle is not a card effect.
    eng = fresh()
    eng._do_pass("B")
    assert board(eng, "B") == 3


def test_timed_buffs_accumulate_cap_and_sweep() -> None:
    # Snake Pitt (Super Lucha), hand-adjudicated 2026-07-20: each qualifying Power
    # turn roll adds +1 Strike / +5 Submission, "(Max +5 to each)" caps the
    # ACCUMULATED total, the buff survives every turn its owner is not active, and is
    # swept immediately after the roll that next makes them active (so it still feeds
    # that roll).
    from srg_sim.state import TimedBuff  # noqa: F401  (round-trip below)

    clause = "+1 to Strike and +5 to Submission (Max +5 to each)"

    def fresh() -> Engine:
        eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
        eng._clause = clause
        return eng

    def grant(eng: Engine, skill: Skill, delta: int, key: str = "A") -> None:
        eng._act_buff_skill(
            fx.BuffSkill(
                skill=skill,
                delta=delta,
                who=fx.Who.SELF,
                duration=fx.Duration.UNTIL_START_OF_YOUR_NEXT_TURN,
                cap=5,
            ),
            key,
        )

    def total(eng: Engine, skill: Skill, key: str = "A") -> int:
        return sum(b.delta for b in eng.state.players[key].timed_buffs if b.skill is skill)

    # Repeat firings of ONE clause accumulate into a single entry and clamp to cap.
    eng = fresh()
    for expected in range(1, 6):
        grant(eng, Skill.STRIKE, 1)
        grant(eng, Skill.SUBMISSION, 5)
        assert total(eng, Skill.STRIKE) == expected
        assert total(eng, Skill.SUBMISSION) == 5  # caps on the first grant
    grant(eng, Skill.STRIKE, 1)
    assert total(eng, Skill.STRIKE) == 5
    assert len(eng.state.players["A"].timed_buffs) == 2  # one per (clause, skill)

    # It reaches the derived stats (the view feeding turn / Finish / breakout rolls).
    eng = fresh()
    base = eng.state.effective_stats("A")[Skill.SUBMISSION.value]
    grant(eng, Skill.SUBMISSION, 5)
    assert eng.state.effective_stats("A")[Skill.SUBMISSION.value] == base + 5

    # Granted on turn 3's roll -> that turn's own sweep must not take it.
    eng = fresh()
    eng.state.turn_no = 3
    grant(eng, Skill.SUBMISSION, 5)
    eng._sweep_next_turn_buffs("A")
    assert total(eng, Skill.SUBMISSION) == 5
    # Survives every turn its owner is not the active player...
    for turn in (4, 5):
        eng.state.turn_no = turn
        eng._sweep_next_turn_buffs("B")
        assert total(eng, Skill.SUBMISSION) == 5
    # ...and is swept right after the roll that next makes them active.
    eng.state.turn_no = 6
    eng._sweep_next_turn_buffs("A")
    assert total(eng, Skill.SUBMISSION) == 0

    # The two durations have separate sweeps; the roll-time one ignores end-of-turn.
    eng = fresh()
    eng._clause = "until the end of the turn"
    eng._act_buff_skill(
        fx.BuffSkill(
            skill=Skill.STRIKE, delta=2, who=fx.Who.SELF, duration=fx.Duration.UNTIL_END_OF_TURN
        ),
        "A",
    )
    eng.state.turn_no = 9
    eng._sweep_next_turn_buffs("A")
    assert total(eng, Skill.STRIKE) == 2

    # Timed buffs are real state -> they must survive a snapshot round-trip.
    from srg_sim.state import GameState

    restored = GameState.from_dict(eng.state.to_dict())
    assert restored.players["A"].timed_buffs == eng.state.players["A"].timed_buffs


def test_blank_stopped_text_suppresses_if_stopped_and_expires() -> None:
    # The Jurassic / "If Stopped" stop-card family: "when you stop a card, the stopped
    # card has blank text until the end of the turn". The blank must land BEFORE the
    # stopped card's own OnStop, so its "If Stopped" text never triggers.
    if_stopped = fx.Effect(
        trigger=fx.OnStop(dir=fx.Direction.YOURS),
        actions=(fx.Draw(n=2),),
        raw_clause="If Stopped, draw 2 cards.",
    )
    blanker = fx.Effect(
        trigger=fx.OnStop(dir=fx.Direction.THEIRS),
        actions=(fx.BlankStoppedText(),),
        raw_clause="the stopped card has blank text until the end of the turn",
    )

    def attack() -> Card:
        return Card(
            db_uuid="attack", name="If Stopped Grapple", number=5,
            atk_type=AtkType.GRAPPLE, play_order=PlayOrder.LEAD, effects=(if_stopped,),
        )

    def stopper(blanks: bool) -> Card:
        return Card(
            db_uuid="stopper", name="Blocker", number=6, atk_type=AtkType.GRAPPLE,
            play_order=PlayOrder.LEAD, effects=((blanker,) if blanks else ()),
        )

    def run(blanks: bool) -> tuple[Engine, int]:
        eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
        eng.state.players["A"].deck = [
            Card(db_uuid=f"n{i}", name=f"n{i}", number=1, atk_type=AtkType.STRIKE,
                 play_order=PlayOrder.LEAD)
            for i in range(6)
        ]
        eng.state.players["A"].hand = []
        eng._apply_stop("A", "B", attack(), stopper(blanks))
        return eng, len(eng.state.players["A"].hand)

    # Baseline: without the blank, "If Stopped, draw 2" resolves normally.
    _, drew = run(False)
    assert drew == 2
    # With the blank, the stopped card fires nothing.
    eng, drew = run(True)
    assert drew == 0
    assert "attack" in eng.state.blanked_text
    # It lasts the rest of the turn, then the turn-boundary sweep clears it.
    assert eng.state.is_text_blanked(attack(), "A")
    eng._sweep_end_of_turn()
    assert not eng.state.is_text_blanked(attack(), "A")


def test_choose_name_binds_one_option_and_gates_the_hit_effects() -> None:
    # Raven: "Choose 1: 'Kendo Stick', 'Steel Chair', or 'Trash Can'. When you hit a
    # card with THAT in the name, draw 2." The binding is made at match start and one
    # concrete OnHit per option is gated on it, so exactly one is ever live.
    NAMES = ("Kendo Stick", "Steel Chair", "Trash Can")
    effects = [
        fx.Effect(
            trigger=fx.StartOfMatch(),
            actions=(fx.ChooseName(options=NAMES),),
            source=fx.EffectSource.GIMMICK,
        )
    ]
    effects += [
        fx.Effect(
            trigger=fx.OnHit(name_contains=(n,)),
            condition=fx.ChosenNameIs(name=n, who=fx.Who.SELF),
            actions=(fx.Draw(n=2),),
            source=fx.EffectSource.GIMMICK,
        )
        for n in NAMES
    ]

    class PickName(HeuristicPolicy):
        def __init__(self, pick: str) -> None:
            super().__init__()
            self.pick = pick

        def choose(self, point, legal, state, key):  # type: ignore[no-untyped-def]
            if point == "name":
                return next((o for o in legal if o["name"] == self.pick), legal[0])
            return super().choose(point, legal, state, key)

    def fresh(pick: str) -> Engine:
        eng = Engine(*bull_vs_fae(), PickName(pick), HeuristicPolicy(), seed=1, created="x")
        eng.state.players["A"].competitor = replace(
            eng.state.players["A"].competitor, effects=tuple(effects)
        )
        return eng

    def hit(eng: Engine, card_name: str) -> int:
        card = Card(db_uuid="hit", name=card_name, number=1, atk_type=AtkType.STRIKE,
                    play_order=PlayOrder.LEAD)
        before = len(eng.state.players["A"].hand)
        eng._run_hit_gimmicks(card, "A")
        return len(eng.state.players["A"].hand) - before

    # Nothing is live before a binding exists.
    eng = fresh("Steel Chair")
    assert hit(eng, "Folding Steel Chair") == 0
    # After setup the binding is recorded and exactly one OnHit is live.
    eng = fresh("Steel Chair")
    eng.setup()
    assert eng.state.players["A"].chosen_name == "Steel Chair"
    assert hit(eng, "Folding Steel Chair") == 2
    assert hit(eng, "Kendo Stick Shot") == 0
    assert hit(eng, "Trash Can Lid") == 0
    assert hit(eng, "Dropkick") == 0
    # A different choice moves which effect is live.
    eng = fresh("Trash Can")
    eng.setup()
    assert hit(eng, "Trash Can Lid") == 2
    assert hit(eng, "Folding Steel Chair") == 0


def test_on_hit_order_gate_with_capped_self_excluding_per_count() -> None:
    # Sticky "the Salamander" Sailboat: "When you hit a Lead, draw 1 card for each
    # other Lead you have in play (Max 3)." The hit card is already on the board when
    # _run_hit_gimmicks fires, so it must be excluded from its own count.
    gimmick = fx.Effect(
        trigger=fx.OnHit(order=PlayOrder.LEAD),
        actions=(
            fx.Draw(
                n=1,
                per=fx.CardFilter(play_order=PlayOrder.LEAD),
                per_who=fx.Who.SELF,
                cap=3,
                per_excludes_trigger=True,
            ),
        ),
        source=fx.EffectSource.GIMMICK,
    )

    def lead(u: str, order: PlayOrder = PlayOrder.LEAD) -> Card:
        return Card(db_uuid=u, name=u, number=1, atk_type=AtkType.STRIKE, play_order=order)

    def hit_with(board_leads: int, hit: Card) -> int:
        eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
        eng.state.players["A"].competitor = replace(
            eng.state.players["A"].competitor, effects=(gimmick,)
        )
        eng.state.players["A"].deck = [lead(f"n{i}") for i in range(20)]
        eng.state.players["A"].hand = []
        eng.state.players["A"].in_play = [lead(f"b{i}") for i in range(board_leads)] + [hit]
        eng._run_hit_gimmicks(hit, "A")
        return len(eng.state.players["A"].hand)

    # "each OTHER Lead": the triggering card never counts for itself.
    assert hit_with(1, lead("hit")) == 1
    assert hit_with(0, lead("hit")) == 0
    # "(Max 3)" clamps the per-count product.
    assert hit_with(5, lead("hit")) == 3
    assert hit_with(3, lead("hit")) == 3
    assert hit_with(2, lead("hit")) == 2
    # The order gate ignores non-Leads however many Leads are on the board.
    assert hit_with(3, lead("hit", PlayOrder.FOLLOWUP)) == 0


def test_choose_reaches_either_board_and_either_discard_pile() -> None:
    # Cherry Glamazon, hand-adjudicated 2026-07-20: "choose 1 card in play and discard
    # it" reaches EITHER board (not just the opponent's), and "bury 1 card in any
    # player's discard pile" picks a SPECIFIC card from either pile (not the top).
    class PickPrefix(HeuristicPolicy):
        def __init__(self, pref: str) -> None:
            super().__init__()
            self.pref = pref

        def choose(self, point, legal, state, key):  # type: ignore[no-untyped-def]
            hit = [o for o in legal if str(o.get("card", "")).startswith(self.pref)]
            return hit[0] if hit else super().choose(point, legal, state, key)

    def card(u: str) -> Card:
        return Card(db_uuid=u, name=u, number=1, atk_type=AtkType.STRIKE, play_order=PlayOrder.LEAD)

    def fresh(pref: str) -> Engine:
        eng = Engine(*bull_vs_fae(), PickPrefix(pref), PickPrefix(pref), seed=1, created="x")
        for side in ("A", "B"):
            eng.state.players[side].in_play = [card(f"{side}play{i}") for i in range(2)]
            eng.state.players[side].discard = [card(f"{side}disc{i}") for i in range(2)]
            eng.state.players[side].deck = [card(f"{side}n{i}") for i in range(4)]
        return eng

    def boards(eng: Engine) -> tuple[int, int]:
        return len(eng.state.players["A"].in_play), len(eng.state.players["B"].in_play)

    # choose=True reaches the opponent's board even with who=SELF...
    eng = fresh("Bplay")
    eng._act_remove_from_play(fx.RemoveFromPlay(who=fx.Who.SELF, count=1, choose=True), "A")
    assert boards(eng) == (2, 1)
    # ...and your OWN board even with who=OPP (the adjudicated part).
    eng = fresh("Aplay")
    eng._act_remove_from_play(fx.RemoveFromPlay(who=fx.Who.OPP, count=1, choose=True), "A")
    assert boards(eng) == (1, 2)
    # choose=False keeps the original who-directed behaviour.
    eng = fresh("Aplay")
    eng._act_remove_from_play(fx.RemoveFromPlay(who=fx.Who.OPP, count=1, choose=False), "A")
    assert boards(eng) == (2, 1)

    # A chosen bury takes the NAMED card from either pile, to its owner's deck bottom.
    eng = fresh("Bdisc1")
    eng._act_bury(fx.Bury(count=1, who=fx.Who.SELF, source=fx.BuryFrom.DISCARD, choose=True), "A")
    assert all(c.db_uuid != "Bdisc1" for c in eng.state.players["B"].discard)
    assert eng.state.players["B"].deck[-1].db_uuid == "Bdisc1"
    assert len(eng.state.players["A"].discard) == 2
    # A discard pile has no meaningful order, so the bury is ALWAYS a choice;
    # choose=False only narrows the pool to the who-side's own pile.
    eng = fresh("Bdisc1")
    eng._act_bury(fx.Bury(count=1, who=fx.Who.SELF, source=fx.BuryFrom.DISCARD), "A")
    assert len(eng.state.players["B"].discard) == 2  # B untouched
    assert len(eng.state.players["A"].discard) == 1
    # ...and the ACTOR picks any card in it, not the top one.
    eng = fresh("Adisc1")
    eng._act_bury(fx.Bury(count=1, who=fx.Who.SELF, source=fx.BuryFrom.DISCARD), "A")
    assert eng.state.players["A"].deck[-1].db_uuid == "Adisc1"
    assert any(c.db_uuid == "Adisc0" for c in eng.state.players["A"].discard)


def test_poison_added_text_is_one_shot_and_outlives_its_source() -> None:
    # The Madness trio (srgpc "poison"): "Your opponent's next Grapple has the added
    # text: 'If stopped, you lose the match via disqualification.'" Queued on the
    # TARGET, so it "stays active until fulfilled even if removed from the board".
    injected = fx.Effect(
        trigger=fx.OnStop(dir=fx.Direction.YOURS),
        actions=(fx.LoseBy(kind=fx.LoseKind.DISQUALIFICATION, who=fx.Who.SELF),),
        raw_clause="If stopped, you lose via DQ.",
    )
    action = fx.AddTextToNext(
        who=fx.Who.OPP, selector=fx.CardFilter(atk_type=AtkType.GRAPPLE), effects=(injected,)
    )

    def card(u: str, atk: AtkType) -> Card:
        return Card(db_uuid=u, name=u, number=1, atk_type=atk, play_order=PlayOrder.LEAD)

    def fresh() -> Engine:
        eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
        eng._act_add_text_to_next(action, "A")
        return eng

    # Queued on the OPPONENT, not the caster.
    eng = fresh()
    assert len(eng.state.players["B"].pending_text) == 1
    assert len(eng.state.players["A"].pending_text) == 0

    # Only a matching card consumes it.
    eng = fresh()
    strike = eng._apply_pending_text("B", card("s", AtkType.STRIKE))
    assert strike.effects == ()
    assert len(eng.state.players["B"].pending_text) == 1
    grapple = eng._apply_pending_text("B", card("g", AtkType.GRAPPLE))
    assert len(grapple.effects) == 1
    assert len(eng.state.players["B"].pending_text) == 0

    # One-shot: the SECOND Grapple gets nothing.
    eng = fresh()
    first = eng._apply_pending_text("B", card("g1", AtkType.GRAPPLE))
    second = eng._apply_pending_text("B", card("g2", AtkType.GRAPPLE))
    assert len(first.effects) == 1 and second.effects == ()

    # It outlives its source leaving the board (both boards cleared).
    eng = fresh()
    for side in ("A", "B"):
        eng.state.players[side].in_play = []
        eng.state.players[side].discard = []
    assert len(eng._apply_pending_text("B", card("g", AtkType.GRAPPLE)).effects) == 1

    # End to end: B plays the poisoned Grapple, A stops it, B loses via DQ.
    eng = fresh()
    poisoned = eng._apply_pending_text("B", card("g", AtkType.GRAPPLE))
    eng._apply_stop("B", "A", poisoned, card("st", AtkType.GRAPPLE))
    eng._resolve_pending()
    assert eng.result is not None
    assert eng.result.winner == "A"
    assert "disqualification" in eng.result.reason.lower()


def test_blank_until_their_next_turn_waits_for_the_targets_turn() -> None:
    # Stiff Right Hand (poison): "Your opponent's gimmick is blank until their next
    # turn." Stored on the TARGET, so it outlives the card going to the discard, and
    # "their next turn" is the next turn the TARGET is active — the caster may win
    # any number of turns in between.
    def blanked_on_turn_3() -> Engine:
        eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
        eng.state.turn_no = 3
        eng._act_blank_gimmick(
            fx.BlankGimmick(who=fx.Who.OPP, duration=fx.Duration.UNTIL_START_OF_YOUR_NEXT_TURN),
            "A",
        )
        return eng

    eng = blanked_on_turn_3()
    assert eng.state.is_gimmick_blanked("B")
    assert not eng.state.is_gimmick_blanked("A")

    # The caster winning turns in between does not clear it.
    for turn in range(4, 9):
        eng.state.turn_no = turn
        eng._sweep_next_turn_buffs("A")
        assert eng.state.is_gimmick_blanked("B"), f"still blanked on turn {turn}"
    # B finally wins a turn roll: the blank ends at the start of THEIR turn.
    eng.state.turn_no = 9
    eng._sweep_next_turn_buffs("B")
    assert not eng.state.is_gimmick_blanked("B")

    # The granting turn's own roll must not clear it.
    eng = blanked_on_turn_3()
    eng._sweep_next_turn_buffs("B")
    assert eng.state.is_gimmick_blanked("B")

    # An untimed (one-shot) blank has no marker and survives the sweep.
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng._act_blank_gimmick(fx.BlankGimmick(who=fx.Who.OPP, duration=fx.Duration.INSTANT), "A")
    assert eng.state.players["B"].blank_until_next_turn is None
    eng.state.turn_no = 7
    eng._sweep_next_turn_buffs("B")
    assert eng.state.is_gimmick_blanked("B")


def test_reveal_for_draw_rolled_skill_draws_on_matching_move_type() -> None:
    # The Winning Ticket: reveal 1 from the opponent's hand; if its move type matches
    # the skill you just rolled, draw 1. The rolled skill comes from the roll context.
    from srg_sim.conditions import RollContext

    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    eng._roll_ctx["A"] = RollContext(skill=Skill.STRIKE, gap=0, value=10)
    strike = Card(db_uuid="s", name="S", number=1, atk_type=AtkType.STRIKE, play_order=PlayOrder.LEAD)
    grapple = Card(db_uuid="g", name="G", number=2, atk_type=AtkType.GRAPPLE, play_order=PlayOrder.LEAD)
    action = fx.RevealForDraw(who=fx.Who.OPP, count=1, draw=1, match_on=fx.RevealMatch.ROLLED_SKILL)
    # Rolled Strike, revealed a Strike -> draw 1; the reveal does not remove the card.
    eng.state.players["B"].hand = [strike]
    before = len(eng.state.players["A"].hand)
    eng._act_reveal_for_draw(action, "A")
    assert len(eng.state.players["A"].hand) == before + 1
    assert strike in eng.state.players["B"].hand
    # Rolled Strike, revealed a Grapple -> no draw (move type differs).
    eng.state.players["B"].hand = [grapple]
    a2 = len(eng.state.players["A"].hand)
    eng._act_reveal_for_draw(action, "A")
    assert len(eng.state.players["A"].hand) == a2


def test_return_to_hand_bounces_to_the_owners_hand() -> None:
    # Fox Assassin V2: "add 1 card any player has in play to their hand" — a bounced
    # card returns to its OWNER's hand, from either board when choose=True.
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    ca = Card(db_uuid="ca", name="A card", number=1, atk_type=AtkType.STRIKE, play_order=PlayOrder.LEAD)
    cb = Card(db_uuid="cb", name="B card", number=2, atk_type=AtkType.GRAPPLE, play_order=PlayOrder.LEAD)
    eng.state.players["A"].in_play = [ca]
    eng.state.players["B"].in_play = [cb]
    eng.state.players["A"].hand = []
    eng.state.players["B"].hand = []
    # choose=True ranges over both boards; the default policy takes legal[0] = A's card.
    eng._act_return_to_hand(fx.ReturnToHand(who=fx.Who.SELF, count=1, choose=True), "A")
    assert ca not in eng.state.players["A"].in_play
    assert ca in eng.state.players["A"].hand
    # who=OPP bounces the opponent's card back to the OPPONENT's hand (disruption).
    eng._act_return_to_hand(fx.ReturnToHand(who=fx.Who.OPP, count=1, choose=False), "A")
    assert cb not in eng.state.players["B"].in_play
    assert cb in eng.state.players["B"].hand


def test_peek_action_reveals_opponent_hand_in_the_actors_observable_view() -> None:
    # Peek ("Look at your opponent's hand") moves no card but grants A a look at B's
    # hand for the rest of the turn — surfaced through observable(), not to_dict.
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    b_hand = [c.db_uuid for c in eng.state.players["B"].hand]
    assert "hand" not in eng.state.observable("A")["players"]["B"]  # redacted before
    eng._act_peek(fx.Peek(who=fx.Who.OPP), "A")
    revealed = eng.state.observable("A")["players"]["B"]
    assert [c["db_uuid"] for c in revealed["hand"]] == b_hand  # A now sees B's hand
    assert "hand" not in eng.state.observable("B")["players"]["A"]  # B still can't see A


def test_shuffle_deck_action_reorders_without_losing_cards() -> None:
    # ShuffleDeck permutes the deck in place (#27 "Shuffle your deck").
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=4, created="x")
    eng.setup()
    before = list(eng.state.players["A"].deck)
    eng._act_shuffle_deck(fx.ShuffleDeck(), "A")
    after = eng.state.players["A"].deck
    assert sorted(c.db_uuid for c in after) == sorted(c.db_uuid for c in before)  # same multiset
    assert len(after) == len(before)


def test_on_roll_skill_trigger_fires_its_actions_only_on_that_skill() -> None:
    # "When you roll Agility for your turn roll, draw 1 and your opponent's next turn
    # roll is -1" (Adrianna Dee, #56) = OnRoll(skill=Agility) -> [Draw, ModifyRoll(OPP)].
    gimmick = fx.Effect(
        trigger=fx.OnRoll(skill=Skill.AGILITY),
        actions=(fx.Draw(n=1), fx.ModifyRoll(who=fx.Who.OPP, delta=-1, when=fx.RollWhen.NEXT)),
        source=fx.EffectSource.GIMMICK,
        raw_clause="roll Agility -> draw 1 and opp next roll -1",
    )

    def _roll(a_skill: Skill) -> Engine:
        eng = Engine(
            make_deck("A", with_effects(vanilla(), (gimmick,))),
            make_deck("B", vanilla()),
            HeuristicPolicy(),
            HeuristicPolicy(),
            seed=1,
        )
        eng.setup()
        eng.state.turn_no = 1
        eng._roll_for = lambda key, use_pending: (a_skill, 9) if key == "A" else (Skill.POWER, 5)  # type: ignore[method-assign]
        return eng

    on = _roll(Skill.AGILITY)
    before = len(on.state.players["A"].hand)
    on._turn_roll()
    assert len(on.state.players["A"].hand) == before + 1  # drew on the Agility roll
    assert on.state.players["B"].pending_roll_mods["next"] == -1  # opponent's next roll -1

    off = _roll(Skill.POWER)  # a non-Agility roll fires nothing
    off_before = len(off.state.players["A"].hand)
    off._turn_roll()
    assert len(off.state.players["A"].hand) == off_before
    assert off.state.players["B"].pending_roll_mods["next"] == 0


def test_choice_action_resolves_exactly_one_branch() -> None:
    # "A or B" effect (Little Guido, #55): Choice resolves ONE branch, chosen by the
    # acting player. The default policy takes legal[0] — here the draw branch — and
    # the other branch (opponent roll debuff) must NOT apply.
    choice = fx.Choice(
        options=(
            fx.ChoiceOption("draw 2", (fx.Draw(n=2),)),
            fx.ChoiceOption(
                "opp -2 next", (fx.ModifyRoll(who=fx.Who.OPP, delta=-2, when=fx.RollWhen.NEXT),)
            ),
        )
    )
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1)
    eng.setup()
    eng.state.turn_no = 1
    before = len(eng.state.players["A"].hand)
    eng._act_choice(choice, "A")
    assert len(eng.state.players["A"].hand) == before + 2  # legal[0] = the draw branch
    assert eng.state.players["B"].pending_roll_mods["next"] == 0  # the other branch was skipped


def test_discard_action_honors_its_type_selector() -> None:
    # Discard(selector=atk_type=X) removes only a matching card (Soborno's cost, #54).
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1)
    eng.setup()
    strike = Card("s1", "Strike", 1, AtkType.STRIKE, PlayOrder.LEAD)
    grapple = Card("g1", "Grapple", 2, AtkType.GRAPPLE, PlayOrder.LEAD)
    eng.state.players["A"].hand = [grapple, strike]
    eng.state.players["A"].discard = []
    eng._act_discard(
        fx.Discard(count=1, who=fx.Who.SELF, selector=fx.CardFilter(atk_type=AtkType.STRIKE)), "A"
    )
    assert [c.name for c in eng.state.players["A"].discard] == ["Strike"]  # only the Strike type
    assert eng.state.players["A"].hand == [grapple]


def test_in_roll_boost_pays_only_when_payable_and_discards_the_typed_cost() -> None:
    # Soborno (#54): "roll Strike -> you MAY discard a Strike card and this roll is +1."
    # OnRollBoost fires in the roll-off (can flip the outcome); HasInHand gates payability.
    boost = fx.Effect(
        trigger=fx.OnRollBoost(skill=Skill.STRIKE, delta=1),
        condition=fx.HasInHand(fx.Who.SELF, fx.CardFilter(atk_type=AtkType.STRIKE)),
        actions=(
            fx.Discard(count=1, who=fx.Who.SELF, selector=fx.CardFilter(atk_type=AtkType.STRIKE)),
        ),
        optional=True,
        source=fx.EffectSource.GIMMICK,
        raw_clause="roll Strike -> discard a Strike for +1",
    )
    strike = Card("s1", "Strike", 1, AtkType.STRIKE, PlayOrder.LEAD)
    grapple = Card("g1", "Grapple", 2, AtkType.GRAPPLE, PlayOrder.LEAD)

    def eng_with(hand: list[Card]) -> Engine:
        eng = Engine(
            make_deck("A", with_effects(vanilla(), (boost,))),
            make_deck("B", vanilla()),
            HeuristicPolicy(),
            HeuristicPolicy(),
            seed=1,
        )
        eng.setup()
        eng.state.turn_no = 1
        eng.state.players["A"].hand = list(hand)
        eng.state.players["A"].discard = []
        return eng

    # Payable: rolled Strike 6, holds a Strike -> +1 (a winning 7) and the Strike is the cost.
    payable = eng_with([strike, grapple])
    assert payable._offer_roll_boost("A", Skill.STRIKE, 6) == 7
    assert [c.name for c in payable.state.players["A"].discard] == ["Strike"]  # not the Grapple
    # Cost only if payable: rolled Strike but holds no Strike -> no boost, no discard.
    broke = eng_with([grapple])
    assert broke._offer_roll_boost("A", Skill.STRIKE, 6) == 6
    assert broke.state.players["A"].discard == []


def _rey_bump_boost() -> fx.Effect:
    # Rey Zerblade (#58): "Once per turn roll: when you would bump, you may discard 1
    # Lead you have in play to add +1 to your turn roll instead." An OnRollBoost with
    # on_bump=True — offered on a tie, cost is an in-play Lead (RemoveFromPlay).
    return fx.Effect(
        trigger=fx.OnRollBoost(skill=None, delta=1, on_bump=True),
        condition=fx.HasInPlay(fx.Who.SELF, fx.CardFilter(play_order=PlayOrder.LEAD)),
        actions=(fx.RemoveFromPlay(fx.CardFilter(play_order=PlayOrder.LEAD), fx.Who.SELF, 1),),
        frequency=fx.FrequencyGuard(kind=fx.Frequency.ONCE_PER_TURN),
        optional=True,
        source=fx.EffectSource.GIMMICK,
        raw_clause="would bump -> discard a Lead for +1",
    )


def test_would_bump_boost_offered_only_on_a_bump_and_pays_an_in_play_lead() -> None:
    lead = Card("l1", "Lead", 1, AtkType.STRIKE, PlayOrder.LEAD)

    def eng_with(board: list[Card]) -> Engine:
        eng = Engine(
            make_deck("A", with_effects(vanilla(), (_rey_bump_boost(),))),
            make_deck("B", vanilla()),
            HeuristicPolicy(),
            HeuristicPolicy(),
            seed=1,
        )
        eng.setup()
        eng.state.turn_no = 1
        eng.state.players["A"].in_play = list(board)
        eng.state.players["A"].discard = []
        return eng

    # NOT offered on the initial roll (on_bump=False path), even holding a Lead in play.
    initial = eng_with([lead])
    assert initial._offer_roll_boost("A", Skill.POWER, 6) == 6
    assert lead in initial.state.players["A"].in_play  # nothing paid

    # Offered on a would-bump: +1 and the in-play Lead is the cost (Lead -> discard).
    bump = eng_with([lead])
    assert bump._offer_roll_boost("A", Skill.POWER, 6, on_bump=True) == 7
    assert lead in bump.state.players["A"].discard and lead not in bump.state.players["A"].in_play

    # Cost only if payable: no Lead in play -> no boost, no discard.
    broke = eng_with([])
    assert broke._offer_roll_boost("A", Skill.POWER, 6, on_bump=True) == 6
    assert broke.state.players["A"].discard == []


def test_would_bump_boost_breaks_the_tie_without_bumping(monkeypatch: pytest.MonkeyPatch) -> None:
    # On a tied roll, Rey pays the Lead to +1 *instead* of bumping: he wins the roll
    # and neither player draws (no bump). Contrast test_bump_makes_both_players_draw.
    eng = Engine(
        make_deck("A", with_effects(vanilla(), (_rey_bump_boost(),))),
        make_deck("B", vanilla()),
        HeuristicPolicy(),
        HeuristicPolicy(),
        seed=1,
    )
    eng.setup()
    eng.state.turn_no = 1
    lead = Card("l1", "Lead", 1, AtkType.STRIKE, PlayOrder.LEAD)
    eng.state.players["A"].in_play = [lead]
    eng.state.players["A"].discard = []
    a0, b0 = len(eng.state.players["A"].deck), len(eng.state.players["B"].deck)
    # Both roll Power 5 (a tie) — the boost breaks it, so _roll_for is called only twice.
    rolls = iter([(Skill.POWER, 5), (Skill.POWER, 5)])
    monkeypatch.setattr(eng, "_roll_for", lambda key, use_pending: next(rolls))
    assert eng._roll_off() == "A"
    assert len(eng.state.players["A"].deck) == a0  # no bump: nobody drew
    assert len(eng.state.players["B"].deck) == b0
    assert lead in eng.state.players["A"].discard  # the Lead paid for the +1


def test_cassandra_flips_the_signs_on_the_opponents_gimmick_only() -> None:
    # Cassandra (#61): a Static FlipGimmickSigns(OPP) negates every printed +/- on the
    # opponent's gimmick. Here A's "+4 to your next roll" comeback flips to -4 while
    # A holds Cassandra's opponent; Cassandra's own gimmick is untouched.
    comeback = fx.Effect(
        trigger=fx.OnRoll(),
        actions=(fx.ModifyRoll(who=fx.Who.SELF, delta=4, when=fx.RollWhen.NEXT),),
        source=fx.EffectSource.GIMMICK,
        raw_clause="your next roll is +4",
    )
    flip = fx.Effect(
        trigger=fx.Static(),
        actions=(fx.FlipGimmickSigns(fx.Who.OPP),),
        source=fx.EffectSource.GIMMICK,
        raw_clause="flip opponent gimmick signs",
    )
    eng = Engine(
        make_deck("A", with_effects(vanilla(), (comeback,))),
        make_deck("B", with_effects(vanilla(), (flip,))),  # B = Cassandra
        HeuristicPolicy(),
        HeuristicPolicy(),
        seed=1,
    )
    eng.setup()
    (a_eff,) = [e for e in eng._standing_effects("A") if isinstance(e.trigger, fx.OnRoll)]
    assert a_eff.actions[0].delta == -4  # +4 comeback negated by Cassandra
    # Cassandra's own gimmick is not self-flipped, and blanking her disables the flip.
    (b_eff,) = eng._standing_effects("B")
    assert isinstance(b_eff.actions[0], fx.FlipGimmickSigns)
    eng.state.players["B"].gimmick_blanked = True
    (a_unflipped,) = [e for e in eng._standing_effects("A") if isinstance(e.trigger, fx.OnRoll)]
    assert a_unflipped.actions[0].delta == 4  # flip gone -> original sign restored


def test_mrs_apocalypse_blanks_the_opponent_gimmick_only_on_a_low_roll() -> None:
    # Mrs. Apocalypse (#59) clause 1: OnRoll(who=OPP) + RollValue(<=7) -> BlankGimmick(OPP)
    # gates on the opponent's ACTUAL rolled value this turn (the new RollValue condition).
    from srg_sim.conditions import RollContext

    blank_low = fx.Effect(
        trigger=fx.OnRoll(who=fx.Who.OPP),
        condition=fx.RollValue(fx.Comparator.LE, 7),
        actions=(fx.BlankGimmick(fx.Who.OPP),),
        source=fx.EffectSource.GIMMICK,
        raw_clause="opp roll <=7 -> blank their gimmick",
    )
    victim = fx.Effect(  # B's own gimmick, the thing that gets blanked
        trigger=fx.OnRoll(),
        actions=(fx.ModifyRoll(who=fx.Who.SELF, delta=1, when=fx.RollWhen.NEXT),),
        source=fx.EffectSource.GIMMICK,
        raw_clause="B gimmick",
    )

    def run(opp_value: int) -> bool:
        eng = Engine(
            make_deck("A", with_effects(vanilla(), (blank_low,))),  # A = Mrs. Apocalypse
            make_deck("B", with_effects(vanilla(), (victim,))),
            HeuristicPolicy(),
            HeuristicPolicy(),
            seed=1,
        )
        eng.setup()
        eng.state.turn_no = 1
        eng._roll_ctx = {
            "A": RollContext(skill=Skill.POWER, gap=opp_value - 8, value=8),
            "B": RollContext(skill=Skill.POWER, gap=8 - opp_value, value=opp_value),
        }
        eng._run_on_roll("A")  # Mrs. Apocalypse reacts to B's roll
        return eng.state.is_gimmick_blanked("B")

    assert run(6) is True  # opp rolled 6 (<=7) -> gimmick blanked
    assert run(7) is True  # boundary: 7 is "7 or less"
    assert run(8) is False  # opp rolled 8 -> not blanked (LE 7 fails)


def test_copy_kat_transforms_on_breakout_and_swaps_its_two_sides() -> None:
    # Copy Kat V2 (#60): a one-way transform. FRONT debuffs the opponent's highest
    # skill -1; a breakout fires OnBreakout -> FlipGimmick(SELF); BACK then buffs Copy
    # Kat's Grapple by the Crowd Meter (capped +5) and the front debuff switches off.
    front_debuff = fx.Effect(
        trigger=fx.Static(),
        condition=fx.Not(fx.GimmickFlipped(fx.Who.OPP)),  # OPP names Copy Kat (fold view)
        actions=(fx.BuffSkill(Skill.POWER, -1, fx.Who.OPP, target_highest=True),),
        source=fx.EffectSource.GIMMICK,
        raw_clause="front: opp highest -1",
    )
    front_flip = fx.Effect(
        trigger=fx.OnBreakout(),
        condition=fx.Not(fx.GimmickFlipped(fx.Who.SELF)),
        actions=(fx.FlipGimmick(fx.Who.SELF),),
        source=fx.EffectSource.GIMMICK,
        raw_clause="front: after a breakout, turn over",
    )
    back_buff = fx.Effect(
        trigger=fx.Static(),
        condition=fx.GimmickFlipped(fx.Who.SELF),
        actions=(fx.BuffSkill(Skill.GRAPPLE, 0, fx.Who.SELF, per_crowd=True, cap=5),),
        source=fx.EffectSource.GIMMICK,
        raw_clause="back: grapple + crowd meter (max 5)",
    )
    eng = Engine(
        make_deck("A", with_effects(vanilla(), (front_debuff, front_flip, back_buff))),
        make_deck("B", vanilla()),
        HeuristicPolicy(),
        HeuristicPolicy(),
        seed=1,
    )
    eng.setup()
    h_b, h_a = eng._holds("B"), eng._holds("A")
    base_b = eng.state.players["B"].competitor.stats.to_dict()
    highest = max(base_b, key=lambda k: base_b[k])

    # FRONT: opponent's highest skill is -1; Copy Kat's Grapple is not yet buffed.
    assert eng.state.effective_stats("B", h_b)[highest] == base_b[highest] - 1
    gr_front = eng.state.effective_stat("A", Skill.GRAPPLE, h_a)
    assert not eng.state.players["A"].gimmick_flipped

    # A breakout turns the card over (OnBreakout fires for both sides; CM +1 -> 4).
    eng.state.crowd_meter = 3
    eng._on_broken_out("A")
    assert eng.state.players["A"].gimmick_flipped and eng.state.crowd_meter == 4

    # BACK: front debuff is gone; Grapple gains min(CrowdMeter, 5).
    assert eng.state.effective_stats("B", h_b)[highest] == base_b[highest]  # debuff off
    assert eng.state.effective_stat("A", Skill.GRAPPLE, h_a) == gr_front + 4
    eng.state.crowd_meter = 10
    assert eng.state.effective_stat("A", Skill.GRAPPLE, h_a) == gr_front + 5  # capped at +5

    # The flip is one-way: a second breakout does not turn it back to the front.
    eng._on_broken_out("A")
    assert eng.state.players["A"].gimmick_flipped


def test_in_roll_either_debuff_applies_once_and_is_capped() -> None:
    # Tomato Tomato Jr.: InRoll(Power, either) -> ModifyRoll(OPP, -1, THIS). The
    # opponent's CURRENT roll drops by 1 when either side rolls Power, capped at -1.
    gimmick = fx.Effect(
        trigger=fx.InRoll(skill=Skill.POWER, either=True),
        actions=(fx.ModifyRoll(fx.Who.OPP, -1, fx.RollWhen.THIS),),
        source=fx.EffectSource.GIMMICK,
        raw_clause="either rolls Power -> opp roll -1 (capped)",
    )
    eng = Engine(
        make_deck("A", with_effects(vanilla(), (gimmick,))),  # A = Tomato
        make_deck("B", vanilla()),
        HeuristicPolicy(),
        HeuristicPolicy(),
        seed=1,
    )
    eng.setup()
    assert eng._apply_in_roll_mods(Skill.POWER, 5, Skill.STRIKE, 5) == (5, 4)  # A rolls Power
    assert eng._apply_in_roll_mods(Skill.STRIKE, 5, Skill.POWER, 5) == (5, 4)  # B rolls Power
    assert eng._apply_in_roll_mods(Skill.POWER, 5, Skill.POWER, 5) == (5, 4)  # both -> capped -1
    assert eng._apply_in_roll_mods(Skill.STRIKE, 6, Skill.GRAPPLE, 5) == (6, 5)  # no Power -> none


def test_hit_a_type_gimmick_fires_only_for_that_attack_type() -> None:
    # D1 (#57): "When you hit a Submission draw 1 card" = a gimmick OnHit(atk_type=
    # Submission) -> Draw, fired by _run_hit_gimmicks when the owner hits that type.
    gimmick = fx.Effect(
        trigger=fx.OnHit(atk_type=AtkType.SUBMISSION),
        actions=(fx.Draw(n=1),),
        source=fx.EffectSource.GIMMICK,
        raw_clause="hit Submission -> draw 1",
    )
    eng = Engine(
        make_deck("A", with_effects(vanilla(), (gimmick,))),
        make_deck("B", vanilla()),
        HeuristicPolicy(),
        HeuristicPolicy(),
        seed=1,
    )
    eng.setup()
    eng.state.turn_no = 1
    sub = Card("u1", "Sub", 30, AtkType.SUBMISSION, PlayOrder.FINISH)
    strike = Card("u2", "Str", 28, AtkType.STRIKE, PlayOrder.FINISH)
    before = len(eng.state.players["A"].hand)
    eng._run_hit_gimmicks(sub, "A")
    assert len(eng.state.players["A"].hand) == before + 1  # drew on the Submission hit
    mid = len(eng.state.players["A"].hand)
    eng._run_hit_gimmicks(strike, "A")
    assert len(eng.state.players["A"].hand) == mid  # a Strike hit does nothing


def test_on_bump_trigger_penalizes_the_opponents_next_roll() -> None:
    # Mastermind's gimmick: OnBump -> the opponent's NEXT turn roll is -2.
    gimmick = fx.Effect(
        trigger=fx.OnBump(),
        actions=(fx.ModifyRoll(who=fx.Who.OPP, delta=-2, when=fx.RollWhen.NEXT),),
        frequency=fx.FrequencyGuard(kind=fx.Frequency.ONCE_PER_TURN),
        raw_clause="bump -> opp next roll -2",
        source=fx.EffectSource.GIMMICK,
    )
    eng = Engine(
        make_deck("A", with_effects(bull(), (gimmick,))),
        make_deck("B", fae()),
        HeuristicPolicy(),
        HeuristicPolicy(),
        seed=1,
        created="x",
    )
    eng.setup()
    eng._run_on_bump()
    assert eng.state.players["B"].pending_roll_mods["next"] == -2  # opponent penalized
    assert eng.state.players["A"].pending_roll_mods["next"] == 0  # self untouched


def test_on_bump_gimmick_fires_only_once_per_turn() -> None:
    # A once-per-turn guard means repeated bumps in one turn punish only once.
    gimmick = fx.Effect(
        trigger=fx.OnBump(),
        actions=(fx.ModifyRoll(who=fx.Who.OPP, delta=-2, when=fx.RollWhen.NEXT),),
        frequency=fx.FrequencyGuard(kind=fx.Frequency.ONCE_PER_TURN),
        raw_clause="bump -> opp next roll -2",
        source=fx.EffectSource.GIMMICK,
    )
    eng = Engine(
        make_deck("A", with_effects(bull(), (gimmick,))),
        make_deck("B", fae()),
        HeuristicPolicy(),
        HeuristicPolicy(),
        seed=1,
        created="x",
    )
    eng.setup()
    eng._run_on_bump()
    eng._run_on_bump()  # a second bump the same turn
    assert eng.state.players["B"].pending_roll_mods["next"] == -2  # still only -2


def test_pending_next_roll_mod_lands_on_the_immediately_following_roll() -> None:
    # #50: a `when=NEXT` roll mod is queued during a turn's action / OnRoll phase,
    # i.e. AFTER that turn's roll-off ran. It must apply to the very NEXT roll-off,
    # not the turn after (the old promote-right-after-the-roll ordering delayed it a
    # full turn, e.g. Enjoy Everything's +2 played on T9 landed on T11 not T10).
    from srg_sim import gamelog as gl

    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    # Queue "+3 to your next roll" the way an OnHit/OnRoll effect would, post roll-off.
    eng._act_modify_roll(fx.ModifyRoll(who=fx.Who.SELF, delta=3, when=fx.RollWhen.NEXT), "A")
    assert eng.state.players["A"].pending_roll_mods == {"this": 0, "next": 3}
    eng.state.turn_no = 1
    eng._roll_off()  # the immediately following roll-off
    eng.state.turn_no = 2
    eng._roll_off()  # the one after that
    a_rolls = [e for e in eng.state.log.events if isinstance(e, gl.Roll) and e.player == "A"]
    t1 = [e for e in a_rolls if e.t == 1]
    t2 = [e for e in a_rolls if e.t == 2]
    assert t1[0].value - t1[0].base == 3  # the +3 lands on the next roll (was 0 pre-#50)
    assert all(e.value - e.base == 0 for e in t2)  # applied exactly once, gone the turn after


def test_blank_gimmick_suppresses_opponent_competitor_gimmick_and_clears() -> None:
    # #47: a WHILE_IN_PLAY BlankGimmick on an in-play card drops the OPPONENT's
    # competitor gimmick out of their standing effects (derived, so it clears when
    # the blanking card leaves play on breakout) — the Savor-the-Moment counter.
    gim = fx.Effect(
        trigger=fx.OnRoll(),
        actions=(fx.Draw(n=2),),
        raw_clause="draw 2 on roll",
        source=fx.EffectSource.GIMMICK,
    )
    da = make_deck("A", with_effects(bull(), (gim,)))
    eng = Engine(
        da, make_deck("B", fae()), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x"
    )
    eng.setup()
    assert gim in eng._standing_effects("A") and not eng.state.is_gimmick_blanked("A")

    blanker = replace(
        _attack(AtkType.STRIKE, PlayOrder.LEAD),
        db_uuid="savor",
        effects=(fx.Effect(trigger=fx.Static(), actions=(fx.BlankGimmick(who=fx.Who.OPP),)),),
    )
    eng.state.players["B"].in_play.append(blanker)  # B blanks A (its opponent)
    assert eng.state.is_gimmick_blanked("A")
    assert gim not in eng._standing_effects("A")  # gimmick suppressed while blanked

    eng.state.players["B"].in_play.remove(blanker)  # source leaves play
    assert not eng.state.is_gimmick_blanked("A")  # blank clears


def test_conditional_blank_gimmick_honors_its_condition() -> None:
    # Savor the Moment: the opponent's gimmick is blank ONLY while "Enjoy Everything"
    # is in play — is_gimmick_blanked must evaluate the effect's condition, not just
    # its presence. Clears when the enabling card leaves play.
    enjoy = Card(
        db_uuid="enjoy",
        name="Enjoy Everything",
        number=10,
        atk_type=AtkType.STRIKE,
        play_order=PlayOrder.LEAD,
    )
    savor = Card(
        db_uuid="savor",
        name="Savor",
        number=16,
        atk_type=AtkType.STRIKE,
        play_order=PlayOrder.FOLLOWUP,
        effects=(
            fx.Effect(
                trigger=fx.Static(),
                condition=fx.HasInPlay(
                    who=fx.Who.SELF, filter=fx.CardFilter(name="Enjoy Everything")
                ),
                actions=(fx.BlankGimmick(who=fx.Who.OPP),),
            ),
        ),
    )
    eng = _fresh()
    a = eng.state.players["A"]
    a.in_play.append(savor)
    assert not eng.state.is_gimmick_blanked("B")  # Enjoy Everything not in play yet
    a.in_play.append(enjoy)
    assert eng.state.is_gimmick_blanked("B")  # condition now holds -> blanked
    a.in_play.remove(enjoy)
    assert not eng.state.is_gimmick_blanked("B")  # condition no longer holds


def test_modify_roll_per_count_scales_with_matching_cards() -> None:
    # Enjoy Everything: next turn roll +1 for EACH Lead the opponent has in play.
    eng = _fresh()
    eng.state.players["B"].in_play = [
        _attack(AtkType.STRIKE, PlayOrder.LEAD),
        _attack(AtkType.GRAPPLE, PlayOrder.LEAD),
        _attack(AtkType.STRIKE, PlayOrder.FOLLOWUP),  # not a Lead -> not counted
    ]
    eng._act_modify_roll(
        fx.ModifyRoll(
            who=fx.Who.SELF,
            delta=1,
            when=fx.RollWhen.NEXT,
            per=fx.CardFilter(play_order=PlayOrder.LEAD),
            per_who=fx.Who.OPP,
        ),
        "A",
    )
    assert eng.state.players["A"].pending_roll_mods["next"] == 2  # two opponent Leads -> +2


def test_blank_gimmick_action_latches_the_stored_flag() -> None:
    # A one-shot/executed BlankGimmick (not the Static/derived path) latches the flag.
    eng = _fresh()
    eng._act_blank_gimmick(fx.BlankGimmick(who=fx.Who.OPP), "A")  # A blanks B
    assert eng.state.players["B"].gimmick_blanked and eng.state.is_gimmick_blanked("B")


def test_search_tutors_a_matching_card_from_deck_to_hand() -> None:
    # Search pulls the first deck card matching the filter into hand and shuffles.
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    a = eng.state.players["A"]
    want = next(c for c in a.deck if c.play_order is PlayOrder.FINISH)
    hand_before = len(a.hand)
    eng._act_search(fx.Search(filter=fx.CardFilter(play_order=PlayOrder.FINISH)), "A")
    assert want in a.hand and want not in a.deck
    assert len(a.hand) == hand_before + 1


def test_search_with_no_match_only_shuffles() -> None:
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    a = eng.state.players["A"]
    a.deck = [c for c in a.deck if c.number != 99]  # (no card #99 exists)
    hand_before, deck_len = len(a.hand), len(a.deck)
    eng._act_search(fx.Search(filter=fx.CardFilter(number=99)), "A")
    assert len(a.hand) == hand_before and len(a.deck) == deck_len  # nothing tutored


def test_search_to_discard_bins_owner_chosen_cards_and_logs_them_public() -> None:
    # "Search your deck for up to N cards and put them into your discard" (#49,
    # Dest.DISCARD): the owner chooses which/how many; a trailing "none" stops early.
    # The binned cards land in the (public) discard, so the move is logged openly.
    from srg_sim import gamelog as gl

    class BinTwoThenStop(Policy):
        def __init__(self) -> None:
            super().__init__("bin-two")
            self.taken = 0

        def choose(self, point, legal, state, key):  # type: ignore[no-untyped-def]
            assert point == "search"  # routed to the owner, per card
            if self.taken < 2 and legal[0]["kind"] != "none":
                self.taken += 1
                return legal[0]
            return next(o for o in legal if o["kind"] == "none")  # decline the rest

    eng = Engine(*bull_vs_fae(), BinTwoThenStop(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    a = eng.state.players["A"]
    deck_before = len(a.deck)
    eng._act_search(fx.Search(dest=fx.Dest.DISCARD, count=4), "A")  # up to 4, take 2
    assert len(a.discard) == 2
    assert len(a.deck) == deck_before - 2
    assert all(c not in a.deck for c in a.discard)
    binned = [e for e in eng.state.log.events if isinstance(e, gl.Discard) and e.source == "deck"]
    assert len(binned) == 2 and all(e.hidden is False for e in binned)  # public in discard


def test_search_to_discard_caps_at_count() -> None:
    # A greedy owner (always bins the first offered) is still capped at `count`.
    class BinGreedy(Policy):
        def __init__(self) -> None:
            super().__init__("greedy")

        def choose(self, point, legal, state, key):  # type: ignore[no-untyped-def]
            return legal[0]  # never reaches the trailing "none"

    eng = Engine(*bull_vs_fae(), BinGreedy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    a = eng.state.players["A"]
    eng._act_search(fx.Search(dest=fx.Dest.DISCARD, count=3), "A")
    assert len(a.discard) == 3  # capped, not the whole deck


def test_add_from_discard_recurs_a_matching_card_to_hand() -> None:
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    a = eng.state.players["A"]
    card = next(c for c in a.deck if c.atk_type is AtkType.GRAPPLE)
    a.deck.remove(card)
    a.discard.append(card)
    eng._act_add_from_discard(
        fx.AddFromDiscard(filter=fx.CardFilter(atk_type=AtkType.GRAPPLE)), "A"
    )
    assert card in a.hand and card not in a.discard


def test_add_from_discard_lets_the_owner_choose_which_match() -> None:
    # Recursion is a player choice (DESIGN.md §7): with >1 match the owner picks via
    # the "target" decision point, not the engine auto-taking the first.
    class PickHighest(Policy):
        def __init__(self) -> None:
            super().__init__("pick-highest")

        def choose(self, point, legal, state, key):  # type: ignore[no-untyped-def]
            assert point == "target"  # engine routed the recur choice to the owner
            return max(legal, key=lambda o: o["number"])

    eng = Engine(*bull_vs_fae(), PickHighest(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    a = eng.state.players["A"]
    subs = [c for c in a.deck if c.atk_type is AtkType.SUBMISSION][:3]
    for c in subs:
        a.deck.remove(c)
        a.discard.append(c)
    want = max(subs, key=lambda c: c.number)
    eng._act_add_from_discard(
        fx.AddFromDiscard(filter=fx.CardFilter(atk_type=AtkType.SUBMISSION)), "A"
    )
    assert want in a.hand and want not in a.discard  # the chosen match, not the first


def test_shuffle_into_deck_recurs_one_card_from_discard_to_deck() -> None:
    # ShuffleIntoDeck moves ONE matching discard card back into the deck; "2 cards"
    # is authored as two actions, so two calls move two.
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    a = eng.state.players["A"]
    subs = [c for c in a.deck if c.atk_type is AtkType.SUBMISSION][:2]
    for c in subs:
        a.deck.remove(c)
        a.discard.append(c)
    deck_before = len(a.deck)
    sel = fx.ShuffleIntoDeck(selector=fx.CardFilter(atk_type=AtkType.SUBMISSION))
    eng._act_shuffle_into_deck(sel, "A")
    eng._act_shuffle_into_deck(sel, "A")
    assert all(c in a.deck for c in subs)
    assert all(c not in a.discard for c in subs)
    assert len(a.deck) == deck_before + 2


def test_recur_to_deck_top_puts_chosen_discards_on_top_of_deck() -> None:
    # #45 Chug-Chug: "up to 3 Finishes from discard on top of deck". The default
    # policy takes matches until they run out; the recurred card lands on TOP.
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    a = eng.state.players["A"]
    fins = [c for c in a.deck if c.play_order is PlayOrder.FINISH][:2]
    for c in fins:
        a.deck.remove(c)
        a.discard.append(c)
    eng._act_recur_to_deck_top(
        fx.RecurToDeckTop(selector=fx.CardFilter(play_order=PlayOrder.FINISH), count=3), "A"
    )
    assert all(c not in a.discard for c in fins)  # both recurred (fewer than the cap)
    assert a.deck[0] in fins and a.deck[1] in fins  # placed on top, ready to redraw


def test_recur_to_deck_top_owner_can_stop_early() -> None:
    class DeclineTarget(Policy):
        def __init__(self) -> None:
            super().__init__("decline")

        def choose(self, point, legal, state, key):  # type: ignore[no-untyped-def]
            return next((o for o in legal if o["kind"] == "none"), legal[0])  # stop at once

    eng = Engine(*bull_vs_fae(), DeclineTarget(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    a = eng.state.players["A"]
    fins = [c for c in a.deck if c.play_order is PlayOrder.FINISH][:2]
    for c in fins:
        a.deck.remove(c)
        a.discard.append(c)
    eng._act_recur_to_deck_top(
        fx.RecurToDeckTop(selector=fx.CardFilter(play_order=PlayOrder.FINISH), count=3), "A"
    )
    assert all(c in a.discard for c in fins)  # declined -> nothing recurred


def test_play_extra_card_grant_is_counted_and_consumed() -> None:
    # #45 Chug-Chug: PlayExtraCard banks a per-turn grant; _consume_extra_play spends it.
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    eng._act_play_extra_card(fx.PlayExtraCard(), "A")
    eng._act_play_extra_card(fx.PlayExtraCard(), "A")
    assert eng.state.players["A"].flags["extra_plays"] == 2
    assert eng._consume_extra_play("A") and eng._consume_extra_play("A")  # spends both
    assert eng._consume_extra_play("A") is False  # none left
    assert eng.state.players["A"].flags["extra_plays"] == 0


def test_turn_loop_runs_an_extra_action_when_granted() -> None:
    # The turn loop takes a second action when the first grants a PlayExtraCard.
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=3, created="x")
    eng.setup()
    seen: list[str] = []

    def fake(key: str) -> None:
        seen.append(key)
        if len(seen) == 1:
            eng._act_play_extra_card(fx.PlayExtraCard(), key)  # first action grants +1

    eng._take_turn_action = fake  # type: ignore[method-assign]
    eng._turn()
    assert len(seen) == 2  # base action + exactly one granted extra


def test_optional_effect_is_gated_and_can_flip_the_opponents_deck() -> None:
    # #45: a "you may" effect (Effect.optional) resolves only when the owner takes
    # it; Big Body Block's rider flips the OPPONENT's top card (Flip who=OPP).
    class Decide(Policy):
        def __init__(self, take: bool) -> None:
            super().__init__("decide")
            self.take = take

        def choose(self, point, legal, state, key):  # type: ignore[no-untyped-def]
            if point == "optional":
                return next(o for o in legal if o["kind"] == ("yes" if self.take else "no"))
            return legal[0]

    rider = fx.Effect(
        trigger=fx.OnHit(),
        actions=(fx.Flip(n=1, who=fx.Who.OPP),),
        optional=True,
        raw_clause="opp may flip their top card",
    )
    for take, delta in ((True, 1), (False, 0)):
        eng = Engine(*bull_vs_fae(), Decide(take), HeuristicPolicy(), seed=1, created="x")
        eng.setup()
        b_deck = len(eng.state.players["B"].deck)
        eng._run_effects((rider,), fx.OnHit, "A")  # A owns the rider -> flips B's deck
        assert len(eng.state.players["B"].deck) == b_deck - delta  # flipped iff taken


def test_remove_from_play_sends_a_chosen_opponent_board_card_to_discard() -> None:
    # #46: board disruption — the ACTOR discards a card the OPPONENT has in play,
    # aimed by the selector; non-matching board cards are untouched.
    eng = _fresh()
    b = eng.state.players["B"]
    lead = replace(_attack(AtkType.STRIKE, PlayOrder.LEAD), db_uuid="lead")
    fu = replace(_attack(AtkType.GRAPPLE, PlayOrder.FOLLOWUP), db_uuid="fu")
    b.in_play = [lead, fu]
    eng._act_remove_from_play(
        fx.RemoveFromPlay(selector=fx.CardFilter(play_order=PlayOrder.FOLLOWUP), who=fx.Who.OPP),
        "A",  # A is the actor; OPP = B
    )
    assert fu in b.discard and fu not in b.in_play  # the aimed card was discarded
    assert lead in b.in_play  # the non-matching Lead stayed on the board
    ev = json.loads(eng.state.log.to_lines()[-1])
    assert ev["type"] == "discard" and ev["from"] == "in_play" and ev["player"] == "B"


def test_remove_from_play_on_empty_board_is_a_noop() -> None:
    eng = _fresh()
    eng.state.players["B"].in_play = []
    eng._act_remove_from_play(fx.RemoveFromPlay(who=fx.Who.OPP), "A")  # nothing to remove
    assert eng.state.players["B"].discard == []


def test_movement_hidden_flag_tracks_private_endpoints() -> None:
    # §8 information model: draws (deck->hand) are hidden; discards (->public
    # pile) never are. A real game exercises both.
    events = [json.loads(x) for x in _play(6).state.log.to_lines()[1:]]
    draws = [e for e in events if e["type"] == "draw"]
    discards = [e for e in events if e["type"] == "discard"]
    assert draws and all(e["hidden"] for e in draws)  # every draw is hidden
    assert all(not e["hidden"] for e in discards)  # discards land in a public pile


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


# -- first-turn redraw (#44: per-player, first won turn, ordered bury, up to N) ---


def _leadless_hand(eng: Engine, key: str, numbers: tuple[int, ...]) -> list[Card]:
    player = eng.state.players[key]
    hand = [next(c for c in player.deck if c.number == n) for n in numbers]
    for c in hand:
        player.deck.remove(c)
    player.hand = list(hand)
    return hand


def test_setup_no_longer_runs_the_first_turn_redraw() -> None:
    # #44: the redraw is NOT a setup step — nobody is flagged and nothing is buried.
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    assert not any(p.flags.get("had_first_turn") for p in eng.state.players.values())
    assert all(json.loads(line)["type"] != "bury" for line in eng.state.log.to_lines()[1:])


def test_first_turn_option_fires_at_most_once_per_player() -> None:
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    a = eng.state.players["A"]
    _leadless_hand(eng, "A", (13, 14, 15))  # no Lead -> heuristic redraws
    eng._first_turn_option("A")
    assert a.flags["had_first_turn"]
    hand_after = list(a.hand)
    eng._first_turn_option("A")  # spent — a no-op now
    assert a.hand == hand_after


def test_first_turn_redraw_orders_the_bury_and_draws_up_to_n() -> None:
    # #44: player buries the revealed hand in a CHOSEN order (not random) and draws
    # UP TO that many (here 2 of 3). The reveal makes the moved cards public.
    class Mull(Policy):
        def __init__(self) -> None:
            super().__init__("mull")

        def choose(self, point, legal, state, key):  # type: ignore[no-untyped-def]
            if point == "mulligan":
                return next(o for o in legal if o["kind"] == "redraw")
            if point == "mulligan_bury":
                return min(legal, key=lambda o: o["number"])  # bury in ascending order
            if point == "mulligan_draw":
                return next(o for o in legal if o["n"] == 2)  # draw only 2 of the 3
            return legal[0]

    eng = Engine(*bull_vs_fae(), Mull(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    a = eng.state.players["A"]
    _leadless_hand(eng, "A", (15, 13, 14))
    eng._first_turn_option("A")
    assert [c.number for c in a.deck[-3:]] == [13, 14, 15]  # buried ascending, bottom
    assert len(a.hand) == 2  # drew up to 2, not all 3
    events = [json.loads(line) for line in eng.state.log.to_lines()[1:]]
    bury = next(e for e in events if e["type"] == "bury")
    assert bury["from"] == "hand" and bury["hidden"] is False  # revealed -> public


def test_first_turn_redraw_not_offered_with_a_lead_in_hand() -> None:
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    a = eng.state.players["A"]
    hand = _leadless_hand(eng, "A", (13, 14))
    lead = next(c for c in a.deck if c.number == 1)  # a Lead
    a.deck.remove(lead)
    a.hand = [*hand, lead]
    before = list(a.hand)
    eng._first_turn_option("A")  # has a Lead -> option not offered, hand kept
    assert a.hand == before and a.flags["had_first_turn"]


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


def test_stop_enters_the_defenders_board_not_discard() -> None:
    # DESIGN.md §6 / walkthrough: the stopping card is PLAYED onto the defender's
    # board and persists; only the stopped attack goes to the attacker's discard.
    eng = _fresh()
    attack = _attack(AtkType.GRAPPLE, PlayOrder.LEAD)
    stopper = replace(eng.state.players["B"].deck[0], atk_type=AtkType.STRIKE, db_uuid="stp")
    eng._apply_stop("A", "B", attack, stopper)
    assert stopper in eng.state.players["B"].in_play
    assert stopper not in eng.state.players["B"].discard
    assert attack in eng.state.players["A"].discard


def test_followup_stop_enters_play_with_no_lead() -> None:
    # A Follow Up used as a stop enters play even with no Lead — stopping bypasses
    # the play-sequence gate — so it can then enable a Finish (DESIGN.md §6, todo #33).
    from srg_sim.engine import _playable

    eng = _fresh()
    attack = _attack(AtkType.STRIKE, PlayOrder.LEAD)
    stopper = _attack(AtkType.SUBMISSION, PlayOrder.FOLLOWUP)
    assert not eng.state.players["B"].in_play
    eng._apply_stop("A", "B", attack, stopper)
    board = eng.state.players["B"].in_play
    assert stopper in board  # FU sits on the board with no Lead beneath it
    fin = _attack(AtkType.SUBMISSION, PlayOrder.FINISH)
    assert _playable(board, fin)  # and now enables a Finish


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


def test_bump_makes_both_players_draw(monkeypatch: pytest.MonkeyPatch) -> None:
    # On a tied turn roll both players bump: draw a card, then re-roll (mechanics §2).
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    eng.state.turn_no = 1
    a0, b0 = len(eng.state.players["A"].deck), len(eng.state.players["B"].deck)
    # _roll_for now returns (skill, value); tie once (5,5) -> bump -> then A wins (6,5).
    rolls = iter([(Skill.POWER, 5), (Skill.POWER, 5), (Skill.POWER, 6), (Skill.POWER, 5)])
    monkeypatch.setattr(eng, "_roll_for", lambda key, use_pending: next(rolls))
    assert eng._roll_off() == "A"
    assert len(eng.state.players["A"].deck) == a0 - 1  # each drew exactly once on the bump
    assert len(eng.state.players["B"].deck) == b0 - 1


def test_stopped_card_fires_none_of_its_text() -> None:
    # srg-rules-confirmed / #45: the stop window precedes a card's OnPlay text, so a
    # stopped card's effect (here a Draw) must NOT resolve — only the attack is discarded.
    class StopFirst(Policy):
        def __init__(self) -> None:
            super().__init__("stop-first")

        def choose(self, point, legal, state, key):  # type: ignore[no-untyped-def]
            return next((o for o in legal if o["kind"] == "stop"), legal[0])

    draw2 = fx.Effect(
        trigger=fx.OnPlay(),
        actions=(fx.Draw(n=2),),
        raw_clause="on play draw 2",
        source=fx.EffectSource.CARD,
    )
    attack = replace(
        _attack(AtkType.GRAPPLE, PlayOrder.LEAD), effects=(draw2,), db_uuid="atk", number=2
    )

    # Stopped: demo card 25 (Strike stop-any) answers the Grapple Lead -> no draw.
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), StopFirst(), seed=1, created="x")
    eng.setup()
    eng.state.turn_no = 1
    a, b = eng.state.players["A"], eng.state.players["B"]
    b.hand = [next(c for c in b.deck if c.number == 25)]
    deck_before = len(a.deck)
    assert eng._resolve_play("A", "B", attack) is False
    assert len(a.deck) == deck_before  # OnPlay Draw(2) was cancelled by the stop
    assert attack in a.discard

    # Unstopped (empty defender hand): the same card's OnPlay Draw(2) DOES fire.
    eng2 = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng2.setup()
    eng2.state.turn_no = 1
    a2 = eng2.state.players["A"]
    eng2.state.players["B"].hand = []
    hand_before = len(a2.hand)
    assert eng2._resolve_play("A", "B", replace(attack, db_uuid="atk2")) is True
    assert len(a2.hand) == hand_before + 2  # drew 2, card resolved into play


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


# -- finish bonuses: whole-combo sum + flat finish-roll bonus (§5) -----------


def _finish_value(eng: Engine) -> int:
    for line in eng.state.log.to_lines():
        e = json.loads(line)
        if e.get("type") == "finish_attempt":
            return int(e["value"])
    raise AssertionError("no finish_attempt logged")


def _combo_card(number: int, order: PlayOrder, skill: Skill, delta: int) -> Card:
    return Card(
        db_uuid=f"c{number}",
        name=f"C{number}",
        number=number,
        atk_type=AtkType.STRIKE,
        play_order=order,
        finish_bonuses=((skill, delta),),
    )


def test_finish_sums_the_whole_in_play_combo_not_just_the_finish_card(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    eng = _fresh()
    monkeypatch.setattr(eng.state.rng, "roll", lambda: Skill.STRIKE)
    monkeypatch.setattr(eng, "_stat", lambda key, skill: 0)  # isolate the bonus
    lead = _combo_card(7, PlayOrder.LEAD, Skill.STRIKE, 1)
    fu = _combo_card(19, PlayOrder.FOLLOWUP, Skill.STRIKE, 2)
    fin = _combo_card(28, PlayOrder.FINISH, Skill.STRIKE, 3)
    eng.state.players["A"].in_play = [lead, fu, fin]
    eng._finish_sequence("A", "B", fin)
    assert _finish_value(eng) == 6  # 1 + 2 + 3, the full Lead+FU+Finish combo


def test_flat_finish_roll_bonus_adds_any_skill(monkeypatch: pytest.MonkeyPatch) -> None:
    eng = _fresh()
    monkeypatch.setattr(eng.state.rng, "roll", lambda: Skill.POWER)  # no combo bonus for Power
    monkeypatch.setattr(eng, "_stat", lambda key, skill: 0)
    boost = fx.Effect(trigger=fx.Static(), actions=(fx.FinishRollBonus(4),))
    fin = Card(
        db_uuid="f",
        name="F",
        number=28,
        atk_type=AtkType.STRIKE,
        play_order=PlayOrder.FINISH,
        effects=(boost,),
    )
    eng.state.players["A"].in_play = [fin]
    eng._finish_sequence("A", "B", fin)
    assert _finish_value(eng) == 4  # flat +4 regardless of the rolled skill


# -- discard: hand-cap + forced discards route through the owner (§6/§7) ------


def test_hand_cap_discards_down_to_ten_by_owner_choice() -> None:
    eng = _fresh()
    hand = [next(c for c in eng.state.players["A"].deck if c.number == n) for n in range(1, 13)]
    eng.state.players["A"].hand = list(hand)  # 12 > cap of 10
    eng._hand_cap("A")
    assert len(eng.state.players["A"].hand) == 10
    assert len(eng.state.players["A"].discard) == 2  # exactly the excess shed


def test_draw_over_cap_discards_immediately_not_at_end_of_turn() -> None:
    # DESIGN.md §6 / todo #28: any draw that puts a player over max caps right then,
    # inside _draw — a top-deck to 11 forces a discard-down before the play action.
    eng = _fresh()
    hand = [next(c for c in eng.state.players["A"].deck if c.number == n) for n in range(1, 11)]
    eng.state.players["A"].hand = list(hand)  # exactly at the cap of 10
    eng._draw("A", 1)  # 11th card must be shed immediately
    assert len(eng.state.players["A"].hand) == 10
    assert len(eng.state.players["A"].discard) == 1


def _static_hand_mod(delta: int, who: fx.Who) -> fx.Effect:
    return fx.Effect(
        trigger=fx.Static(),
        actions=(fx.MaxHandSize(delta, who),),
        duration=fx.Duration.WHILE_IN_PLAY,
    )


def test_hand_cap_respects_a_raised_self_maximum() -> None:
    # DESIGN.md §6 / todo #37: a Static MaxHandSize on your own board raises the cap,
    # so a hand that would overflow the base 10 is kept intact.
    eng = _fresh()
    eng.state.players["A"].competitor = replace(
        eng.state.players["A"].competitor, effects=(_static_hand_mod(2, fx.Who.SELF),)
    )
    hand = [next(c for c in eng.state.players["A"].deck if c.number == n) for n in range(1, 13)]
    eng.state.players["A"].hand = list(hand)  # 12 cards, cap now 12
    eng._hand_cap("A")
    assert len(eng.state.players["A"].hand) == 12  # nothing shed
    assert eng.state.players["A"].discard == []


def test_opponent_card_lowering_max_forces_immediate_discard() -> None:
    # DESIGN.md §6 / todo #37: the cap is continuous — a card entering A's play that
    # lowers B's max hand size makes B discard down right then, with no draw of B's.
    eng = _fresh()
    b_hand = [next(c for c in eng.state.players["B"].deck if c.number == n) for n in range(1, 11)]
    eng.state.players["B"].hand = list(b_hand)  # B sits at the base cap of 10
    card = replace(
        next(c for c in eng.state.players["A"].deck if c.number == 7),
        effects=(_static_hand_mod(-2, fx.Who.OPP),),
    )
    eng.state.players["A"].in_play.append(card)  # B's max is now 8
    eng._enforce_hand_caps()
    assert len(eng.state.players["B"].hand) == 8  # B shed the excess with no draw
    assert len(eng.state.players["B"].discard) == 2
    assert eng.state.players["A"].hand == []  # A's own hand untouched


def test_opponent_forced_discard_targets_and_lets_the_owner_choose() -> None:
    eng = _fresh()
    a_hand = [next(c for c in eng.state.players["A"].deck if c.number == 7)]
    b_hand = [next(c for c in eng.state.players["B"].deck if c.number == n) for n in (7, 28)]
    eng.state.players["A"].hand = list(a_hand)
    eng.state.players["B"].hand = list(b_hand)
    # A plays a card reading "your opponent discards 1": B (the owner) chooses.
    eng._act_discard(fx.Discard(count=1, who=fx.Who.OPP), "A")
    assert len(eng.state.players["B"].hand) == 1  # B shed one
    assert eng.state.players["A"].hand == a_hand  # A's own hand untouched
    # B's heuristic protects the Finish (28) and sheds the dead Lead (7).
    assert eng.state.players["B"].hand[0].number == 28


def test_random_discard_uses_the_seeded_rng_not_the_policy() -> None:
    eng = _fresh()
    hand = [next(c for c in eng.state.players["A"].deck if c.number == n) for n in (7, 28)]
    eng.state.players["A"].hand = list(hand)
    eng._discard_from_hand("A", 1, random=True)
    assert len(eng.state.players["A"].hand) == 1
    assert len(eng.state.players["A"].discard) == 1  # a card left the hand for the pile


# -- snapshot mid-game -------------------------------------------------------


def test_mid_game_state_snapshot_round_trips() -> None:
    from srg_sim.state import GameState

    eng = _play(9)
    snap = eng.state.to_dict()
    assert GameState.from_dict(snap).to_dict() == snap


# -- mastermind-v3 behaviors -------------------------------------------------


def _bare(number: int, order: PlayOrder, atk: AtkType, effects: tuple[fx.Effect, ...]) -> Card:
    return Card(
        db_uuid=f"t-{number}",
        name=f"card {number}",
        number=number,
        atk_type=atk,
        play_order=order,
        effects=effects,
    )


def test_counts_as_in_play_counts_a_card_as_n() -> None:
    from srg_sim import conditions

    lead_strike = _bare(
        1,
        PlayOrder.LEAD,
        AtkType.STRIKE,
        (
            fx.Effect(
                trigger=fx.Static(),
                actions=(
                    fx.CountsAsInPlay(
                        fx.CardFilter(play_order=PlayOrder.LEAD, atk_type=AtkType.STRIKE), 2
                    ),
                ),
            ),
        ),
    )
    board = [lead_strike]
    assert conditions.count_in_play(board, fx.CardFilter(play_order=PlayOrder.LEAD)) == 2
    assert conditions.count_in_play(board, fx.CardFilter(atk_type=AtkType.STRIKE)) == 2
    # a Follow-up query the declaration does not imply falls back to the base 0
    assert conditions.count_in_play(board, fx.CardFilter(play_order=PlayOrder.FOLLOWUP)) == 0


def test_unstoppable_card_cannot_be_stopped_by_a_follow_up() -> None:
    attack = _bare(
        11,
        PlayOrder.LEAD,
        AtkType.GRAPPLE,
        (fx.Effect(trigger=fx.Static(), actions=(fx.Unstoppable(by_order=PlayOrder.FOLLOWUP),)),),
    )
    stop_eff = (fx.Effect(trigger=fx.Static(), actions=(fx.Stop(atk_type=AtkType.GRAPPLE),)),)
    follow_up_stopper = _bare(6, PlayOrder.FOLLOWUP, AtkType.STRIKE, stop_eff)
    lead_stopper = _bare(1, PlayOrder.LEAD, AtkType.STRIKE, stop_eff)
    eng = _fresh()
    assert eng._card_can_stop("B", follow_up_stopper, attack) is False  # unstoppable by Follow Ups
    assert eng._card_can_stop("B", lead_stopper, attack) is True  # a Lead stop still works


def test_t_virus_finish_bonus_doubles_on_bump() -> None:
    t_virus = _bare(
        30,
        PlayOrder.FINISH,
        AtkType.SUBMISSION,
        (fx.Effect(trigger=fx.Static(), actions=(fx.DoubleFinishIfBumped(),)),),
    )
    object.__setattr__(t_virus, "finish_bonuses", ((Skill.GRAPPLE, 2),))
    eng = _fresh()
    eng._turn_bumped = False
    assert eng._card_finish_bonus(t_virus, Skill.GRAPPLE) == 2
    eng._turn_bumped = True
    assert eng._card_finish_bonus(t_virus, Skill.GRAPPLE) == 4  # doubled on a bumped turn


def test_per_count_draw_scales_with_the_board() -> None:
    draw = fx.Draw(n=1, per=fx.CardFilter(play_order=PlayOrder.LEAD), per_who=fx.Who.SELF)
    eng = _fresh()
    eng.state.players["A"].in_play = [
        _bare(n, PlayOrder.LEAD, AtkType.STRIKE, ()) for n in (1, 2, 3)
    ]
    before = len(eng.state.players["A"].hand)
    eng._act_draw(draw, "A")
    assert len(eng.state.players["A"].hand) == before + 3  # one per Lead in play


def test_elective_same_skill_bump_grant_charges_and_election() -> None:
    from dataclasses import replace

    eng = _fresh()
    grant = fx.Effect(trigger=fx.Static(), actions=(fx.ElectBumpOnSameSkill(uses=2),))
    ent = eng.state.players["A"].entrance
    eng.state.players["A"].entrance = replace(ent, effects=(grant,))
    assert eng._elective_bump_owner() == "A"  # A holds a charged grant, B does not
    # HeuristicPolicy elects the bump only when behind on the roll
    assert eng._elect_bump("A", 3, 5) is True  # losing -> bump into a re-roll
    assert eng.state.players["A"].freq_counters["match:elect_bump"] == 1  # a charge spent
    assert eng._elect_bump("A", 6, 2) is False  # winning -> keep the win, no bump
    assert eng.state.players["A"].freq_counters["match:elect_bump"] == 1  # unchanged
    eng.state.players["A"].freq_counters["match:elect_bump"] = 2
    assert eng._elective_bump_owner() is None  # both charges exhausted


def test_also_lead_makes_a_finish_playable_when_hand_holds_only_it() -> None:
    also_lead = _bare(
        28,
        PlayOrder.FINISH,
        AtkType.STRIKE,
        (
            fx.Effect(
                trigger=fx.Static(),
                actions=(fx.AlsoLead(fx.HandSizeCompare(fx.Comparator.LE, fx.Vs.VALUE, 1)),),
            ),
        ),
    )
    eng = _fresh()
    eng.state.players["A"].in_play = []  # no Follow Up, so a Finish is normally unplayable
    eng.state.players["A"].hand = [also_lead]
    opts = eng._playable_options("A")
    assert [o["number"] for o in opts] == [28]  # playable as a Lead because the hand is bare
    # with another card in hand the condition fails and the Finish is unplayable again
    eng.state.players["A"].hand = [also_lead, _bare(2, PlayOrder.LEAD, AtkType.STRIKE, ())]
    playable_finishes = [o for o in eng._playable_options("A") if o["number"] == 28]
    assert playable_finishes == []


def _hand_loss_flag() -> fx.Effect:
    return fx.Effect(
        trigger=fx.Static(),
        actions=(fx.SuppressSelfHandLoss(),),
        source=fx.EffectSource.GIMMICK,
    )


def test_suppress_self_hand_loss_voids_only_your_own_effects() -> None:
    """Sami "Death Machine" (V2): "you do not bury or discard cards from your hand
    for your OWN card effects" — an opponent's effect still takes the cards."""
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    discard = fx.Discard(count=1, who=fx.Who.SELF, random=True)
    # Baseline: no flag -> A's own effect costs A a card.
    before = len(eng.state.players["A"].hand)
    eng._act_discard(discard, "A")
    assert len(eng.state.players["A"].hand) == before - 1
    # With the flag, A's own effect no longer costs A a card.
    eng.state.players["A"].competitor = replace(
        eng.state.players["A"].competitor, effects=(_hand_loss_flag(),)
    )
    held = len(eng.state.players["A"].hand)
    eng._act_discard(discard, "A")
    assert len(eng.state.players["A"].hand) == held
    # But the OPPONENT's effect still takes one ("for your OWN card effects").
    eng._act_discard(fx.Discard(count=1, who=fx.Who.OPP, random=True), "B")
    assert len(eng.state.players["A"].hand) == held - 1


def test_suppress_self_hand_loss_covers_hand_bury_too() -> None:
    """The declaration reads "bury OR discard", so both chokepoints are voided."""
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    bury = fx.Bury(count=1, who=fx.Who.SELF, random=True, source=fx.BuryFrom.HAND)
    before = len(eng.state.players["A"].hand)
    eng._act_bury(bury, "A")
    assert len(eng.state.players["A"].hand) == before - 1
    eng.state.players["A"].competitor = replace(
        eng.state.players["A"].competitor, effects=(_hand_loss_flag(),)
    )
    held = len(eng.state.players["A"].hand)
    eng._act_bury(bury, "A")
    assert len(eng.state.players["A"].hand) == held


def test_suppress_self_hand_loss_does_not_protect_the_opponent() -> None:
    """The flag is owner-scoped: A holding it must not stop A's effect from making
    B discard (that is the whole point of Sami WR's other branch)."""
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    eng.state.players["A"].competitor = replace(
        eng.state.players["A"].competitor, effects=(_hand_loss_flag(),)
    )
    before = len(eng.state.players["B"].hand)
    eng._act_discard(fx.Discard(count=1, who=fx.Who.OPP, random=True), "A")
    assert len(eng.state.players["B"].hand) == before - 1


def _no_dq(scope: fx.DqScope) -> fx.Effect:
    return fx.Effect(
        trigger=fx.Static(),
        actions=(fx.DisqualificationRule(enabled=False, scope=scope),),
        source=fx.EffectSource.GIMMICK,
    )


def test_dq_rule_scope_self_protects_only_its_owner() -> None:
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    eng.state.players["A"].competitor = replace(
        eng.state.players["A"].competitor, effects=(_no_dq(fx.DqScope.SELF),)
    )
    assert eng._is_dq_immune("A")
    assert not eng._is_dq_immune("B")


def test_dq_rule_scope_match_protects_both_players() -> None:
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    eng.state.players["B"].competitor = replace(
        eng.state.players["B"].competitor, effects=(_no_dq(fx.DqScope.MATCH),)
    )
    assert eng._is_dq_immune("A")
    assert eng._is_dq_immune("B")


def test_blanked_gimmick_declares_no_dq_immunity() -> None:
    """Hand-adjudicated 2026-07-20: blanking a gimmick makes its text inert, so
    Cardona's "you cannot be disqualified" dies with it — the same rule the
    suppression flags and ConsideredCompare already followed."""
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    eng.state.players["A"].competitor = replace(
        eng.state.players["A"].competitor, effects=(_no_dq(fx.DqScope.SELF),)
    )
    assert eng._is_dq_immune("A")
    eng.state.players["A"].gimmick_blanked = True
    assert not eng._is_dq_immune("A")


def _on_hit_draw(who: fx.Who) -> fx.Effect:
    return fx.Effect(
        trigger=fx.OnHit(order=PlayOrder.FOLLOWUP, who=who),
        actions=(fx.Draw(n=1, who=fx.Who.SELF),),
        source=fx.EffectSource.GIMMICK,
    )


def _followup() -> Card:
    return Card(
        db_uuid="fu",
        name="Follow Through",
        number=1,
        atk_type=AtkType.STRIKE,
        play_order=PlayOrder.FOLLOWUP,
    )


def test_on_hit_who_opp_fires_only_on_the_opponents_hit() -> None:
    """El Super Hombre V2: "after your OPPONENT hits a Follow Up" (schema v43)."""
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    eng.state.players["A"].competitor = replace(
        eng.state.players["A"].competitor, effects=(_on_hit_draw(fx.Who.OPP),)
    )
    before = len(eng.state.players["A"].hand)
    eng._run_hit_gimmicks(_followup(), "B")  # opponent hit it
    assert len(eng.state.players["A"].hand) == before + 1
    held = len(eng.state.players["A"].hand)
    eng._run_hit_gimmicks(_followup(), "A")  # A's own hit must NOT fire it
    assert len(eng.state.players["A"].hand) == held


def test_on_hit_who_self_is_unchanged_by_the_new_field() -> None:
    """The default, and every pre-v43 node: only the hitter's own gimmicks fire."""
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    eng.state.players["A"].competitor = replace(
        eng.state.players["A"].competitor, effects=(_on_hit_draw(fx.Who.SELF),)
    )
    before = len(eng.state.players["A"].hand)
    eng._run_hit_gimmicks(_followup(), "A")
    assert len(eng.state.players["A"].hand) == before + 1
    held = len(eng.state.players["A"].hand)
    eng._run_hit_gimmicks(_followup(), "B")
    assert len(eng.state.players["A"].hand) == held


# -- Father Light: deferred forced reveal-and-play (schema v55) --------------


def _mk(uuid: str, order: PlayOrder, number: int) -> Card:
    return Card(db_uuid=uuid, name=uuid, number=number, atk_type=AtkType.STRIKE, play_order=order)


def test_father_light_arming_is_a_one_shot_flag_on_the_opponent() -> None:
    eng = _fresh()
    eng._act_force_reveal_play(fx.ForceRevealPlay(fx.Who.OPP), "A")  # A's gimmick arms B
    assert "forced_reveal_play" in eng.state.players["B"].flags
    assert "forced_reveal_play" not in eng.state.players["A"].flags
    eng._act_force_reveal_play(fx.ForceRevealPlay(fx.Who.OPP), "A")  # re-arm: armed once
    assert eng._consume_forced_reveal_play("B")
    assert not eng._consume_forced_reveal_play("B")  # consumed, then clear


def test_father_light_forces_the_only_playable_card_the_lead() -> None:
    eng = _fresh()
    eng.state.players["A"].hand = []  # defender cannot stop
    eng.state.players["B"].in_play = []
    eng.state.players["B"].hand = [_mk("b-lead", PlayOrder.LEAD, 101), _mk("b-fu", PlayOrder.FOLLOWUP, 102)]
    assert eng._forced_reveal_and_play("B", "A")  # a card was forced
    b = eng.state.players["B"]
    assert any(c.db_uuid == "b-lead" for c in b.in_play)  # the only playable card landed
    assert [c.db_uuid for c in b.hand] == ["b-fu"]  # the Follow Up remains


def test_father_light_nothing_playable_reveals_the_hand_and_plays_nothing() -> None:
    eng = _fresh()
    eng.state.players["A"].hand = []
    eng.state.players["B"].in_play = []
    eng.state.players["B"].hand = [_mk("b-fu", PlayOrder.FOLLOWUP, 101), _mk("b-fin", PlayOrder.FINISH, 102)]
    assert not eng._forced_reveal_and_play("B", "A")  # nothing playable
    b = eng.state.players["B"]
    assert len(b.hand) == 2 and b.in_play == []  # hand untouched, nothing played


def test_father_light_take_turn_action_consumes_the_armed_forced_play() -> None:
    eng = _fresh()
    eng.state.players["A"].hand = []
    eng.state.players["B"].in_play = []
    eng.state.players["B"].hand = [_mk("b-lead", PlayOrder.LEAD, 101)]
    eng._act_force_reveal_play(fx.ForceRevealPlay(fx.Who.OPP), "A")
    eng.state.active = "B"
    eng._take_turn_action("B")
    b = eng.state.players["B"]
    assert any(c.db_uuid == "b-lead" for c in b.in_play)  # forced to play the Lead
    assert "forced_reveal_play" not in b.flags  # flag consumed


# -- Mr. Rey: deferred next-turn hand<->discard swap grant (schema v56) -------


class _AlwaysSwap(HeuristicPolicy):
    """Says yes to the optional swap and picks the first card at each zone pick."""

    def decide(self, point: str, key: str, legal: list) -> dict:  # type: ignore[override]
        return legal[0]


def test_mr_rey_grant_arms_then_promotes_then_expires() -> None:
    eng = _fresh()
    eng._act_grant_swap_next_turn(fx.GrantSwapNextTurn(fx.Who.SELF), "A")  # A grants itself
    assert eng.state.players["A"].flags.get("swap_grant_next")
    eng._promote_swap_grant_for(eng.state.players["A"])  # next -> this
    assert eng.state.players["A"].flags.get("swap_grant_this")
    assert "swap_grant_next" not in eng.state.players["A"].flags
    eng._promote_swap_grant_for(eng.state.players["A"])  # unused -> expires
    assert "swap_grant_this" not in eng.state.players["A"].flags


def test_mr_rey_offer_performs_the_swap_when_usable() -> None:
    eng = Engine(*bull_vs_fae(), _AlwaysSwap(), _AlwaysSwap(), seed=1, created="x")
    eng.state.turn_no = 1
    h1 = _mk("h1", PlayOrder.LEAD, 101)
    d1 = _mk("d1", PlayOrder.LEAD, 102)
    eng.state.players["A"].hand = [h1]
    eng.state.players["A"].discard = [d1]
    eng.state.players["A"].flags["swap_grant_this"] = True
    eng._offer_swap_grant("A")
    a = eng.state.players["A"]
    assert any(c.db_uuid == "d1" for c in a.hand)  # discard card entered hand
    assert any(c.db_uuid == "h1" for c in a.discard)  # hand card went to discard
    assert "swap_grant_this" not in a.flags  # consumed


def test_mr_rey_offer_is_a_noop_without_a_grant() -> None:
    eng = Engine(*bull_vs_fae(), _AlwaysSwap(), _AlwaysSwap(), seed=1, created="x")
    eng.state.turn_no = 1
    eng.state.players["A"].hand = [_mk("h1", PlayOrder.LEAD, 101)]
    eng.state.players["A"].discard = [_mk("d1", PlayOrder.LEAD, 102)]
    eng._offer_swap_grant("A")  # no swap_grant_this
    assert any(c.db_uuid == "h1" for c in eng.state.players["A"].hand)  # unchanged


def test_mr_rey_empty_discard_consumes_the_grant() -> None:
    eng = _fresh()
    eng.state.players["A"].hand = [_mk("h1", PlayOrder.LEAD, 101)]
    eng.state.players["A"].discard = []
    eng.state.players["A"].flags["swap_grant_this"] = True
    eng._offer_swap_grant("A")
    a = eng.state.players["A"]
    assert len(a.hand) == 1 and "swap_grant_this" not in a.flags  # window passes, consumed
