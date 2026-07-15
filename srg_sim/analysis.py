"""Batch match runner for matchup analysis (DESIGN.md §10, M2).

Run N seeded games for one fixed matchup — two enriched :class:`~srg_sim.cards.Deck`
s plus two policy factories — and collect their outcomes. Each game is a pure
function of its seed (:func:`~srg_sim.engine.Engine` routes all randomness through
the seeded RNG), so a batch is reproducible and order-independent: game *i* uses
seed ``base + i`` and depends on nothing else. This module is the foundation the
aggregation layer (win-rate, finish mix, crowd-meter curves — todo #15) builds on;
it deliberately does no summarizing itself, only produces the per-game record.
"""

from __future__ import annotations

from collections.abc import Callable, Iterable
from dataclasses import dataclass

from srg_sim.cards import Deck
from srg_sim.engine import Engine, GameResult
from srg_sim.gamelog import GameLog
from srg_sim.policy import HeuristicPolicy, Policy

PolicyFactory = Callable[[], Policy]


@dataclass(frozen=True)
class Matchup:
    """A fixed pairing to batch over: two enriched decks and two policy factories.

    Policies are supplied as **factories** (not instances) so every game gets a
    fresh pair — determinism holds even if a policy carries per-game state (e.g. a
    future ``LearnedPolicy``). A ``Deck`` is immutable across a game (the engine
    copies each side into play and never mutates the source), so one ``Deck``
    object is safely reused for the whole batch.
    """

    deck_a: Deck
    deck_b: Deck
    policy_a: PolicyFactory = HeuristicPolicy
    policy_b: PolicyFactory = HeuristicPolicy
    created: str = ""


@dataclass(frozen=True)
class GameOutcome:
    """One game's result, tagged with the seed that produced it.

    ``log`` is populated only when the batch is asked to keep logs (it is heavy
    for large N); the result alone is enough for win-rate/length aggregation.
    """

    seed: int
    result: GameResult
    log: GameLog | None = None


def run_game(matchup: Matchup, seed: int, *, keep_log: bool = False) -> GameOutcome:
    """Play one match at ``seed`` and return its outcome.

    Pure in ``(matchup, seed)``: re-running yields the same result (and a
    byte-identical log). The log is retained only when ``keep_log`` is set.
    """
    engine = Engine(
        matchup.deck_a,
        matchup.deck_b,
        matchup.policy_a(),
        matchup.policy_b(),
        seed=seed,
        created=matchup.created,
    )
    result = engine.play()
    return GameOutcome(seed=seed, result=result, log=engine.state.log if keep_log else None)


def run_batch(
    matchup: Matchup, seeds: Iterable[int], *, keep_logs: bool = False
) -> list[GameOutcome]:
    """Play one game per seed in ``seeds``; return outcomes in iteration order.

    ``seeds`` is any iterable of ints (commonly :func:`seed_range`). Games are
    independent, so the outcome for a given seed does not depend on batch order.
    """
    return [run_game(matchup, seed, keep_log=keep_logs) for seed in seeds]


def seed_range(count: int, start: int = 0) -> range:
    """The contiguous seed range ``[start, start + count)`` for ``count`` games."""
    if count < 0:
        raise ValueError(f"count must be non-negative, got {count}")
    return range(start, start + count)
