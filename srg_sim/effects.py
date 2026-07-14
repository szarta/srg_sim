"""Effect IR: Trigger, Condition, Action, Effect, Unsupported (DESIGN.md §3).

Cards, competitor gimmicks, and Entrance effects all compile to one typed IR.
The engine executes **only** this IR, never raw text. Everything here is a
frozen, hashable dataclass that round-trips to JSON via :func:`to_json` /
:func:`from_json` (or :meth:`IRNode.to_dict` / :func:`from_dict`).

Serialization is generic and tag-based: every node serializes to a dict whose
``"kind"`` is its class name; enums serialize to their (string) value.
Deserialization dispatches on ``"kind"`` and uses each field's declared type to
rebuild enums, nested nodes, and tuples. Because the format is stable and
human-legible, hand-authored IR (``overrides.yaml``, DESIGN.md §4) uses it too.

An ``Effect`` is a ``(trigger, condition, actions)`` triple plus duration,
frequency guard, provenance, and the raw clause it was compiled from.
"""

from __future__ import annotations

import json
import types
from dataclasses import dataclass, fields
from enum import Enum
from typing import Any, Union, get_args, get_origin, get_type_hints

from srg_sim.cards import AtkType, PlayOrder, Skill

# ---------------------------------------------------------------------------
# Serialization framework
# ---------------------------------------------------------------------------

_REGISTRY: dict[str, type[IRNode]] = {}
_HINTS: dict[type, dict[str, Any]] = {}

# Reserved key holding a node's class name in its serialized form. Deliberately
# not a valid Python identifier, so it can never collide with a dataclass field
# name (e.g. ``LoseBy.kind`` / ``FrequencyGuard.kind``).
_TAG = "@type"


@dataclass(frozen=True)
class IRNode:
    """Base for every IR node. Subclasses are frozen dataclasses.

    Auto-registers each subclass by name so :func:`from_dict` can find it.
    """

    def __init_subclass__(cls, **kwargs: Any) -> None:
        super().__init_subclass__(**kwargs)
        _REGISTRY[cls.__name__] = cls

    def to_dict(self) -> dict[str, Any]:
        data: dict[str, Any] = {_TAG: type(self).__name__}
        for f in fields(self):
            data[f.name] = _encode(getattr(self, f.name))
        return data


def _encode(value: Any) -> Any:
    if isinstance(value, IRNode):
        return value.to_dict()
    if isinstance(value, Enum):
        return value.value
    if isinstance(value, list | tuple):
        return [_encode(v) for v in value]
    return value


def _hints(cls: type) -> dict[str, Any]:
    cached = _HINTS.get(cls)
    if cached is None:
        cached = get_type_hints(cls)
        _HINTS[cls] = cached
    return cached


def _decode(value: Any, hint: Any) -> Any:
    if value is None:
        return None
    origin = get_origin(hint)
    if origin is None:
        return _decode_atom(value, hint)
    if origin in (list, tuple):
        elem = get_args(hint)[0]
        decoded = [_decode(v, elem) for v in value]
        return tuple(decoded) if origin is tuple else decoded
    if origin is Union or origin is types.UnionType:
        return _decode_union(value, get_args(hint))
    return value


def _decode_atom(value: Any, hint: Any) -> Any:
    if isinstance(hint, type) and issubclass(hint, IRNode):
        return _decode_node(value)
    if isinstance(hint, type) and issubclass(hint, Enum):
        return hint(value)
    return value


def _decode_union(value: Any, args: tuple[Any, ...]) -> Any:
    variants = [a for a in args if a is not type(None)]
    if len(variants) == 1:
        return _decode(value, variants[0])
    if isinstance(value, dict) and _TAG in value:
        return _decode_node(value)
    return value


def _decode_node(data: dict[str, Any]) -> IRNode:
    cls = _REGISTRY[data[_TAG]]
    hints = _hints(cls)
    kwargs = {f.name: _decode(data[f.name], hints[f.name]) for f in fields(cls) if f.name in data}
    return cls(**kwargs)


def from_dict(data: dict[str, Any]) -> IRNode:
    """Rebuild any IR node from its ``to_dict`` representation."""
    return _decode_node(data)


def to_json(node: IRNode) -> str:
    """Serialize an IR node to a JSON string."""
    return json.dumps(node.to_dict())


def from_json(text: str) -> IRNode:
    """Rebuild an IR node from a JSON string produced by :func:`to_json`."""
    return _decode_node(json.loads(text))


# ---------------------------------------------------------------------------
# Shared enums
# ---------------------------------------------------------------------------


class Who(Enum):
    """Whose state an effect reads or mutates."""

    SELF = "SELF"
    OPP = "OPP"


class Comparator(Enum):
    GT = ">"
    GE = ">="
    EQ = "="
    LT = "<"


class Vs(Enum):
    """Right-hand side of a comparison."""

    OPP = "OPP"  # opponent's value (hand size)
    OPP_SAME = "OPP_SAME"  # opponent's value in the SAME skill
    VALUE = "VALUE"  # the literal ``value`` field


