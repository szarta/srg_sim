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
from dataclasses import dataclass, fields, replace
from enum import Enum
from typing import Any, TypeVar, Union, get_args, get_origin, get_type_hints

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
    LE = "<="


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
    WHILE_IN_DISCARD = "WHILE_IN_DISCARD"  # active while the source sits in its owner's discard
    # TIMED: granted imperatively when the effect fires, swept at the END of the turn
    # it was granted in ("until the end of the turn", ~81 cards).
    UNTIL_END_OF_TURN = "UNTIL_END_OF_TURN"
    # TIMED: swept at the start of the owner's next ACTIVE turn. A turn is shared and
    # its active player is only known once the turn roll resolves, so the sweep runs
    # immediately AFTER that roll: the buff still feeds the roll that makes the turn
    # yours, then dies. It survives every turn the owner is not active.
    # Hand-adjudicated 2026-07-20.
    UNTIL_START_OF_YOUR_NEXT_TURN = "UNTIL_START_OF_YOUR_NEXT_TURN"


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
    # Put the searched card on TOP of the (shuffled) deck (Heartache Kid).
    DECK_TOP = "DECK_TOP"


class BuryFrom(Enum):
    """Source zone a :class:`Bury` draws from. ``DISCARD`` (the default) is the
    pass-and-recycle bury (discard pile -> bottom of deck); ``HAND`` is the
    card-text bury ("bury N cards in [your/their] hand" -> bottom of deck)."""

    DISCARD = "DISCARD"
    HAND = "HAND"


class CountZone(Enum):
    """Zone a :class:`BuffSkill` ``per``-count ranges over — "for each card you have
    **in play**" vs "in your **discard** pile"."""

    IN_PLAY = "IN_PLAY"
    DISCARD = "DISCARD"


class DqScope(Enum):
    """Reach of a :class:`DisqualificationRule` toggle. ``SELF`` = "you cannot be
    disqualified" (only the owner); ``MATCH`` = "this match has no disqualifications"
    (every player)."""

    SELF = "SELF"
    MATCH = "MATCH"


class CompareDomain(Enum):
    """Which comparison :class:`ConsideredCompare` overrides — ``SKILL`` forces every
    ``SkillCompare`` of the declaring player vs the opponent; ``HAND`` every
    ``HandSizeCompare``."""

    SKILL = "SKILL"
    HAND = "HAND"


class RevealMatch(Enum):
    """Which revealed cards count toward the draw in :class:`RevealForDraw` —
    ``STOP`` each revealed Stop (Bartholomew Hooke); ``ROLLED_SKILL`` each whose
    move type equals the actor's just-rolled skill (The Winning Ticket)."""

    STOP = "STOP"
    ROLLED_SKILL = "ROLLED_SKILL"


class CompareOrder(Enum):
    """How :class:`ConsideredCompare` resolves the subject vs the opponent —
    ``GREATER`` = always considered higher/more (RaRa Perre); ``LESS`` = always
    considered lower/fewer (Theo the Greek Neo V2)."""

    GREATER = "GREATER"
    LESS = "LESS"


class ScryRest(Enum):
    """What a :class:`Scry` does with revealed cards that are neither taken to hand
    nor buried by the fixed ``bury`` count. ``RETURN`` puts them back on top of the
    deck (the actor reorders by value); ``CHOOSE`` lets the actor decide, per card,
    between returning it on top and burying it to the deck bottom."""

    RETURN = "RETURN"
    CHOOSE = "CHOOSE"


class RevealDest(Enum):
    """Where a :class:`RevealRoute` sends the revealed card. ``HAND`` = the deck
    owner's hand; ``FLIP`` = mill it to the discard pile; ``BURY`` = the deck bottom;
    ``LEAVE`` = keep it on top (the declined "you may" branch)."""

    LEAVE = "LEAVE"
    HAND = "HAND"
    FLIP = "FLIP"
    BURY = "BURY"


