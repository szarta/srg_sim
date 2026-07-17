"""Finish odds for a matchup: signature finishes first, logoless if strictly better.

For each attack type we score the competitor's signature finish and the best
generic **Logoless** finish against the defender using the validated
:func:`srg_sim.finish.finish_odds`, and check whether the defender can skill-stop
that type via :func:`srg_sim.stops.evaluate_stop` ("open lane" = it cannot). The
logoless alternative is surfaced only when it beats the signature — the "use a
logoless finish if it's better" rule from the reference tooling.
"""

from __future__ import annotations

import re
from collections.abc import Callable
from dataclasses import dataclass

from srg_sim.cards import Competitor
from srg_sim.finish import finish_odds
from srg_sim.report.carddb import FinishRecord, ReportCardDB, stat_dict
from srg_sim.stops import StopEvaluation, evaluate_stop

_FINISH_TYPES = ("Strike", "Grapple", "Submission")

# A conditional finish is really a *vector* of possible outcomes (which conditions
# are met); for the report only the weakest (no conditions) and strongest (all met)
# bounds matter. Two robust patterns lift a finish from its floor to its ceiling:
_REROLL_RE = re.compile(r"re-?roll your finish roll", re.I)  # -> allow_reroll=True
_DOUBLE_RE = re.compile(r"double these bonuses", re.I)  # -> the printed bonuses x2


@dataclass(frozen=True)
class FinishOption:
    """One finish candidate with its per-Crowd-Meter success odds. ``curve`` is the
    floor (no conditions met); ``strong_curve`` is the ceiling (all met), set only when
    the finish has conditional power, with ``condition`` naming what enables it."""

    finish: FinishRecord
    bonus: dict[str, int]
    is_signature: bool
    curve: dict[int, float]  # crowd_meter -> P(finish succeeds), conditions UNMET (floor)
    strong_curve: dict[int, float] | None = None  # conditions MET (ceiling), or None
    condition: str = ""  # human note of what unlocks the ceiling

    def odds_at(self, cm: int) -> float:
        return self.curve[cm]

    def strong_at(self, cm: int) -> float:
        return (self.strong_curve or self.curve)[cm]

    @property
    def has_ceiling(self) -> bool:
        """The finish has conditional power — a ceiling distinct from its floor."""
        return self.strong_curve is not None


def finish_variant(
    fr: FinishRecord, base_bonus: dict[str, int]
) -> tuple[dict[str, int], bool, str]:
    """The ceiling parameters for a finish: ``(strong_bonus, allow_reroll, condition)``.
    Detects the two conditional patterns — "double these bonuses" (Mastermind T-Virus)
    and "re-roll your Finish roll" (Tomato Tornado). No pattern -> floor == ceiling."""
    text = fr.rules_text or ""
    strong = dict(base_bonus)
    reroll = False
    notes: list[str] = []
    if _DOUBLE_RE.search(text):
        strong = {skill: n * 2 for skill, n in strong.items()}
        cond = _if_clause(text, _DOUBLE_RE)
        notes.append(f"2× bonuses if {cond}" if cond else "2× bonuses")
    if _REROLL_RE.search(text):
        reroll = True
        cond = _if_clause(text, _REROLL_RE)
        notes.append(f"re-roll if {cond}" if cond else "may re-roll the Finish roll")
    return strong, reroll, "; ".join(notes)


def _if_clause(text: str, keyword: re.Pattern[str]) -> str:
    """The ``If <…>,`` condition of the sentence that contains ``keyword`` (else "")."""
    for sentence in re.split(r"(?<=[.!])\s+", text):
        if keyword.search(sentence):
            m = re.search(r"\bIf\b\s+(.+?),", sentence, re.I)
            if m:
                return m.group(1).strip()
    return ""


@dataclass(frozen=True)
class FinishLine:
    """The best line of one attack type: signature, a better logoless alt, stop."""

    atk_type: str
    signature: FinishOption | None
    logoless: FinishOption | None  # set only when strictly better than the signature
    open_lane: bool  # the defender cannot skill-stop this type
    stop: StopEvaluation

    @property
    def best(self) -> FinishOption | None:
        """The best FLOOR option (a strictly-better logoless, else the signature)."""
        return self.logoless or self.signature

    @property
    def ceiling_best(self) -> FinishOption | None:
        """The best CEILING option — the signature's conditions-met case weighed against
        a better logoless (which has no ceiling), so a conditional bomb can win here even
        when its floor lost to the logoless."""
        cands = [o for o in (self.signature, self.logoless) if o is not None]
        return max(cands, key=_score_strong) if cands else None


