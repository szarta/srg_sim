"""Finish / breakout math — PORTED from ``fae_comp/supershow.py`` (DESIGN.md §6).

This is the authoritative finish-roll model, itself a mirror of the validated
web tool ``FinishCalculator.jsx``. **Do not re-derive it**; keep it in parity
with the source (``tests/test_finish.py`` checks a case batch).

The per-die-face rules (extracted here as public primitives so the engine's
seeded-roll finish sequence shares the exact same logic):

* Finish value per face = finisher stat + finish bonus + crowd meter (uncapped).
* A defender stat breaks out iff ``stat - penalty >= finish_value``, EXCEPT at
  Crowd Meter 0 a raw 10 always breaks out (ignoring penalty).
* A finish value >= 11 at Crowd Meter > 0 is unbreakoutable (auto-success).

``finish_odds`` returns the exact ``Fraction`` probability the finisher succeeds
(the defender fails to break out), enumerating the finisher's six die faces.
Competitor skills are passed as ``{skill_name: value}`` mappings — exactly what
``cards.Stats.to_dict()`` produces.
"""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from fractions import Fraction

SKILLS = ["Power", "Agility", "Technique", "Submission", "Grapple", "Strike"]


def is_auto_success(finish_value: int, crowd_meter: int) -> bool:
    """A finish value >= 11 at Crowd Meter > 0 is unbreakoutable (auto-success)."""
    return finish_value >= 11 and crowd_meter > 0


def stat_breaks_out(stat_value: int, finish_value: int, penalty: int, crowd_meter: int) -> bool:
    """Whether a single defender stat breaks out of a ``finish_value`` finish.

    At Crowd Meter 0 a raw 10 always breaks out (ignoring penalty); otherwise a
    stat breaks out iff ``stat - penalty >= finish_value``. The raw-10 case is the
    srgpc.net "Crowd Meter 0 Safety" ruling — "roll your highest printed skill and you
    break out regardless." The ruling says "highest printed skill" (not "10") to cover
    multi-competitor formats (Trios/tornado); we model only singles, where each
    competitor's stats are a permutation of {10,9,8,7,6,5}, so the highest printed skill
    is always exactly 10 — hence ``== 10`` is equivalent here.
    """
    if crowd_meter == 0 and stat_value == 10:
        return True
    return stat_value - penalty >= finish_value


def _count_breakout_stats(
    opp_stats: Mapping[str, int], finish_value: int, penalty: int, crowd_meter: int
) -> int:
    """How many of the opponent's 6 stats break out of a ``finish_value`` finish."""
    return sum(
        1 for s in SKILLS if stat_breaks_out(opp_stats[s], finish_value, penalty, crowd_meter)
    )


def _breakout_prob_for_finish(
    finish_value: int,
    opp_stats: Mapping[str, int],
    attempts: int,
    crowd_meter: int,
    penalties: Sequence[int] | None,
) -> Fraction:
    """P(opponent breaks out) against one finish face."""
    if is_auto_success(finish_value, crowd_meter):
        return Fraction(0)  # auto-success, unbreakoutable
    if penalties:
        prob_all_fail = Fraction(1)
        for i in range(attempts):
            pen = penalties[i] if i < len(penalties) else 0
            can = _count_breakout_stats(opp_stats, finish_value, pen, crowd_meter)
            prob_all_fail *= 1 - Fraction(can, 6)
        return 1 - prob_all_fail
    can = _count_breakout_stats(opp_stats, finish_value, 0, crowd_meter)
    return 1 - (1 - Fraction(can, 6)) ** attempts


def _average_breakout(probs: Sequence[Fraction], allow_reroll: bool) -> Fraction:
    """Average breakout prob over the finisher's 6 die faces. With a reroll the
    finisher rolls two faces and keeps the better (lower breakout prob)."""
    if not allow_reroll:
        return sum(probs, Fraction(0)) / 6
    total = Fraction(0)
    for p1 in probs:
        for p2 in probs:
            total += min(p1, p2)
    return total / 36


def finish_odds(
    finisher_skills: Mapping[str, int],
    defender_skills: Mapping[str, int],
    finish_bonus: Mapping[str, int] | None = None,
    crowd_meter: int = 0,
    breakout_attempts: int = 3,
    breakout_penalties: Sequence[int] | None = None,
    opponent_modifiers: Mapping[str, int] | None = None,
    allow_reroll: bool = False,
) -> Fraction:
    """Exact probability the FINISHER succeeds (opponent fails to break out).

    Mirrors ``FinishCalculator.jsx``. Finish value per face = finisher stat +
    ``finish_bonus[skill]`` + ``crowd_meter`` (uncapped). The opponent breaks out
    on a face if any of up to ``breakout_attempts`` rolls succeeds.

    ``finish_bonus`` / ``opponent_modifiers``: ``{skill: int}`` (default 0s).
    ``breakout_penalties``: per-attempt penalty SUBTRACTED from opponent stats,
    so POSITIVE hurts the opponent, e.g. ``[0, 1, 1]`` = "-1 to their 2nd and 3rd
    breakout rolls". ``allow_reroll``: finisher may reroll the finish face,
    keeping the better.

    Returns a ``Fraction`` = P(finish succeeds) = 1 - P(breakout).
    """
    finish_bonus = finish_bonus or {}
    opponent_modifiers = opponent_modifiers or {}
    opp = {s: defender_skills[s] + opponent_modifiers.get(s, 0) for s in SKILLS}
    finish_values = [finisher_skills[s] + finish_bonus.get(s, 0) + crowd_meter for s in SKILLS]
    probs = [
        _breakout_prob_for_finish(fv, opp, breakout_attempts, crowd_meter, breakout_penalties)
        for fv in finish_values
    ]
    breakout = _average_breakout(probs, allow_reroll)
    return 1 - breakout