class RevealFrom(Enum):
    """Which end of the deck a :class:`RevealRoute` reveals from. ``CHOOSE`` is the
    actor's pick ("the top or bottom card"), resolved blind to the top since the card
    is not yet known."""

    TOP = "TOP"
    BOTTOM = "BOTTOM"
    CHOOSE = "CHOOSE"


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
    # Case-insensitive substring match on the card's title (``name_contains``) or
    # rules text (``text_contains``): "a card with 'X' (or 'Y') in the name/text".
    # OR of substrings; empty = no constraint. Pure substring ("Table" ⊂ "Stable").
    name_contains: tuple[str, ...] = ()
    text_contains: tuple[str, ...] = ()


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
class InRoll(IRNode):
    """An automatic (no-cost) modifier applied DURING the roll-off — after both players
    roll, before the winner is decided — that adjusts the *current* roll via its
    ``ModifyRoll(when=THIS)`` actions. Unlike :class:`OnRollBoost` (optional, self-only,
    cost-paying) it fires unconditionally and its action's ``who`` picks the target, so
    it can debuff the opponent's current roll. ``skill`` gates on the rolled skill;
    ``who`` says whose roll must match it (``SELF``/``OPP``). ``either`` overrides ``who``
    to "fires once if EITHER player rolled ``skill``" — and because it is one effect with
    one action, the modifier is applied once (capped), never doubled (Tomato Tomato Jr.:
    "when you or your target roll Power, your target's turn roll is -1")."""

    skill: Skill | None = None
    who: Who = Who.SELF
    either: bool = False


@dataclass(frozen=True)
class OnRollBoost(IRNode):
    """Offered DURING the owner's roll-off — right after they roll ``skill`` and
    *before* the winner is decided — an optional, cost-paying boost that adds
    ``delta`` to THIS roll (Soborno: "when you roll Strike/Grapple/Submission, you may
    discard a card of that move type and your turn roll is +1"). Unlike :class:`OnRoll`
    (which fires *after* the roll for a NEXT-roll comeback), this can flip the current
    roll's outcome. The effect's ``condition`` gates payability (only offered when the
    cost can be paid) and its ``actions`` are the cost; ``optional`` makes it a "may".

    ``on_bump`` scopes *when* the boost is offered. Default ``False`` offers it on the
    initial roll (Soborno). ``True`` offers it only on a would-bump — a tie, before the
    bump's draw+re-roll — so paying the cost adds ``delta`` and breaks the tie *instead*
    of bumping (Rey Zerblade: "when you would bump, you may discard 1 Lead you have in
    play to add +1 to your turn roll instead")."""

    skill: Skill | None = None
    delta: int = 1
    on_bump: bool = False


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
    """When a stop happens in the given direction. ``order``, when set, gates on the
    STOPPED card's play order — "when your opponent stops your Finish" (La Fenix
    Super Lucha); ``None`` = any stopped card (the default)."""

    dir: Direction
    order: PlayOrder | None = None


@dataclass(frozen=True)
class OnHit(IRNode):
    """When a matching card resolves into play (DESIGN.md §3, "hit").

    On a *card's own* effects, all gates are empty — it fires when that card hits.
    On a *competitor gimmick* (a standing effect), ``atk_type`` fires whenever the
    owner hits a card of that attack type (D1: "when you hit a Submission, draw 1");
    ``name_contains`` / ``text_contains`` gate on the hit card's title / rules text
    ("when you hit a card with 'X' in the name") — case-insensitive OR-substring,
    combined by AND with ``atk_type``. A played card and a stop entering play both
    count as hits (srg-rules-confirmed)."""

    atk_type: AtkType | None = None
    name_contains: tuple[str, ...] = ()
    text_contains: tuple[str, ...] = ()
    # "When you hit a card" (any card) as a standing gimmick — fires on every hit
    # (Bartholomew Hooke). Override-only; parser fragments leave it False.
    on_any: bool = False


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
class OnBury(IRNode):
    """When a card or Gimmick causes the owner to bury cards (The Cyclone V1) / when
    the owner buries OR discards cards from their hand from an effect (Tommy
    Stillwell). Fires ONLY after an EFFECT-caused bury (``_act_bury``) / effect-caused
    hand discard (``_act_discard``) — never the mechanical pass-and-recycle
    (``_do_pass``) or the hand-cap trim, which bypass those paths. ``who`` = whose
    bury fires it (SELF = "causes you"). ``from_hand_only`` limits to hand buries
    (Tommy); ``also_discard`` additionally fires on an effect-caused hand discard
    (Tommy's "bury or discard"). Fires once per bury/discard event."""

    who: Who
    from_hand_only: bool = False
    also_discard: bool = False


