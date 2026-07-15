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
``bury``, ``optional``, ``target``.
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
            candidate = self._least_valuable(
                [o for o in plays if o.get("order") == need], state, key
            )
            if candidate is not None:
                return candidate  # advance one chain with the least valuable card
        return _by_kind(legal, "pass") or legal[0]  # hold stops, pass to gather more

    def _at_stop(self, legal: list[Option], state: GameState, key: str) -> Option:
        stops = [o for o in legal if o.get("kind") == "stop"]
        if stops and legal[0].get("vs_order") == "Finish":
            return stops[0]  # the real threat — spend a stop
        return legal[0]  # let a Lead / Follow Up resolve; save the stop

    def _at_bury(self, legal: list[Option], state: GameState, key: str) -> Option:
        """Recycle a card from discard back into the deck (refined in the bury task)."""
        return legal[0]

    def _at_optional(self, legal: list[Option], state: GameState, key: str) -> Option:
        """Take optional edges (reroll / self-buff) when offered."""
        return _by_kind(legal, "yes") or legal[0]

    @staticmethod
    def _least_valuable(options: list[Option], state: GameState, key: str) -> Option | None:
        """The build card we'd most willingly spend: non-stops first, then offline
        stops, holding online stops (highest value) in hand for defense."""
        if not options:
            return None
        return min(
            options, key=lambda o: _play_value(_hand_card(state, key, o["card"]), state, key)
        )


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


def _hand_card(state: GameState, key: str, uuid: str) -> Card:
    return next(c for c in state.players[key].hand if c.db_uuid == uuid)


def _by_kind(legal: list[Option], kind: str) -> Option | None:
    return next((o for o in legal if o.get("kind") == kind), None)