def _option(
    db: ReportCardDB,
    me: dict[str, int],
    opp: dict[str, int],
    fr: FinishRecord,
    cms: tuple[int, ...],
    signature: bool,
) -> FinishOption:
    bonus = db.finish_bonus(fr)
    curve = {cm: float(finish_odds(me, opp, finish_bonus=bonus, crowd_meter=cm)) for cm in cms}
    strong_bonus, reroll, condition = finish_variant(fr, bonus)
    strong_curve = None
    if reroll or strong_bonus != bonus:  # the finish has a distinct ceiling
        strong_curve = {
            cm: float(
                finish_odds(me, opp, finish_bonus=strong_bonus, crowd_meter=cm, allow_reroll=reroll)
            )
            for cm in cms
        }
    return FinishOption(
        finish=fr,
        bonus=bonus,
        is_signature=signature,
        curve=curve,
        strong_curve=strong_curve,
        condition=condition,
    )


def signature_curves(
    db: ReportCardDB, me: Competitor, opp: Competitor, cms: tuple[int, ...]
) -> list[FinishOption]:
    """Every signature finish of ``me`` with its CM curve vs ``opp`` (for section 8)."""
    my, their = stat_dict(me), stat_dict(opp)
    return [_option(db, my, their, fr, cms, True) for fr in db.finishes_for(me)]


# The early Crowd Meter (0-2) is where finishes are actually contested; past CM2 the
# match "starts favoring nearly anything", so odds saturate and stop discriminating.
_EARLY_CM = 2


def _score(opt: FinishOption) -> float:
    """Success odds over the EARLY Crowd Meter (CM<=2) — the window where finishes are
    contested. Past CM2 everything saturates toward 100%, so ranking there is noise.
    Falls back to the full curve if the requested CMs are all above the early window."""
    early = [v for cm, v in opt.curve.items() if cm <= _EARLY_CM]
    return sum(early) if early else sum(opt.curve.values())


def _score_strong(opt: FinishOption) -> float:
    """Like :func:`_score` but over the CEILING curve (conditions met) — used to rank
    the best conditional finish; equals :func:`_score` for a finish with no ceiling."""
    early = [opt.strong_at(cm) for cm in opt.curve if cm <= _EARLY_CM]
    return sum(early) if early else sum(opt.strong_at(cm) for cm in opt.curve)


def _best(
    db: ReportCardDB,
    me: dict[str, int],
    opp: dict[str, int],
    cands: list[FinishRecord],
    cms: tuple[int, ...],
    signature: bool,
) -> FinishOption | None:
    best: FinishOption | None = None
    for fr in cands:
        opt = _option(db, me, opp, fr, cms, signature)
        if best is None or _score(opt) > _score(best):
            best = opt
    return best


def finish_lines(
    db: ReportCardDB, me: Competitor, opp: Competitor, cms: tuple[int, ...] = (0, 1, 2, 3, 4, 5)
) -> list[FinishLine]:
    """Per-attack-type best line vs ``opp``: signature, a better logoless, and stop."""
    my, their = stat_dict(me), stat_dict(opp)
    sig_by_type: dict[str, list[FinishRecord]] = {t: [] for t in _FINISH_TYPES}
    for fr in db.finishes_for(me):
        if fr.atk_type in sig_by_type:
            sig_by_type[fr.atk_type].append(fr)
    logoless = db.logoless_finishes()
    lines = []
    for atk in _FINISH_TYPES:
        sig = _best(db, my, their, sig_by_type[atk], cms, signature=True)
        logo = _best(db, my, their, logoless.get(atk, []), cms, signature=False)
        # Surface the logoless alt only when it strictly beats the signature (over
        # the whole CM curve, so a high-CM saturation tie doesn't count as "better").
        if sig is not None and logo is not None and _score(logo) <= _score(sig):
            logo = None
        stop = evaluate_stop(their, atk, my)
        lines.append(FinishLine(atk, sig, logo, not stop["online"], stop))
    return lines


def _best_line(lines: list[FinishLine], score: Callable[[FinishLine], float]) -> FinishLine | None:
    """The strongest line by ``score``, preferring open lanes (else best overall)."""
    scored = [ln for ln in lines if ln.best is not None]
    if not scored:
        return None
    pool = [ln for ln in scored if ln.open_lane] or scored
    return max(pool, key=score)


def most_open_line(lines: list[FinishLine]) -> FinishLine | None:
    """The strongest line to throw by its FLOOR (conditions unmet) — the guaranteed best."""
    return _best_line(lines, lambda ln: _score(ln.best) if ln.best else 0.0)


def best_ceiling_line(lines: list[FinishLine]) -> FinishLine | None:
    """The strongest line by its CEILING (conditions met) — the best conditional bomb."""
    return _best_line(lines, lambda ln: _score_strong(ln.ceiling_best) if ln.ceiling_best else 0.0)
