"""Assemble the per-competitor and whole-matchup report data.

Pulls together the pieces each report section needs — comp-type, turn-roll odds,
signature finish curves, per-type finish lines, the most-open line, and skill-
requirement payoffs — into plain frozen dataclasses the renderer consumes. This is
the boundary between "compute the numbers" and "lay them out"; it holds no RST.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any

from srg_sim import effects as fx
from srg_sim.cards import Competitor
from srg_sim.report import classify, finishes, skillreqs
from srg_sim.report.carddb import ReportCardDB
from srg_sim.report.classify import CompType
from srg_sim.report.finishes import FinishLine, FinishOption
from srg_sim.report.skillreqs import PriorityCard
from srg_sim.report.turn import TurnOdds, turn_odds


@dataclass(frozen=True)
class CompetitorReport:
    """One side of the matchup, from that competitor's point of view."""

    comp: Competitor
    comp_type: CompType
    turn_win: float  # this competitor's chance to win a turn roll vs the opponent
    signature_finishes: list[FinishOption]  # each with its CM curve (section 8)
    finish_lines: list[FinishLine]  # per-type signature + logoless-if-better + stop
    most_open: FinishLine | None
    skill_req_cards: list[PriorityCard]  # curated tech cards this comp can run (ranked)
    personal_cards: tuple[str, ...] = ()  # no-requirement disruption Leads (standing note)
    unsupported_gimmick: tuple[str, ...] = ()  # gimmick clauses the parser can't yet model
    notes: str = ""
    notable_cards: tuple[str, ...] = ()

    @property
    def image_uuid(self) -> str:
        return self.comp.db_uuid

    @property
    def gimmick_modeled(self) -> bool:
        """True when no gimmick clause is Unsupported — every clause is counted."""
        return not self.unsupported_gimmick

    @property
    def _has_modeled_effect(self) -> bool:
        """The gimmick has at least one clause the engine executes (not all Unsupported)."""
        return any(
            not all(isinstance(a, fx.Unsupported) for a in eff.actions) for eff in self.comp.effects
        )

    @property
    def gimmick_fully_unmodeled(self) -> bool:
        """No gimmick clause is modeled — odds/type reflect the base stat line only."""
        return bool(self.unsupported_gimmick) and not self._has_modeled_effect

    @property
    def gimmick_partial(self) -> bool:
        """Some clauses are modeled and counted, but at least one is still Unsupported."""
        return bool(self.unsupported_gimmick) and self._has_modeled_effect


@dataclass(frozen=True)
class MatchupData:
    """Both sides plus the shared turn-roll result and report parameters."""

    a: CompetitorReport
    b: CompetitorReport
    turn: TurnOdds
    cms: tuple[int, ...]
    seed: int
    _overrides: dict[str, Any] = field(default_factory=dict)

    @property
    def title(self) -> str:
        return f"{self.a.comp.name} vs {self.b.comp.name}"


def build_matchup(
    db: ReportCardDB,
    name_a: str,
    name_b: str,
    *,
    cms: tuple[int, ...] = (0, 1, 2, 3, 4, 5),
    mc_games: int = 50_000,
    seed: int = 11,
) -> MatchupData:
    """Resolve both competitors and compute every Phase-1 report section."""
    comp_a = db.resolve_competitor(name_a)
    comp_b = db.resolve_competitor(name_b)
    turn = turn_odds(comp_a, comp_b, mc_games=mc_games, seed=seed)
    overrides = classify.load_overrides()
    a = _side(db, comp_a, comp_b, turn.win_a, cms, overrides)
    b = _side(db, comp_b, comp_a, turn.win_b, cms, overrides)
    return MatchupData(a=a, b=b, turn=turn, cms=cms, seed=seed, _overrides=overrides)


def _side(
    db: ReportCardDB,
    me: Competitor,
    opp: Competitor,
    turn_win: float,
    cms: tuple[int, ...],
    overrides: dict[str, Any],
) -> CompetitorReport:
    lines = finishes.finish_lines(db, me, opp, cms)
    entry = _entry(me, overrides)
    return CompetitorReport(
        comp=me,
        comp_type=classify.classify(me, overrides),
        turn_win=turn_win,
        signature_finishes=finishes.signature_curves(db, me, opp, cms),
        finish_lines=lines,
        most_open=finishes.most_open_line(lines),
        skill_req_cards=skillreqs.top_for(me, opp),
        personal_cards=skillreqs.personal_choice(),
        unsupported_gimmick=_unsupported_clauses(me),
        notes=str(entry.get("notes") or ""),
        notable_cards=tuple(entry.get("notable_cards") or ()),
    )


def _unsupported_clauses(comp: Competitor) -> tuple[str, ...]:
    """Raw gimmick clauses that compiled to an ``Unsupported`` sentinel — surfaced so
    the report never silently understates a not-yet-modeled gimmick (DESIGN.md §4)."""
    out = [
        eff.raw_clause or action.raw_text
        for eff in comp.effects
        for action in eff.actions
        if isinstance(action, fx.Unsupported)
    ]
    return tuple(dict.fromkeys(out))


def _entry(comp: Competitor, overrides: dict[str, Any]) -> dict[str, Any]:
    for key in (comp.db_uuid, comp.name):
        if key in overrides:
            return overrides[key]
    return {}
