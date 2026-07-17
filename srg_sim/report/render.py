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
from srg_sim.report.skillreqs import MAX_SKILL_REQ_CARDS, PriorityCard

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
    full = [s.comp.name for s in (data.a, data.b) if s.gimmick_fully_unmodeled]
    partial = [s.comp.name for s in (data.a, data.b) if s.gimmick_partial]
    out: list[str] = []
    if full:
        out += [
            f".. warning:: {' and '.join(full)}'s gimmick is not yet modeled by the rules "
            "parser, so the turn-roll odds and comp-type below reflect the **base stat line "
            "only** — the gimmick's effect is not counted (its text is on the card image).",
            "",
        ]
    if partial:
        out += [
            f".. note:: {' and '.join(partial)}'s gimmick is modeled except for one clause "
            "that is not yet counted (its full text is on the card image).",
            "",
        ]
    return out


def _competitor_block(cr: CompetitorReport, images: Mapping[str, str]) -> list[str]:
    # Stats + gimmick text are omitted on purpose — they're on the card image below.
    lines = _heading(cr.comp.name, "-")
    lines += _picture(cr.image_uuid, images)
    lines += _type_line(cr)
    lines += _turn_line(cr)
    lines += _open_line(cr)
    lines += _stops_block(cr)
    lines += _finish_odds_section(cr, images)
    lines += _skillreq_section(cr)
    lines += _notes_section(cr)
    return lines


def _picture(uuid: str, images: Mapping[str, str]) -> list[str]:
    rel = images.get(uuid)
    if not rel:
        return []
    return [f".. image:: {rel}", "   :height: 300px", "   :align: left", ""]


def _type_line(cr: CompetitorReport) -> list[str]:
    also = f" (also {', '.join(cr.comp_type.also)})" if cr.comp_type.also else ""
    src = "curated" if cr.comp_type.source == "override" else "auto"
    out = [f"**Type:** {cr.comp_type.label}{also} *({src})*", ""]
    if cr.gimmick_fully_unmodeled:
        out += [
            "*(gimmick not yet modeled — turn odds & type reflect the base stat line only)*",
            "",
        ]
    elif cr.gimmick_partial:
        out += ["*(one gimmick clause not yet counted; the rest is modeled)*", ""]
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
    cm = _contested_cap(best)  # a representative early Crowd Meter, not the saturated ceiling
    return [
        f"**Most open finish line:** {ml.atk_type} — **{best.finish.name}**{tag} "
        f"({lane}), {best.odds_at(cm):.0%} at CM{cm}.",
        "",
    ]


def _contested_cap(opt: FinishOption) -> int:
    """The top of the contested Crowd-Meter window (highest CM <= 2 in the curve)."""
    early = [cm for cm in opt.curve if cm <= 2]
    return max(early) if early else max(opt.curve)


def _stops_block(cr: CompetitorReport) -> list[str]:
    parts = []
    for ln in cr.finish_lines:
        mark = "can stop" if ln.stop["online"] else "open"
        parts.append(f"{ln.atk_type}: {mark}")
    return ["**Their skill stops (this matchup):** " + "; ".join(parts) + ".", ""]


def _finish_odds_section(cr: CompetitorReport, images: Mapping[str, str]) -> list[str]:
    if not cr.signature_finishes:
        return _heading("Finish odds", "~") + ["*(no signature finishes on record.)*", ""]
    cms = sorted(cr.signature_finishes[0].curve)
    lines = _heading(f"Finish odds (CM{cms[0]}–{cms[-1]})", "~")
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
    # Judged on the early Crowd Meter (CM0-2), where finishes are actually contested;
    # show the CM where the logoless card most out-performs the signature.
    out = ["**Better logoless alternatives** (early Crowd Meter):", ""]
    for ln in better:
        logo, sig = ln.logoless, ln.signature
        assert logo is not None
        cm = _best_gap_cm(logo, sig)
        base = f" vs {sig.finish.name} {sig.odds_at(cm):.0%}" if sig else ""
        out.append(
            f"* {ln.atk_type}: **{logo.finish.name}** {logo.odds_at(cm):.0%} at CM{cm}{base}."
        )
    return out + [""]


def _best_gap_cm(logo: FinishOption, sig: FinishOption | None) -> int:
    """The early Crowd Meter (CM<=2) where ``logo`` most out-performs ``sig``."""
    early = [cm for cm in sorted(logo.curve) if cm <= 2] or sorted(logo.curve)
    if sig is None:
        return early[0]
    return max(early, key=lambda cm: logo.odds_at(cm) - sig.odds_at(cm))


_TIER_LABEL = {"auto": "auto-include", "equal8": "Equal-8 stop"}
_LIVE_LABEL = {True: "yes", False: "no", None: "situational"}


def _skillreq_section(cr: CompetitorReport) -> list[str]:
    lines = _heading("Key skill-requirement cards", "~")
    lines += [
        f"Run at most **{MAX_SKILL_REQ_CARDS}**; ranked by priority (auto-includes, then "
        "Equal-8 stops). *Live* = the card's stop is online for this stat line / matchup.",
        "",
    ]
    if cr.skill_req_cards:
        lines += [".. list-table::", "   :header-rows: 1", "   :widths: 42 16 26 12", ""]
        lines += ["   * - Card", "     - Tier", "     - Requirement", "     - Live?"]
        for card in cr.skill_req_cards:
            lines += _skillreq_row(card)
        lines += [""]
    else:
        lines += ["*(no priority tech cards are runnable on this stat line.)*", ""]
    if cr.personal_cards:
        joined = ", ".join(cr.personal_cards)
        lines += [f"**Personal-choice Leads** (no requirement): {joined}.", ""]
    return lines


def _skillreq_row(card: PriorityCard) -> list[str]:
    tier = _TIER_LABEL.get(card.tier, card.tier)
    return [
        f"   * - {card.name}",
        f"     - {tier}",
        f"     - {card.req_str}",
        f"     - {_LIVE_LABEL[card.live]}",
    ]


def _notes_section(cr: CompetitorReport) -> list[str]:
    out: list[str] = []
    if cr.notable_cards:
        out += ["**Notable cards:** " + ", ".join(cr.notable_cards) + "", ""]
    if cr.notes:
        out += ["**Notes:**", "", f"   {cr.notes.strip()}", ""]
    return out
