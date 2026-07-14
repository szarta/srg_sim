"""Domain model: Card, Competitor, EntranceCard, Deck, and enums (DESIGN.md §2).

Every type here is immutable, hashable, and serializable (``to_dict`` /
``from_dict``). Enum *values* are the exact strings the card database uses (see
``srg_card_search_website``), so cards round-trip without translation.

Cards, competitors, and entrances each carry compiled ``Effect`` IR
(``effects.py``). To avoid a circular import — ``effects`` imports the enums
from here — ``Effect`` is referenced only under ``TYPE_CHECKING`` and rebuilt via
a lazy import inside ``from_dict``. Nothing in this module imports ``effects`` at
runtime.
"""

from __future__ import annotations

from dataclasses import dataclass
from enum import Enum
from typing import TYPE_CHECKING, Any, cast

if TYPE_CHECKING:
    from srg_sim.effects import Effect


class Skill(Enum):
    """The six competitor skills (each competitor owns the values {5..10})."""

    POWER = "Power"
    AGILITY = "Agility"
    TECHNIQUE = "Technique"
    SUBMISSION = "Submission"
    GRAPPLE = "Grapple"
    STRIKE = "Strike"


class AtkType(Enum):
    """A card's attack type. RPS: Strike ▷ Grapple ▷ Submission ▷ Strike.

    ``NONE`` covers cards with no attack type (competitors, entrances).
    """

    STRIKE = "Strike"
    GRAPPLE = "Grapple"
    SUBMISSION = "Submission"
    NONE = "None"


class PlayOrder(Enum):
    """A card's ordering stage. ``NONE`` covers cards outside the ordering chain."""

    LEAD = "Lead"
    FOLLOWUP = "Followup"
    FINISH = "Finish"
    NONE = "None"


# Canonical skill ordering, used to normalize finish-bonus tuples so equal bonus
# sets compare equal regardless of the order they were supplied in.
_SKILL_INDEX = {skill: i for i, skill in enumerate(Skill)}


def atk_type_from_number(number: int) -> AtkType:
    """Attack type implied by a main-deck card number (DESIGN.md §2).

    ``n mod 3``: 1 → Strike, 2 → Grapple, 0 → Submission. Cards come in triples
    (one of each type per consecutive triple).
    """
    return (AtkType.SUBMISSION, AtkType.STRIKE, AtkType.GRAPPLE)[number % 3]


def _effects_to_list(effects: tuple[Effect, ...]) -> list[dict[str, Any]]:
    return [e.to_dict() for e in effects]


def _effects_from_list(raw: list[dict[str, Any]]) -> tuple[Effect, ...]:
    # Lazy import breaks the cards <-> effects import cycle at runtime.
    from srg_sim.effects import Effect as _Effect
    from srg_sim.effects import from_dict as _from_dict

    return tuple(cast("_Effect", _from_dict(item)) for item in raw)


@dataclass(frozen=True)
class Stats:
    """A competitor's six skill values (base, before any buffs)."""

    power: int
    agility: int
    technique: int
    submission: int
    grapple: int
    strike: int

    def get(self, skill: Skill) -> int:
        """The value for ``skill`` (``skill.name`` maps to the field name)."""
        return int(getattr(self, skill.name.lower()))

    def is_bijection_5_to_10(self) -> bool:
        """True iff the six values are exactly {5, 6, 7, 8, 9, 10} (DESIGN.md §2)."""
        return sorted(self.get(s) for s in Skill) == [5, 6, 7, 8, 9, 10]

    def to_dict(self) -> dict[str, int]:
        return {skill.value: self.get(skill) for skill in Skill}

    @classmethod
    def from_dict(cls, data: dict[str, int]) -> Stats:
        return cls(**{skill.name.lower(): data[skill.value] for skill in Skill})


