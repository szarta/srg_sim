"""Tests for the domain model: enums, Stats, Card, Competitor, Entrance, Deck."""

from __future__ import annotations

import pytest
from srg_sim.cards import (
    DECK_SIZE,
    AtkType,
    Card,
    Competitor,
    Deck,
    EntranceCard,
    PlayOrder,
    Skill,
    Stats,
    atk_type_from_number,
)
from srg_sim.effects import BuffSkill, Effect, OnPlay


def _card(number: int, **overrides: object) -> Card:
    defaults: dict[str, object] = {
        "db_uuid": f"u{number}",
        "name": f"card-{number}",
        "number": number,
        "atk_type": atk_type_from_number(number),
        "play_order": PlayOrder.LEAD,
    }
    defaults.update(overrides)
    return Card(**defaults)  # type: ignore[arg-type]


def _full_deck() -> Deck:
    comp = Competitor("c-uuid", "The Bull", "World Championship", _bull_stats())
    entrance = EntranceCard("e-uuid", "Calling in Kanik")
    cards = tuple(_card(n) for n in range(1, DECK_SIZE + 1))
    return Deck(comp, entrance, cards)


def _bull_stats() -> Stats:
    return Stats(power=10, agility=5, technique=6, submission=7, grapple=9, strike=8)


# --- enums / helpers -------------------------------------------------------


@pytest.mark.parametrize(
    ("number", "expected"),
    [
        (1, AtkType.STRIKE),
        (2, AtkType.GRAPPLE),
        (3, AtkType.SUBMISSION),
        (28, AtkType.STRIKE),  # "Coin Flip" #28 is Strike in the DB
        (30, AtkType.SUBMISSION),
    ],
)
def test_atk_type_from_number(number: int, expected: AtkType) -> None:
    assert atk_type_from_number(number) is expected


# --- Stats -----------------------------------------------------------------


def test_stats_get_and_bijection() -> None:
    stats = _bull_stats()
    assert stats.get(Skill.POWER) == 10
    assert stats.get(Skill.AGILITY) == 5
    assert stats.is_bijection_5_to_10()


def test_stats_non_bijection() -> None:
    assert not Stats(5, 5, 5, 5, 5, 5).is_bijection_5_to_10()


def test_stats_round_trip() -> None:
    stats = _bull_stats()
    assert Stats.from_dict(stats.to_dict()) == stats
    assert stats.to_dict()["Power"] == 10


# --- Card ------------------------------------------------------------------


def test_card_atk_type_cross_check() -> None:
    good = _card(1, atk_type=AtkType.STRIKE)
    assert good.expected_atk_type is AtkType.STRIKE
    assert good.atk_type_matches_number()
    bad = _card(1, atk_type=AtkType.GRAPPLE)
    assert not bad.atk_type_matches_number()


def test_card_finish_bonus_lookup_and_normalization() -> None:
    a = _card(28, finish_bonuses=((Skill.GRAPPLE, 2), (Skill.STRIKE, 1)))
    b = _card(28, finish_bonuses=((Skill.STRIKE, 1), (Skill.GRAPPLE, 2)))
    # order-independent equality + hashing after normalization
    assert a == b
    assert hash(a) == hash(b)
    assert a.bonus_for(Skill.STRIKE) == 1
    assert a.bonus_for(Skill.GRAPPLE) == 2
    assert a.bonus_for(Skill.POWER) == 0


def test_card_round_trip_with_effects() -> None:
    card = _card(
        28,
        atk_type=AtkType.STRIKE,
        play_order=PlayOrder.FINISH,
        finish_bonuses=((Skill.STRIKE, 1), (Skill.SUBMISSION, 3)),
        tags=("Super Lucha",),
        raw_text="+1 to Strike\n+3 to Submission",
        effects=(Effect(trigger=OnPlay(), actions=(BuffSkill(Skill.STRIKE, 1),)),),
    )
    restored = Card.from_dict(card.to_dict())
    assert restored == card
    assert restored.effects[0].actions[0] == BuffSkill(Skill.STRIKE, 1)


def test_card_is_hashable() -> None:
    assert len({_card(1), _card(1), _card(2)}) == 2


# --- Competitor ------------------------------------------------------------


def test_competitor_round_trip() -> None:
    comp = Competitor(
        db_uuid="c-uuid",
        name="Soborno",
        division="World Championship",
        stats=Stats(power=5, technique=9, agility=10, strike=8, submission=6, grapple=7),
        gimmick_text="When you roll Strike, Grapple, or Submission...",
        effects=(Effect(trigger=OnPlay(), actions=(BuffSkill(Skill.POWER, 1),)),),
        related_finishes=("f1", "f2", "f3"),
    )
    assert Competitor.from_dict(comp.to_dict()) == comp


def test_competitor_is_hashable() -> None:
    comp = Competitor("c", "n", "d", _bull_stats())
    assert comp in {comp}


# --- EntranceCard ----------------------------------------------------------


def test_entrance_round_trip() -> None:
    entrance = EntranceCard(
        db_uuid="e-uuid",
        name="Calling in Kanik",
        raw_text="Some entrance text.",
        effects=(Effect(trigger=OnPlay()),),
    )
    assert EntranceCard.from_dict(entrance.to_dict()) == entrance


# --- Deck ------------------------------------------------------------------


def test_valid_deck_passes_integrity() -> None:
    deck = _full_deck()
    assert deck.validate() == []
    assert deck.is_valid()
    assert deck.card_by_number(1) is not None
    assert deck.card_by_number(30) is not None
    assert deck.card_by_number(31) is None


def test_deck_round_trip() -> None:
    deck = _full_deck()
    assert Deck.from_dict(deck.to_dict()) == deck


def test_deck_wrong_count_flagged() -> None:
    deck = Deck(
        _full_deck().competitor, _full_deck().entrance, tuple(_card(n) for n in range(1, 30))
    )
    problems = deck.validate()
    assert any("expected 30 cards" in p for p in problems)
    assert any("missing card numbers" in p for p in problems)
    assert not deck.is_valid()


def test_deck_duplicate_number_flagged() -> None:
    cards = list(_full_deck().cards)
    cards[29] = _card(1)  # replace #30 with a second #1
    deck = Deck(_full_deck().competitor, _full_deck().entrance, tuple(cards))
    problems = deck.validate()
    assert any("duplicate card numbers: [1]" in p for p in problems)
    assert any("missing card numbers: [30]" in p for p in problems)


def test_deck_is_hashable() -> None:
    deck = _full_deck()
    assert deck in {deck}
