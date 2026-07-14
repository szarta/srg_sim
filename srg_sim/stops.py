"""Skill-stop online logic — PORTED from ``fae_comp/skill_stops.py`` (DESIGN.md §6).

Cards 13 / 14 / 15 are Follow Ups whose stop is only "online" depending on
skills. Each is keyed to a pair of skills; the three pairs partition all six
skills. **Do not re-derive this**; keep it in parity with the source.

RPS: Strike stops Grapple, Grapple stops Submission, Submission stops Strike, so
each finish type is stopped by exactly one skill-stop card:

* Grapple finish    <- card 13 (Strike-type),     pair (Strike, Agility)
* Submission finish <- card 14 (Grapple-type),    pair (Power, Grapple)
* Strike finish     <- card 15 (Submission-type), pair (Technique, Submission)

A stop comes online via any of:

* beat-opponent : your keyed skill value  >  opponent's SAME skill (strict)
* equal-8       : one paired skill >= 8, then your OWN two skills compared
* Colossal Smash: card 14 only; Power 10 AND Grapple 9 -> guaranteed stop-Submission

Skills are passed as ``{skill_name: value}`` mappings (what ``Stats.to_dict()``
produces).
"""

from __future__ import annotations

from collections.abc import Mapping
from fractions import Fraction
from typing import TypedDict

SKILLS = ["Power", "Agility", "Technique", "Submission", "Grapple", "Strike"]

# finish_type -> (card number, (skillX, skillY))  where the pair is the two
# skills that printing of the card can be keyed to.
STOP_CARDS: dict[str, tuple[int, tuple[str, str]]] = {
    "Grapple": (13, ("Strike", "Agility")),
    "Submission": (14, ("Power", "Grapple")),
    "Strike": (15, ("Technique", "Submission")),
}


class StopEvaluation(TypedDict):
    """Result of :func:`evaluate_stop` (mirrors the source's dict)."""

    card: int
    pair: tuple[str, str]
    online: bool
    reasons: list[str]
    offline_notes: list[str]
    best_beat_key: tuple[str, int]
    random_online_prob: Fraction


def evaluate_stop(
    defender: Mapping[str, int],
    finish_type: str,
    opponent: Mapping[str, int] | None = None,
) -> StopEvaluation:
    """Evaluate the defender's skill stop against a finish of ``finish_type``.

    If ``opponent`` is None, only matchup-independent variants (equal-8,
    Colossal) are decided; beat-opponent is reported as a probability vs a random
    opponent via ``random_online_prob``.
    """
    d = defender
    o = opponent
    card, (x, y) = STOP_CARDS[finish_type]
    reasons: list[str] = []
    offline: list[str] = []

    # --- beat-opponent variants (one per paired skill) ---
    for k in (x, y):
        if o is not None:
            if d[k] > o[k]:
                reasons.append(f"beat-opp: your {k} {d[k]} > their {k} {o[k]}")
            else:
                offline.append(f"beat-opp {k}: {d[k]} !> {o[k]}")

    # --- equal-8 variants (self-referential): req A>=8, then B>A ---
    for a, b in ((x, y), (y, x)):
        if d[a] >= 8:
            if d[b] > d[a]:
                reasons.append(f"equal-8 (req {a}>=8): your {b} {d[b]} > your {a} {d[a]}")
            else:
                offline.append(f"equal-8 (req {a}>=8): {b} {d[b]} !> {a} {d[a]}")
        else:
            offline.append(f"equal-8 (req {a}>=8): {a}={d[a]} fails requirement")

    # --- Colossal Smash (card 14 only) ---
    if finish_type == "Submission":
        if d["Power"] == 10 and d["Grapple"] == 9:
            # Power >= opp Power; Power is 10 so always satisfied
            reasons.append("Colossal Smash: Power 10 >= opp Power (guaranteed)")
        else:
            offline.append(
                f"Colossal Smash: needs Power 10 & Grapple 9 (have {d['Power']}/{d['Grapple']})"
            )

    # --- vs-random-opponent probability for the best beat-opp key ---
    best_key = x if d[x] >= d[y] else y
    rand_prob = Fraction(max(0, d[best_key] - 5), 6)  # opp same-skill ~ U{5..10}

    return {
        "card": card,
        "pair": (x, y),
        "online": bool(reasons),
        "reasons": reasons,
        "offline_notes": offline,
        "best_beat_key": (best_key, d[best_key]),
        "random_online_prob": rand_prob,
    }
