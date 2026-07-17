"""Seeded RNG wrapper: roll(), shuffle(), reveal() (DESIGN.md §6).

Every non-deterministic step in the engine goes through one :class:`SeededRNG`
so a game is fully reproducible from its ``header.seed`` (DESIGN.md §8 replay).
The wrapper is snapshottable (:meth:`snapshot` / :meth:`restore`) so a
:class:`~srg_sim.state.GameState` can be serialized mid-game and resumed
bit-for-bit.

**Portable generator (the cross-engine contract).** The generator is canonical
**splitmix64**, not Python's ``random.Random``, so the exact draw stream is
reproducible in the Rust engine and across targets (native / ``wasm32``) — the
prerequisite for byte-identical logs (``docs/design/substrate-split.md`` §5). The
whole contract is small enough to restate here and must be matched bit-for-bit by
any other implementation:

* **state** — a single unsigned 64-bit word; **seed** sets it directly.
* **next()** — advance by the golden-ratio increment ``0x9E3779B97F4A7C15`` and
  apply the two splitmix64 mixing steps; return the 64-bit result.
* :meth:`roll` — ``SKILL_FACES[next() % 6]``.
* :meth:`shuffle` — downward Fisher–Yates: for ``i`` from ``n-1`` to ``1``, swap
  ``i`` with ``next() % (i + 1)``.
* :meth:`reveal` — ``items[next() % len(items)]``.
* :meth:`randint` — ``low + next() % (high - low + 1)`` (inclusive).

Modulo bias over ``2**64`` is negligible and, more importantly, *identical* in
both engines, so a fixed ``% n`` rule keeps the two streams in lockstep.
"""

from __future__ import annotations

from collections.abc import MutableSequence, Sequence
from typing import Any, TypeVar

from srg_sim.cards import Skill

T = TypeVar("T")

# The six die faces, in a fixed order so a given RNG state always maps a draw to
# the same face regardless of dict/enum iteration quirks. Part of the contract.
SKILL_FACES: tuple[Skill, ...] = tuple(Skill)

_MASK64 = (1 << 64) - 1
_GAMMA = 0x9E3779B97F4A7C15  # splitmix64 increment (golden ratio)
_MIX1 = 0xBF58476D1CE4E5B9
_MIX2 = 0x94D049BB133111EB


class SeededRNG:
    """A canonical **splitmix64** generator exposing the engine's primitives.

    Construct with an integer ``seed`` (used to initialize the 64-bit state and
    retained for the log header). Reproducibility comes from :meth:`snapshot` /
    :meth:`restore`, which capture the single-word state exactly.
    """

    def __init__(self, seed: int) -> None:
        self.seed = seed
        self._state = seed & _MASK64

    def _next(self) -> int:
        """Advance and return the next 64-bit output (canonical splitmix64)."""
        self._state = (self._state + _GAMMA) & _MASK64
        z = self._state
        z = ((z ^ (z >> 30)) * _MIX1) & _MASK64
        z = ((z ^ (z >> 27)) * _MIX2) & _MASK64
        return z ^ (z >> 31)

    def roll(self) -> Skill:
        """Return one of the six skill faces, uniformly (DESIGN.md §6)."""
        return SKILL_FACES[self._next() % 6]

    def shuffle(self, items: MutableSequence[Any]) -> None:
        """Shuffle ``items`` in place via downward Fisher–Yates (deterministic)."""
        for i in range(len(items) - 1, 0, -1):
            j = self._next() % (i + 1)
            items[i], items[j] = items[j], items[i]

    def reveal(self, items: Sequence[T]) -> T:
        """Return one element of ``items`` uniformly (random discard / reveal)."""
        if not items:
            raise ValueError("cannot reveal from an empty sequence")
        return items[self._next() % len(items)]

    def randint(self, low: int, high: int) -> int:
        """A uniform integer in ``[low, high]`` (inclusive), for misc. effects."""
        return low + self._next() % (high - low + 1)

    def snapshot(self) -> dict[str, Any]:
        """Serialize the generator state (JSON-friendly, see DESIGN.md §5)."""
        return {"seed": self.seed, "state": self._state}

    @classmethod
    def restore(cls, data: dict[str, Any]) -> SeededRNG:
        """Rebuild a generator from :meth:`snapshot` output (bit-exact resume)."""
        rng = cls(data["seed"])
        rng._state = data["state"] & _MASK64
        return rng
