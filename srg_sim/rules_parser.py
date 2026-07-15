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
    Always,
    BuffSkill,
    Bury,
    CardFilter,
    Comparator,
    Condition,
    CrowdMeterCompare,
    Direction,
    Discard,
    Draw,
    Duration,
    Effect,
    EffectSource,
    FinishBonus,
    Flip,
    Frequency,
    FrequencyGuard,
    HasInPlay,
    LoseBy,
    LoseKind,
    ModifyRoll,
    OnHit,
    OnPlay,
    OnStop,
    RollWhen,
    ShuffleIntoDeck,
    SkillCompare,
    Static,
    Stop,
    Trigger,
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
_YOURS = Direction.YOURS


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


def _rule(pattern: str, builder: Callable[[re.Match[str]], Effect]) -> tuple[re.Pattern[str], Any]:
    return re.compile(pattern + r"$"), builder


_RULES: list[tuple[re.Pattern[str], Callable[[re.Match[str]], Effect]]] = [
    _rule(
        rf"\+(\d+) to {_SK}",
        lambda m: _eff(
            Static(), [FinishBonus(_skill(m[2]), int(m[1]))], duration=Duration.WHILE_IN_PLAY
        ),
    ),
    _rule(r"Draw (\d+) cards?", lambda m: _eff(OnHit(), [Draw(n=int(m[1]))])),
    _rule(
        r"Your next turn roll is \+(\d+)",
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
    _rule(rf"Stop any {_ATK}", lambda m: _eff(OnPlay(), [Stop(atk_type=_atk(m[1]))])),
    _rule(
        rf"Stop any {_ORD} {_ATK}",
        lambda m: _eff(OnPlay(), [Stop(order=_order(m[1]), atk_type=_atk(m[2]))]),
    ),
    _rule(
        rf"Stop any {_ORD} {_ATK} or {_ORD} {_ATK}",
        lambda m: _eff(
            OnPlay(),
            [
                Stop(order=_order(m[1]), atk_type=_atk(m[2])),
                Stop(order=_order(m[3]), atk_type=_atk(m[4])),
            ],
        ),
    ),
    _rule(
        rf"If your {_SK} skill is greater than your opponent's {_SK} skill, stop any {_ATK}",
        lambda m: _eff(
            OnPlay(),
            [Stop(atk_type=_atk(m[3]))],
            condition=SkillCompare(_skill(m[1]), Comparator.GT, Who.SELF, Vs.OPP_SAME),
        ),
    ),
    _rule(
        rf"If your opponent has another {_ATK} in play, stop any {_ATK}",
        lambda m: _eff(
            OnPlay(),
            [Stop(atk_type=_atk(m[2]))],
            condition=HasInPlay(Who.OPP, CardFilter(atk_type=_atk(m[1]))),
        ),
    ),
    _rule(
        rf"If the [Cc]rowd [Mm]eter is (\d+) or greater, stop any (?:Follow Up )?{_ATK}",
        lambda m: _eff(
            OnPlay(),
            [Stop(atk_type=_atk(m[2]))],
            condition=CrowdMeterCompare(Comparator.GE, int(m[1])),
        ),
    ),
]

# Frequency-guard headers (a standalone clause scoping the clauses that follow).
_FREQ_HEADERS: list[tuple[re.Pattern[str], Frequency]] = [
    (re.compile(r"Once (?:per|a) match:?$", re.I), Frequency.ONCE_PER_MATCH),
    (re.compile(r"Once (?:per|a) turn:?$", re.I), Frequency.ONCE_PER_TURN),
]
_N_PER_MATCH = re.compile(r"(\d+) times per match:?$", re.I)


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
            return builder(m)
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
        clauses = [c for c in split_clauses(_record_text(rec)) if _freq_header(c) is None]
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
