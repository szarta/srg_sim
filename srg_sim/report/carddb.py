"""Card-DB lookup for the matchup report (competitors, finishes, logoless pool).

A thin layer over :class:`srg_sim.loader.CardIndex` that resolves a competitor by
name (exact, then substring), pulls its signature finishes via ``related_finishes``,
and caches the generic **Logoless** finish pool. Competitor gimmicks and finish
bonuses are compiled through the *real* rules parser (:mod:`srg_sim.rules_parser`),
so downstream classification and finish math see validated IR rather than a regex.
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import Any

from srg_sim import rules_parser as rp
from srg_sim.cards import Competitor, Skill, Stats
from srg_sim.effects import EffectSource
from srg_sim.loader import DEFAULT_CARDS_YAML, CardIndex, LoaderError

# Competitor cards come in single / tag-team (Tornado) / trio variants; all six
# carry the skill line the report needs.
_COMP_TYPES = ("SingleCompetitorCard", "TornadoCompetitorCard", "TrioCompetitorCard")
_SKILL_KEYS = ("power", "technique", "agility", "submission", "grapple", "strike")
_FINISH_TYPES = ("Strike", "Grapple", "Submission")
LOGOLESS_TAG = "Logoless"


@dataclass(frozen=True)
class FinishRecord:
    """A finish card as the report needs it (name, type, text, provenance)."""

    db_uuid: str
    name: str
    atk_type: str
    deck_card_number: int | None
    rules_text: str
    tags: tuple[str, ...]
    srg_url: str | None


class ReportCardDB:
    """Report-oriented view over a ``cards.yaml`` export."""

    def __init__(self, index: CardIndex) -> None:
        self.index = index
        self.overrides = rp.load_overrides()
        self._logoless: dict[str, list[FinishRecord]] | None = None

    @classmethod
    def from_yaml(cls, path: str | Path = DEFAULT_CARDS_YAML) -> ReportCardDB:
        return cls(CardIndex.from_yaml(path))

    # -- competitor resolution --------------------------------------------

    def resolve_competitor(self, name_or_uuid: str) -> Competitor:
        """Resolve a competitor by db_uuid, exact name, or unique substring, and
        return it with its gimmick compiled to IR (via ``enrich_competitor``)."""
        rec = self._resolve_comp_record(name_or_uuid)
        return rp.enrich_competitor(_competitor_from_record(rec), self.overrides)

    def _resolve_comp_record(self, ref: str) -> dict[str, Any]:
        comps = [r for r in self.index.records if r.get("card_type") in _COMP_TYPES]
        by_uuid = next((r for r in comps if r.get("db_uuid") == ref), None)
        if by_uuid is not None:
            return by_uuid
        low = ref.lower()
        exact = [r for r in comps if str(r.get("name", "")).lower() == low]
        if exact:
            return exact[0]
        subs = [r for r in comps if low in str(r.get("name", "")).lower()]
        if len(subs) == 1:
            return subs[0]
        if not subs:
            raise LoaderError(f"no competitor matching {ref!r}")
        names = ", ".join(sorted(str(r.get("name")) for r in subs))
        raise LoaderError(f"ambiguous competitor {ref!r}; candidates: {names}")

    # -- finishes ----------------------------------------------------------

    def finishes_for(self, comp: Competitor) -> list[FinishRecord]:
        """The competitor's signature finishes, resolved from ``related_finishes``
        and ordered by deck-card number (28/29/30 = Strike/Grapple/Submission)."""
        out = [self._finish_record(uuid) for uuid in comp.related_finishes]
        found = [f for f in out if f is not None]
        return sorted(found, key=lambda f: f.deck_card_number or 0)

    def logoless_finishes(self) -> dict[str, list[FinishRecord]]:
        """Generic ``Logoless``-tagged finishes, grouped by attack type (cached)."""
        if self._logoless is None:
            pool: dict[str, list[FinishRecord]] = {t: [] for t in _FINISH_TYPES}
            for rec in self.index.records:
                if rec.get("play_order") == "Finish" and LOGOLESS_TAG in (rec.get("tags") or []):
                    fr = _finish_record_from(rec)
                    if fr.atk_type in pool:
                        pool[fr.atk_type].append(fr)
            self._logoless = pool
        return self._logoless

    def _finish_record(self, uuid: str) -> FinishRecord | None:
        rec = self.index._by_uuid.get(uuid)  # noqa: SLF001 (report reads the raw index)
        return _finish_record_from(rec) if rec else None

    def finish_bonus(self, fr: FinishRecord) -> dict[str, int]:
        """The finish's combo bonus as ``{skill_value: delta}`` for ``finish_odds``.

        Compiled through the parser (``FinishBonus`` actions), so it matches how the
        engine scores the finish — not an ad-hoc "+N to <skill>" regex."""
        effects = rp.parse_text(fr.rules_text, EffectSource.CARD, fr.db_uuid, self.overrides)
        return {skill.value: delta for skill, delta in rp.finish_bonuses(effects)}


def _competitor_from_record(rec: dict[str, Any]) -> Competitor:
    missing = [k for k in _SKILL_KEYS if rec.get(k) is None]
    if missing:
        raise LoaderError(f"competitor {rec.get('name')!r} is missing skills: {missing}")
    stats = Stats(**{k: int(rec[k]) for k in _SKILL_KEYS})
    return Competitor(
        db_uuid=str(rec["db_uuid"]),
        name=str(rec["name"]),
        division=str(rec.get("division") or ""),
        stats=stats,
        gimmick_text=str(rec.get("rules_text") or ""),
        effects=(),
        related_finishes=tuple(rec.get("related_finishes") or ()),
    )


def _finish_record_from(rec: dict[str, Any]) -> FinishRecord:
    number = rec.get("deck_card_number")
    return FinishRecord(
        db_uuid=str(rec["db_uuid"]),
        name=str(rec.get("name") or ""),
        atk_type=str(rec.get("atk_type") or ""),
        deck_card_number=int(number) if number is not None else None,
        rules_text=str(rec.get("rules_text") or ""),
        tags=tuple(rec.get("tags") or ()),
        srg_url=(str(rec["srg_url"]) if rec.get("srg_url") else None),
    )


def stat_dict(comp: Competitor) -> dict[str, int]:
    """The competitor's six skills as ``{"Power": v, ...}`` (the odds-engine shape)."""
    return comp.stats.to_dict()


def skill_from_value(value: str) -> Skill:
    """``"Power"`` -> :class:`Skill` (raises ``KeyError`` on an unknown skill)."""
    return {s.value: s for s in Skill}[value]
