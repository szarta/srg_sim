"""rules_text -> [Effect]: pattern grammar + overrides.yaml + coverage (DESIGN.md §4).

Three layers, tried in order per DESIGN.md §4:

1. **Pattern grammar** — a library of whole-clause regexes for the recurring
   shapes (``+N to <skill>``, ``Draw N cards``, ``Your (next) turn roll is +N``,
   ``Your opponent's <skill> is -N``, ``Stop any <order?> <type>``, the skill-vs
   -opponent stop conditionals, ``Bury/Flip/Add/Shuffle N ...``, ``If stopped, you
   lose ...``). Text is split into clauses (newlines / sentences); a leading
   ``Once per match:`` / ``N times per match:`` header scopes the frequency guard.
2. **Curated overrides** (``overrides.yaml``, keyed by db_uuid) — hand-authored IR
   for cards the grammar can't parse; the top-96 gimmicks land here first.
3. **``Unsupported(raw_clause, reason)``** — anything left over, so it is logged
   and measurable, never silently dropped.

:func:`coverage` tallies grammar / override / unsupported over any record set and
surfaces the most-common unparsed shapes — the report that drives M3 to
``unsupported == 0`` across the top-96 (``TOP_DIVISIONS``). :func:`enrich_card` /
:func:`enrich_deck` attach the compiled IR (and finish bonuses) to loaded domain
objects, the bridge from :mod:`srg_sim.loader` to a playable deck.
"""

from __future__ import annotations

import re
from collections import Counter
from collections.abc import Callable
from dataclasses import dataclass, replace
from pathlib import Path
from typing import Any, cast

import yaml

from srg_sim.cards import (
    AtkType,
    Card,
    Competitor,
    Deck,
    EntranceCard,
    PlayOrder,
    Skill,
)
from srg_sim.effects import (
    AddFromDiscard,
    AlsoLead,
    Always,
    BuffSkill,
    Bury,
    CardFilter,
    Comparator,
    Condition,
    CountsAsInPlay,
    CrowdMeterCompare,
    DeckEnd,
    Direction,
    Discard,
    DoubleFinishIfBumped,
    Draw,
    Duration,
    Effect,
    EffectSource,
    FinishBonus,
    FinishRollBonus,
    Flip,
    Frequency,
    FrequencyGuard,
    HandSizeCompare,
    HasInPlay,
    LoseBy,
    LoseKind,
    MaxHandSize,
    ModifyRoll,
    OnHit,
    OnPlay,
    OnStop,
    Peek,
    RecurToDeckTop,
    RevealAndDiscard,
    RollWhen,
    ShuffleDeck,
    ShuffleIntoDeck,
    SkillCompare,
    Static,
    Stop,
    Trigger,
    Unstoppable,
    Unsupported,
    Vs,
    Who,
    from_dict,
)

OVERRIDES_YAML = Path(__file__).resolve().parent.parent / "overrides.yaml"

# The top-96 competitive subset (DESIGN.md §1): Worlds (top 64) + Underworld.
TOP_DIVISIONS = frozenset({"World Championship", "Underworld"})

_SKILLS = {s.value: s for s in Skill}
_ATKS = {a.value: a for a in AtkType if a is not AtkType.NONE}
_ORDERS = {"Lead": PlayOrder.LEAD, "Follow Up": PlayOrder.FOLLOWUP, "Finish": PlayOrder.FINISH}

_SK = r"(Power|Technique|Agility|Strike|Submission|Grapple)"
_ATK = r"(Strike|Grapple|Submission)"
_ORD = r"(Lead|Follow Up|Finish)"
_ORD_RE = r"Lead|Follow Up|Finish"  # non-capturing forms for the stop-target parser
_ATK_RE = r"Strike|Grapple|Submission"
_YOURS = Direction.YOURS

_STOP_PART = re.compile(rf"(?:({_ORD_RE}) )?({_ATK_RE})")


def _skill(text: str) -> Skill:
    return _SKILLS[text]


def _atk(text: str) -> AtkType:
    return _ATKS[text]


def _order(text: str) -> PlayOrder:
    return _ORDERS[text]


def _eff(
    trigger: Trigger,
    actions: list[Any],
    condition: Condition | None = None,
    duration: Duration = Duration.INSTANT,
) -> Effect:
    """A partial Effect; provenance/frequency are filled in by :func:`_compile`."""
    return Effect(
        trigger=trigger,
        condition=condition if condition is not None else Always(),
        actions=tuple(actions),
        duration=duration,
    )


