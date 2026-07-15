"""Policy ABC + RandomPolicy, HeuristicPolicy — where "player skill" lives (§7).

A :class:`Policy` is handed a **decision point**, the **legal option set**, and
the (observable) :class:`~srg_sim.state.GameState`, and returns one option. The
engine logs every call as a ``decision`` event (``point`` + ``legal`` + ``chosen``
+ ``policy``), so the imitation-learning dataset (DESIGN.md §7, M4) falls out for
free — a ``LearnedPolicy`` consumes exactly these tuples.

Options are plain JSON-able dicts (a ``"kind"`` tag plus fields the engine maps
back to a card), so ``legal``/``chosen`` serialize directly into the log. The two
shipped policies:

* :class:`RandomPolicy` — uniform over the legal set, drawn from the engine's one
  seeded stream (``state.rng``), so a random game is still reproducible by seed.
* :class:`HeuristicPolicy` — small, transparent, playstyle-aware rules (build one
  chain while hoarding stops; spend stops on Finishes); the M1 baseline to beat.

Decision points (the skill surface): ``mulligan``, ``turn_action``, ``stop``,
``bury``, ``discard``, ``optional``, ``target``.
"""

from __future__ import annotations

from abc import ABC, abstractmethod
from typing import TYPE_CHECKING, Any

from srg_sim import conditions
from srg_sim.cards import Card, PlayOrder
from srg_sim.effects import Stop

if TYPE_CHECKING:
    from srg_sim.state import GameState

Option = dict[str, Any]


class Policy(ABC):
    """Chooses one legal option at each decision point (DESIGN.md §7)."""

    def __init__(self, name: str) -> None:
        self.name = name

    @abstractmethod
    def choose(self, point: str, legal: list[Option], state: GameState, key: str) -> Option:
        """Return one element of ``legal`` for player ``key`` at ``point``."""


class RandomPolicy(Policy):
    """Uniform choice over the legal set, using the engine's seeded RNG."""

    def __init__(self, name: str = "random") -> None:
        super().__init__(name)

    def choose(self, point: str, legal: list[Option], state: GameState, key: str) -> Option:
        return state.rng.reveal(legal)


class HeuristicPolicy(Policy):
    """A transparent, playstyle-aware baseline (SUPERSHOW_MECHANICS §3, user notes).

    Offense: go for the win when a Finish is playable; otherwise build **one** chain
    minimally (a Lead, then a Follow Up), committing the *least valuable* card and
    **holding online stops back** — then pass to gather stops rather than play a
    stop as a weak attack. Defense: spend a stop on the real threat (a Finish) and
    let Leads / Follow Ups resolve, since a stop is worth more saved for the Finish.
    """

    def __init__(self, name: str = "heuristic") -> None:
        super().__init__(name)

    def choose(self, point: str, legal: list[Option], state: GameState, key: str) -> Option:
        handler = getattr(self, f"_at_{point}", None)
        return handler(legal, state, key) if handler else legal[0]

    def _at_mulligan(self, legal: list[Option], state: GameState, key: str) -> Option:
        """Keep a hand that can open (has a Lead); otherwise redraw once."""
        has_lead = any(c.play_order is PlayOrder.LEAD for c in state.players[key].hand)
        return _by_kind(legal, "keep" if has_lead else "redraw") or legal[0]

    def _at_turn_action(self, legal: list[Option], state: GameState, key: str) -> Option:
        plays = [o for o in legal if o.get("kind") == "play"]
        finish = next((o for o in plays if o.get("order") == "Finish"), None)
        if finish is not None:
            return finish  # go for the win
        need = _next_build_order(state.players[key].in_play)
        if need is not None:
            candidate = self._cheapest_builder(
                [o for o in plays if o.get("order") == need], state, key
            )
            if candidate is not None:
                return candidate  # advance one chain with a non-stop / offline stop
        return _by_kind(legal, "pass") or legal[0]  # hold stops, pass to gather more

    def _at_stop(self, legal: list[Option], state: GameState, key: str) -> Option:
        stops = [o for o in legal if o.get("kind") == "stop"]
        if stops and legal[0].get("vs_order") == "Finish":
            return stops[0]  # the real threat — spend a stop
        return legal[0]  # let a Lead / Follow Up resolve; save the stop

    def _at_bury(self, legal: list[Option], state: GameState, key: str) -> Option:
        """When passing, recycle the most valuable card from discard back into the
        deck: a Finish to re-attempt (the "keep pushing the stopped Finish" line),
        then a stop to re-defend, before dead cards."""
        return max(legal, key=lambda o: _recycle_value(_discard_card(state, key, o["card"])))

    def _at_discard(self, legal: list[Option], state: GameState, key: str) -> Option:
        """Shed the least valuable card (hand-cap or a forced discard): a dead card
        before an offline stop before an online stop, and never a needed chain piece
        or a Finish unless forced — then the least valuable Finish (§7, user notes)."""
        return min(
            legal, key=lambda o: _discard_keep_value(_hand_card(state, key, o["card"]), state, key)
        )

    def _at_optional(self, legal: list[Option], state: GameState, key: str) -> Option:
        """Take optional edges (reroll / self-buff) when offered."""
        return _by_kind(legal, "yes") or legal[0]

    @staticmethod
    def _cheapest_builder(options: list[Option], state: GameState, key: str) -> Option | None:
        """The card we'd most willingly commit to build a chain: the least valuable
        (non-stop < offline stop). Returns None if the only options are ONLINE stops
        — we never spend those offensively; pass and hoard them instead."""
        if not options:
            return None
        best = min(
            options, key=lambda o: _play_value(_hand_card(state, key, o["card"]), state, key)
        )
        return best if _play_value(_hand_card(state, key, best["card"]), state, key) < 2 else None


