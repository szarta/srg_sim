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
    Draw,
    EffectSource,
    FinishBonus,
    Flip,
    Frequency,
    LoseBy,
    ModifyRoll,
    RollWhen,
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
    assert isinstance(act, FinishBonus)
    assert act.skill is Skill.GRAPPLE
    assert act.delta == 2
    assert isinstance(rp.parse_text("+2 to Grapple", CARD)[0].trigger, Static)


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


def test_add_from_discard() -> None:
    assert isinstance(_one("Add 1 card from your discard pile to your hand."), AddFromDiscard)


def test_shuffle_into_deck() -> None:
    assert isinstance(_one("Shuffle 2 cards from your discard pile into your deck."), ShuffleIntoDeck)


def test_stop_plain_and_ordered() -> None:
    assert _one("Stop any Grapple.") == Stop(atk_type=AtkType.GRAPPLE)
    assert _one("Stop any Lead Strike.") == Stop(order=PlayOrder.LEAD, atk_type=AtkType.STRIKE)


def test_stop_dual_order() -> None:
    effect = rp.parse_text("Stop any Follow Up Strike or Finish Strike.", CARD)[0]
    assert len(effect.actions) == 2
    assert all(isinstance(a, Stop) for a in effect.actions)


def test_conditional_stop_skill_compare() -> None:
    effect = rp.parse_text(
        "If your Submission skill is greater than your opponent's Submission skill, stop any Strike.",
        CARD,
    )[0]
    assert isinstance(effect.condition, SkillCompare)
    assert effect.condition.skill is Skill.SUBMISSION
    assert isinstance(effect.actions[0], Stop)


def test_conditional_stop_crowd_meter() -> None:
    effect = rp.parse_text("If the Crowd Meter is 2 or greater, stop any Follow Up Grapple.", CARD)[0]
    assert isinstance(effect.condition, CrowdMeterCompare)
    assert effect.condition.value == 2


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
