"""Synthetic demo decks for engine tests (until ``loader.py`` lands, Task 8).

Two legal 30-card decks (one card per number 1..30) built straight from the
domain model, so engine tests need no card DB. The cards follow the canonical
30-card map (SUPERSHOW_MECHANICS §3): attack type by ``n mod 3``; Leads
{1–12, 25–27}, Follow Ups {13–24}, Finishes {28–30}. Stops are **text-driven**,
so the demo cards carry the map's ``Stop`` effects (each stopping the type it
beats): 1–3 stop Leads, 4–6 stop Follow Ups, 13–15 skill stops (keyed on the
type-skill), 19–21 see-1 stops, 25–27 stop-any; 7–12/16–18/22–24 are plain.
Competitors are vanilla so the turn duel is the ≈50/50 baseline of DESIGN.md §11;
:func:`with_effects` adds gimmick effects for targeted tests.
"""

from __future__ import annotations

from srg_sim.cards import (
    Card,
    Competitor,
    Deck,
    EntranceCard,
    PlayOrder,
    Skill,
    Stats,
    atk_type_from_number,
)
from srg_sim.effects import (
    CardFilter,
    Comparator,
    Effect,
    HasInPlay,
    SkillCompare,
    Static,
    Stop,
    Vs,
    Who,
)

FINISH_NUMBERS = (28, 29, 30)
LEAD_NUMBERS = frozenset({*range(1, 13), 25, 26, 27})

BULL_STATS = Stats(power=10, technique=6, agility=5, submission=8, grapple=9, strike=7)
FAE_STATS = Stats(power=10, technique=7, agility=6, submission=9, grapple=5, strike=8)

_FINISH_BONUS = {
    28: (Skill.STRIKE, 2),  # 28 % 3 == 1 -> Strike
    29: (Skill.GRAPPLE, 2),  # 29 % 3 == 2 -> Grapple
    30: (Skill.SUBMISSION, 2),  # 30 % 3 == 0 -> Submission
}

# RPS: a card of a given attack type stops the type it beats.
_BEATS = {
    atk_type_from_number(1): atk_type_from_number(2),  # Strike -> Grapple
    atk_type_from_number(2): atk_type_from_number(3),  # Grapple -> Submission
    atk_type_from_number(3): atk_type_from_number(1),  # Submission -> Strike
}
# The type-skill a skill stop (13-15) keys on, by its own attack type.
_TYPE_SKILL = {
    atk_type_from_number(1): Skill.STRIKE,
    atk_type_from_number(2): Skill.GRAPPLE,
    atk_type_from_number(3): Skill.SUBMISSION,
}


def _order(number: int) -> PlayOrder:
    if number in FINISH_NUMBERS:
        return PlayOrder.FINISH
    return PlayOrder.LEAD if number in LEAD_NUMBERS else PlayOrder.FOLLOWUP


def _stop_effect(number: int) -> Effect | None:
    """The Stop effect a demo card carries, per the 30-card map (None = no stop)."""
    stopped = _BEATS[atk_type_from_number(number)]  # the type it beats
    if number <= 3:
        stop = Stop(order=PlayOrder.LEAD, atk_type=stopped)
        cond = None
    elif number <= 6:
        stop = Stop(order=PlayOrder.FOLLOWUP, atk_type=stopped)
        cond = None
    elif 13 <= number <= 15:  # skill stop: online iff your type-skill > opponent's
        keyed = _TYPE_SKILL[atk_type_from_number(number)]
        stop = Stop(atk_type=stopped)
        cond = SkillCompare(keyed, Comparator.GT, Who.SELF, Vs.OPP_SAME)
    elif 19 <= number <= 21:  # see-1: only if opp already has that type in play
        stop = Stop(atk_type=stopped)
        cond = HasInPlay(Who.OPP, CardFilter(atk_type=stopped))
    elif 25 <= number <= 27:  # stop-any, unconditional
        stop = Stop(atk_type=stopped)
        cond = None
    else:
        return None
    if cond is None:
        return Effect(trigger=Static(), actions=(stop,))
    return Effect(trigger=Static(), condition=cond, actions=(stop,))


def _card(side: str, number: int) -> Card:
    bonus = _FINISH_BONUS.get(number)
    stop = _stop_effect(number)
    return Card(
        db_uuid=f"{side}-{number:02d}",
        name=f"{side} Card {number}",
        number=number,
        atk_type=atk_type_from_number(number),
        play_order=_order(number),
        finish_bonuses=(bonus,) if bonus else (),
        effects=(stop,) if stop else (),
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
