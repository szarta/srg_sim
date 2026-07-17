"""Render a matchup to a one-page "scouting card" RST (single-page PDF via xelatex).

A compact side-by-side of the two competitors — turn-roll win %, the best finish to
throw with its early Crowd-Meter (CM0–2) odds, open finish lanes, and premium skill-
stop access (the "13/14/15" stops — Al13N Invasion / Beg for Mercy / Sealed Away — and
the best Equal-8 stop). This is the at-a-glance companion to the full multi-page report
(:mod:`srg_sim.report.render`); it holds the same numbers, laid out for one glance.
"""

from __future__ import annotations

from collections.abc import Mapping

from srg_sim.report.model import CompetitorReport, MatchupData
from srg_sim.report.render import _verdict_role
from srg_sim.report.skillreqs import GlanceStops, StopAccess, glance_stops

_ROLES = ("fav", "lean", "even", "unfav")
_GLANCE_CMS = (0, 1, 2)  # the contested early Crowd Meter — where finishes are decided


def render_glance(data: MatchupData, images: Mapping[str, str] | None = None) -> str:
    """The one-page ``index.rst`` for a matchup scouting card."""
    img = images or {}
    a_stops = glance_stops(data.a.comp, data.b.comp)
    b_stops = glance_stops(data.b.comp, data.a.comp)
    lines: list[str] = []
    lines += _heading(f"Scouting Card — {data.title}", "=")
    lines += _roles_block()
    lines += _caveat(data)
    lines += _table(data, img, a_stops, b_stops)
    lines += _footnote()
    return "\n".join(lines) + "\n"


def _heading(text: str, char: str) -> list[str]:
    return [text, char * len(text), ""]


def _roles_block() -> list[str]:
    lines = [f".. role:: {role}" for role in _ROLES] + [""]
    return lines + [
        ".. raw:: html",
        "",
        "   <style>",
        "   .fav{color:#1F7A43;font-weight:bold}",
        "   .lean{color:#4E8A2E;font-weight:bold}",
        "   .even{color:#9A6A12}",
        "   .unfav{color:#A5362B;font-weight:bold}",
        "   </style>",
        "",
    ]


def _caveat(data: MatchupData) -> list[str]:
    full = [s.comp.name for s in (data.a, data.b) if s.gimmick_fully_unmodeled]
    partial = [s.comp.name for s in (data.a, data.b) if s.gimmick_partial]
    out: list[str] = []
    if full:
        out += [
            f".. note:: {' and '.join(full)}'s gimmick is not yet modeled — the odds "
            "below reflect the base stat line only.",
            "",
        ]
    if partial:
        out += [
            f".. note:: {' and '.join(partial)}'s gimmick is modeled except for one "
            "clause not yet counted (its full text is on the card).",
            "",
        ]
    return out


def _table(
    data: MatchupData, images: Mapping[str, str], a_stops: GlanceStops, b_stops: GlanceStops
) -> list[str]:
    lines = [".. list-table::", "   :header-rows: 1", "   :stub-columns: 1", "   :widths: 16 42 42", ""]
    lines += _row("", f"**{data.a.comp.name}**", f"**{data.b.comp.name}**")
    lines += _row("", _image(data.a.image_uuid, images), _image(data.b.image_uuid, images))
    lines += _row("Turn roll", _turn(data.a), _turn(data.b))
    lines += _row("Best finish", _best_finish(data.a), _best_finish(data.b))
    lines += _row("Open lanes", _open_lanes(data.a), _open_lanes(data.b))
    lines += _row("Big skill stops", _big_stops(a_stops), _big_stops(b_stops))
    lines += _row("Equal-8 stop", _equal8(a_stops), _equal8(b_stops))
    return lines + [""]


def _row(label: str, a: str, b: str) -> list[str]:
    """One list-table row; a cell may be multi-line (indented continuation)."""
    return [f"   * - {label}", *_cell(a), *_cell(b)]


def _cell(text: str) -> list[str]:
    first, *rest = text.split("\n")
    return [f"     - {first}", *(f"       {ln}" for ln in rest)]


def _image(uuid: str, images: Mapping[str, str]) -> str:
    rel = images.get(uuid)
    return f".. image:: {rel}\n   :height: 150px" if rel else "—"


def _turn(cr: CompetitorReport) -> str:
    return f":{_verdict_role(cr.turn_win)}:`{cr.turn_win:.0%}`"


def _best_finish(cr: CompetitorReport) -> str:
    ml = cr.most_open
    if ml is None or ml.best is None:
        return "walled — nothing lands cleanly"
    best = ml.best
    tag = " *(logoless)*" if not best.is_signature else ""
    lane = "open" if ml.open_lane else "contested"
    odds = " · ".join(f"CM{cm} :{_verdict_role(best.odds_at(cm))}:`{best.odds_at(cm):.0%}`" for cm in _GLANCE_CMS)
    return f"**{best.finish.name}** ({ml.atk_type}, {lane}){tag}\n{odds}"


def _open_lanes(cr: CompetitorReport) -> str:
    lanes = [ln.atk_type for ln in cr.finish_lines if ln.open_lane]
    return ", ".join(lanes) if lanes else "none — every lane is stoppable"


def _big_stops(stops: GlanceStops) -> str:
    if not stops.big:
        return "none runnable"
    return ", ".join(_stop_label(s) for s in stops.big)


def _equal8(stops: GlanceStops) -> str:
    return _stop_label(stops.equal8) if stops.equal8 else "—"


_LIVE = {True: "online", False: "offline", None: "situational"}


def _stop_label(stop: StopAccess) -> str:
    role = {"online": "fav", "offline": "unfav", "situational": "even"}[_LIVE[stop.live]]
    short = stop.name.split(" (")[0]  # drop the "(Skill)" suffix for compactness
    return f"{short} :{role}:`{_LIVE[stop.live]}`"


def _footnote() -> list[str]:
    return [
        ".. raw:: latex",
        "",
        "   \\vfill",
        "",
        "*Turn-roll win % and finish odds use the validated finish/breakout math; "
        "finish odds shown at the contested early Crowd Meter (CM0–2). Big skill stops: "
        "Al13N Invasion (#13), Beg for Mercy (#15), Sealed Away (#20).*",
        "",
    ]
