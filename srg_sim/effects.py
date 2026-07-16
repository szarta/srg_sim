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
    DISCARD = "DISCARD"


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
    """After ``who`` makes a turn roll — outcome-agnostic (fires whether that roll
    won or lost the turn), so roll-value gimmicks like the Bull's "3 less than your
    target's roll" comeback live here rather than on ``OnWinTurn`` / ``OnLoseTurn``.
    ``skill`` scopes to a rolled skill; ``None`` means any skill (the Bull cares
    only about the gap, gated by a ``RollGap*`` condition)."""

    skill: Skill | None = None
    who: Who = Who.SELF


@dataclass(frozen=True)
class OnRollBoost(IRNode):
    """Offered DURING the owner's roll-off — right after they roll ``skill`` and
    *before* the winner is decided — an optional, cost-paying boost that adds
    ``delta`` to THIS roll (Soborno: "when you roll Strike/Grapple/Submission, you may
    discard a card of that move type and your turn roll is +1"). Unlike :class:`OnRoll`
    (which fires *after* the roll for a NEXT-roll comeback), this can flip the current
    roll's outcome. The effect's ``condition`` gates payability (only offered when the
    cost can be paid) and its ``actions`` are the cost; ``optional`` makes it a "may"."""

    skill: Skill | None = None
    delta: int = 1


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
class OnBump(IRNode):
    """When the owner **bumps** — a tied turn roll that forces both players to draw
    one and re-roll (SUPERSHOW_MECHANICS §2). Both sides bump on a tie, so each
    owner's ``OnBump`` fires; a bump-punish gimmick (Mastermind's "when you bump,
    your opponent's next turn roll is -2") lives here. Fires once per bump, so gate
    it with a once-per-turn frequency to punish only once when rolls tie repeatedly.
    """


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
    count: int = 1
    cmp: Comparator = Comparator.GE


@dataclass(frozen=True)
class HasInHand(IRNode):
    """The player holds at least ``count`` cards matching ``filter`` in hand — the
    payability gate for a cost (Soborno: "a card of that move type")."""

    who: Who
    filter: CardFilter = CardFilter()
    count: int = 1


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
    who: Who = Who.SELF  # SELF, or OPP for "your opponent draws N" / "each player draws N"


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
    """Move the top ``n`` cards of a deck to its discard pile (§5).

    ``who`` picks whose deck: ``SELF`` (your own top-``n``) or ``OPP`` (e.g. Big
    Body Block, "your opponent reveals the top card of their deck, they may flip
    it" — an opponent-decided flip, so pair it with ``Effect.optional``)."""

    n: int = 1
    who: Who = Who.SELF


@dataclass(frozen=True)
class Discard(IRNode):
    """Move ``count`` cards from a hand to its discard pile.

    ``who`` picks whose hand (SELF, or the opponent's for "your opponent discards
    N"). The hand's **owner** always chooses which cards to drop — even on an
    opponent-forced discard — unless ``random``, when the RNG picks. Mirrors
    :class:`Bury`'s who/random split.
    """

    selector: CardFilter = CardFilter()
    count: int = 1
    who: Who = Who.SELF
    random: bool = False


@dataclass(frozen=True)
class Search(IRNode):
    filter: CardFilter = CardFilter()
    dest: Dest = Dest.HAND
    count: int = 1  # "up to N" — a DISCARD search bins up to this many chosen cards


@dataclass(frozen=True)
class ShuffleDeck(IRNode):
    """Shuffle an entire deck ("Shuffle your deck"). Distinct from
    :class:`ShuffleIntoDeck`, which folds discard cards back in first."""

    who: Who = Who.SELF


@dataclass(frozen=True)
class ShuffleIntoDeck(IRNode):
    selector: CardFilter = CardFilter()


@dataclass(frozen=True)
class AddFromDiscard(IRNode):
    filter: CardFilter = CardFilter()


@dataclass(frozen=True)
class RecurToDeckTop(IRNode):
    """Put up to ``count`` matching cards from the discard pile ON TOP of the deck
    (Chug-Chug-Chug: "Put up to 3 Finishes from your discard pile on top of your
    deck"; DESIGN.md §3). The owner chooses how many (0..``count``) and which — an
    "up to" recycle that reloads a stopped Finish to redraw next turn. Distinct
    from :class:`ShuffleIntoDeck`, which returns a card to the *bottom* and
    reshuffles; the on-top placement is the tempo that matters."""

    selector: CardFilter = CardFilter()
    count: int = 1


@dataclass(frozen=True)
class RemoveFromPlay(IRNode):
    """Board disruption: move up to ``count`` cards a player has in play to their
    discard ("Discard 1 card your opponent has in play"; DESIGN.md §3).

    ``who`` picks whose board is hit (``OPP`` for the common opponent-disruption
    case, ``SELF`` for self-sacrifice). The **acting** player — the one resolving
    the effect — chooses which matching in-play card(s) to remove, so a disruptive
    attack is aimed, not random. A no-match board is a no-op.
    """

    selector: CardFilter = CardFilter()
    who: Who = Who.OPP
    count: int = 1