# ---------------------------------------------------------------------------
# Grammar: each rule is (compiled regex over the stripped clause, builder).
# ---------------------------------------------------------------------------


def _rule(
    pattern: str, builder: Callable[[re.Match[str]], Effect | None]
) -> tuple[re.Pattern[str], Any]:
    return re.compile(pattern + r"$"), builder


def _stop_targets(text: str) -> list[Stop] | None:
    """Parse a "stop any …" target into ``Stop`` actions, or ``None`` if any part is
    not a plain ``<type>`` / ``<order> <type>`` (declining unmodelled targets like
    "… even if it cannot be stopped"). Handles the "X or Y" two-target form."""
    stops: list[Stop] = []
    for part in re.split(r"\s+or\s+", text.strip()):
        m = _STOP_PART.fullmatch(part.strip())
        if m is None:
            return None
        stops.append(Stop(order=_order(m[1]) if m[1] else None, atk_type=_atk(m[2])))
    return stops or None


def _stop_eff(target: str, condition: Condition | None) -> Effect | None:
    """An ``OnPlay`` stop effect for a "stop any ``target``" clause, or ``None`` when
    the target is not a modelled stop shape (so the clause falls through)."""
    stops = _stop_targets(target)
    if stops is None:
        return None
    return _eff(OnPlay(), list(stops), condition=condition)


_CNT_ORD = {"lead": PlayOrder.LEAD, "follow up": PlayOrder.FOLLOWUP, "finish": PlayOrder.FINISH}
_CNT_ATK = {
    "strike": AtkType.STRIKE,
    "grapple": AtkType.GRAPPLE,
    "submission": AtkType.SUBMISSION,
}


def _count_filter(text: str) -> CardFilter | None:
    """Parse a count descriptor — "Lead", "Strike", "Lead Strike", "Follow up Strike"
    (case-insensitive, optional trailing "s") — into a :class:`CardFilter`, or ``None``
    if unrecognized (so the "for each …" clause falls through to Unsupported)."""
    t = text.strip().lower().rstrip("s")
    m = re.fullmatch(r"(?:(lead|follow up|finish) )?(strike|grapple|submission)", t)
    if m:
        order = _CNT_ORD[m[1]] if m[1] else None
        return CardFilter(play_order=order, atk_type=_CNT_ATK[m[2]])
    if t in _CNT_ORD:
        return CardFilter(play_order=_CNT_ORD[t])
    return None


def _per_roll(m: Any, delta: int, desc: str, per_who: Who, trigger: Trigger) -> Effect | None:
    """ "+delta to your next turn roll for each <desc> in play" (declines on a bad desc)."""
    per = _count_filter(desc)
    if per is None:
        return None
    return _eff(trigger, [ModifyRoll(Who.SELF, delta, RollWhen.NEXT, per=per, per_who=per_who)])


def _per_draw(m: Any, n: int, desc: str) -> Effect | None:
    """ "Draw N for each <desc> you have in play"; OnPlay so the source is not counted."""
    per = _count_filter(desc)
    if per is None:
        return None
    return _eff(OnPlay(), [Draw(n=n, per=per, per_who=Who.SELF)])


def _per_discard(m: Any, n: int, desc: str) -> Effect | None:
    """ "Opponent discards N for each <desc> you have in play"; OnPlay (source uncounted)."""
    per = _count_filter(desc)
    if per is None:
        return None
    return _eff(OnPlay(), [Discard(count=n, who=Who.OPP, per=per, per_who=Who.SELF)])