class Duration(Enum):
    """How long a buff / passive effect stays active (DESIGN.md §3)."""

    WHILE_IN_PLAY = "WHILE_IN_PLAY"  # active while the source card is in play
    WHILE_GIMMICK_ACTIVE = "WHILE_GIMMICK_ACTIVE"  # active while gimmick not blanked
    INSTANT = "INSTANT"  # one-shot mutation, no lasting state


class Frequency(Enum):
    UNLIMITED = "UNLIMITED"
    ONCE_PER_TURN = "ONCE_PER_TURN"
    ONCE_PER_MATCH = "ONCE_PER_MATCH"
    N_PER_MATCH = "N_PER_MATCH"


class DeckEnd(Enum):
    TOP = "TOP"
    BOTTOM = "BOTTOM"


class RollWhen(Enum):
    THIS = "THIS"
    NEXT = "NEXT"


class Direction(Enum):
    """Direction of a stop, from the effect owner's point of view."""

    YOURS = "YOURS"
    THEIRS = "THEIRS"


class LoseKind(Enum):
    DISQUALIFICATION = "DISQUALIFICATION"
    PINFALL = "PINFALL"


class Dest(Enum):
    HAND = "HAND"


class Until(Enum):
    END_OF_TURN = "END_OF_TURN"


class EffectSource(Enum):
    CARD = "card"
    GIMMICK = "gimmick"
    ENTRANCE = "entrance"


# ---------------------------------------------------------------------------
# Card selection
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class CardFilter(IRNode):
    """A predicate over cards, used by search / bury / discard / has-in-* nodes.

    All criteria are optional and combine by AND. ``raw`` carries a free-form
    descriptor for shapes not yet formalized (kept round-trippable, never lost).
    """

    number: int | None = None
    atk_type: AtkType | None = None
    play_order: PlayOrder | None = None
    tag: str | None = None
    name: str | None = None
    raw: str | None = None


# ---------------------------------------------------------------------------
# Triggers — WHEN an effect fires
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class OnPlay(IRNode):
    """When this card is played."""


@dataclass(frozen=True)
class OnRoll(IRNode):
    """When ``who`` rolls ``skill`` for a turn / finish roll."""

    skill: Skill
    who: Who = Who.SELF


@dataclass(frozen=True)
class OnWinTurn(IRNode):
    """After the turn roll resolves in the owner's favor."""


@dataclass(frozen=True)
class OnLoseTurn(IRNode):
    """After the turn roll resolves against the owner.

    ``by`` optionally scopes to a losing gap (open in DESIGN.md §3; ``None`` =
    any loss).
    """

    by: int | None = None


@dataclass(frozen=True)
class OnStop(IRNode):
    """When a stop happens in the given direction."""

    dir: Direction


@dataclass(frozen=True)
class OnHit(IRNode):
    """When a matching card resolves into play (DESIGN.md §3, "hit")."""

    keyword: str | None = None
    name: str | None = None


@dataclass(frozen=True)
class StartOfTurn(IRNode):
    """At the start of the owner's turn."""


@dataclass(frozen=True)
class StartOfMatch(IRNode):
    """At match setup, before opening hands."""


@dataclass(frozen=True)
class Static(IRNode):
    """Always-on passive; scoped by the effect's ``duration``."""


# ---------------------------------------------------------------------------
# Conditions — a predicate on GameState (composable via And / Or / Not)
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class Always(IRNode):
    """The always-true condition."""


@dataclass(frozen=True)
class And(IRNode):
    items: tuple[Condition, ...] = ()


@dataclass(frozen=True)
class Or(IRNode):
    items: tuple[Condition, ...] = ()


@dataclass(frozen=True)
class Not(IRNode):
    item: Condition = Always()


@dataclass(frozen=True)
class SkillCompare(IRNode):
    skill: Skill
    cmp: Comparator
    who: Who = Who.SELF
    vs: Vs = Vs.OPP_SAME
    value: int | None = None


@dataclass(frozen=True)
class HandSizeCompare(IRNode):
    cmp: Comparator
    vs: Vs = Vs.OPP
    value: int | None = None
    who: Who = Who.SELF


@dataclass(frozen=True)
class CrowdMeterCompare(IRNode):
    cmp: Comparator
    value: int


@dataclass(frozen=True)
class HasInPlay(IRNode):
    who: Who
    filter: CardFilter = CardFilter()


@dataclass(frozen=True)
class HasInDiscard(IRNode):
    who: Who
    filter: CardFilter = CardFilter()


@dataclass(frozen=True)
class RollWasSkill(IRNode):
    skill: Skill


@dataclass(frozen=True)
class RollGapExactly(IRNode):
    k: int


@dataclass(frozen=True)
class RollGapAtLeast(IRNode):
    k: int


# ---------------------------------------------------------------------------
# Actions — the mutations
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class Draw(IRNode):
    n: int = 1
    source: DeckEnd = DeckEnd.TOP