@dataclass(frozen=True)
class OnBreakout(IRNode):
    """After a breakout resolves — the shared match event that clears both boards and
    bumps the Crowd Meter (SUPERSHOW_MECHANICS §5). ``who`` selects whose breakout
    fires it: ``None`` = any breakout ("after a breakout, ..." — Copy Kat: "turn this
    card over"); ``SELF`` = you broke out; ``OPP`` = your opponent broke out. Gate with
    :class:`GimmickFlipped` so a one-way transform fires only while still on its front."""

    who: Who | None = None


@dataclass(frozen=True)
class OnShuffle(IRNode):
    """Fires when the ``who``-side's deck is shuffled by a card/gimmick EFFECT (any
    effect-caused shuffle: explicit "shuffle your deck", or the incidental shuffle after
    a search/tutor/shuffle-into-deck/hand-into-deck). NOT the match-start setup shuffle,
    nor the private bury-ordering shuffle. ``who`` = whose shuffle fires it from the
    owner's POV (OPP = "when your opponent shuffles their deck" — Memes Dealer V2)."""

    who: Who


@dataclass(frozen=True)
class OnDiscardMove(IRNode):
    """Fires when one or more cards LEAVE the ``who``-side's discard pile because of a
    card/gimmick EFFECT — "when your opponent moves any number of cards from their
    discard pile with their card effect or Gimmick" (Brumeister V2). Covers every
    effect-driven exit: recur-to-hand, shuffle-into-deck, recur-to-deck-top, the
    hand/discard swap, and an effect-caused discard-pile bury. Fires ONCE per action,
    not per card ("any number of cards"). Deliberately NOT fired by the mechanical
    pass-and-recycle, which is not a card effect. ``who`` is read as the owner of the
    PILE, from the effect owner's POV (OPP = "your opponent['s] discard pile")."""

    who: Who


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
    vs_skill: Skill | None = (
        None  # OPP_SAME vs a DIFFERENT opponent skill ("your Strike > opp Agility")
    )


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
class InPlayCompare(IRNode):
    """Cross-board in-play count compare: ``who``'s count of cards in play matching
    ``filter`` compared (``cmp``) against ``vs_who``'s count of the same filter. "When
    your target has more Strikes in play [than you]" (Snake Pitt V3): ``who=OPP``,
    ``vs_who=SELF``, ``cmp=">"``, filter ``atk_type=Strike``. Honors ``CountsAsInPlay``
    on both boards (via :func:`conditions.count_in_play`)."""

    filter: CardFilter = CardFilter()
    cmp: Comparator = Comparator.GT
    who: Who = Who.OPP
    vs_who: Who = Who.SELF


@dataclass(frozen=True)
class RollWasSkill(IRNode):
    skill: Skill


@dataclass(frozen=True)
class RollGapExactly(IRNode):
    k: int


@dataclass(frozen=True)
class RollGapAtLeast(IRNode):
    k: int


@dataclass(frozen=True)
class RollLeadAtLeast(IRNode):
    """The owner rolled at least ``k`` *higher* than the opponent this turn — the
    mirror of :class:`RollGapAtLeast` (owner rolled ``k`` lower). Reads the signed
    ``gap`` (opponent − owner) on the :class:`~srg_sim.conditions.RollContext`: a lead
    of ``k`` is ``gap <= -k``. False without a roll context (YamatoHama: "when your
    turn roll is at least 3 greater than your opponent's, bury 3 in their discard")."""

    k: int


@dataclass(frozen=True)
class SameRolledSkill(IRNode):
    """True iff the owner and their target rolled the **same skill** for this
    turn-roll (Hex, Nic Nemeth). Reads the post-roll context's ``skill`` vs
    ``opp_skill``; false without a roll context or in single-sided contexts."""