_RULES: list[tuple[re.Pattern[str], Callable[[re.Match[str]], Effect | None]]] = [
    # Flat "+N to your Finish rolls" (any skill, finish-only) — before the bare
    # "+N to <skill>" combo rule, since that rule's skill list excludes "Finish".
    _rule(
        r"\+(\d+) to (?:your )?Finish rolls?",
        lambda m: _eff(Static(), [FinishRollBonus(int(m[1]))], duration=Duration.WHILE_IN_PLAY),
    ),
    _rule(
        r"Your Finish rolls? (?:is|are) \+(\d+)",
        lambda m: _eff(Static(), [FinishRollBonus(int(m[1]))], duration=Duration.WHILE_IN_PLAY),
    ),
    _rule(
        rf"\+(\d+) to {_SK}",
        lambda m: _eff(
            Static(), [FinishBonus(_skill(m[2]), int(m[1]))], duration=Duration.WHILE_IN_PLAY
        ),
    ),
    # Persistent self skill buff ("Your Strike is +N"): unlike the combo bonus this
    # folds into derived stats, so it applies to turn, breakout, AND finish rolls.
    _rule(
        rf"Your {_SK} is \+(\d+)",
        lambda m: _eff(
            Static(),
            [BuffSkill(_skill(m[1]), int(m[2]), Who.SELF, Duration.WHILE_IN_PLAY)],
            duration=Duration.WHILE_IN_PLAY,
        ),
    ),
    _rule(
        r"Each player draws? (\d+) cards?",
        lambda m: _eff(OnHit(), [Draw(n=int(m[1])), Draw(n=int(m[1]), who=Who.OPP)]),
    ),
    _rule(
        r"Your opponent draws? (\d+) cards?",
        lambda m: _eff(OnHit(), [Draw(n=int(m[1]), who=Who.OPP)]),
    ),
    _rule(r"Draw (\d+) cards?", lambda m: _eff(OnHit(), [Draw(n=int(m[1]))])),
    _rule(
        r"Draw the bottom (\d+) cards? of your deck",
        lambda m: _eff(OnHit(), [Draw(n=int(m[1]), source=DeckEnd.BOTTOM)]),
    ),
    _rule(r"Shuffle your deck", lambda m: _eff(OnHit(), [ShuffleDeck()])),
    # Information: "Look at your opponent's hand" — grants a temporary reveal of the
    # opponent's (otherwise size-only) hand for the rest of the turn (info model #34).
    _rule(
        r"Look at your opponent'?s hand",
        lambda m: _eff(OnHit(), [Peek(who=Who.OPP)]),
    ),
    _rule(
        r"Your next turn roll is \+(\d+)",
        lambda m: _eff(OnHit(), [ModifyRoll(Who.SELF, int(m[1]), RollWhen.NEXT)]),
    ),
    _rule(
        r"\+(\d+) to your next turn roll",
        lambda m: _eff(OnHit(), [ModifyRoll(Who.SELF, int(m[1]), RollWhen.NEXT)]),
    ),
    _rule(
        r"Your turn roll is \+(\d+)",
        lambda m: _eff(OnHit(), [ModifyRoll(Who.SELF, int(m[1]), RollWhen.THIS)]),
    ),
    _rule(
        r"Your opponent's next turn roll is -(\d+)",
        lambda m: _eff(OnHit(), [ModifyRoll(Who.OPP, -int(m[1]), RollWhen.NEXT)]),
    ),
    _rule(
        rf"Your opponent's {_SK} is -(\d+)",
        lambda m: _eff(
            Static(),
            [BuffSkill(_skill(m[1]), -int(m[2]), Who.OPP, Duration.WHILE_IN_PLAY)],
            duration=Duration.WHILE_IN_PLAY,
        ),
    ),
    # Persistent maximum-hand-size modifiers (DESIGN.md §6): fold into the derived
    # hand cap like a Static skill buff. Signed delta only — the bare-number
    # absolute forms ("… is 3"), the "for each"/"equal to"/"halved" scalings, and
    # the conditional "When …" forms fall through to Unsupported (anchored regex).
    _rule(
        r"Each player's maximum hand ?size is ([+-]\d+)",
        lambda m: _eff(
            Static(),
            [MaxHandSize(int(m[1]), Who.SELF), MaxHandSize(int(m[1]), Who.OPP)],
            duration=Duration.WHILE_IN_PLAY,
        ),
    ),
    _rule(
        r"(?:Your opponent's|Your target's|Their) maximum hand ?size is ([+-]\d+)",
        lambda m: _eff(
            Static(), [MaxHandSize(int(m[1]), Who.OPP)], duration=Duration.WHILE_IN_PLAY
        ),
    ),
    _rule(
        r"Your maximum hand ?size is ([+-]\d+)",
        lambda m: _eff(
            Static(), [MaxHandSize(int(m[1]), Who.SELF)], duration=Duration.WHILE_IN_PLAY
        ),
    ),
    _rule(
        r"If stopped, you lose the match via disqualification",
        lambda m: _eff(OnStop(_YOURS), [LoseBy(LoseKind.DISQUALIFICATION, Who.SELF)]),
    ),
    _rule(
        r"If stopped, you lose the match via pinfall",
        lambda m: _eff(OnStop(_YOURS), [LoseBy(LoseKind.PINFALL, Who.SELF)]),
    ),
    _rule(r"Flip (\d+) cards?", lambda m: _eff(OnHit(), [Flip(n=int(m[1]))])),
    _rule(
        r"Bury (\d+) cards? in your opponent's discard pile",
        lambda m: _eff(OnHit(), [Bury(count=int(m[1]), who=Who.OPP)]),
    ),
    _rule(
        r"Bury (\d+) cards?(?: in your discard pile)?",
        lambda m: _eff(OnHit(), [Bury(count=int(m[1]), who=Who.SELF)]),
    ),
    # Discard: opponent-forced (the owner still chooses which, unless random) and
    # self-discard; random variants first so they win over the chosen forms.
    _rule(
        r"[Yy]our opponent randomly discards (\d+) cards?(?: (?:from|in) their hand)?",
        lambda m: _eff(OnHit(), [Discard(count=int(m[1]), who=Who.OPP, random=True)]),
    ),
    _rule(
        r"[Yy]our opponent discards (\d+) random cards?(?: (?:from|in) their hand)?",
        lambda m: _eff(OnHit(), [Discard(count=int(m[1]), who=Who.OPP, random=True)]),
    ),
    _rule(
        r"[Yy]our opponent discards (\d+) cards?(?: (?:from|in) their hand)?",
        lambda m: _eff(OnHit(), [Discard(count=int(m[1]), who=Who.OPP)]),
    ),
    _rule(
        r"[Rr]andomly discard (\d+) cards?(?: from your hand)?",
        lambda m: _eff(OnHit(), [Discard(count=int(m[1]), who=Who.SELF, random=True)]),
    ),
    _rule(
        r"[Dd]iscard (\d+) random cards?(?: from your hand)?",
        lambda m: _eff(OnHit(), [Discard(count=int(m[1]), who=Who.SELF, random=True)]),
    ),
    _rule(
        r"[Dd]iscard (\d+) cards?(?: from your hand)?",
        lambda m: _eff(OnHit(), [Discard(count=int(m[1]), who=Who.SELF)]),
    ),
    _rule(
        r"Add (\d+) cards? from your discard pile to your hand",
        lambda m: _eff(OnHit(), [AddFromDiscard(CardFilter())]),
    ),
    _rule(
        rf"Add (\d+) {_ATK} from your discard pile to your hand",
        lambda m: _eff(OnHit(), [AddFromDiscard(CardFilter(atk_type=_atk(m[2])))]),
    ),
    _rule(
        r"Shuffle (?:up to )?(\d+) cards? from your discard pile into your deck",
        lambda m: _eff(OnHit(), [ShuffleIntoDeck(CardFilter())]),
    ),
    # Recur a card discard -> top of deck ("Put N cards from your discard pile on top
    # of your deck"). RecurToDeckTop is an "up to" recycle; a bare "N" is authored as
    # its count. The self-conditional "another <type> in play" variant fires OnPlay so
    # the just-played card is not yet on the board (count=1 = a genuinely OTHER card).
    _rule(
        r"Put (?:up to )?(\d+) cards? from your discard pile on top of your deck",
        lambda m: _eff(OnHit(), [RecurToDeckTop(count=int(m[1]))]),
    ),
    _rule(
        rf"If you have another {_ATK} in play, put (?:up to )?(\d+) cards? "
        r"from your discard pile on top of your deck",
        lambda m: _eff(
            OnPlay(),
            [RecurToDeckTop(count=int(m[2]))],
            condition=HasInPlay(Who.SELF, CardFilter(atk_type=_atk(m[1])), count=1),
        ),
    ),
    # "If you have another <type> in play, draw N and your next turn roll is +M"
    # (Double Wrist Twist). OnPlay + count=1 as above.
    _rule(
        rf"If you have another {_ATK} in play, draw (\d+) cards? "
        r"and your next turn roll is \+(\d+)",
        lambda m: _eff(
            OnPlay(),
            [Draw(n=int(m[2])), ModifyRoll(Who.SELF, int(m[3]), RollWhen.NEXT)],
            condition=HasInPlay(Who.SELF, CardFilter(atk_type=_atk(m[1])), count=1),
        ),
    ),
    # "Cannot be stopped by Follow Ups" — a static self-declaration read by the stop
    # check (a Follow-Up stopper cannot stop this card).
    _rule(
        r"Cannot be stopped by Follow ?Ups?",
        lambda m: _eff(
            Static(), [Unstoppable(by_order=PlayOrder.FOLLOWUP)], duration=Duration.WHILE_IN_PLAY
        ),
    ),
    # "This card counts as N <order> <type>s in play" — a static count self-declaration
    # (feeds per-count roll/draw/discard scaling and HasInPlay count gates).
    _rule(
        r"This card counts as (\d+) (Lead|Follow [Uu]p|Finish) (Strike|Grapple|Submission)s? "
        r"in play",
        lambda m: _eff(
            Static(),
            [CountsAsInPlay(_count_filter(f"{m[2]} {m[3]}") or CardFilter(), int(m[1]))],
            duration=Duration.WHILE_IN_PLAY,
        ),
    ),
    # Per-count next turn roll ("+N for each <X> your opponent has / you have in play").
    # Opponent-counted fires OnHit (source card's timing is irrelevant to the opponent's
    # board); self-counted fires OnPlay so the just-played card is not counted.
    _rule(
        r"Your next turn roll is \+(\d+) for each (.+?) your opponent has in play",
        lambda m: _per_roll(m, int(m[1]), m[2], Who.OPP, OnHit()),
    ),
    _rule(
        r"Your next turn roll is \+(\d+) for each (.+?) you have in play",
        lambda m: _per_roll(m, int(m[1]), m[2], Who.SELF, OnPlay()),
    ),
    # Per-count draw ("Draw N for each [other] <X> you have in play"); OnPlay so the
    # source card is not yet on the board ("other").
    _rule(
        r"Draw (\d+) cards? for each (?:other )?(.+?) you have in play",
        lambda m: _per_draw(m, int(m[1]), m[2]),
    ),
    # Per-count opponent discard ("Your opponent discards N ... for each <X> you have
    # in play"); OnPlay so the source card is not counted.
    _rule(
        r"Your opponent discards (\d+) cards?(?: from their hand)? for each (.+?) you have in play",
        lambda m: _per_discard(m, int(m[1]), m[2]),
    ),
    # "Your opponent randomly reveals N cards in their hand and discards all revealed
    # Stops" — reveal N random, drop the Stops among them (0..N leave).
    _rule(
        r"Your opponent randomly reveals (\d+) cards?(?: in their hand)? "
        r"and discards all revealed [Ss]tops",
        lambda m: _eff(OnHit(), [RevealAndDiscard(count=int(m[1]), who=Who.OPP)]),
    ),
    # "If you have no other cards in your hand, this card is also a Lead" — the Finish
    # becomes playable as a Lead while the hand holds only this card (size <= 1).
    _rule(
        r"If you have no other cards in your hand, this card is also a Lead",
        lambda m: _eff(
            Static(),
            [AlsoLead(HandSizeCompare(Comparator.LE, Vs.VALUE, 1))],
            duration=Duration.WHILE_IN_PLAY,
        ),
    ),
    # "If you bumped on the last turn roll, double these bonuses" — a static self-
    # declaration read by the finish sequence (T-Virus). "these bonuses" = the card's
    # own printed Finish bonuses, doubled when the turn's roll-off involved a bump.
    _rule(
        r"If you bumped on the last turn roll, double these bonuses",
        lambda m: _eff(Static(), [DoubleFinishIfBumped()], duration=Duration.WHILE_IN_PLAY),
    ),
    # Conditional Finish-roll bonus keyed on the rolled skill, for whoever finishes
    # ("If either player rolls Agility for their Finish roll, their roll is +N";
    # the card text prints "play" for "player" on some cards).
    _rule(
        rf"If either play(?:er)? rolls {_SK} for their Finish roll, their roll is \+(\d+)",
        lambda m: _eff(
            Static(),
            [FinishRollBonus(int(m[2]), when_skill=_skill(m[1]), either=True)],
            duration=Duration.WHILE_IN_PLAY,
        ),
    ),
    # "stop any <target>" — the target is parsed by _stop_targets (bare type,
    # "<order> <type>", or "X or Y"), shared by the unconditional and conditional
    # forms; an unmodelled target declines (None) so the clause stays Unsupported.
    _rule(r"Stop any (.+)", lambda m: _stop_eff(m[1], None)),
    # Skill stop: online while your skill beats your opponent's ("skill"/apostrophe
    # optional across printings). The "at least N greater" offset form is not yet
    # modelled (SkillCompare has no delta) and stays Unsupported.
    _rule(
        rf"If your {_SK}(?: skill)? is greater than your opponent'?s {_SK}(?: skill)?, "
        r"stop any (.+)",
        lambda m: _stop_eff(m[3], SkillCompare(_skill(m[1]), Comparator.GT, Who.SELF, Vs.OPP_SAME)),
    ),
    _rule(
        rf"If your opponent has another {_ATK} in play, stop any (.+)",
        lambda m: _stop_eff(m[2], HasInPlay(Who.OPP, CardFilter(atk_type=_atk(m[1])))),
    ),
    # Count-gated variant: "N other <type>s in play" -> HasInPlay with a >=N count
    # (the attacking card isn't in play yet when the stop is offered, so every
    # matching card in the opponent's in-play is genuinely "other"). Comma before
    # "stop" is optional across printings.
    _rule(
        rf"If your opponent has (\d+) other {_ATK}s in play,? stop any (.+)",
        lambda m: _stop_eff(
            m[3], HasInPlay(Who.OPP, CardFilter(atk_type=_atk(m[2])), count=int(m[1]))
        ),
    ),
    _rule(
        r"If the [Cc]rowd [Mm]eter is (\d+) or greater, stop any (.+)",
        lambda m: _stop_eff(m[2], CrowdMeterCompare(Comparator.GE, int(m[1]))),
    ),
]

