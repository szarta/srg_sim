"""The three card enums the Effect IR references (lifted from the retired oracle's
``srg_sim/cards.py``). Kept minimal — the override expander needs only these."""

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