@dataclass(frozen=True)
class OppWonLastRoll(IRNode):
    """True iff the effect owner's opponent won the *previous* turn's roll-off
    (``GameState.last_roll_winner``). False before turn 1 (no previous roll). Gates a
    re-roll offer (Robert 'The Brain' Dunn: "if your opponent won the last turn roll,
    you may re-roll your turn roll")."""


@dataclass(frozen=True)
class GimmickFlipped(IRNode):
    """True iff ``who``'s competitor card has been turned over to its back side (by
    :class:`FlipGimmick`). Gates a two-sided gimmick's front effects (``Not(...)``)
    against its back effects (Copy Kat V2)."""

    who: Who = Who.SELF


@dataclass(frozen=True)
class DuringTurn(IRNode):
    """True iff it is currently ``who``'s turn — the active player (roll-off winner)
    is the ``who``-side. Gates a continuous effect to a turn phase ("during your
    opponent's turn: …" — La Fenix). Reads ``GameState.active``."""

    who: Who = Who.SELF


@dataclass(frozen=True)
class RollValue(IRNode):
    """The rolled value of the current turn roll compared against ``value`` — gates on
    the **actual number rolled** this turn (not a static stat), read from the
    :class:`~srg_sim.conditions.RollContext`. Which roll it reads is set by the
    trigger's ``who`` (Mrs. Apocalypse: ``OnRoll(who=OPP)`` + ``RollValue(LE, 7)`` =
    "when your opponent's turn roll is 7 or less"). False without a roll context."""

    cmp: Comparator
    value: int


@dataclass(frozen=True)
class PrintedRollValue(IRNode):
    """The rolled skill's **printed** (base, unbuffed) stat on the ``who``-side's
    competitor equals ``value`` — "when your opponent rolls their printed 8 skill"
    (Collin the Chrononaut). ``who`` picks whose printed stat to read (the roller),
    following the trigger's ``who`` like :class:`RollValue`. False without a roll."""

    who: Who
    value: int


# ---------------------------------------------------------------------------
# Actions — the mutations
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class Draw(IRNode):
    n: int = 1
    source: DeckEnd = DeckEnd.TOP
    who: Who = Who.SELF  # SELF, or OPP for "your opponent draws N" / "each player draws N"
    per: CardFilter | None = None  # if set, `n` scales by the count of matching cards...
    per_who: Who = Who.SELF  # ...in `per_who`'s in-play board ("draw 1 for each Lead you have")


@dataclass(frozen=True)
class Bury(IRNode):
    """Move ``count`` cards to the **bottom of a deck** (no separate "buried" zone).

    ``source`` picks the origin zone: ``DISCARD`` (default) is the pass-and-recycle
    bury (top ``count`` of the discard pile); ``HAND`` is the card-text bury ("bury
    N cards in [your/their] hand"), where the hand owner chooses which unless
    ``random``. ``who`` picks whose zone (SELF or the opponent's). ``selector``
    picks which cards (empty = any / engine's choice). ``random=True`` buries a
    random selection instead of a chosen one."""

    selector: CardFilter = CardFilter()
    count: int = 1
    who: Who = Who.SELF
    random: bool = False
    source: BuryFrom = BuryFrom.DISCARD


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
    per: CardFilter | None = None  # if set, `count` scales by the count of matching cards...
    per_who: Who = (
        Who.SELF
    )  # ...in `per_who`'s in-play board ("discard 1 for each Strike you have")


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
class SwapHandDiscard(IRNode):
    """"Switch 1 card in your hand with 1 card in your discard pile" (Collin, Mr.
    Rey): the owner picks one hand card out (→ discard, via the ``discard``/shed
    point) and one discard card in (→ hand, via the ``target``/tutor point). A no-op
    if either zone is empty. The "you may" lives on ``Effect.optional``."""


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
class CountsAsInPlay(IRNode):
    """A static self-declaration that the source card counts as ``count`` cards
    matching ``selector`` for any "in play" tally ("This card counts as 2 Lead
    Strikes in play"). Read by :func:`conditions.count_in_play`; it mutates no
    state, so the engine folds it like a Static marker (executing it is a no-op).
    It lifts every count that ``selector`` *implies* — a Lead-Strike declaration
    raises the Lead count, the Strike count, and the Lead-Strike count alike — and
    feeds per-count roll/draw/discard scaling and ``HasInPlay`` count gates."""

    selector: CardFilter = CardFilter()
    count: int = 2


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
class ReturnToHand(IRNode):
    """"Add ``count`` card(s) in play to their hand" (Fox Assassin V2): bounce
    matching in-play cards back to their OWNER's hand. ``who`` picks the board;
    ``choose`` (like :class:`ShuffleHandDraw`) lets the acting player pick from EITHER
    board — "any player has in play". A no-match board is a no-op."""

    selector: CardFilter = CardFilter()
    who: Who = Who.OPP
    count: int = 1
    choose: bool = False


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
class Scry(IRNode):
    """Look at / reveal cards from the top (and/or bottom) of ``deck``'s deck, then
    route them by value. The effect owner (the "actor") takes ``to_hand`` of them to
    the deck owner's hand, buries ``bury`` to the deck bottom (the *worst* on its own
    deck, the *best* on an opponent's — sabotage, e.g. The Oracle), and disposes of
    the leftovers per ``rest``. ``reveal`` makes the seen cards public (logged);
    ``reveal=False`` is a private "look at". Covers reveal-top-of-deck gimmicks
    (Perfect Assistant, Split, Ricky Riot, The Oracle).
    """

    deck: Who
    top: int = 0
    bottom: int = 0
    reveal: bool = False
    to_hand: int = 0
    bury: int = 0
    rest: ScryRest = ScryRest.RETURN


