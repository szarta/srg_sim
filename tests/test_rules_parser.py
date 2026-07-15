"""Tests for the rules_text -> Effect pipeline (DESIGN.md §4).

Grammar / override / Unsupported behaviour and the coverage report run offline;
a real-DB group (skipped when the export is absent) checks coverage stays healthy
and that an enriched real deck plays.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any

import pytest
from srg_sim import rules_parser as rp
from srg_sim.cards import AtkType, PlayOrder, Skill
from srg_sim.effects import (
    AddFromDiscard,
    BuffSkill,
    Bury,
    CrowdMeterCompare,
    DeckEnd,
    Discard,
    Draw,
    Duration,
    EffectSource,
    FinishBonus,
    FinishRollBonus,
    Flip,
    Frequency,
    LoseBy,
    ModifyRoll,
    RollWhen,
    ShuffleDeck,
    ShuffleIntoDeck,
    SkillCompare,
    Static,
    Stop,
    Unsupported,
    Who,
)
from srg_sim.loader import DEFAULT_CARDS_YAML, CardIndex

CARD = EffectSource.CARD


def _one(clause: str) -> Any:
    """The single action compiled from a one-clause card text."""
    effects = rp.parse_text(clause, CARD)
    assert len(effects) == 1
    return effects[0].actions[0]


# --- grammar: the high-frequency shapes ------------------------------------


def test_finish_bonus() -> None:
    act = _one("+2 to Grapple")
    assert isinstance(act, FinishBonus)  # a combo number: finish-only, per-skill
    assert act.skill is Skill.GRAPPLE
    assert act.delta == 2
    assert isinstance(rp.parse_text("+2 to Grapple", CARD)[0].trigger, Static)


def test_flat_finish_roll_bonus_is_any_skill() -> None:
    for clause in ("+3 to your Finish rolls", "+6 to Finish rolls", "Your Finish roll is +2"):
        act = _one(clause)
        assert isinstance(act, FinishRollBonus), clause
    assert _one("+3 to your Finish rolls").delta == 3


def test_persistent_self_skill_buff_is_all_rolls_not_a_combo_bonus() -> None:
    # "Your <skill> is +N" is a Static BuffSkill (folds into every roll), NOT the
    # finish-only combo bonus that a bare "+N to <skill>" compiles to.
    act = _one("Your Strike is +1")
    assert isinstance(act, BuffSkill)
    assert (act.skill, act.delta, act.who, act.duration) == (
        Skill.STRIKE,
        1,
        Who.SELF,
        Duration.WHILE_IN_PLAY,
    )


def test_draw() -> None:
    act = _one("Draw 3 cards.")
    assert isinstance(act, Draw)
    assert act.n == 3


def test_next_turn_roll_buff() -> None:
    act = _one("Your next turn roll is +1.")
    assert isinstance(act, ModifyRoll)
    assert act.who is Who.SELF
    assert act.delta == 1
    assert act.when is RollWhen.NEXT


def test_opponent_next_turn_roll_debuff() -> None:
    act = _one("Your opponent's next turn roll is -1.")
    assert act.who is Who.OPP
    assert act.delta == -1


def test_opponent_skill_debuff() -> None:
    act = _one("Your opponent's Strike is -2.")
    assert isinstance(act, BuffSkill)
    assert act.skill is Skill.STRIKE
    assert act.delta == -2
    assert act.who is Who.OPP


def test_lose_by_disqualification() -> None:
    effect = rp.parse_text("If stopped, you lose the match via disqualification.", CARD)[0]
    assert isinstance(effect.actions[0], LoseBy)


def test_flip() -> None:
    act = _one("Flip 2 cards.")
    assert isinstance(act, Flip)
    assert act.n == 2


def test_bury_self_and_opponent() -> None:
    assert _one("Bury 1 card.").who is Who.SELF
    opp = _one("Bury 2 cards in your opponent's discard pile.")
    assert isinstance(opp, Bury)
    assert opp.who is Who.OPP
    assert opp.count == 2


def test_discard_self_and_opponent_chosen_and_random() -> None:
    me = _one("Discard 2 cards from your hand.")
    assert isinstance(me, Discard)
    assert (me.who, me.count, me.random) == (Who.SELF, 2, False)

    opp = _one("Your opponent discards 1 card from their hand.")
    assert (opp.who, opp.random) == (Who.OPP, False)

    opp_rand = _one("Your opponent randomly discards 2 cards from their hand.")
    assert (opp_rand.who, opp_rand.count, opp_rand.random) == (Who.OPP, 2, True)

    opp_rand2 = _one("Your opponent discards 1 random card from their hand.")
    assert (opp_rand2.who, opp_rand2.random) == (Who.OPP, True)


def test_discard_with_trailing_rider_stays_unsupported() -> None:
    # "... for each Stop they have in play" isn't modeled -> never silently dropped.
    act = _one("Your opponent discards 1 card from their hand for each Stop they have in play.")
    assert type(act).__name__ == "Unsupported"


def test_add_from_discard() -> None:
    assert isinstance(_one("Add 1 card from your discard pile to your hand."), AddFromDiscard)


def test_shuffle_into_deck() -> None:
    assert isinstance(
        _one("Shuffle 2 cards from your discard pile into your deck."), ShuffleIntoDeck
    )


def test_stop_plain_and_ordered() -> None:
    assert _one("Stop any Grapple.") == Stop(atk_type=AtkType.GRAPPLE)
    assert _one("Stop any Lead Strike.") == Stop(order=PlayOrder.LEAD, atk_type=AtkType.STRIKE)


def test_stop_dual_order() -> None:
    effect = rp.parse_text("Stop any Follow Up Strike or Finish Strike.", CARD)[0]
    assert len(effect.actions) == 2
    assert all(isinstance(a, Stop) for a in effect.actions)


def test_conditional_stop_skill_compare() -> None:
    clause = (
        "If your Submission skill is greater than your opponent's "
        "Submission skill, stop any Strike."
    )
    effect = rp.parse_text(clause, CARD)[0]
    assert isinstance(effect.condition, SkillCompare)
    assert effect.condition.skill is Skill.SUBMISSION
    assert isinstance(effect.actions[0], Stop)


def test_conditional_stop_crowd_meter() -> None:
    effect = rp.parse_text("If the Crowd Meter is 2 or greater, stop any Follow Up Grapple.", CARD)[
        0
    ]
    assert isinstance(effect.condition, CrowdMeterCompare)
    assert effect.condition.value == 2


# --- #27 coverage cleanup: metadata, skill-stop printings, draws, shuffle ----


def test_skill_requirement_is_metadata_not_an_effect() -> None:
    # A deck-build constraint printed on the card, not a match effect: recognized
    # and skipped (never Unsupported), and it doesn't count against coverage.
    assert rp.parse_text("Skill Requirement: Submission 8+", CARD) == []
    effects = rp.parse_text("Draw 2 cards.\nSkill Requirement: Strike 10+, Agility 9+", CARD)
    assert len(effects) == 1 and isinstance(effects[0].actions[0], Draw)


def test_skill_stop_printed_without_the_word_skill() -> None:
    # Some printings drop "skill": "If your Power is greater than your opponent's Power".
    effect = rp.parse_text(
        "If your Power is greater than your opponent's Power, stop any Submission.", CARD
    )[0]
    assert isinstance(effect.condition, SkillCompare)
    assert effect.condition.skill is Skill.POWER
    assert effect.actions == (Stop(atk_type=AtkType.SUBMISSION),)


def test_conditional_stop_with_dual_order_target() -> None:
    clause = (
        "If your Submission skill is greater than your opponent's Submission skill, "
        "stop any Follow Up Strike or Finish Strike."
    )
    effect = rp.parse_text(clause, CARD)[0]
    assert isinstance(effect.condition, SkillCompare)
    assert [a.order for a in effect.actions] == [PlayOrder.FOLLOWUP, PlayOrder.FINISH]


def test_unmodelled_stop_target_declines_to_unsupported() -> None:
    # "even if it cannot be stopped" isn't modelled, so the clause stays Unsupported
    # rather than silently dropping the qualifier.
    act = _one("Stop any Finish Strike even if it cannot be stopped.")
    assert isinstance(act, Unsupported)


def test_each_player_and_opponent_draw() -> None:
    each = rp.parse_text("Each player draws 1 card.", CARD)[0]
    assert [(a.n, a.who) for a in each.actions] == [(1, Who.SELF), (1, Who.OPP)]
    opp = _one("Your opponent draws 2 cards.")
    assert isinstance(opp, Draw) and opp.n == 2 and opp.who is Who.OPP


def test_draw_from_the_bottom_of_the_deck() -> None:
    act = _one("Draw the bottom 3 cards of your deck.")
    assert isinstance(act, Draw) and act.n == 3 and act.source is DeckEnd.BOTTOM


def test_plus_n_to_your_next_turn_roll() -> None:
    act = _one("+1 to your next turn roll.")
    assert isinstance(act, ModifyRoll) and act.when is RollWhen.NEXT and act.delta == 1


def test_shuffle_your_deck() -> None:
    act = _one("Shuffle your deck.")
    assert isinstance(act, ShuffleDeck) and act.who is Who.SELF
    # The compound "Shuffle your deck and draw…" is not a single action, stays Unsupported.
    assert isinstance(_one("Shuffle your deck and draw 1 card."), Unsupported)


# --- frequency headers, unsupported, multi-clause --------------------------


def test_frequency_header_scopes_following_clauses() -> None:
    effects = rp.parse_text("Once per match:\nDraw 1 card.", CARD)
    assert len(effects) == 1  # the header is not itself an effect
    assert effects[0].frequency.kind is Frequency.ONCE_PER_MATCH


def test_n_times_per_match_header() -> None:
    effects = rp.parse_text("2 times per match:\nDraw 1 card.", CARD)
    assert effects[0].frequency.kind is Frequency.N_PER_MATCH
    assert effects[0].frequency.n == 2


def test_unknown_clause_is_unsupported_never_dropped() -> None:
    act = _one("Summon a dragon from the shadow realm.")
    assert isinstance(act, Unsupported)
    assert act.raw_text.startswith("Summon a dragon")


def test_multi_clause_text() -> None:
    effects = rp.parse_text("+1 to Strike\n+3 to Submission\nDraw 1 card.", CARD)
    assert len(effects) == 3


def test_finish_bonuses_are_summed() -> None:
    effects = rp.parse_text("+1 to Strike\n+3 to Submission\n+2 to Grapple", CARD)
    bonuses = dict(rp.finish_bonuses(effects))
    assert bonuses == {Skill.STRIKE: 1, Skill.SUBMISSION: 3, Skill.GRAPPLE: 2}


# --- overrides --------------------------------------------------------------


def test_override_wins_over_grammar() -> None:
    from srg_sim.effects import Effect, OnPlay

    override_effect = Effect(trigger=OnPlay(), actions=(Draw(n=9),), raw_clause="curated")
    overrides = {"u1": [override_effect.to_dict()]}
    # The text would normally parse as a finish bonus; the override replaces it.
    effects = rp.parse_text("+1 to Power", CARD, db_uuid="u1", overrides=overrides)
    assert effects == [override_effect]


def test_shipped_overrides_file_loads() -> None:
    assert rp.load_overrides() == {}  # documented example is commented out


# --- enrichment (loader bridge) --------------------------------------------


def _bare_card(number: int, text: str) -> Any:
    from srg_sim.cards import Card

    return Card(
        db_uuid=f"c{number}",
        name=f"C{number}",
        number=number,
        atk_type=AtkType.STRIKE,
        play_order=PlayOrder.FINISH,
        raw_text=text,
    )


def test_enrich_card_sets_effects_and_finish_bonuses() -> None:
    card = _bare_card(28, "+2 to Strike\nDraw 1 card.")
    enriched = rp.enrich_card(card)
    assert len(enriched.effects) == 2
    assert enriched.finish_bonuses == ((Skill.STRIKE, 2),)


# --- coverage report --------------------------------------------------------


def _rec(uuid: str, text: str) -> dict[str, Any]:
    return {"db_uuid": uuid, "card_type": "MainDeckCard", "rules_text": text}


def test_coverage_counts_grammar_override_unsupported() -> None:
    records = [
        _rec("a", "+1 to Power\nDraw 1 card."),  # 2 grammar
        _rec("b", "Summon a dragon.\nBanish the ref."),  # 2 unsupported
        _rec("c", "anything at all"),  # 1 override
    ]
    report = rp.coverage(records, overrides={"c": []})
    assert report.grammar == 2
    assert report.unsupported == 2
    assert report.override == 1
    assert report.total == 5
    assert 0.0 < report.rate < 1.0
    assert report.top_unparsed  # unparsed shapes recorded


def test_coverage_frequency_headers_are_not_counted() -> None:
    report = rp.coverage([_rec("a", "Once per match:\nDraw 1 card.")])
    assert report.total == 1  # header excluded, one real clause


def test_coverage_skips_skill_requirement_metadata() -> None:
    report = rp.coverage([_rec("a", "Draw 1 card.\nSkill Requirement: Power 8+")])
    assert report.total == 1 and report.grammar == 1  # metadata excluded, not unsupported


def test_is_top96() -> None:
    assert rp.is_top96({"division": "World Championship"})
    assert rp.is_top96({"division": "Underworld"})
    assert not rp.is_top96({"division": "Hardcore"})


# --- real card DB (skipped when the export is absent) ----------------------

requires_db = pytest.mark.skipif(
    not DEFAULT_CARDS_YAML.exists(), reason=f"card export not available: {DEFAULT_CARDS_YAML}"
)
_DECKS = Path(__file__).resolve().parent.parent / "decks"


@requires_db
def test_real_main_deck_coverage_is_healthy() -> None:
    records = [r for r in CardIndex.from_yaml().records if r.get("card_type") == "MainDeckCard"]
    report = rp.coverage(records)
    assert report.total > 1000
    assert report.rate > 0.4  # the +N-to-skill bulk alone clears this


@requires_db
def test_enriched_real_deck_plays() -> None:
    from srg_sim.engine import Engine
    from srg_sim.loader import load_deck
    from srg_sim.policy import HeuristicPolicy

    idx = CardIndex.from_yaml()
    bull = rp.enrich_deck(load_deck(_DECKS / "bull.yaml", idx).deck)
    fae = rp.enrich_deck(load_deck(_DECKS / "fae.yaml", idx).deck)
    # A finish card now carries parsed bonuses.
    assert any(c.finish_bonuses for c in bull.cards)
    result = Engine(bull, fae, HeuristicPolicy(), HeuristicPolicy(), seed=7, created="x").play()
    assert result.reason in {"finish", "count_out", "disqualification", "pinfall"}