# Frequency-guard headers (a standalone clause scoping the clauses that follow).
_FREQ_HEADERS: list[tuple[re.Pattern[str], Frequency]] = [
    (re.compile(r"Once (?:per|a) match:?$", re.I), Frequency.ONCE_PER_MATCH),
    (re.compile(r"Once (?:per|a) turn:?$", re.I), Frequency.ONCE_PER_TURN),
]
_N_PER_MATCH = re.compile(r"(\d+) times per match:?$", re.I)

# Non-effect metadata clauses: recognized and skipped (not a game effect, and not
# Unsupported). "Skill Requirement: <skill> N+" is a deck-BUILD constraint printed on
# the card, not something that resolves during a match (DESIGN.md §4).
_METADATA = [re.compile(r"Skill Requirement:", re.I)]


def _is_metadata(clause: str) -> bool:
    stripped = clause.strip()
    return any(pattern.match(stripped) for pattern in _METADATA)


def split_clauses(text: str) -> list[str]:
    """Split rules text into clauses on newlines and sentence boundaries."""
    if not text:
        return []
    parts = re.split(r"[\n\r]+|(?<=[.])\s+", text)
    return [p.strip() for p in parts if p.strip()]


def _freq_header(clause: str) -> tuple[Frequency, int | None] | None:
    stripped = clause.strip()
    for pattern, freq in _FREQ_HEADERS:
        if pattern.match(stripped):
            return freq, None
    m = _N_PER_MATCH.match(stripped)
    if m:
        return Frequency.N_PER_MATCH, int(m[1])
    return None