@dataclass(frozen=True)
class RevealRoute(IRNode):
    """Reveal the top card of ``deck``'s deck and route it by a runtime predicate: if
    its ``atk_type`` equals ``match_atk`` it goes to ``on_match``, otherwise to
    ``on_fail`` (an optional "you may flip/bury it" is taken only when worthwhile if
    ``fail_optional`` — shed junk off your own deck, disrupt a valuable card on an
    opponent's). Destinations: HAND (deck owner's hand), FLIP (mill to discard), BURY
    (deck bottom), LEAVE (keep on top). One effect per rolled skill, ``match_atk``
    baked to that skill's move type (Candy MaM, Flame Fighter).
    """

    deck: Who
    match_atk: AtkType
    on_match: RevealDest
    on_fail: RevealDest
    fail_optional: bool = False
    reveal: bool = False
    reveal_from: RevealFrom = RevealFrom.TOP
    # True=even, False=odd -> number-parity predicate (Smart Mark's blind odd/even
    # guess); None keeps the atk_type==match_atk predicate.
    match_parity: bool | None = None


@dataclass(frozen=True)
class ShuffleHandDraw(IRNode):
    """Shuffle a player's hand back into their deck, shuffle it, then draw ``count``
    fresh cards — a mid-match hand refresh (Cyclone V2, on a bump). ``choose`` lets
    the actor pick which player ("either player"); otherwise ``who`` selects."""

    who: Who
    count: int
    choose: bool = False


@dataclass(frozen=True)
class ModifyRoll(IRNode):
    who: Who
    delta: int
    when: RollWhen = RollWhen.THIS
    per: CardFilter | None = None  # if set, delta scales by the count of matching cards...
    per_who: Who = Who.OPP  # ...in `per_who`'s in-play board ("+1 for each Lead your opp has")