@dataclass(frozen=True)
class Bury(IRNode):
    """Move ``count`` cards from a discard pile to the **bottom of that deck**.

    ``who`` picks whose discard/deck (SELF or the opponent's, e.g. "bury N cards
    in your opponent's discard pile"). ``selector`` picks which discard cards
    (empty = engine's choice). The card's owner (or the actor, for an opponent
    bury) chooses the buried order; ``random=True`` buries in random order. There
    is no separate "buried" zone — a buried card lives at the bottom of the deck.
    """

    selector: CardFilter = CardFilter()
    count: int = 1
    who: Who = Who.SELF
    random: bool = False


@dataclass(frozen=True)
class Flip(IRNode):
    """Move the top ``n`` cards of the deck to the discard pile (§5)."""

    n: int = 1


@dataclass(frozen=True)
class Discard(IRNode):
    selector: CardFilter = CardFilter()
    count: int = 1


@dataclass(frozen=True)
class Search(IRNode):
    filter: CardFilter = CardFilter()
    dest: Dest = Dest.HAND


@dataclass(frozen=True)
class ShuffleIntoDeck(IRNode):
    selector: CardFilter = CardFilter()


@dataclass(frozen=True)
class AddFromDiscard(IRNode):
    filter: CardFilter = CardFilter()


@dataclass(frozen=True)
class ModifyRoll(IRNode):
    who: Who
    delta: int
    when: RollWhen = RollWhen.THIS


@dataclass(frozen=True)
class BuffSkill(IRNode):
    skill: Skill
    delta: int
    who: Who = Who.SELF
    duration: Duration = Duration.WHILE_IN_PLAY


@dataclass(frozen=True)
class Reroll(IRNode):
    who: Who
    once: bool = True


@dataclass(frozen=True)
class WinTie(IRNode):
    who: Who


@dataclass(frozen=True)
class Bump(IRNode):
    who: Who


@dataclass(frozen=True)
class Stop(IRNode):
    order: PlayOrder | None = None
    atk_type: AtkType | None = None
    source_is_skillreq: bool = False


@dataclass(frozen=True)
class BlankGimmick(IRNode):
    who: Who
    duration: Duration = Duration.WHILE_IN_PLAY


@dataclass(frozen=True)
class BlankText(IRNode):
    selector: CardFilter = CardFilter()
    until: Until = Until.END_OF_TURN


@dataclass(frozen=True)
class LoseBy(IRNode):
    kind: LoseKind
    who: Who = Who.SELF


@dataclass(frozen=True)
class CrowdMeter(IRNode):
    delta: int


@dataclass(frozen=True)
class PlayExtraCard(IRNode):
    order: PlayOrder | None = None


@dataclass(frozen=True)
class SetFinishRoll(IRNode):
    value: int
    condition: Condition = Always()


@dataclass(frozen=True)
class FinishBonus(IRNode):
    skill: Skill
    delta: int


@dataclass(frozen=True)
class BreakoutModifier(IRNode):
    delta: int = 0
    attempts: int | None = None


# ---------------------------------------------------------------------------
# Unsupported sentinel + the Effect itself
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class Unsupported(IRNode):
    """A clause the parser could not confidently map. Engine ignores it but logs
    it, so coverage is always measurable and no gimmick is silently mis-played.
    """

    raw_text: str
    reason: str


@dataclass(frozen=True)
class FrequencyGuard(IRNode):
    """How often an effect may fire. ``n`` applies only to ``N_PER_MATCH``."""

    kind: Frequency = Frequency.UNLIMITED
    n: int | None = None


@dataclass(frozen=True)
class Effect(IRNode):
    """A ``(trigger, condition, actions)`` triple with duration + provenance."""

    trigger: Trigger
    condition: Condition = Always()
    actions: tuple[ActionOrUnsupported, ...] = ()
    duration: Duration = Duration.INSTANT
    frequency: FrequencyGuard = FrequencyGuard()
    raw_clause: str = ""
    source: EffectSource = EffectSource.CARD


# ---------------------------------------------------------------------------
# Type unions (also drive get_type_hints-based deserialization)
# ---------------------------------------------------------------------------

Trigger = (
    OnPlay | OnRoll | OnWinTurn | OnLoseTurn | OnStop | OnHit | StartOfTurn | StartOfMatch | Static
)

Condition = (
    Always
    | And
    | Or
    | Not
    | SkillCompare
    | HandSizeCompare
    | CrowdMeterCompare
    | HasInPlay
    | HasInDiscard
    | RollWasSkill
    | RollGapExactly
    | RollGapAtLeast
)

Action = (
    Draw
    | Bury
    | Flip
    | Discard
    | Search
    | ShuffleIntoDeck
    | AddFromDiscard
    | ModifyRoll
    | BuffSkill
    | Reroll
    | WinTie
    | Bump
    | Stop
    | BlankGimmick
    | BlankText
    | LoseBy
    | CrowdMeter
    | PlayExtraCard
    | SetFinishRoll
    | FinishBonus
    | BreakoutModifier
)

ActionOrUnsupported = Action | Unsupported
