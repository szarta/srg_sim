"""Synthetic demo decks for engine tests (until ``loader.py`` lands, Task 8).

Two legal 30-card decks (one card per number 1..30) built straight from the
domain model, so engine tests need no card DB. Attack types follow the
number rule; numbers 28-30 are Finishes carrying a matching finish bonus, 13-15
are the skill-stop Follow Ups, 1-12 are Leads, the rest Followups. Competitors
are vanilla (no gimmick effects) so a duel is the ≈50/50 baseline of DESIGN.md
§11; :func:`with_effects` adds effects for targeted tests.
"""

from __future__ import annotations

from srg_sim.cards import (
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
from srg_sim.effects import Effect

FINISH_NUMBERS = (28, 29, 30)

BULL_STATS = Stats(power=10, technique=6, agility=5, submission=8, grapple=9, strike=7)
FAE_STATS = Stats(power=10, technique=7, agility=6, submission=9, grapple=5, strike=8)

_FINISH_BONUS = {
    28: (Skill.STRIKE, 2),  # 28 % 3 == 1 -> Strike
    29: (Skill.GRAPPLE, 2),  # 29 % 3 == 2 -> Grapple
    30: (Skill.SUBMISSION, 2),  # 30 % 3 == 0 -> Submission
}


def _order(number: int) -> PlayOrder:
    if number in FINISH_NUMBERS:
        return PlayOrder.FINISH
    return PlayOrder.LEAD if number <= 12 else PlayOrder.FOLLOWUP


def _card(side: str, number: int) -> Card:
    bonus = _FINISH_BONUS.get(number)
    return Card(
        db_uuid=f"{side}-{number:02d}",
        name=f"{side} Card {number}",
        number=number,
        atk_type=atk_type_from_number(number),
        play_order=_order(number),
        finish_bonuses=(bonus,) if bonus else (),
    )


def make_deck(side: str, competitor: Competitor) -> Deck:
    """A legal 30-card deck for ``side`` fronted by ``competitor``."""
    entrance = EntranceCard(db_uuid=f"{side}-ent", name=f"{side} Entrance")
    cards = tuple(_card(side, n) for n in range(1, 31))
    return Deck(competitor=competitor, entrance=entrance, cards=cards)


def bull() -> Competitor:
    return Competitor(db_uuid="c-bull", name="The Bull", division="Worlds", stats=BULL_STATS)


def fae() -> Competitor:
    return Competitor(db_uuid="c-fae", name="Fae Dragon", division="Worlds", stats=FAE_STATS)


def with_effects(competitor: Competitor, effects: tuple[Effect, ...]) -> Competitor:
    """Copy ``competitor`` with gimmick ``effects`` attached (for targeted tests)."""
    from dataclasses import replace

    return replace(competitor, effects=effects)


def bull_vs_fae() -> tuple[Deck, Deck]:
    return make_deck("A", bull()), make_deck("B", fae())
