"""Evaluate Effect-IR conditions against live game state (DESIGN.md §3).

A :class:`~srg_sim.effects.Condition` is a predicate on the current
:class:`~srg_sim.state.GameState`, evaluated relative to the effect's **owner**
(the player the effect belongs to; ``SELF`` is the owner, ``OPP`` the other side).
:func:`holds` dispatches on the node type and returns a bool.

This is what turns a skill stop "online" (``SkillCompare``), gates a see-1 stop
(``HasInPlay``), and lets conditional ``Static`` buffs resolve — so a card that
raises a skill can flip a stop online, and one that lowers the opponent's can flip
theirs offline (SUPERSHOW_MECHANICS §4/§6).

``SkillCompare`` reads the **derived** stats (base + unconditional buffs) via
``effective_stats`` with no evaluator, which both reflects active buffs *and*
avoids a buff→condition→buff recursion: a conditional buff's own condition sees
unconditional buffs only. Roll-scoped conditions (``RollWasSkill`` / ``RollGap*``)
need a :class:`RollContext`; without one they are simply false.
"""

from __future__ import annotations

import operator
from collections.abc import Callable, Iterable
from dataclasses import dataclass
from typing import TYPE_CHECKING, Any

from srg_sim import effects as fx
from srg_sim.cards import Card, Skill

if TYPE_CHECKING:
    from srg_sim.state import GameState

_CMP: dict[fx.Comparator, Callable[[int, int], bool]] = {
    fx.Comparator.GT: operator.gt,
    fx.Comparator.GE: operator.ge,
    fx.Comparator.EQ: operator.eq,
    fx.Comparator.LT: operator.lt,
    fx.Comparator.LE: operator.le,
}


@dataclass(frozen=True)
class RollContext:
    """The current turn roll, for roll-scoped conditions (from the owner's view).

    ``gap`` is the **opponent's** rolled value minus the **owner's**, so a positive
    gap means the owner rolled *lower* by that much — i.e. ``RollGapExactly(3)``
    reads "your roll is exactly 3 less than your target's" (the Bull). It is signed:
    rolling *higher* gives a negative gap, which no ``RollGap*(k>0)`` matches, so a
    "rolled lower by k" gimmick correctly stays silent when you roll high."""

    skill: Skill | None = None
    gap: int | None = None  # opponent's roll minus owner's; positive => owner rolled lower
    value: int | None = None  # the owner's own rolled value this turn (for RollValue)
    # The OTHER side's rolled skill — set only in the post-roll / pair contexts, so
    # SameRolledSkill can compare the two. None in single-sided switch contexts.
    opp_skill: Skill | None = None


def card_matches(card: Card, filt: fx.CardFilter) -> bool:
    """True iff ``card`` satisfies every set criterion of ``filt`` (AND; ``raw`` ignored)."""
    if filt.number is not None and card.number != filt.number:
        return False
    if filt.atk_type is not None and card.atk_type is not filt.atk_type:
        return False
    if filt.play_order is not None and card.play_order is not filt.play_order:
        return False
    if filt.tag is not None and filt.tag not in card.tags:
        return False
    if filt.name is not None and card.name != filt.name:
        return False
    if filt.name_contains and not _any_substr_ci(filt.name_contains, card.name):
        return False
    return not (filt.text_contains and not _any_substr_ci(filt.text_contains, card.raw_text))


def _any_substr_ci(needles: tuple[str, ...], haystack: str) -> bool:
    """True iff ``haystack`` contains any of ``needles`` as a case-insensitive
    substring (pure substring — "Table" matches "Stable"; OR over the needles)."""
    hay = haystack.lower()
    return any(n.lower() in hay for n in needles)


def _filter_implies(sel: fx.CardFilter, query: fx.CardFilter) -> bool:
    """True iff every card matching ``sel`` necessarily matches ``query`` — i.e.
    ``query`` is no more restrictive than ``sel`` (each field ``query`` constrains,
    ``sel`` constrains identically). So a Lead-Strike declaration implies the looser
    "Lead" and "Strike" queries, but not "Follow up" (``raw`` is ignored)."""
    for field in ("number", "atk_type", "play_order", "tag", "name"):
        q = getattr(query, field)
        if q is not None and getattr(sel, field) != q:
            return False
    return True


def _counts_as(card: Card, query: fx.CardFilter) -> int:
    """The largest ``CountsAsInPlay`` count this card declares for a ``query`` its
    selector implies (0 if none) — e.g. "counts as 2 Lead Strikes" returns 2 for a
    Lead / Strike / Lead-Strike query, 0 for a Follow-up query."""
    best = 0
    for eff in card.effects:
        for action in eff.actions:
            if isinstance(action, fx.CountsAsInPlay) and _filter_implies(action.selector, query):
                best = max(best, action.count)
    return best


