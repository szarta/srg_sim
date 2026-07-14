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
* :class:`HeuristicPolicy` — small, transparent rules (aggressive attacker,
  defends when a stop is online); the M1 baseline to beat.

Decision points (the skill surface): ``mulligan``, ``turn_action``, ``continue``,
``stop``, ``bury``, ``optional``, ``target``.
"""

from __future__ import annotations

from abc import ABC, abstractmethod
from typing import TYPE_CHECKING, Any

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
    """A transparent baseline: attack hard, defend when a stop is available."""

    def __init__(self, name: str = "heuristic") -> None:
        super().__init__(name)

    def choose(self, point: str, legal: list[Option], state: GameState, key: str) -> Option:
        handler = getattr(self, f"_at_{point}", None)
        return handler(legal, state, key) if handler else legal[0]

    def _at_mulligan(self, legal: list[Option], state: GameState, key: str) -> Option:
        """Keep a hand that can open (has a Lead); otherwise redraw once."""
        has_lead = any(c.play_order.value == "Lead" for c in state.players[key].hand)
        want = "keep" if has_lead else "redraw"
        return _by_kind(legal, want) or legal[0]

    def _at_turn_action(self, legal: list[Option], state: GameState, key: str) -> Option:
        """Play a card if any is legal (prefer a Finish); else pass."""
        return self._best_play(legal) or _by_kind(legal, "pass") or legal[0]

    def _at_continue(self, legal: list[Option], state: GameState, key: str) -> Option:
        """Extend the combo toward a Finish; else stop the chain."""
        return self._best_play(legal) or _by_kind(legal, "stop_chain") or legal[0]

    def _at_stop(self, legal: list[Option], state: GameState, key: str) -> Option:
        """Stop the attack whenever a legal stop exists."""
        return _by_kind(legal, "stop") or legal[0]

    def _at_bury(self, legal: list[Option], state: GameState, key: str) -> Option:
        """Bury a non-Lead card first, keeping openers in hand."""
        non_lead = next((o for o in legal if o.get("order") != "Lead"), None)
        return non_lead or legal[0]

    def _at_optional(self, legal: list[Option], state: GameState, key: str) -> Option:
        """Take optional edges (reroll / self-buff) when offered."""
        return _by_kind(legal, "yes") or legal[0]

    @staticmethod
    def _best_play(legal: list[Option]) -> Option | None:
        """The highest-stage play option available (Finish > Followup > Lead)."""
        plays = [o for o in legal if o.get("kind") == "play"]
        if not plays:
            return None
        rank = {"Finish": 3, "Followup": 2, "Lead": 1}
        return max(plays, key=lambda o: rank.get(o.get("order", ""), 0))


def _by_kind(legal: list[Option], kind: str) -> Option | None:
    return next((o for o in legal if o.get("kind") == kind), None)
