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

import math
import statistics
from collections import Counter
from collections.abc import Callable, Iterable, Sequence
from dataclasses import dataclass, field
from typing import Any

from srg_sim.cards import Deck
from srg_sim.engine import Engine, GameResult
from srg_sim.gamelog import CrowdMeter, GameLog
from srg_sim.policy import HeuristicPolicy, Policy

PolicyFactory = Callable[[], Policy]

SIDES = ("A", "B")
WILSON_Z = 1.96  # 95% two-sided normal quantile for the Wilson score interval


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


# ---------------------------------------------------------------------------
# Aggregation: a batch of outcomes -> MatchupReport (todo #15)
# ---------------------------------------------------------------------------


def wilson_interval(successes: int, n: int, z: float = WILSON_Z) -> tuple[float, float]:
    """Wilson score confidence interval for a binomial proportion (default 95%).

    Preferred over the normal approximation at the small N and extreme rates a
    lopsided matchup produces; returns ``(0.0, 0.0)`` for an empty sample.
    """
    if n == 0:
        return (0.0, 0.0)
    p = successes / n
    denom = 1 + z * z / n
    center = (p + z * z / (2 * n)) / denom
    margin = z * math.sqrt(p * (1 - p) / n + z * z / (4 * n * n)) / denom
    return (max(0.0, center - margin), min(1.0, center + margin))


@dataclass(frozen=True)
class MatchupReport:
    """Aggregate metrics for a batch of games, computed from their JSONL logs.

    Built by :meth:`from_outcomes` over a batch that kept its logs
    (``run_batch(..., keep_logs=True)``); every log-derived metric — finish-type
    mix, stop usage, the crowd-meter curve — needs the event stream, so an
    outcome without a log is rejected. :meth:`to_dict` yields JSON-ready primitives
    for the ``analyze`` CLI export (todo #16).
    """

    games: int
    wins: dict[str, int]  # "A" | "B" | "draw" -> count
    win_rate: dict[str, float]  # per side, as a fraction of all games
    win_ci: dict[str, tuple[float, float]]  # Wilson 95% interval per side
    reasons: dict[str, int]  # finish | count_out | disqualification | pinfall | turn_cap
    finish_types: dict[str, int]  # winning-finish atk_type -> count (finish wins only)
    length: dict[str, float]  # min | max | mean | median of game length (turns)
    stops: dict[str, float]  # mean stops played per game, per side
    crowd_meter_curve: list[float] = field(default_factory=list)  # mean CM by turn index

    @classmethod
    def from_outcomes(cls, outcomes: Sequence[GameOutcome]) -> MatchupReport:
        logs = _require_logs(outcomes)
        results = [o.result for o in outcomes]
        n = len(results)
        wins = _win_counts(results)
        turns = [r.turns for r in results]
        return cls(
            games=n,
            wins=wins,
            win_rate={s: (wins[s] / n if n else 0.0) for s in SIDES},
            win_ci={s: wilson_interval(wins[s], n) for s in SIDES},
            reasons=dict(Counter(r.reason for r in results)),
            finish_types=_finish_type_mix(outcomes),
            length=_length_stats(turns),
            stops=_stop_rates(logs, n),
            crowd_meter_curve=_crowd_curve(
                [_cm_series(lg, t) for lg, t in zip(logs, turns, strict=True)]
            ),
        )

    def to_dict(self) -> dict[str, Any]:
        """JSON-ready view (CI tuples become ``[lo, hi]`` lists) for export."""
        return {
            "games": self.games,
            "wins": dict(self.wins),
            "win_rate": dict(self.win_rate),
            "win_ci": {s: list(ci) for s, ci in self.win_ci.items()},
            "reasons": dict(self.reasons),
            "finish_types": dict(self.finish_types),
            "length": dict(self.length),
            "stops": dict(self.stops),
            "crowd_meter_curve": list(self.crowd_meter_curve),
        }


def _require_logs(outcomes: Sequence[GameOutcome]) -> list[GameLog]:
    if any(o.log is None for o in outcomes):
        raise ValueError("MatchupReport needs game logs — run the batch with keep_logs=True")
    return [o.log for o in outcomes if o.log is not None]


def _win_counts(results: Sequence[GameResult]) -> dict[str, int]:
    counts = {"A": 0, "B": 0, "draw": 0}
    for r in results:
        counts[r.winner if r.winner in counts else "draw"] += 1
    return counts


def _length_stats(turns: Sequence[int]) -> dict[str, float]:
    if not turns:
        return {"min": 0.0, "max": 0.0, "mean": 0.0, "median": 0.0}
    return {
        "min": float(min(turns)),
        "max": float(max(turns)),
        "mean": statistics.fmean(turns),
        "median": float(statistics.median(turns)),
    }


def _finish_type_mix(outcomes: Sequence[GameOutcome]) -> dict[str, int]:
    """Count winning finishes by the finishing card's attack type (finish wins only)."""
    mix: Counter[str] = Counter()
    for outcome in outcomes:
        if outcome.result.reason == "finish" and outcome.log is not None:
            atk = _winning_finish_type(outcome.log)
            if atk is not None:
                mix[atk] += 1
    return dict(mix)


def _winning_finish_type(log: GameLog) -> str | None:
    """The atk_type of the winning finish: the last ``Finish`` play in the game,
    which — because a successful finish ends the match — is the one that won."""
    atk: str | None = None
    for event in log.events:
        if event.TYPE == "play" and getattr(event, "order", None) == "Finish":
            atk = getattr(event, "atk_type", None)
    return atk


def _stop_rates(logs: Sequence[GameLog], games: int) -> dict[str, float]:
    """Mean number of stops each side plays per game."""
    totals = {s: 0 for s in SIDES}
    for log in logs:
        for event in log.events:
            if event.TYPE == "stop":
                player = getattr(event, "player", None)
                if player in totals:
                    totals[player] += 1
    return {s: (totals[s] / games if games else 0.0) for s in SIDES}


def _cm_series(log: GameLog, turns: int) -> list[int]:
    """Crowd-meter value at the end of each turn ``1..turns`` (forward-filled).

    The log emits a ``crowd_meter`` event only when the meter changes, so each
    turn inherits the last posted value (0 until the first change)."""
    changes = [(e.t, e.value) for e in log.events if isinstance(e, CrowdMeter)]
    series: list[int] = []
    current, idx = 0, 0
    for turn in range(1, turns + 1):
        while idx < len(changes) and changes[idx][0] <= turn:
            current = changes[idx][1]
            idx += 1
        series.append(current)
    return series


def _crowd_curve(series: Sequence[list[int]]) -> list[float]:
    """Mean crowd-meter by turn index across games, averaging only games that
    reached that turn (ragged: later indices average fewer, longer games)."""
    span = max((len(s) for s in series), default=0)
    curve: list[float] = []
    for i in range(span):
        column = [s[i] for s in series if i < len(s)]
        curve.append(statistics.fmean(column))
    return curve