def count_in_play(cards: Iterable[Card], query: fx.CardFilter, exclude: Card | None = None) -> int:
    """Count cards in a board matching ``query``, honoring ``CountsAsInPlay`` self-
    declarations (a card that "counts as N" contributes N instead of 1). ``exclude``
    drops one card object (the just-played source, for "each **other** … in play")."""
    total = 0
    for card in cards:
        if card is exclude:
            continue
        base = 1 if card_matches(card, query) else 0
        total += max(base, _counts_as(card, query))
    return total


def _who(state: GameState, owner: str, who: fx.Who) -> str:
    return owner if who is fx.Who.SELF else state.opponent_of(owner)


def _skill_value(state: GameState, key: str, skill: Skill) -> int:
    # No evaluator: derived stats reflect unconditional buffs and cannot recurse.
    return state.effective_stats(key)[skill.value]


def holds(
    cond: fx.Condition, state: GameState, owner: str, roll: RollContext | None = None
) -> bool:
    """Whether ``cond`` holds for ``owner`` in ``state`` (unknown nodes → False)."""
    handler = _HANDLERS.get(type(cond))
    return handler(cond, state, owner, roll) if handler else False


def _h_always(c: Any, s: GameState, o: str, r: RollContext | None) -> bool:
    return True


def _h_and(c: fx.And, s: GameState, o: str, r: RollContext | None) -> bool:
    return all(holds(x, s, o, r) for x in c.items)


def _h_or(c: fx.Or, s: GameState, o: str, r: RollContext | None) -> bool:
    return any(holds(x, s, o, r) for x in c.items)


def _h_not(c: fx.Not, s: GameState, o: str, r: RollContext | None) -> bool:
    return not holds(c.item, s, o, r)


def _h_skill(c: fx.SkillCompare, s: GameState, o: str, r: RollContext | None) -> bool:
    subject = _who(s, o, c.who)
    # "Your skills are considered higher than your opponent's" (RaRa Perre): a
    # vs-opponent skill comparison of `subject` resolves a fixed way.
    if c.vs is not fx.Vs.VALUE:
        order = _considered_compare(s, subject, fx.CompareDomain.SKILL)
        if order is not None:
            return _forced_cmp(c.cmp, order)
    left = _skill_value(s, subject, c.skill)
    if c.vs is fx.Vs.VALUE:
        right = c.value or 0
    else:  # OPP_SAME: the subject's opponent — the same skill, or `vs_skill` if set
        right = _skill_value(s, s.opponent_of(subject), c.vs_skill or c.skill)
    return _CMP[c.cmp](left, right)


def _h_handsize(c: fx.HandSizeCompare, s: GameState, o: str, r: RollContext | None) -> bool:
    subject = _who(s, o, c.who)
    # "You are considered to have fewer cards in hand" (Theo V2): a vs-opponent
    # hand-size comparison of `subject` resolves a fixed way.
    if c.vs is not fx.Vs.VALUE:
        order = _considered_compare(s, subject, fx.CompareDomain.HAND)
        if order is not None:
            return _forced_cmp(c.cmp, order)
    left = len(s.players[subject].hand)
    right = (c.value or 0) if c.vs is fx.Vs.VALUE else len(s.players[s.opponent_of(subject)].hand)
    return _CMP[c.cmp](left, right)


def _forced_cmp(cmp: fx.Comparator, order: fx.CompareOrder) -> bool:
    """Resolve a comparison whose left side is forced by a ``ConsideredCompare``
    override: ``GREATER`` = the subject is treated as strictly higher/more (``>``/``>=``
    hold, else fail); ``LESS`` as strictly lower/fewer. Strict, so ``=`` never holds."""
    if order is fx.CompareOrder.GREATER:
        return cmp in (fx.Comparator.GT, fx.Comparator.GE)
    return cmp in (fx.Comparator.LT, fx.Comparator.LE)


def _considered_compare(
    s: GameState, key: str, domain: fx.CompareDomain
) -> fx.CompareOrder | None:
    """The active ``ConsideredCompare`` override of ``key`` for ``domain``, if any
    (RaRa Perre, Theo V2). Scans ``key``'s own active static declarations (competitor
    gimmick unless blanked, entrance, in-play), honoring each declaration's condition."""
    for effects, active in s._buff_sources(key, s.players[key]):
        if not active:
            continue
        for eff in effects:
            if not isinstance(eff.trigger, fx.Static):
                continue
            for a in eff.actions:
                if (
                    isinstance(a, fx.ConsideredCompare)
                    and a.domain is domain
                    and holds(eff.condition, s, key)
                ):
                    return a.order
    return None