def _match_grammar(clause: str) -> Effect | None:
    stripped = clause.strip().rstrip(".").strip()
    for pattern, builder in _RULES:
        m = pattern.match(stripped)
        if m:
            eff = builder(m)
            if eff is not None:  # a builder may decline (e.g. an unmodelled stop target)
                return eff
    return None


def _compile(clause: str, source: EffectSource, freq: Frequency, n: int | None) -> Effect:
    guard = FrequencyGuard(kind=freq, n=n)
    eff = _match_grammar(clause)
    if eff is not None:
        return replace(eff, raw_clause=clause, source=source, frequency=guard)
    return Effect(
        trigger=OnPlay(),
        actions=(Unsupported(raw_text=clause, reason="no grammar match"),),
        raw_clause=clause,
        source=source,
        frequency=guard,
    )


def parse_text(
    text: str,
    source: EffectSource,
    db_uuid: str | None = None,
    overrides: dict[str, list[dict[str, Any]]] | None = None,
) -> list[Effect]:
    """Compile ``text`` into Effects: overrides win, then grammar, then Unsupported."""
    if overrides and db_uuid in overrides:
        return [cast(Effect, from_dict(entry)) for entry in overrides[db_uuid]]
    effects: list[Effect] = []
    freq, n = Frequency.UNLIMITED, None
    for clause in split_clauses(text):
        header = _freq_header(clause)
        if header is not None:
            freq, n = header
            continue
        if _is_metadata(clause):
            continue  # deck-build metadata, not a match effect
        effects.append(_compile(clause, source, freq, n))
    return effects