@dataclass(frozen=True)
class BuffSkill(IRNode):
    """A persistent ``+delta`` (or ``-delta``) to ``who``'s ``skill``, folded into the
    derived stats (DESIGN.md §5). Two dynamic variants (Copy Kat V2): ``target_highest``
    retargets from the fixed ``skill`` to whichever of the target's skills is highest
    (its base line — "your opponent's highest skill is -1"); ``per_crowd`` replaces
    ``delta`` with the current Crowd Meter, clamped to ``cap`` when set ("your Grapple
    is + the Crowd Meter (Max +5)"). Both default off, so a plain buff is unchanged."""

    skill: Skill
    delta: int
    who: Who = Who.SELF
    duration: Duration = Duration.WHILE_IN_PLAY
    target_highest: bool = False
    per_crowd: bool = False
    cap: int | None = None
    # When set, the bonus is ``delta * (count of the target's cards in ``per_zone``
    # matching ``per``)``, clamped to ``cap`` — "+1 for each card you have in play
    # with 'Chin' in the name (Max +3)".
    per: CardFilter | None = None
    per_zone: CountZone = CountZone.IN_PLAY


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
class AddText(IRNode):
    """"Your cards with ``name_contains`` in the name have the added text ``effects``"
    (El Super Santa / Sabu / El Super Hombre). A ``Static`` gimmick declaration read
    at play time: when active (its ``Effect.condition`` holds), each of the owner's
    played cards whose title matches ``name_contains`` (case-insensitive OR) gains
    ``effects``, run alongside the card's own effects. Never executed as a mutation
    (a passive marker); the injected effects carry their own triggers (usually
    ``OnPlay``)."""

    name_contains: tuple[str, ...] = ()
    effects: tuple[Effect, ...] = ()


@dataclass(frozen=True)
class Reroll(IRNode):
    who: Who
    once: bool = True
    # "Choose any player to re-roll" (Grim Librarian): the owner picks which side
    # re-rolls (overrides ``who``).
    choose: bool = False
    # ``THIS`` re-rolls the current roll (structural, read in the roll-off); ``NEXT``
    # grants a one-shot re-roll for the owner's next turn roll (King Brian Cage).
    when: RollWhen = RollWhen.THIS


@dataclass(frozen=True)
class SwitchRolledSkill(IRNode):
    """"When you roll ``from_skill`` for your turn roll or Finish roll, you may switch
    it to ``to``" (Scott Prime V1/V2). Read structurally in BOTH roll paths (the turn
    roll-off and the Finish roll), a no-op in ``_ACTIONS``; fires when the rolled
    skill == ``from_skill``. The "you may" lives on the ``Effect.optional`` flag. A
    switched turn die keeps its roll mods (value recomputed on ``to``'s stat); a
    switched Finish die recomputes base + combo from ``to``.

    The field is ``from_skill`` (not ``from``) because ``from`` is a Python keyword
    and ``to_dict`` emits field names verbatim — same rename as ``reveal_from``."""

    from_skill: Skill
    to: Skill


@dataclass(frozen=True)
class WinTie(IRNode):
    who: Who


@dataclass(frozen=True)
class Bump(IRNode):
    who: Who


@dataclass(frozen=True)
class ElectBumpOnSameSkill(IRNode):
    """A static roll-off grant (Mastermind's "Ringside Ruckus With The Floats"
    entrance): when the owner and target roll the **same skill** for the turn roll
    but different values, the owner MAY elect to bump instead of resolving —
    ``uses`` times per match. A normal value tie already bumps for free, so this
    only adds the value-differs case. Read structurally in the roll-off (a no-op to
    execute); the per-match budget is tracked in the owner's freq counters. Electing
    a bump both fires the owner's OnBump punish and arms a bumped finish (T-Virus)."""

    uses: int = 2


@dataclass(frozen=True)
class Stop(IRNode):
    order: PlayOrder | None = None
    atk_type: AtkType | None = None
    source_is_skillreq: bool = False


@dataclass(frozen=True)
class StopRequiresTag(IRNode):
    """A passive marker paired with a sibling :class:`Stop` in the same effect: the
    stop is only legal when the attacked card carries ``tag`` — "Stop any Grapple
    with a Spotlight" (Stop(atk_type=Grapple) + StopRequiresTag("Spotlight")). Read
    by ``_card_can_stop``; never executed as a mutation."""

    tag: str = ""


@dataclass(frozen=True)
class Unstoppable(IRNode):
    """A static self-declaration that the source card cannot be stopped by stops of
    play-order ``by_order`` ("Cannot be stopped by Follow Ups"); ``by_order=None``
    means it cannot be stopped at all. Read by the stop-resolution check, which drops
    any candidate stopper of that order. Executing it is a no-op (a Static marker)."""

    by_order: PlayOrder | None = None


