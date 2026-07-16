"""GameState / PlayerState with serializable snapshots (DESIGN.md §5).

The state is **mutable** (the engine advances it in place) but fully
snapshottable: :meth:`GameState.to_dict` / :meth:`from_dict` round-trip every
zone plus the RNG's internal state, so any position is reproducible and diffable.
The event log (``gamelog``) is the separate JSONL stream and is intentionally
*not* part of a state snapshot.

**Derived stats (DESIGN.md §5).** There is no stored ``static_buffs``. A player's
effective skills are *computed on demand* from base competitor stats plus every
active ``Static`` ``BuffSkill``: those on cards in ``in_play`` (source always
present), on the entrance (present all match), and on the competitor gimmick
*unless* it is blanked. A ``BuffSkill`` targets ``SELF`` (its owner) or ``OPP``
(the other player), so a card can buff either side. This one view feeds turn
rolls, stop checks, and breakout rolls, so a card leaving play or a gimmick being
blanked simply drops out of the recomputation.
"""

from __future__ import annotations

from collections.abc import Callable, Iterable, Iterator
from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Any

from srg_sim.cards import Card, Competitor, EntranceCard, Skill
from srg_sim.effects import (
    Always,
    BlankGimmick,
    BuffSkill,
    Condition,
    Effect,
    MaxHandSize,
    Static,
    Who,
)
from srg_sim.rng import SeededRNG

if TYPE_CHECKING:
    from srg_sim.gamelog import GameLog

# A condition evaluator the engine can supply so conditional Static buffs resolve
# against live state; without one, only unconditional (``Always``) buffs apply.
ConditionHolds = Callable[[Condition], bool]


@dataclass
class PlayerState:
    """One side's competitor, entrance, and card zones (DESIGN.md §5).

    ``pending_roll_mods`` holds ``{"this", "next"}`` turn-roll deltas;
    ``freq_counters`` tracks per-turn / per-match frequency-guard usage; ``flags``
    is a scratch dict for one-off engine bookkeeping.
    """

    competitor: Competitor
    entrance: EntranceCard
    hand: list[Card] = field(default_factory=list)
    deck: list[Card] = field(default_factory=list)
    discard: list[Card] = field(default_factory=list)
    in_play: list[Card] = field(default_factory=list)
    pending_roll_mods: dict[str, int] = field(default_factory=lambda: {"this": 0, "next": 0})
    freq_counters: dict[str, int] = field(default_factory=dict)
    gimmick_blanked: bool = False
    flags: dict[str, Any] = field(default_factory=dict)

    def draw(self, n: int = 1) -> list[Card]:
        """Move up to ``n`` cards from the top of ``deck`` to ``hand``; return them."""
        drawn = self.deck[:n]
        del self.deck[:n]
        self.hand.extend(drawn)
        return drawn

    def to_dict(self) -> dict[str, Any]:
        return {
            "competitor": self.competitor.to_dict(),
            "entrance": self.entrance.to_dict(),
            "hand": _cards_to_list(self.hand),
            "deck": _cards_to_list(self.deck),
            "discard": _cards_to_list(self.discard),
            "in_play": _cards_to_list(self.in_play),
            "pending_roll_mods": dict(self.pending_roll_mods),
            "freq_counters": dict(self.freq_counters),
            "gimmick_blanked": self.gimmick_blanked,
            "flags": dict(self.flags),
        }

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> PlayerState:
        return cls(
            competitor=Competitor.from_dict(data["competitor"]),
            entrance=EntranceCard.from_dict(data["entrance"]),
            hand=_cards_from_list(data["hand"]),
            deck=_cards_from_list(data["deck"]),
            discard=_cards_from_list(data["discard"]),
            in_play=_cards_from_list(data["in_play"]),
            pending_roll_mods=dict(data["pending_roll_mods"]),
            freq_counters=dict(data["freq_counters"]),
            gimmick_blanked=data["gimmick_blanked"],
            flags=dict(data["flags"]),
        )


def _cards_to_list(cards: Iterable[Card]) -> list[dict[str, Any]]:
    return [c.to_dict() for c in cards]


def _cards_from_list(raw: list[dict[str, Any]]) -> list[Card]:
    return [Card.from_dict(c) for c in raw]


