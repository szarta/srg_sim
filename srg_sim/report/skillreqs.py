"""Skill-requirement payoff cards a competitor can run.

Some main-deck cards are gated behind a printed ``Skill Requirement: <Skill> N+``
(a deck-build constraint, recognized as metadata by the rules parser). A
competitor with a high skill unlocks the exclusive, high-requirement payoffs; this
surfaces the top few such cards a competitor *satisfies*, ranked by how demanding
the requirement is (the more exclusive, the more it defines a build).
"""

from __future__ import annotations

import re
from dataclasses import dataclass

from srg_sim.cards import Competitor
from srg_sim.report.carddb import ReportCardDB

_REQ_MARKER = re.compile(r"Skill Requirement:\s*(.+)", re.I)
_REQ_PART = re.compile(r"(Power|Technique|Agility|Submission|Grapple|Strike)\s*(\d+)\+", re.I)


@dataclass(frozen=True)
class SkillReqCard:
    """A main card with its parsed skill requirement(s)."""

    name: str
    atk_type: str
    play_order: str
    requirements: tuple[tuple[str, int], ...]  # (skill, threshold)
    db_uuid: str

    @property
    def max_req(self) -> int:
        return max(n for _, n in self.requirements)


def parse_requirements(text: str) -> tuple[tuple[str, int], ...]:
    """Parse ``Skill Requirement: Strike 10+, Agility 9+`` into ``(("Strike",10),...)``."""
    reqs: list[tuple[str, int]] = []
    for line in text.splitlines():
        marker = _REQ_MARKER.search(line)
        if marker:
            for skill, n in _REQ_PART.findall(marker.group(1)):
                reqs.append((skill.capitalize(), int(n)))
    return tuple(reqs)


def all_skill_req_cards(db: ReportCardDB) -> list[SkillReqCard]:
    """Every main-deck card carrying a skill requirement (parsed from its text)."""
    out: list[SkillReqCard] = []
    for rec in db.index.records:
        if rec.get("card_type") != "MainDeckCard":
            continue
        reqs = parse_requirements(str(rec.get("rules_text") or ""))
        if reqs:
            out.append(
                SkillReqCard(
                    name=str(rec.get("name") or ""),
                    atk_type=str(rec.get("atk_type") or ""),
                    play_order=str(rec.get("play_order") or ""),
                    requirements=reqs,
                    db_uuid=str(rec.get("db_uuid") or ""),
                )
            )
    return out


def top_for(db: ReportCardDB, comp: Competitor, limit: int = 5) -> list[SkillReqCard]:
    """The skill-requirement payoffs ``comp`` most wants to run (top ``limit``).

    Playable = the competitor meets every requirement. Ranked to lean into the
    competitor's identity: cards gated on a skill the competitor is *strong* in come
    first (by their value in the required skill), then by how demanding the
    requirement is (exclusivity), then gated-skill count, then name."""
    stats = comp.stats.to_dict()
    playable = [
        card
        for card in all_skill_req_cards(db)
        if all(stats[skill] >= n for skill, n in card.requirements)
    ]
    playable.sort(
        key=lambda c: (
            -max(stats[skill] for skill, _ in c.requirements),  # leverages a top skill
            -c.max_req,  # more exclusive requirement
            -len(c.requirements),
            c.name,
        )
    )
    return playable[:limit]
