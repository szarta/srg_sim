"""Matchup-report generator: two competitors -> a Sphinx (HTML + xelatex PDF) report.

Phase 1 (the quantitative core) reuses the validated odds engines
(:mod:`srg_sim.finish`, :mod:`srg_sim.stops`, :mod:`srg_sim.engine`) and the card
domain (:mod:`srg_sim.loader`, :mod:`srg_sim.rules_parser`) — it never re-derives
the math. See the plan and DESIGN.md §9. The public entry point is
:func:`srg_sim.report.build.build_report`, driven by ``srg-sim report A B``.
"""

from __future__ import annotations

from srg_sim.report.carddb import FinishRecord, ReportCardDB
from srg_sim.report.model import CompetitorReport, MatchupData

__all__ = ["FinishRecord", "ReportCardDB", "CompetitorReport", "MatchupData"]
