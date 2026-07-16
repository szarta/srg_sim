"""Render :class:`MatchupData` to Sphinx RST (HTML + xelatex PDF).

Mirrors the section-emitter style of ``fae_comp/gen_pod_rst.py``: each section is a
small function appending lines, assembled into one ``index.rst``. Card images are
referenced as ``_images/<uuid>.png`` (converted by :mod:`srg_sim.report.build`);
an image absent from the supplied map is simply skipped, never a hard failure.
"""

from __future__ import annotations

from collections.abc import Mapping

from srg_sim.report.finishes import FinishOption
from srg_sim.report.model import CompetitorReport, MatchupData
from srg_sim.report.skillreqs import SkillReqCard

_ROLES = ("fav", "lean", "even", "unfav")


def render_report(data: MatchupData, images: Mapping[str, str] | None = None) -> str:
    """The full ``index.rst`` text for a matchup report."""
    img = images or {}
    lines: list[str] = []
    lines += _title(data.title)
    lines += _roles_block()
    lines += _intro(data)
    lines += _competitor_block(data.a, img)
    lines += ["", ".. raw:: latex", "", "   \\clearpage", ""]
    lines += _competitor_block(data.b, img)
    return "\n".join(lines) + "\n"


def _heading(text: str, char: str) -> list[str]:
    return [text, char * len(text), ""]


def _title(title: str) -> list[str]:
    return _heading(f"Supershow Matchup — {title}", "=")


def _roles_block() -> list[str]:
    lines = [f".. role:: {role}" for role in _ROLES] + [""]
    # PDF colors come from the conf.py \DUrole* macros; HTML needs matching CSS.
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


def _intro(data: MatchupData) -> list[str]:
    method = (
        "exact enumeration" if data.turn.method == "exact" else f"engine sim, n={data.turn.n:,}"
    )
    caveat = _gimmick_caveat(data)
    text = (
        f"Head-to-head odds for **{data.a.comp.name}** vs **{data.b.comp.name}**. "
        f"Finish success uses the validated finish/breakout math at Crowd Meter "
        f"{data.cms[0]}–{data.cms[-1]}, over the full pool (signatures + Logoless). "
        f"Turn-roll win % via {method}."
    )
    return [text, "", *caveat]


def _gimmick_caveat(data: MatchupData) -> list[str]:
    unmodeled = [s.comp.name for s in (data.a, data.b) if not s.gimmick_modeled]
    if not unmodeled:
        return []
    who = " and ".join(unmodeled)
    return [
        f".. warning:: {who}'s gimmick is not yet modeled by the rules parser, so the "
        "turn-roll odds and comp-type below reflect the **base stat line only** — the "
        "gimmick's effect is not counted. The raw gimmick text is shown in each section.",
        "",
    ]


def _competitor_block(cr: CompetitorReport, images: Mapping[str, str]) -> list[str]:
    stat = _statline(cr)
    lines = _heading(f"{cr.comp.name} — {stat}", "-")
    lines += _picture(cr.image_uuid, images)
    lines += _type_and_gimmick(cr)
    lines += _turn_line(cr)
    lines += _open_line(cr)
    lines += _stops_block(cr)
    lines += _finish_odds_section(cr, images)
    lines += _skillreq_section(cr)
    lines += _notes_section(cr)
    return lines


def _statline(cr: CompetitorReport) -> str:
    s = cr.comp.stats
    return (
        f"P{s.power} A{s.agility} T{s.technique} "
        f"Su{s.submission} G{s.grapple} St{s.strike}  ·  {cr.comp.division}"
    )


def _picture(uuid: str, images: Mapping[str, str]) -> list[str]:
    rel = images.get(uuid)
    if not rel:
        return []
    return [f".. image:: {rel}", "   :height: 260px", "   :align: left", ""]


def _type_and_gimmick(cr: CompetitorReport) -> list[str]:
    also = f" (also {', '.join(cr.comp_type.also)})" if cr.comp_type.also else ""
    src = "curated" if cr.comp_type.source == "override" else "auto"
    out = [f"**Type:** {cr.comp_type.label}{also} *({src})*", ""]
    if cr.comp.gimmick_text:
        out += ["**Gimmick:**", "", f"   {cr.comp.gimmick_text.strip()}", ""]
    if not cr.gimmick_modeled:
        out += ["*(gimmick not yet modeled — odds below are base-stat only)*", ""]
    return out


def _turn_line(cr: CompetitorReport) -> list[str]:
    role = _verdict_role(cr.turn_win)
    return [f"**Turn roll:** :{role}:`{cr.turn_win:.0%}` chance to win the roll-off.", ""]


