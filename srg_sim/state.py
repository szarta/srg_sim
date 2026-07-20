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
from typing import TYPE_CHECKING, Any, cast

from srg_sim import conditions
from srg_sim.cards import Card, Competitor, EntranceCard, Skill
from srg_sim.effects import (
    Always,
    BlankGimmick,
    BlankText,
    BuffSkill,
    CardFilter,
    Duration,
    Condition,
    CountZone,
    Duration,
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
class TimedBuff:
    """One live timed skill buff on a player (DESIGN.md §3).

    Unlike the continuous Static buffs — re-derived from the board on every stats
    read — a timed buff is granted imperatively when its effect fires and persists
    as state until its sweep. ``source`` is the granting clause: re-firing the SAME
    clause accumulates into the existing entry (clamped to ``cap``), which is what
    makes "(Max +5 to each)" a ceiling across repeat triggers. ``granted_turn`` lets
    the UNTIL_START_OF_YOUR_NEXT_TURN sweep tell the granting turn from the owner's
    next active turn.
    """

    skill: Skill
    delta: int
    until: Duration
    source: str
    cap: int | None = None
    granted_turn: int = 0

    def to_dict(self) -> dict[str, Any]:
        return {
            "skill": self.skill.value,
            "delta": self.delta,
            "until": self.until.value,
            "source": self.source,
            "cap": self.cap,
            "granted_turn": self.granted_turn,
        }

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> TimedBuff:
        return cls(
            skill=Skill(data["skill"]),
            delta=data["delta"],
            until=Duration(data["until"]),
            source=data["source"],
            cap=data.get("cap"),
            granted_turn=data.get("granted_turn", 0),
        )


@dataclass
class PendingText:
    """A queued one-shot "added text" waiting for its target's next matching card
    (:class:`AddTextToNext` — the Madness trio).

    Held on the TARGET player, not the source card, which is what makes it survive the
    source leaving the board (srgpc: poison "stays active until fulfilled even if
    removed from the board"). Consumed when a matching card is played, whether or not
    that card is then stopped."""

    selector: CardFilter
    effects: tuple[Effect, ...] = ()
    source: str = ""

    def to_dict(self) -> dict[str, Any]:
        return {
            "selector": self.selector.to_dict(),
            "effects": [e.to_dict() for e in self.effects],
            "source": self.source,
        }

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> PendingText:
        from srg_sim.effects import from_dict as node_from_dict

        return cls(
            selector=cast(CardFilter, node_from_dict(data["selector"])),
            effects=tuple(cast(Effect, node_from_dict(e)) for e in data["effects"]),
            source=data.get("source", ""),
        )


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
    # One-shot "re-roll your NEXT turn roll" grants (King Brian Cage): ``next`` set
    # when the effect fires, promoted to ``this`` at the owner's next turn start.
    reroll_grants: dict[str, int] = field(default_factory=lambda: {"this": 0, "next": 0})
    # Live TIMED skill buffs granted to THIS player (stored on the target, not the
    # granter); folded into derived stats and swept at the matching turn boundary.
    timed_buffs: list[TimedBuff] = field(default_factory=list)
    # The option bound by ChooseName ("Choose 1: 'Kendo Stick', ..." — Raven), fixed
    # for the rest of the match and read by the ChosenNameIs condition.
    chosen_name: str | None = None
    # Queued one-shot "added text" for this player's next matching card; survives the
    # source card leaving play.
    pending_text: list[PendingText] = field(default_factory=list)
    # Set when THIS player's gimmick was blanked "until their next turn" (Stiff Right
    # Hand) — the turn it was granted on. Swept, with gimmick_blanked, at the start of
    # this player's next ACTIVE turn. Stored state, so like every poison it outlives
    # the source card leaving the board.
    blank_until_next_turn: int | None = None
    freq_counters: dict[str, int] = field(default_factory=dict)
    gimmick_blanked: bool = False
    gimmick_flipped: bool = False  # competitor card turned to its back side (Copy Kat V2)
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
            "reroll_grants": dict(self.reroll_grants),
            "timed_buffs": [b.to_dict() for b in self.timed_buffs],
            "chosen_name": self.chosen_name,
            "pending_text": [p.to_dict() for p in self.pending_text],
            "blank_until_next_turn": self.blank_until_next_turn,
            "freq_counters": dict(self.freq_counters),
            "gimmick_blanked": self.gimmick_blanked,
            "gimmick_flipped": self.gimmick_flipped,
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
            reroll_grants=dict(data.get("reroll_grants", {"this": 0, "next": 0})),
            timed_buffs=[TimedBuff.from_dict(b) for b in data.get("timed_buffs", [])],
            chosen_name=data.get("chosen_name"),
            pending_text=[PendingText.from_dict(p) for p in data.get("pending_text", [])],
            blank_until_next_turn=data.get("blank_until_next_turn"),
            freq_counters=dict(data["freq_counters"]),
            gimmick_blanked=data["gimmick_blanked"],
            gimmick_flipped=data.get("gimmick_flipped", False),
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
    # The previous turn's roll-off winner (None before turn 1), for a re-roll gimmick
    # gated on "your opponent won the last turn roll" (Robert 'The Brain' Dunn).
    last_roll_winner: str | None = None
    # db_uuids whose text is blanked for the REST OF THIS TURN by BlankStoppedText;
    # card-identity scoped (not selector scoped) and cleared by the turn-boundary sweep.
    blanked_text: set[str] = field(default_factory=set)
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
        entrance or an in-play card, WHOSE CONDITION HOLDS (e.g. Savor the Moment:
        "if you have Enjoy Everything in play, your opponent's Gimmick is blank";
        DESIGN.md §3/§5). Derived like a Static buff, so a WHILE_IN_PLAY blank clears
        the moment its source leaves play or its condition stops holding. A gimmick
        MAY blank the opponent's gimmick (GM Calace V2, Mr. Snap V1): the owner's own
        Static competitor effects are scanned too, but only while that owner's gimmick
        is itself active. The re-entrancy guard defends the resulting blank<->blank
        loop and the pathological case of a blank gated on a stat comparison (whose
        evaluation reads effective_stats -> _buff_sources -> here again)."""
        if self.players[key].gimmick_blanked:
            return True
        guard: set[str] = self.__dict__.setdefault("_blank_guard", set())
        if key in guard:
            return False  # re-entrant stat-gated blank: fall back to no blank
        guard.add(key)
        try:
            for owner, player in self.players.items():
                # A gimmick-sourced continuous blank ("while you have 5 X in play, your
                # opponent's Gimmick is blank" — GM Calace V2, Mr. Snap V1) fires only
                # while the owner's OWN gimmick is active; entrance/in-play blanks always
                # apply. The blank<->blank recursion is bounded by the guard. Only a
                # Static blank is continuous here — a *triggered* BlankGimmick
                # (OnRoll/OnHit) latches the flag via the executor instead.
                gimmick = () if self.is_gimmick_blanked(owner) else player.competitor.effects
                for effects in (
                    gimmick,
                    player.entrance.effects,
                    *(c.effects for c in player.in_play),
                ):
                    for eff in effects:
                        if not isinstance(eff.trigger, Static):
                            continue
                        targets = any(
                            isinstance(a, BlankGimmick)
                            and (owner if a.who is Who.SELF else self.opponent_of(owner)) == key
                            for a in eff.actions
                        )
                        if targets and conditions.holds(eff.condition, self, owner):
                            return True
        finally:
            guard.discard(key)
        return False

    def is_text_blanked(self, card: Card, owner: str) -> bool:
        """Whether ``card`` (owned by ``owner``) has its printed text blanked — some
        player has an active Static ``BlankText`` (on an entrance or in-play card)
        whose ``who`` targets ``owner`` and whose ``selector`` matches the card ("Your
        opponent's Spotlights are blank"). A blanked card fires none of its own
        effects and cannot stop.

        A card blanked by a stop this turn (``blanked_text``, from ``BlankStoppedText``)
        stays blanked regardless of zone until the turn ends."""
        if card.db_uuid in self.blanked_text:
            return True
        for decl_owner, player in self.players.items():
            # (effects, is_discard) per source zone. A WHILE_IN_DISCARD effect is active
            # only from the discard pile ("when this card is in your discard pile, …");
            # every other duration is active only while the source is in play/entrance.
            live = [(player.entrance.effects, False)] + [
                (c.effects, False) for c in player.in_play
            ]
            dead = [(c.effects, True) for c in player.discard]
            for effects, is_discard in live + dead:
                for eff in effects:
                    if (eff.duration is Duration.WHILE_IN_DISCARD) != is_discard:
                        continue  # effect not active from this zone
                    hit = any(
                        isinstance(a, BlankText)
                        and (decl_owner if a.who is Who.SELF else self.opponent_of(decl_owner))
                        == owner
                        and conditions.card_matches(card, a.selector)
                        for a in eff.actions
                    )
                    if hit and conditions.holds(eff.condition, self, decl_owner):
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
        # TIMED buffs are already resolved (condition checked, delta accumulated and
        # capped at grant time), so they fold in unconditionally. Folding at this one
        # chokepoint is what makes them apply to turn rolls, Finish rolls and breakout
        # rolls alike; a stop that becomes a Finish can roll on the opponent's turn,
        # while the buff is still live.
        for buff in self.players[key].timed_buffs:
            stats[buff.skill.value] += buff.delta
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
                    skill_key, delta = self._resolve_buff(buff, target)
                    stats[skill_key] += delta

    def _resolve_buff(self, buff: BuffSkill, target: str) -> tuple[str, int]:
        """The ``(skill-key, delta)`` a buff contributes, expanding Copy Kat's dynamic
        variants: ``target_highest`` retargets to the target's highest base skill (ties
        broken by stat order, deterministically); ``per_crowd`` uses the Crowd Meter as
        the delta, clamped to ``cap`` when set. A plain buff returns ``(skill, delta)``."""
        if buff.target_highest:
            base = self.players[target].competitor.stats.to_dict()
            skill_key = max(base, key=lambda k: base[k])
        else:
            skill_key = buff.skill.value
        if buff.per_crowd:
            delta = self.crowd_meter if buff.cap is None else min(self.crowd_meter, buff.cap)
        elif buff.per is not None:
            # "+delta for each card in `per_zone` matching `per`", clamped to cap.
            raw = self._count_in_zone(buff.per, buff.per_zone, target) * buff.delta
            delta = raw if buff.cap is None else min(raw, buff.cap)
        else:
            delta = buff.delta
        return skill_key, delta

    def _count_in_zone(self, filt: CardFilter, zone: CountZone, target: str) -> int:
        """Count the target's cards in ``zone`` matching ``filt`` (per-count buffs)."""
        player = self.players[target]
        cards = player.in_play if zone is CountZone.IN_PLAY else player.discard
        return sum(1 for c in cards if conditions.card_matches(c, filt))

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
        The viewer's own hand is fully visible; an opponent's hand is also revealed
        while an active :class:`~srg_sim.effects.Peek` ("Look at your opponent's
        hand") grants ``viewer`` a look this turn (:meth:`_peeked`). RNG, per-player ``flags``,
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
        # Own hand always full; an opponent's hand is a count only unless a Peek
        # ("Look at your opponent's hand") is revealing it this turn (info model #38).
        if key == viewer or self._peeked(viewer, key):
            view["hand"] = _cards_to_list(player.hand)
        else:
            view["hand_size"] = len(player.hand)  # opponent hand: count only
        return view

    def _peeked(self, viewer: str, key: str) -> bool:
        """Whether ``viewer`` has an active peek on ``key``'s hand: a :class:`Peek`
        action ("Look at your opponent's hand") grants a look for the rest of the
        peeker's turn, so a stored peek expires automatically once ``turn_no``
        advances past it (DESIGN.md §7). ``viewer`` never peeks their own hand."""
        if viewer == key:
            return False
        peek = self.players[viewer].flags.get("peek")
        return isinstance(peek, dict) and peek.get(key) == self.turn_no

    def to_dict(self) -> dict[str, Any]:
        """Snapshot the position (players, crowd meter, turn, RNG). Excludes the log."""
        return {
            "players": {k: p.to_dict() for k, p in self.players.items()},
            "rng": self.rng.snapshot(),
            "crowd_meter": self.crowd_meter,
            "active": self.active,
            "turn_no": self.turn_no,
            "last_roll_winner": self.last_roll_winner,
            "blanked_text": sorted(self.blanked_text),
        }

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> GameState:
        return cls(
            players={k: PlayerState.from_dict(p) for k, p in data["players"].items()},
            rng=SeededRNG.restore(data["rng"]),
            crowd_meter=data["crowd_meter"],
            active=data["active"],
            turn_no=data["turn_no"],
            last_roll_winner=data.get("last_roll_winner"),
            blanked_text=set(data.get("blanked_text", [])),
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
