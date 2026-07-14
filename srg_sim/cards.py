"""Domain model: Card, Competitor, EntranceCard, Deck, and enums (DESIGN.md §2).

Only the shared **enums** are implemented so far — the Effect IR (``effects.py``)
depends on them. The ``Card`` / ``Competitor`` / ``EntranceCard`` / ``Deck``
dataclasses are the subject of a later milestone task and remain unimplemented.

Enum *values* are the exact strings the card database uses (see
``srg_card_search_website``), so cards round-trip without translation.
"""

from __future__ import annotations

from enum import Enum


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