def _next_build_order(board: list[Card]) -> str | None:
    """The next chain link to commit: a Lead if none in play, then a Follow Up;
    None once the chain is Lead+Follow Up (wait to draw a Finish)."""
    if not any(c.play_order is PlayOrder.LEAD for c in board):
        return "Lead"
    if not any(c.play_order is PlayOrder.FOLLOWUP for c in board):
        return "Followup"
    return None


def has_stop_effect(card: Card) -> bool:
    """True iff the card can stop anything (carries a parsed ``Stop`` action)."""
    return any(isinstance(a, Stop) for eff in card.effects for a in eff.actions)


def _stop_online(card: Card, state: GameState, key: str) -> bool:
    return any(
        isinstance(a, Stop) and conditions.holds(eff.condition, state, key)
        for eff in card.effects
        for a in eff.actions
    )


def _play_value(card: Card, state: GameState, key: str) -> int:
    """How reluctant we are to play this card (0 spend freely … 2 hold): a non-stop
    is 0, an offline stop 1, an online stop 2 (keep it for defense)."""
    if not has_stop_effect(card):
        return 0
    return 2 if _stop_online(card, state, key) else 1


def _discard_keep_value(card: Card, state: GameState, key: str) -> int:
    """How reluctant we are to discard this hand card (higher = keep longer):
    a Finish (win condition) > a chain piece the board still needs > an online stop
    (ready defense) > an offline stop (might come online) > a dead card. So a forced
    discard sheds dead cards first and protects the line being pushed."""
    if card.play_order is PlayOrder.FINISH:
        return 4
    if _needed_piece(card, state, key):
        return 3
    if has_stop_effect(card):
        return 2 if _stop_online(card, state, key) else 1
    return 0


def _needed_piece(card: Card, state: GameState, key: str) -> bool:
    """A Lead / Follow Up whose order the player's persistent board still needs to
    advance the chain (so we hold it rather than discard it)."""
    return card.play_order.value == _next_build_order(state.players[key].in_play)


def _recycle_value(card: Card) -> int:
    """Priority for recycling a discard card back into the deck (higher = keep):
    a Finish (re-attempt) over a stop (re-defend) over a dead card."""
    if card.play_order is PlayOrder.FINISH:
        return 3
    if has_stop_effect(card):
        return 2
    return 1


def _hand_card(state: GameState, key: str, uuid: str) -> Card:
    return next(c for c in state.players[key].hand if c.db_uuid == uuid)


def _discard_card(state: GameState, key: str, uuid: str) -> Card:
    return next(c for c in state.players[key].discard if c.db_uuid == uuid)


def _by_kind(legal: list[Option], kind: str) -> Option | None:
    return next((o for o in legal if o.get("kind") == kind), None)