def _iter_static_buffs(effects: Iterable[Effect]) -> Iterator[tuple[Effect, BuffSkill]]:
    """Yield ``(effect, BuffSkill)`` for every Static buff in ``effects``.

    Only ``Static``-triggered effects fold into the derived-stats view; one-shot
    ``BuffSkill`` actions under other triggers are applied by the executor.
    """
    for eff in effects:
        if isinstance(eff.trigger, Static):
            for action in eff.actions:
                if isinstance(action, BuffSkill):
                    yield eff, action


def _iter_static_hand_mods(effects: Iterable[Effect]) -> Iterator[tuple[Effect, MaxHandSize]]:
    """Yield ``(effect, MaxHandSize)`` for every Static hand-cap modifier in
    ``effects`` — the derived-hand-cap analogue of :func:`_iter_static_buffs`."""
    for eff in effects:
        if isinstance(eff.trigger, Static):
            for action in eff.actions:
                if isinstance(action, MaxHandSize):
                    yield eff, action


@dataclass
class GameState:
    """Both players plus the shared match state (DESIGN.md §5).

    ``active`` is the player key whose turn it is; ``rng`` is the single seeded
    generator; ``log`` (when set) is the live event stream and is *not* snapshotted.
    """

    players: dict[str, PlayerState]
    rng: SeededRNG
    crowd_meter: int = 0
    active: str = "A"
    turn_no: int = 0
    log: GameLog | None = None

    def opponent_of(self, key: str) -> str:
        """The other player's key (two-player game)."""
        return next(k for k in self.players if k != key)

    def _buff_sources(
        self, owner: str, player: PlayerState
    ) -> Iterator[tuple[Iterable[Effect], bool]]:
        """(effects, active?) for each of a player's Static-buff sources. The
        competitor gimmick drops out while blanked (derived, so a WHILE_IN_PLAY
        blank ends when the blanking card leaves play)."""
        yield player.competitor.effects, not self.is_gimmick_blanked(owner)
        yield player.entrance.effects, True
        for card in player.in_play:
            yield card.effects, True

    def is_gimmick_blanked(self, key: str) -> bool:
        """Whether ``key``'s competitor gimmick is currently suppressed — by the
        stored flag (a one-shot / StartOfMatch blank set via the BlankGimmick
        handler) OR by any active BlankGimmick that targets ``key`` from an
        entrance or an in-play card (e.g. Savor the Moment; DESIGN.md §3/§5). The
        blank is derived like a Static buff, so a WHILE_IN_PLAY blank clears the
        moment its source leaves play (breakout). A gimmick never blanks itself, so
        competitor effects are not scanned here — no recursion with _buff_sources."""
        if self.players[key].gimmick_blanked:
            return True
        for owner, player in self.players.items():
            sources = [player.entrance.effects, *(c.effects for c in player.in_play)]
            for effects in sources:
                for eff in effects:
                    if any(
                        isinstance(a, BlankGimmick)
                        and (owner if a.who is Who.SELF else self.opponent_of(owner)) == key
                        for a in eff.actions
                    ):
                        return True
        return False

    def effective_stats(self, key: str, holds: ConditionHolds | None = None) -> dict[str, int]:
        """Derived ``{skill: value}`` for ``key`` (base + active Static buffs).

        ``holds`` optionally resolves conditional Static buffs against live state;
        without it, only unconditional buffs contribute (DESIGN.md §5).
        """
        stats = self.players[key].competitor.stats.to_dict()
        for owner, player in self.players.items():
            self._apply_owner_buffs(stats, key, owner, player, holds)
        return stats

    def _apply_owner_buffs(
        self,
        stats: dict[str, int],
        target: str,
        owner: str,
        player: PlayerState,
        holds: ConditionHolds | None,
    ) -> None:
        for effects, active in self._buff_sources(owner, player):
            if not active:
                continue
            for eff, buff in _iter_static_buffs(effects):
                if _buffs(owner, buff, target) and _condition_ok(eff.condition, holds):
                    stats[buff.skill.value] += buff.delta

    def effective_stat(self, key: str, skill: Skill, holds: ConditionHolds | None = None) -> int:
        """The single derived value for ``skill`` (convenience over :meth:`effective_stats`)."""
        return self.effective_stats(key, holds)[skill.value]

    def effective_hand_cap(self, key: str, base: int, holds: ConditionHolds | None = None) -> int:
        """Derived maximum hand size for ``key`` (``base`` + active Static hand mods).

        Folds every :class:`MaxHandSize` the way :meth:`effective_stats` folds
        Static buffs: a card raising your own cap or lowering your opponent's is
        read here on demand (DESIGN.md §5/§6). ``holds`` resolves conditional mods;
        without it only unconditional ones apply. Clamped at zero.
        """
        cap = base
        for owner, player in self.players.items():
            cap += self._owner_hand_mods(key, owner, player, holds)
        return max(0, cap)

    def _owner_hand_mods(
        self, target: str, owner: str, player: PlayerState, holds: ConditionHolds | None
    ) -> int:
        total = 0
        for effects, active in self._buff_sources(owner, player):
            if not active:
                continue
            for eff, mod in _iter_static_hand_mods(effects):
                if _targets(owner, mod.who, target) and _condition_ok(eff.condition, holds):
                    total += mod.delta
        return total

    def observable(self, viewer: str) -> dict[str, Any]:
        """What ``viewer`` may legitimately see (DESIGN.md §7 information model).

        The redacted view a human at the table would have — feeds M4 imitation
        learning so a policy trains on honest observations, not on hidden state.
        Public everywhere: both competitors, entrances, ``in_play`` boards,
        ``discard`` piles, and gimmick-blank status. Private: a player sees only
        the *size* of the opponent's hand, and **every** deck is a size only —
        deck order is hidden from everyone, owner included (the five-region model).
        The viewer's own hand is fully visible. RNG, per-player ``flags``,
        ``freq_counters``, and ``pending_roll_mods`` are engine bookkeeping, not
        table-visible zones, so they are omitted. Unlike :meth:`to_dict` this is a
        lossy projection — for replay/snapshots use ``to_dict``.
        """
        return {
            "viewer": viewer,
            "crowd_meter": self.crowd_meter,
            "active": self.active,
            "turn_no": self.turn_no,
            "players": {k: self._observe_player(k, viewer) for k in self.players},
        }

    def _observe_player(self, key: str, viewer: str) -> dict[str, Any]:
        """One player's zones as ``viewer`` sees them (see :meth:`observable`)."""
        player = self.players[key]
        view: dict[str, Any] = {
            "competitor": player.competitor.to_dict(),
            "entrance": player.entrance.to_dict(),
            "in_play": _cards_to_list(player.in_play),
            "discard": _cards_to_list(player.discard),
            "gimmick_blanked": self.is_gimmick_blanked(key),  # derived: stored flag or active blank
            "deck_size": len(player.deck),  # order hidden from everyone, owner included
        }
        if key == viewer:
            view["hand"] = _cards_to_list(player.hand)
        else:
            view["hand_size"] = len(player.hand)  # opponent hand: count only
        return view

    def to_dict(self) -> dict[str, Any]:
        """Snapshot the position (players, crowd meter, turn, RNG). Excludes the log."""
        return {
            "players": {k: p.to_dict() for k, p in self.players.items()},
            "rng": self.rng.snapshot(),
            "crowd_meter": self.crowd_meter,
            "active": self.active,
            "turn_no": self.turn_no,
        }

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> GameState:
        return cls(
            players={k: PlayerState.from_dict(p) for k, p in data["players"].items()},
            rng=SeededRNG.restore(data["rng"]),
            crowd_meter=data["crowd_meter"],
            active=data["active"],
            turn_no=data["turn_no"],
        )


def _targets(owner: str, who: Who, target: str) -> bool:
    """True iff an effect owned by ``owner`` with ``who`` lands on ``target``
    (SELF = owner, OPP = the other player)."""
    return (who is Who.SELF) == (owner == target)


def _buffs(owner: str, buff: BuffSkill, target: str) -> bool:
    """True iff a buff owned by ``owner`` lands on ``target`` (SELF=owner, OPP=other)."""
    return _targets(owner, buff.who, target)


def _condition_ok(condition: Condition, holds: ConditionHolds | None) -> bool:
    """Unconditional buffs always apply; conditional ones need a ``holds`` evaluator."""
    if isinstance(condition, Always):
        return True
    return holds is not None and holds(condition)