def _verdict_role(win: float) -> str:
    if win >= 0.55:
        return "fav"
    if win >= 0.52:
        return "lean"
    if win >= 0.48:
        return "even"
    return "unfav"


def _open_line(cr: CompetitorReport) -> list[str]:
    ml = cr.most_open
    if ml is None or ml.best is None:
        return ["**Most open finish line:** walled — no finish lands cleanly.", ""]
    lane = "open lane" if ml.open_lane else "contested"
    best = ml.best
    tag = " *(logoless)*" if not best.is_signature else ""
    return [
        f"**Most open finish line:** {ml.atk_type} — **{best.finish.name}**{tag} "
        f"({lane}), {best.odds_at(max(best.curve)):.0%} at CM{max(best.curve)}.",
        "",
    ]


def _stops_block(cr: CompetitorReport) -> list[str]:
    parts = []
    for ln in cr.finish_lines:
        mark = "can stop" if ln.stop["online"] else "open"
        parts.append(f"{ln.atk_type}: {mark}")
    return ["**Their skill stops (this matchup):** " + "; ".join(parts) + ".", ""]


def _finish_odds_section(cr: CompetitorReport, images: Mapping[str, str]) -> list[str]:
    lines = _heading("Finish odds (CM1–5)", "~")
    if not cr.signature_finishes:
        return lines + ["*(no signature finishes on record.)*", ""]
    lines += _finish_table(cr, images)
    logo = _logoless_notes(cr)
    return lines + logo


def _finish_table(cr: CompetitorReport, images: Mapping[str, str]) -> list[str]:
    cms = sorted(next(iter(cr.signature_finishes)).curve)
    widths = "26 22 " + " ".join(["10"] * len(cms))
    header = ["   * - Finish", "     - Bonus"] + [f"     - CM{cm}" for cm in cms]
    lines = [".. list-table::", "   :header-rows: 1", f"   :widths: {widths}", ""]
    lines += header
    for opt in cr.signature_finishes:
        lines += _finish_row(opt, cms, images)
    return lines + [""]


def _finish_row(opt: FinishOption, cms: list[int], images: Mapping[str, str]) -> list[str]:
    label = f"**{opt.finish.name}** ({opt.finish.atk_type} #{opt.finish.deck_card_number})"
    rel = images.get(opt.finish.db_uuid)
    if rel:
        cell = [f"   * - .. image:: {rel}", "          :height: 110px", "", f"       {label}"]
    else:
        cell = [f"   * - {label}"]
    row = cell + [f"     - {_bonus_str(opt.bonus)}"]
    row += [f"     - {opt.odds_at(cm):.0%}" for cm in cms]
    return row


def _bonus_str(bonus: dict[str, int]) -> str:
    if not bonus:
        return "—"
    return " ".join(f"+{d} {sk[:2]}" for sk, d in sorted(bonus.items(), key=lambda kv: -kv[1]))


def _logoless_notes(cr: CompetitorReport) -> list[str]:
    better = [ln for ln in cr.finish_lines if ln.logoless is not None]
    if not better:
        return []
    out = ["**Better logoless alternatives:**", ""]
    for ln in better:
        logo, sig = ln.logoless, ln.signature
        assert logo is not None
        ref = max(logo.curve)
        base = f" (vs signature {sig.odds_at(ref):.0%})" if sig else ""
        out.append(
            f"* {ln.atk_type}: **{logo.finish.name}** {logo.odds_at(ref):.0%} at CM{ref}{base}."
        )
    return out + [""]


def _skillreq_section(cr: CompetitorReport) -> list[str]:
    lines = _heading("Key skill-requirement cards", "~")
    if not cr.skill_req_cards:
        return lines + ["*(none this competitor uniquely enables.)*", ""]
    lines += [".. list-table::", "   :header-rows: 1", "   :widths: 40 20 40", ""]
    lines += ["   * - Card", "     - Type", "     - Requirement"]
    for card in cr.skill_req_cards:
        lines += _skillreq_row(card)
    return lines + [""]


def _skillreq_row(card: SkillReqCard) -> list[str]:
    req = ", ".join(f"{skill} {n}+" for skill, n in card.requirements)
    kind = f"{card.atk_type} {card.play_order}".strip()
    return [f"   * - {card.name}", f"     - {kind}", f"     - {req}"]


def _notes_section(cr: CompetitorReport) -> list[str]:
    out: list[str] = []
    if cr.notable_cards:
        out += ["**Notable cards:** " + ", ".join(cr.notable_cards) + "", ""]
    if cr.notes:
        out += ["**Notes:**", "", f"   {cr.notes.strip()}", ""]
    return out