@dataclass(frozen=True)
class AlsoLead(IRNode):
    """Static self-declaration that the source card may also be played as a Lead —
    starting a play chain without the normal order prerequisite — while ``condition``
    holds ("If you have no other cards in your hand, this card is also a Lead" —
    Broken Butterfly). Read by the engine's playability check; a no-op to execute."""

    condition: Condition = Always()


@dataclass(frozen=True)
class DoubleFinishIfBumped(IRNode):
    """A static self-declaration: double THIS card's printed Finish bonuses if the
    finisher bumped on the turn roll that set up the finish ("If you bumped on the
    last turn roll, double these bonuses" — T-Virus). Read by the finish sequence,
    which doubles the card's ``bonus_for`` contribution; a no-op to execute."""


@dataclass(frozen=True)
class RevealAndDiscard(IRNode):
    """Reveal ``count`` random cards from ``who``'s hand and discard those that can
    act as Stops ("Your opponent randomly reveals 3 cards in their hand and discards
    all revealed Stops" — Spin Wheel Kick). Distinct from :class:`Discard`, which
    drops a fixed count: here 0..``count`` leave, depending on how many revealed cards
    are Stops. The RNG picks which cards are revealed."""

    count: int = 3
    who: Who = Who.OPP


@dataclass(frozen=True)
class RevealForDraw(IRNode):
    """"Your opponent randomly reveals ``count`` card(s) in their hand: draw ``draw``
    for each revealed card matched by ``match_on`` — a Stop (Bartholomew Hooke), or a
    card whose move type equals the actor's just-rolled skill (The Winning Ticket).
    Reveals stay in hand."""

    who: Who = Who.OPP
    count: int = 1
    draw: int = 2
    match_on: RevealMatch = RevealMatch.STOP


@dataclass(frozen=True)
class BlankGimmick(IRNode):
    who: Who
    duration: Duration = Duration.WHILE_IN_PLAY


@dataclass(frozen=True)
class FlipGimmick(IRNode):
    """Turn ``who``'s competitor card over — a one-way transform to its back side
    (Copy Kat V2). Sets a persistent flip flag read by :class:`GimmickFlipped`, so the
    front's effects (gated ``Not(GimmickFlipped)``) switch off and the back's
    (gated ``GimmickFlipped``) switch on. Idempotent: flipping an already-flipped
    gimmick is a no-op, so re-firing on a later breakout does not flip back."""

    who: Who = Who.SELF


@dataclass(frozen=True)
class BlankText(IRNode):
    """A Static declaration: ``who``'s cards matching ``selector`` have their printed
    text blanked while the source stays in play — "Your opponent's Spotlights are
    blank." Read via ``GameState.is_text_blanked``; a blanked card fires none of its
    own effects and cannot stop. Never executed as a mutation."""

    selector: CardFilter = CardFilter()
    who: Who = Who.OPP


@dataclass(frozen=True)
class LoseBy(IRNode):
    kind: LoseKind
    who: Who = Who.SELF


@dataclass(frozen=True)
class DisqualificationRule(IRNode):
    """A Static match-rule toggle: ``enabled=False`` = "no disqualifications",
    ``enabled=True`` re-enables them. ``scope`` is who it reaches. Read at the
    disqualification-loss point, not executed."""

    enabled: bool = False
    scope: DqScope = DqScope.SELF


@dataclass(frozen=True)
class ConsideredCompare(IRNode):
    """A Static meta-comparison override "for card effects": the declaring player's
    ``domain`` comparison vs the opponent always resolves as ``order`` regardless of
    the real values (RaRa Perre "skills considered higher"; Theo V2 "considered fewer
    cards in hand"). Read in ``conditions.holds``, not executed."""

    domain: CompareDomain
    order: CompareOrder