@dataclass(frozen=True)
class Card:
    """A main-deck card (``number`` 1–30). ``finish_bonuses`` and ``effects`` are
    populated by the rules parser; the raw text is retained for audit.
    """

    db_uuid: str
    name: str
    number: int
    atk_type: AtkType
    play_order: PlayOrder
    finish_bonuses: tuple[tuple[Skill, int], ...] = ()
    tags: tuple[str, ...] = ()
    raw_text: str = ""
    effects: tuple[Effect, ...] = ()

    def __post_init__(self) -> None:
        # Normalize finish-bonus order so equal bonus sets are equal/hash alike.
        ordered = tuple(sorted(self.finish_bonuses, key=lambda pair: _SKILL_INDEX[pair[0]]))
        object.__setattr__(self, "finish_bonuses", ordered)

    def bonus_for(self, skill: Skill) -> int:
        """Finish bonus added when ``skill`` is rolled for the finish (0 if none)."""
        for bonus_skill, delta in self.finish_bonuses:
            if bonus_skill is skill:
                return delta
        return 0

    @property
    def expected_atk_type(self) -> AtkType:
        """The attack type implied by ``number`` (DESIGN.md §2 cross-check)."""
        return atk_type_from_number(self.number)

    def atk_type_matches_number(self) -> bool:
        """True iff ``atk_type`` agrees with ``number`` (loader logs mismatches)."""
        return self.atk_type is self.expected_atk_type

    def to_dict(self) -> dict[str, Any]:
        return {
            "db_uuid": self.db_uuid,
            "name": self.name,
            "number": self.number,
            "atk_type": self.atk_type.value,
            "play_order": self.play_order.value,
            "finish_bonuses": {skill.value: delta for skill, delta in self.finish_bonuses},
            "tags": list(self.tags),
            "raw_text": self.raw_text,
            "effects": _effects_to_list(self.effects),
        }

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> Card:
        bonuses = tuple((Skill(k), v) for k, v in data.get("finish_bonuses", {}).items())
        return cls(
            db_uuid=data["db_uuid"],
            name=data["name"],
            number=data["number"],
            atk_type=AtkType(data["atk_type"]),
            play_order=PlayOrder(data["play_order"]),
            finish_bonuses=bonuses,
            tags=tuple(data.get("tags", [])),
            raw_text=data.get("raw_text", ""),
            effects=_effects_from_list(data.get("effects", [])),
        )


@dataclass(frozen=True)
class Competitor:
    """A single competitor (one per side in a SingleCompetitor game)."""

    db_uuid: str
    name: str
    division: str
    stats: Stats
    gimmick_text: str = ""
    effects: tuple[Effect, ...] = ()
    related_finishes: tuple[str, ...] = ()

    def to_dict(self) -> dict[str, Any]:
        return {
            "db_uuid": self.db_uuid,
            "name": self.name,
            "division": self.division,
            "stats": self.stats.to_dict(),
            "gimmick_text": self.gimmick_text,
            "effects": _effects_to_list(self.effects),
            "related_finishes": list(self.related_finishes),
        }

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> Competitor:
        return cls(
            db_uuid=data["db_uuid"],
            name=data["name"],
            division=data["division"],
            stats=Stats.from_dict(data["stats"]),
            gimmick_text=data.get("gimmick_text", ""),
            effects=_effects_from_list(data.get("effects", [])),
            related_finishes=tuple(data.get("related_finishes", [])),
        )


@dataclass(frozen=True)
class EntranceCard:
    """A competitor's Entrance card (no attack type, no ordering stage)."""

    db_uuid: str
    name: str
    raw_text: str = ""
    effects: tuple[Effect, ...] = ()

    def to_dict(self) -> dict[str, Any]:
        return {
            "db_uuid": self.db_uuid,
            "name": self.name,
            "raw_text": self.raw_text,
            "effects": _effects_to_list(self.effects),
        }

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> EntranceCard:
        return cls(
            db_uuid=data["db_uuid"],
            name=data["name"],
            raw_text=data.get("raw_text", ""),
            effects=_effects_from_list(data.get("effects", [])),
        )


# A legal main deck holds exactly one card of each number 1..30.
DECK_SIZE = 30
_DECK_NUMBERS = frozenset(range(1, DECK_SIZE + 1))


@dataclass(frozen=True)
class Deck:
    """One side's deck: a competitor, an entrance, and exactly 30 cards.

    Format legality (card-pool rules) is **not** enforced here (DESIGN.md §2).
    """

    competitor: Competitor
    entrance: EntranceCard
    cards: tuple[Card, ...] = ()

    def validate(self) -> list[str]:
        """Return a list of integrity problems (empty means the deck is legal)."""
        problems: list[str] = []
        if len(self.cards) != DECK_SIZE:
            problems.append(f"expected {DECK_SIZE} cards, got {len(self.cards)}")
        numbers = [c.number for c in self.cards]
        present = set(numbers)
        missing = sorted(_DECK_NUMBERS - present)
        if missing:
            problems.append(f"missing card numbers: {missing}")
        if len(numbers) != len(present):
            dupes = sorted(n for n in present if numbers.count(n) > 1)
            problems.append(f"duplicate card numbers: {dupes}")
        return problems

    def is_valid(self) -> bool:
        return not self.validate()

    def card_by_number(self, number: int) -> Card | None:
        return next((c for c in self.cards if c.number == number), None)

    def to_dict(self) -> dict[str, Any]:
        return {
            "competitor": self.competitor.to_dict(),
            "entrance": self.entrance.to_dict(),
            "cards": [c.to_dict() for c in self.cards],
        }

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> Deck:
        return cls(
            competitor=Competitor.from_dict(data["competitor"]),
            entrance=EntranceCard.from_dict(data["entrance"]),
            cards=tuple(Card.from_dict(c) for c in data["cards"]),
        )
