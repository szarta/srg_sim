"""Curated priority for the skill-requirement "tech" cards a competitor should run.

Deckbuilding allows only two skill-requirement cards, so rather than list every
card with a ``Skill Requirement:`` line, the report ranks a hand-curated set of
high-impact cards (``skill_cards.yaml``): auto-include payoffs first, then the
Equal-8 skill stops (critical in the equal-stat matchups). For each, it reports
whether the competitor can *run* it (meets the requirement) and whether its stop is
*live* for this competitor's stat line / this matchup.
"""

from __future__ import annotations

import re
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import yaml

from srg_sim.cards import Competitor

SKILL_CARDS_YAML = Path(__file__).resolve().parent / "skill_cards.yaml"

_REQ_MARKER = re.compile(r"Skill Requirement:\s*(.+)", re.I)
_REQ_PART = re.compile(r"(Power|Technique|Agility|Submission|Grapple|Strike)\s*(\d+)\+", re.I)
_TIER_RANK = {"auto": 0, "equal8": 1}
MAX_SKILL_REQ_CARDS = 2  # deckbuilding limit


@dataclass(frozen=True)
class PriorityCard:
    """A curated tech card the competitor can run, with its live/priority status."""

    name: str
    tier: str  # "auto" | "equal8"
    requirements: tuple[tuple[str, int], ...]
    live: bool | None  # stop online for this comp/matchup; None = situational

    @property
    def req_str(self) -> str:
        return ", ".join(f"{skill} {n}+" for skill, n in self.requirements)


def parse_requirements(text: str) -> tuple[tuple[str, int], ...]:
    """Parse ``Skill Requirement: Strike 10+, Agility 9+`` into ``(("Strike",10),...)``."""
    reqs: list[tuple[str, int]] = []
    for line in text.splitlines():
        marker = _REQ_MARKER.search(line)
        if marker:
            for skill, n in _REQ_PART.findall(marker.group(1)):
                reqs.append((skill.capitalize(), int(n)))
    return tuple(reqs)


def load_priority(path: str | Path = SKILL_CARDS_YAML) -> dict[str, Any]:
    """Load the curated priority table (``priority`` list + ``personal_choice``)."""
    return yaml.safe_load(Path(path).read_text()) or {}


def _stat_of(token: str, me: dict[str, int], opp: dict[str, int]) -> int:
    return opp[token[4:]] if token.startswith("opp:") else me[token]


def _online_holds(online: list[Any], me: dict[str, int], opp: dict[str, int]) -> bool:
    left, op, right = online
    lv, rv = _stat_of(left, me, opp), _stat_of(right, me, opp)
    return lv >= rv if op == "ge" else lv > rv


def priority_cards(
    me: Competitor, opp: Competitor, entries: list[dict[str, Any]]
) -> list[PriorityCard]:
    """Runnable curated cards for ``me`` vs ``opp``, ranked by tier then live-ness."""
    ms, os_ = me.stats.to_dict(), opp.stats.to_dict()
    out: list[PriorityCard] = []
    for entry in entries:
        reqs = tuple((k, int(v)) for k, v in (entry.get("requires") or {}).items())
        if not all(ms[skill] >= n for skill, n in reqs):
            continue  # can't run it
        online = entry.get("online")
        live = _online_holds(online, ms, os_) if online else None
        out.append(PriorityCard(str(entry["name"]), str(entry.get("tier", "")), reqs, live))
    out.sort(key=lambda c: (_TIER_RANK.get(c.tier, 9), _live_rank(c.live), -_req_height(c), c.name))
    return out


def _live_rank(live: bool | None) -> int:
    return {True: 0, None: 1, False: 2}[live]


def _req_height(card: PriorityCard) -> int:
    return max((n for _, n in card.requirements), default=0)


def top_for(me: Competitor, opp: Competitor, limit: int = 6) -> list[PriorityCard]:
    """The competitor's best runnable curated tech cards vs ``opp`` (top ``limit``)."""
    return priority_cards(me, opp, load_priority().get("priority", []))[:limit]


def personal_choice(table: dict[str, Any] | None = None) -> tuple[str, ...]:
    """The no-requirement disruption Leads (Apocalypse / Rejected), a standing note."""
    return tuple((table or load_priority()).get("personal_choice", []))


@dataclass(frozen=True)
class StopAccess:
    """One premium stop a competitor can run, with its online status for the matchup."""

    name: str
    live: bool | None  # True=online, False=runnable-but-offline, None=situational (board-gated)


@dataclass(frozen=True)
class GlanceStops:
    """The scouting one-pager's stop summary for one competitor vs an opponent."""

    big: tuple[StopAccess, ...]  # runnable premium stops (Al13N / Beg for Mercy / Sealed Away)
    equal8: StopAccess | None  # best runnable Equal-8 stop (online first), if any


def glance_stops(
    me: Competitor, opp: Competitor, table: dict[str, Any] | None = None
) -> GlanceStops:
    """The at-a-glance stop access for ``me`` vs ``opp``: which premium "13/14/15"
    stops are runnable (with online status) and the best runnable Equal-8 stop."""
    tbl = table or load_priority()
    ms, os_ = me.stats.to_dict(), opp.stats.to_dict()
    big: list[StopAccess] = []
    for entry in tbl.get("glance_skill_stops", []):
        reqs = (entry.get("requires") or {}).items()
        if not all(ms[skill] >= int(n) for skill, n in reqs):
            continue  # can't run it on this stat line
        online = entry.get("online")
        live = _online_holds(online, ms, os_) if online else None
        big.append(StopAccess(str(entry["name"]), live))
    equal8 = [e for e in tbl.get("priority", []) if e.get("tier") == "equal8"]
    ranked = priority_cards(me, opp, equal8)  # runnable, sorted online-first
    best = ranked[0] if ranked else None
    return GlanceStops(tuple(big), StopAccess(best.name, best.live) if best else None)