@dataclass(frozen=True)
class SuppressOpponentDraw(IRNode):
    """A Static declaration: "your opponent does not draw for your card effects"
    (Sami "The Draw" Callihan). Read at ``_act_draw`` — a ``Draw(who=OPP)`` resolved
    by the declaring player is voided. Not executed as a mutation."""


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
    """A "+N to your Finish rolls" — added to the owner's Finish roll, summed across
    in-play cards. Finish attempts only; it does not help breakout rolls (a defender's
    rolls are a separate check). By default any-skill/flat; ``when_skill`` gates it to
    a Finish roll of that skill ("if either player rolls Agility for their Finish roll,
    their roll is +1"), and ``either`` marks that the bonus applies to whoever makes
    the Finish roll rather than only the card's owner."""

    delta: int
    when_skill: Skill | None = None  # None = any skill; else only when this skill is rolled
    either: bool = False  # applies to whichever player makes the Finish roll (Spin Wheel Kick)
    # When set, the bonus is ``delta * (count of per_who's cards in per_zone matching
    # this filter)`` — "your Finish roll is +1 for each Spotlight you have in play".
    per: CardFilter | None = None
    per_who: Who = Who.SELF
    per_zone: CountZone = CountZone.IN_PLAY


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
class FlipGimmickSigns(IRNode):
    """Gimmick marker (Cassandra): "change '+' to '-' and '-' to '+' on your target's
    Gimmick". While active, every printed +/- modifier on the **opponent's** gimmick is
    negated. Carried as a ``Static`` passive read when the opponent's gimmick effects
    are gathered (like :class:`LowestRollWins`), never executed as a mutation; the
    negation itself is :func:`flip_signs`. ``who`` is the target whose gimmick flips."""

    who: Who = Who.OPP


# Action nodes whose ``delta`` is a printed +/- modifier that :func:`flip_signs`
# negates. Count-like fields (Draw.n, Discard.count) carry no sign and are left alone.
_SIGNED_DELTA = (
    ModifyRoll,
    BuffSkill,
    CrowdMeter,
    MaxHandSize,
    FinishBonus,
    FinishRollBonus,
    BreakoutModifier,
)


_A = TypeVar("_A", bound=IRNode)


def _negate_action(action: _A) -> _A:
    """Negate the signed ``delta`` on one action (recursing into a :class:`Choice`'s
    branches); anything without a printed +/- is returned unchanged. The return type
    mirrors the input, so a :class:`Choice`'s ``tuple[Action, ...]`` stays that width."""
    if isinstance(action, Choice):
        return replace(
            action,
            options=tuple(
                replace(opt, actions=tuple(_negate_action(a) for a in opt.actions))
                for opt in action.options
            ),
        )
    if isinstance(action, _SIGNED_DELTA):
        return replace(action, delta=-action.delta)
    return action


def flip_signs(effect: Effect) -> Effect:
    """Return a copy of ``effect`` with every printed +/- modifier negated — the
    transform Cassandra's :class:`FlipGimmickSigns` applies to the opponent's gimmick."""
    return replace(effect, actions=tuple(_negate_action(a) for a in effect.actions))


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
    | InRoll
    | OnRollBoost
    | OnWinTurn
    | OnLoseTurn
    | OnStop
    | OnHit
    | OnBump
    | OnBury
    | StartOfTurn
    | StartOfMatch
    | OnBreakout
    | OnShuffle
    | OnDiscardMove
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
    | InPlayCompare
    | RollWasSkill
    | RollGapExactly
    | RollGapAtLeast
    | RollLeadAtLeast
    | RollValue
    | PrintedRollValue
    | SameRolledSkill
    | OppWonLastRoll
    | GimmickFlipped
    | DuringTurn
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
    | SwapHandDiscard
    | RecurToDeckTop
    | CountsAsInPlay
    | RemoveFromPlay
    | ReturnToHand
    | RevealAndDiscard
    | RevealForDraw
    | Peek
    | ModifyRoll
    | BuffSkill
    | MaxHandSize
    | AddText
    | Reroll
    | SwitchRolledSkill
    | WinTie
    | Bump
    | ElectBumpOnSameSkill
    | Stop
    | StopRequiresTag
    | BlankGimmick
    | FlipGimmick
    | BlankText
    | LoseBy
    | ConsideredCompare
    | SuppressOpponentDraw
    | CrowdMeter
    | PlayExtraCard
    | SetFinishRoll
    | FinishBonus
    | FinishRollBonus
    | BreakoutModifier
    | LowestRollWins
    | FlipGimmickSigns
    | Unstoppable
    | AlsoLead
    | DoubleFinishIfBumped
    | Choice
)

ActionOrUnsupported = Action | Unsupported