def finish_bonuses(effects: list[Effect]) -> tuple[tuple[Skill, int], ...]:
    """Sum every ``FinishBonus`` action into ``(skill, delta)`` pairs (for Card)."""
    totals: dict[Skill, int] = {}
    for eff in effects:
        for action in eff.actions:
            if isinstance(action, FinishBonus):
                totals[action.skill] = totals.get(action.skill, 0) + action.delta
    return tuple(totals.items())


# ---------------------------------------------------------------------------
# Overrides + enrichment (bridge to the loader)
# ---------------------------------------------------------------------------


def load_overrides(path: str | Path = OVERRIDES_YAML) -> dict[str, list[dict[str, Any]]]:
    """Load the hand-authored override table (db_uuid -> list of Effect dicts)."""
    raw = yaml.safe_load(Path(path).read_text())
    return raw or {}


def enrich_card(card: Card, overrides: dict[str, list[dict[str, Any]]] | None = None) -> Card:
    """Attach compiled effects and finish bonuses to a loader-built ``Card``."""
    effects = parse_text(card.raw_text, EffectSource.CARD, card.db_uuid, overrides)
    return replace(card, effects=tuple(effects), finish_bonuses=finish_bonuses(effects))


def enrich_competitor(
    comp: Competitor, overrides: dict[str, list[dict[str, Any]]] | None = None
) -> Competitor:
    effects = parse_text(comp.gimmick_text, EffectSource.GIMMICK, comp.db_uuid, overrides)
    return replace(comp, effects=tuple(effects))