def _h_crowd(c: fx.CrowdMeterCompare, s: GameState, o: str, r: RollContext | None) -> bool:
    return _CMP[c.cmp](s.crowd_meter, c.value)


def _h_in_play(c: fx.HasInPlay, s: GameState, o: str, r: RollContext | None) -> bool:
    n = count_in_play(s.players[_who(s, o, c.who)].in_play, c.filter)
    return _CMP[c.cmp](n, c.count)


def _h_in_hand(c: fx.HasInHand, s: GameState, o: str, r: RollContext | None) -> bool:
    n = sum(card_matches(card, c.filter) for card in s.players[_who(s, o, c.who)].hand)
    return n >= c.count


def _h_in_discard(c: fx.HasInDiscard, s: GameState, o: str, r: RollContext | None) -> bool:
    return any(card_matches(card, c.filter) for card in s.players[_who(s, o, c.who)].discard)


def _h_chosen_name_is(c: fx.ChosenNameIs, s: GameState, o: str, r: RollContext | None) -> bool:
    return s.players[_who(s, o, c.who)].chosen_name == c.name


def _h_in_play_compare(c: fx.InPlayCompare, s: GameState, o: str, r: RollContext | None) -> bool:
    n = count_in_play(s.players[_who(s, o, c.who)].in_play, c.filter)
    m = count_in_play(s.players[_who(s, o, c.vs_who)].in_play, c.filter)
    return _CMP[c.cmp](n, m)


def _h_roll_was(c: fx.RollWasSkill, s: GameState, o: str, r: RollContext | None) -> bool:
    return r is not None and r.skill is c.skill


def _h_gap_exact(c: fx.RollGapExactly, s: GameState, o: str, r: RollContext | None) -> bool:
    return r is not None and r.gap == c.k


def _h_gap_at_least(c: fx.RollGapAtLeast, s: GameState, o: str, r: RollContext | None) -> bool:
    return r is not None and r.gap is not None and r.gap >= c.k


def _h_lead_at_least(c: fx.RollLeadAtLeast, s: GameState, o: str, r: RollContext | None) -> bool:
    # A lead of k = the owner rolled k higher = gap (opp - owner) <= -k.
    return r is not None and r.gap is not None and r.gap <= -c.k


def _h_roll_value(c: fx.RollValue, s: GameState, o: str, r: RollContext | None) -> bool:
    return r is not None and r.value is not None and _CMP[c.cmp](r.value, c.value)


def _h_printed_roll_value(
    c: fx.PrintedRollValue, s: GameState, o: str, r: RollContext | None
) -> bool:
    if r is None or r.skill is None:
        return False
    subject = _who(s, o, c.who)
    return s.players[subject].competitor.stats.to_dict()[r.skill.value] == c.value


def _h_same_rolled_skill(c: fx.SameRolledSkill, s: GameState, o: str, r: RollContext | None) -> bool:
    return r is not None and r.skill is not None and r.skill is r.opp_skill


def _h_opp_won_last(c: fx.OppWonLastRoll, s: GameState, o: str, r: RollContext | None) -> bool:
    return s.last_roll_winner is not None and s.last_roll_winner == s.opponent_of(o)


def _h_gimmick_flipped(c: fx.GimmickFlipped, s: GameState, o: str, r: RollContext | None) -> bool:
    return s.players[_who(s, o, c.who)].gimmick_flipped


def _h_during_turn(c: fx.DuringTurn, s: GameState, o: str, r: RollContext | None) -> bool:
    return s.active == _who(s, o, c.who)


_HANDLERS: dict[type, Callable[[Any, GameState, str, RollContext | None], bool]] = {
    fx.Always: _h_always,
    fx.And: _h_and,
    fx.Or: _h_or,
    fx.Not: _h_not,
    fx.SkillCompare: _h_skill,
    fx.HandSizeCompare: _h_handsize,
    fx.CrowdMeterCompare: _h_crowd,
    fx.HasInPlay: _h_in_play,
    fx.HasInHand: _h_in_hand,
    fx.HasInDiscard: _h_in_discard,
    fx.InPlayCompare: _h_in_play_compare,
    fx.ChosenNameIs: _h_chosen_name_is,
    fx.RollWasSkill: _h_roll_was,
    fx.RollGapExactly: _h_gap_exact,
    fx.RollGapAtLeast: _h_gap_at_least,
    fx.RollLeadAtLeast: _h_lead_at_least,
    fx.RollValue: _h_roll_value,
    fx.PrintedRollValue: _h_printed_roll_value,
    fx.SameRolledSkill: _h_same_rolled_skill,
    fx.OppWonLastRoll: _h_opp_won_last,
    fx.GimmickFlipped: _h_gimmick_flipped,
    fx.DuringTurn: _h_during_turn,
}
