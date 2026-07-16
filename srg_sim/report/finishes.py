"""Finish odds for a matchup: signature finishes first, logoless if strictly better.

For each attack type we score the competitor's signature finish and the best
generic **Logoless** finish against the defender using the validated
:func:`srg_sim.finish.finish_odds`, and check whether the defender can skill-stop
that type via :func:`srg_sim.stops.evaluate_stop` ("open lane" = it cannot). The
logoless alternative is surfaced only when it beats the signature — the "use a
logoless finish if it's better" rule from the reference tooling.
"""

from __future__ import annotations

from dataclasses import dataclass

from srg_sim.cards import Competitor
from srg_sim.finish import finish_odds
from srg_sim.report.carddb import FinishRecord, ReportCardDB, stat_dict
from srg_sim.stops import StopEvaluation, evaluate_stop

_FINISH_TYPES = ("Strike", "Grapple", "Submission")


@dataclass(frozen=True)
class FinishOption:
    """One finish candidate with its per-Crowd-Meter success odds."""

    finish: FinishRecord
    bonus: dict[str, int]
    is_signature: bool
    curve: dict[int, float]  # crowd_meter -> P(finish succeeds)

    def odds_at(self, cm: int) -> float:
        return self.curve[cm]


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
        return self.logoless or self.signature


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
    return FinishOption(finish=fr, bonus=bonus, is_signature=signature, curve=curve)


def signature_curves(
    db: ReportCardDB, me: Competitor, opp: Competitor, cms: tuple[int, ...]
) -> list[FinishOption]:
    """Every signature finish of ``me`` with its CM curve vs ``opp`` (for section 8)."""
    my, their = stat_dict(me), stat_dict(opp)
    return [_option(db, my, their, fr, cms, True) for fr in db.finishes_for(me)]


def _score(opt: FinishOption) -> float:
    """Total success odds across the CM curve — distinguishes finishes that both
    saturate to 100% at high Crowd Meter by how they do at low CM (where it matters)."""
    return sum(opt.curve.values())


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
    db: ReportCardDB, me: Competitor, opp: Competitor, cms: tuple[int, ...] = (1, 2, 3, 4, 5)
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


def most_open_line(lines: list[FinishLine]) -> FinishLine | None:
    """The strongest line to throw: best odds among open lanes, else best overall."""
    scored = [ln for ln in lines if ln.best is not None]
    if not scored:
        return None
    open_lanes = [ln for ln in scored if ln.open_lane]
    pool = open_lanes or scored
    return max(pool, key=lambda ln: _score(ln.best))  # type: ignore[arg-type]
