"""Conformance-corpus tests (DESIGN.md §6 harness; ``srg_sim.conformance``).

The parametrized replay test is the exact assertion the Rust engine (task 75) must
also pass: given a fixture's ``(seed, decks, decisions[])``, reproduce its stored
canonical log byte-for-byte. The drift guard keeps the committed corpus in sync
with the generating specs + current engine — a behaviour change fails it until the
corpus is regenerated (``python -m tests.conformance_corpus``).
"""

from __future__ import annotations

import json
from typing import Any

import pytest
from srg_sim import conformance as C

from tests import conformance_corpus as corpus

_PATHS = C.corpus_paths()


def _norm(obj: Any) -> Any:
    """JSON round-trip so a freshly generated fixture compares equal to a committed
    one (stable key order, JSON scalar types)."""
    return json.loads(json.dumps(obj, sort_keys=True))


def test_corpus_is_nonempty() -> None:
    assert _PATHS, "no committed fixtures under fixtures/conformance/"


@pytest.mark.parametrize("path", _PATHS, ids=lambda p: p.stem)
def test_fixture_replays_byte_identically(path: Any) -> None:
    """Replay the fixture's inputs and assert the canonical log matches exactly."""
    C.verify_fixture(C.load_fixture(path))


def test_committed_corpus_matches_specs() -> None:
    """Drift guard: the committed files equal a fresh generation. Fails on any
    engine-behaviour change or spec edit until the corpus is regenerated."""
    fresh = corpus.fixtures()
    committed = [C.load_fixture(p) for p in _PATHS]
    assert len(fresh) == len(committed), (
        "fixture count differs from specs — run `python -m tests.conformance_corpus`"
    )
    for generated, on_disk in zip(fresh, committed, strict=True):
        assert _norm(generated) == on_disk, (
            f"fixture {generated['label']!r} drifted from the committed corpus — "
            "run `python -m tests.conformance_corpus`"
        )