def enrich_entrance(
    ent: EntranceCard, overrides: dict[str, list[dict[str, Any]]] | None = None
) -> EntranceCard:
    effects = parse_text(ent.raw_text, EffectSource.ENTRANCE, ent.db_uuid, overrides)
    return replace(ent, effects=tuple(effects))


def enrich_deck(deck: Deck, overrides: dict[str, list[dict[str, Any]]] | None = None) -> Deck:
    """Compile every card / competitor / entrance in a deck into playable IR."""
    return replace(
        deck,
        competitor=enrich_competitor(deck.competitor, overrides),
        entrance=enrich_entrance(deck.entrance, overrides),
        cards=tuple(enrich_card(c, overrides) for c in deck.cards),
    )


# ---------------------------------------------------------------------------
# Coverage report (DESIGN.md §4)
# ---------------------------------------------------------------------------


@dataclass
class CoverageReport:
    """Clause-level coverage over a record set (DESIGN.md §4)."""

    total: int
    grammar: int
    override: int
    unsupported: int
    top_unparsed: list[tuple[str, int]]

    @property
    def parsed(self) -> int:
        return self.grammar + self.override

    @property
    def rate(self) -> float:
        return self.parsed / self.total if self.total else 1.0


def _record_text(rec: dict[str, Any]) -> str:
    return rec.get("rules_text") or rec.get("rules-text") or ""


def _normalize_shape(clause: str) -> str:
    shape = re.sub(r"\b\d+\b", "N", clause)
    shape = re.sub(_SK, "<S>", shape)
    return shape.strip()


def coverage(
    records: list[dict[str, Any]], overrides: dict[str, list[dict[str, Any]]] | None = None
) -> CoverageReport:
    """Tally grammar / override / unsupported clauses across ``records``."""
    total = grammar = override = unsupported = 0
    shapes: Counter[str] = Counter()
    for rec in records:
        clauses = [
            c
            for c in split_clauses(_record_text(rec))
            if _freq_header(c) is None and not _is_metadata(c)
        ]
        if overrides and rec.get("db_uuid") in overrides:
            total += len(clauses)
            override += len(clauses)
            continue
        for clause in clauses:
            total += 1
            if _match_grammar(clause) is not None:
                grammar += 1
            else:
                unsupported += 1
                shapes[_normalize_shape(clause)] += 1
    return CoverageReport(total, grammar, override, unsupported, shapes.most_common(20))


def is_top96(record: dict[str, Any]) -> bool:
    """True for a competitor in the top-96 competitive subset (DESIGN.md §1)."""
    return record.get("division") in TOP_DIVISIONS