@dataclass(frozen=True)
class Peek(IRNode):
    """Look at an otherwise-hidden hand ("Look at your opponent's hand"; §3/§7).

    A pure *information* action — it moves no card. It grants the acting player
    temporary observability of ``who``'s hand (normally ``OPP``, whose hand is
    otherwise a size-only zone): the engine records the peek on the viewer so
    :meth:`GameState.observable` reveals that hand's contents for the rest of the
    peeker's turn. That reveal is the decision-time hook a policy reads to act on
    the seen cards. ``SELF`` (your own, already-visible hand) is a no-op.
    """

    who: Who = Who.OPP


@dataclass(frozen=True)
class ModifyRoll(IRNode):
    who: Who
    delta: int
    when: RollWhen = RollWhen.THIS
    per: CardFilter | None = None  # if set, delta scales by the count of matching cards...
    per_who: Who = Who.OPP  # ...in `per_who`'s in-play board ("+1 for each Lead your opp has")


@dataclass(frozen=True)
class BuffSkill(IRNode):
    skill: Skill
    delta: int
    who: Who = Who.SELF
    duration: Duration = Duration.WHILE_IN_PLAY


@dataclass(frozen=True)
class MaxHandSize(IRNode):
    """Modify a player's maximum hand size (DESIGN.md §3/§6).

    As a ``Static`` action it folds into the *derived* hand cap — parallel to a
    ``Static`` :class:`BuffSkill` folding into effective stats — so it is read on
    demand, never stored. ``delta`` is signed (``+`` raises the owner's cap, ``-``
    lowers it); ``who`` targets ``SELF`` (owner) or ``OPP``. The cap is enforced
    continuously: any time a player sits above it — after a draw, or after an
    opponent's card lowers it — they discard down to it.
    """

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
    """A card's printed combo bonus for one skill ("+2 to Grapple"). Applies to the
    **Finish roll only**, summed across every card in the finisher's play sequence
    (Lead + Follow Up + Finish). Not a turn/breakout modifier — for those, a card
    reads "Your <skill> is +N", which compiles to a persistent :class:`BuffSkill`."""

    skill: Skill
    delta: int


@dataclass(frozen=True)
class FinishRollBonus(IRNode):
    """A flat "+N to your Finish rolls" — added to the owner's Finish roll whatever
    skill is rolled (any-skill), summed across in-play cards. Finish attempts only;
    it does not help breakout rolls (a defender's rolls are a separate check)."""

    delta: int


@dataclass(frozen=True)
class BreakoutModifier(IRNode):
    delta: int = 0
    attempts: int | None = None


@dataclass(frozen=True)
class LowestRollWins(IRNode):
    """Turn-roll gimmick marker (Fae Dragon): while active, the roll-off is won by
    the **lowest** roll instead of the highest (SUPERSHOW_MECHANICS §2). Global —
    if *either* competitor's active gimmick declares it, the whole roll-off flips.
    Carried as a ``Static`` passive read at roll-off time (like a Static
    :class:`BuffSkill`), never executed as a mutation."""


@dataclass(frozen=True)
class ChoiceOption(IRNode):
    """One branch of a :class:`Choice`: a human-readable ``label`` plus the actions
    taken if this branch is picked."""

    label: str = ""
    actions: tuple[Action, ...] = ()


@dataclass(frozen=True)
class Choice(IRNode):
    """Pick exactly ONE branch of actions — an "A or B" effect (Little Guido: "Draw 1
    card OR your opponent's next turn roll is -2"). The acting player chooses which
    branch resolves at execution time (a ``choice`` decision point), so it is where a
    policy's read of the position matters. Distinct from :class:`Effect.optional`,
    which is a single-branch yes/no."""

    options: tuple[ChoiceOption, ...] = ()


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
    optional: bool = False  # a "you may" effect: the decider chooses whether it resolves


# ---------------------------------------------------------------------------
# Type unions (also drive get_type_hints-based deserialization)
# ---------------------------------------------------------------------------

Trigger = (
    OnPlay
    | OnRoll
    | OnRollBoost
    | OnWinTurn
    | OnLoseTurn
    | OnStop
    | OnHit
    | OnBump
    | StartOfTurn
    | StartOfMatch
    | Static
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
    | HasInHand
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
    | ShuffleDeck
    | ShuffleIntoDeck
    | AddFromDiscard
    | RecurToDeckTop
    | RemoveFromPlay
    | Peek
    | ModifyRoll
    | BuffSkill
    | MaxHandSize
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
    | FinishRollBonus
    | BreakoutModifier
    | LowestRollWins
    | Choice
)

ActionOrUnsupported = Action | Unsupported
