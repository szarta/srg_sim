"""Post-game review: reconstruct each decision's player-view + oracle truth (§7/§8).

The instrumentation behind todo #42 — playing a match against the engine and then
learning from it. During play a human sees only :meth:`GameState.observable`
(their own hand, the opponent's hand *size*, deck *sizes*); the engine sees
everything. This module recovers **both** views, after the fact, for every
decision the human made, so a stronger line can be judged against the full state
*without* having influenced the live choice (DESIGN.md §10 M4 — "per-decision
divergence: how a human differs").

Nothing here changes the log schema (the review gate). A recorded match already
carries every ``decision`` event (``legal`` + ``chosen``) and a seed; because all
randomness is seeded and every human choice is recorded, **replaying** the
recorded decisions reproduces the match exactly. :class:`ReplayPolicy` feeds those
decisions back, and :func:`reconstruct` snapshots the observable and oracle states
at the instant the engine consults the policy — the "observable-state ref" §8
promised, materialized on demand rather than stored.
"""

from __future__ import annotations

from collections.abc import Iterable
from dataclasses import dataclass
from typing import TYPE_CHECKING, Any

from srg_sim.cards import Deck
from srg_sim.engine import Engine, GameResult
from srg_sim.gamelog import Decision, GameLog, Header, PlayerInfo
from srg_sim.policy import Option, ReplayPolicy

if TYPE_CHECKING:
    from srg_sim.loader import CardIndex
    from srg_sim.state import GameState

Overrides = dict[str, list[dict[str, object]]]


@dataclass(frozen=True)
class ReviewRecord:
    """One decision, with both the view the chooser had and the full oracle truth.

    ``player_view`` is exactly what a player at the table could know at that
    instant (:meth:`GameState.observable`); ``oracle`` is the complete position
    (:meth:`GameState.to_dict`) — opponent hand, deck order, RNG state and all.
    A review/critique pass reads ``player_view`` to reproduce the decision the
    human faced and ``oracle`` to score it against a line only hindsight allows.
    """

    turn: int
    point: str
    player: str
    policy: str
    legal: list[Option]
    chosen: Option
    player_view: dict[str, Any]
    oracle: dict[str, Any]

    def to_dict(self) -> dict[str, Any]:
        return {
            "turn": self.turn,
            "point": self.point,
            "player": self.player,
            "policy": self.policy,
            "legal": self.legal,
            "chosen": self.chosen,
            "player_view": self.player_view,
            "oracle": self.oracle,
        }


@dataclass(frozen=True)
class Reconstruction:
    """The result of replaying a recorded match: its outcome + every decision."""

    result: GameResult
    records: list[ReviewRecord]

    def for_player(self, key: str) -> list[ReviewRecord]:
        """Just ``key``'s decisions — the human's own turns, for a focused review."""
        return [r for r in self.records if r.player == key]


class _CapturingReplay(ReplayPolicy):
    """A :class:`ReplayPolicy` that also snapshots both views at each decision.

    Reconstruction rides the normal replay: the engine hands us the live
    :class:`GameState` when it consults the policy, so ``choose`` is the one place
    where the observable projection and the oracle snapshot line up with the
    recorded ``chosen`` — capture there, then defer to the replay for the choice.
    """

    def __init__(self, decisions: list[Option], policy_name: str, sink: list[ReviewRecord]) -> None:
        super().__init__(decisions, name=policy_name)
        self._sink = sink

    def choose(self, point: str, legal: list[Option], state: GameState, key: str) -> Option:
        chosen = super().choose(point, legal, state, key)
        self._sink.append(
            ReviewRecord(
                turn=state.turn_no,
                point=point,
                player=key,
                policy=self.name,
                legal=legal,
                chosen=chosen,
                player_view=state.observable(key),
                oracle=state.to_dict(),
            )
        )
        return chosen


def rebuild_decks(header: Header, index: CardIndex, overrides: Overrides) -> dict[str, Deck]:
    """Rebuild both players' compiled decks from a log header (§8 replay).

    Competitor/entrance by name, main cards by ``db_uuid`` in recorded order, then
    each deck's rules re-compiled to IR. Shared by ``srg-sim replay`` and review so
    a header rebuilds to the same decks either way.
    """
    return {key: _deck_from_info(info, index, overrides) for key, info in header.players.items()}


def _deck_from_info(info: PlayerInfo, index: CardIndex, overrides: Overrides) -> Deck:
    from srg_sim import rules_parser as rp

    deck = Deck(
        competitor=index.competitor(info.competitor),
        entrance=index.entrance(info.entrance),
        cards=tuple(index.main_card({"db_uuid": uuid}) for uuid in info.deck),
    )
    return rp.enrich_deck(deck, overrides)


def _decisions_by_player(log: GameLog) -> dict[str, list[Option]]:
    """Each player's recorded ``chosen`` options, in log order (one list per key)."""
    by_player: dict[str, list[Option]] = {key: [] for key in log.header.players}
    for event in log.events:
        if isinstance(event, Decision):
            by_player.setdefault(event.player, []).append(event.chosen)
    return by_player


def reconstruct(log: GameLog, index: CardIndex, overrides: Overrides) -> Reconstruction:
    """Replay a recorded match, capturing both views at every decision (§7/§8).

    Rebuilds the decks from the header (via the card index) and defers to
    :func:`reconstruct_with_decks`. Works on any recorded match — a ``kind:"real"``
    human game or a ``kind:"sim"`` log (the latter useful as a test vehicle and for
    reviewing how a policy played).
    """
    return reconstruct_with_decks(log, rebuild_decks(log.header, index, overrides))


def reconstruct_with_decks(log: GameLog, decks: dict[str, Deck]) -> Reconstruction:
    """Reconstruction core: drive the engine over already-compiled ``decks`` (§7/§8).

    Feeds each side a :class:`ReplayPolicy` built from the recorded ``decision``
    events and captures both views at every decision. Split from :func:`reconstruct`
    so it runs without a card index — the header seed, kind, and recorded decisions
    are all it needs, which also makes it directly testable on synthetic decks.
    """
    by_player = _decisions_by_player(log)
    sink: list[ReviewRecord] = []
    policies = {
        key: _CapturingReplay(by_player.get(key, []), log.header.players[key].policy, sink)
        for key in decks
    }
    engine = Engine(
        decks["A"],
        decks["B"],
        policies["A"],
        policies["B"],
        seed=log.header.seed,
        created=log.header.created,
        kind=log.header.kind,
    )
    result = engine.play()
    return Reconstruction(result=result, records=sink)


def records_to_ndjson(records: Iterable[ReviewRecord]) -> str:
    """One review record per line as JSON (NDJSON) — feeds the #36 training export."""
    import json

    return "".join(json.dumps(r.to_dict()) + "\n" for r in records)
