"""Seeded RNG wrapper: roll(), shuffle(), reveal() (DESIGN.md §6).

Every non-deterministic step in the engine goes through one :class:`SeededRNG`
so a game is fully reproducible from its ``header.seed`` (DESIGN.md §8 replay).
The wrapper is snapshottable (:meth:`snapshot` / :meth:`restore`) so a
:class:`~srg_sim.state.GameState` can be serialized mid-game and resumed
bit-for-bit.

Three primitives cover the engine's needs (DESIGN.md §6):

* :meth:`roll` — pick one of the six skill faces uniformly; the roll's *value*
  is that skill's derived stat (computed by the caller, not here).
* :meth:`shuffle` — deterministic in-place shuffle (deck shuffles, searches).
* :meth:`reveal` — pick one element of a zone uniformly (random discard/reveal).
"""

from __future__ import annotations

import random
from collections.abc import MutableSequence, Sequence
from typing import Any, TypeVar

from srg_sim.cards import Skill

T = TypeVar("T")

# The six die faces, in a fixed order so a given RNG state always maps a draw to
# the same face regardless of dict/enum iteration quirks.
SKILL_FACES: tuple[Skill, ...] = tuple(Skill)


class SeededRNG:
    """A ``random.Random`` wrapper exposing only the engine's three primitives.

    Construct with an integer ``seed``; the seed is retained purely for the log
    header. Reproducibility comes from :meth:`snapshot` / :meth:`restore`, which
    capture the generator's full internal state, not just the seed.
    """

    def __init__(self, seed: int) -> None:
        self.seed = seed
        self._rng = random.Random(seed)

    def roll(self) -> Skill:
        """Return one of the six skill faces, uniformly (DESIGN.md §6)."""
        return self._rng.choice(SKILL_FACES)

    def shuffle(self, items: MutableSequence[Any]) -> None:
        """Shuffle ``items`` in place, deterministically for the current state."""
        self._rng.shuffle(items)

    def reveal(self, items: Sequence[T]) -> T:
        """Return one element of ``items`` uniformly (random discard / reveal)."""
        if not items:
            raise ValueError("cannot reveal from an empty sequence")
        return self._rng.choice(list(items))

    def randint(self, low: int, high: int) -> int:
        """A uniform integer in ``[low, high]`` (inclusive), for misc. effects."""
        return self._rng.randint(low, high)

    def snapshot(self) -> dict[str, Any]:
        """Serialize the full generator state (JSON-friendly, see DESIGN.md §5)."""
        version, internal, gauss = self._rng.getstate()
        return {"seed": self.seed, "version": version, "state": list(internal), "gauss": gauss}

    @classmethod
    def restore(cls, data: dict[str, Any]) -> SeededRNG:
        """Rebuild a generator from :meth:`snapshot` output (bit-exact resume)."""
        rng = cls(data["seed"])
        state = (data["version"], tuple(data["state"]), data["gauss"])
        rng._rng.setstate(state)
        return rng
