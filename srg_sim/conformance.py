"""Conformance corpus: ``(seed, decks, decisions[])`` fixtures + their canonical
game log — the cross-engine parity harness (``docs/design/substrate-split.md`` §6).

Each fixture is a **self-contained** conformance case: the seed, both fully
serialized decks, the ordered ``decision`` stream each player made, and the
**canonical game log** those inputs must produce. The Python engine generates the
corpus now; the Rust engine (task 75) must, given the same
``(seed, decks, decisions[])``, emit a byte-identical canonical log. Because the
fixtures embed the decks, the harness needs no card DB and no knowledge of
``tests/demo_decks`` — it is language-neutral data.

**Why replay works.** A fixture is verified by feeding each side a
:class:`~srg_sim.policy.ReplayPolicy` over its recorded decisions and re-running
from the seed. Replay is byte-exact only if the *generating* policy never draws
from the shared RNG at a decision point (or the RNG streams would diverge) — so
the corpus is generated with the deterministic, RNG-free
:class:`~srg_sim.policy.HeuristicPolicy` family, never ``RandomPolicy``. The
``ReplayPolicy`` is named with the generating policy's name so the log's header
and ``decision`` events match byte-for-byte.

The stored log is frozen in the file, so :func:`verify_fixture` (stored vs. a
fresh replay) doubles as a **golden-log drift guard**: any engine behaviour change
diverges from the committed log and fails :mod:`tests.test_conformance` until the
corpus is regenerated (``python -m srg_sim.conformance``) — the deliberate gate on
the reference implementation.
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

from srg_sim.cards import Deck
from srg_sim.engine import Engine
from srg_sim.gamelog import Decision, GameLog
from srg_sim.policy import Option, Policy, ReplayPolicy

# Bump when the fixture envelope (not the §8 log inside it) changes shape.
FIXTURE_SCHEMA = 1

_CORPUS_DIR = Path(__file__).resolve().parent.parent / "fixtures" / "conformance"
_KEYS = ("A", "B")


# ---------------------------------------------------------------------------
# Canonical log + decision extraction
# ---------------------------------------------------------------------------


def canonical_log(log: GameLog) -> list[dict[str, Any]]:
    """The header followed by every event, each as its ``to_dict`` — the exact
    byte-comparable form two engines must agree on. ``created`` is generated as
    ``""`` (kept out of the engine for determinism), so nothing here is wall-clock
    dependent."""
    return [log.header.to_dict(), *(event.to_dict() for event in log.events)]


def decisions_by_player(log: GameLog) -> dict[str, list[Option]]:
    """Each player's recorded ``chosen`` options, in log order (mirrors
    :func:`srg_sim.review._decisions_by_player`)."""
    out: dict[str, list[Option]] = {key: [] for key in log.header.players}
    for event in log.events:
        if isinstance(event, Decision):
            out.setdefault(event.player, []).append(event.chosen)
    return out


# ---------------------------------------------------------------------------
# Generate / replay / verify
# ---------------------------------------------------------------------------


def generate_fixture(
    label: str, deck_a: Deck, deck_b: Deck, policy_a: Policy, policy_b: Policy, seed: int
) -> dict[str, Any]:
    """Play one deterministic match and capture it as a fixture dict."""
    engine = _play(deck_a, deck_b, policy_a, policy_b, seed)
    log = engine.state.log
    assert log is not None
    return {
        "fixture_schema": FIXTURE_SCHEMA,
        "label": label,
        "seed": seed,
        "kind": "sim",
        "policies": {"A": policy_a.name, "B": policy_b.name},
        "decks": {"A": deck_a.to_dict(), "B": deck_b.to_dict()},
        "decisions": decisions_by_player(log),
        "log": canonical_log(log),
    }


def replay_log(fixture: dict[str, Any]) -> list[dict[str, Any]]:
    """Re-run a fixture's inputs (seed, decks, decisions) and return the canonical
    log the current engine produces — what :func:`verify_fixture` compares."""
    decks = {key: Deck.from_dict(fixture["decks"][key]) for key in _KEYS}
    policies = {
        key: ReplayPolicy(list(fixture["decisions"][key]), name=fixture["policies"][key])
        for key in _KEYS
    }
    engine = _play(
        decks["A"], decks["B"], policies["A"], policies["B"], fixture["seed"], kind=fixture["kind"]
    )
    log = engine.state.log
    assert log is not None
    return canonical_log(log)


def verify_fixture(fixture: dict[str, Any]) -> None:
    """Replay a fixture and assert it reproduces its stored log; raise with the
    first divergence on mismatch."""
    got = replay_log(fixture)
    expected = fixture["log"]
    if got != expected:
        raise ConformanceMismatch(_first_diff(fixture.get("label", "?"), expected, got))


def _play(
    deck_a: Deck, deck_b: Deck, policy_a: Policy, policy_b: Policy, seed: int, kind: str = "sim"
) -> Engine:
    engine = Engine(deck_a, deck_b, policy_a, policy_b, seed=seed, created="", kind=kind)
    engine.play()
    return engine


def _first_diff(label: str, expected: list[Any], got: list[Any]) -> str:
    for i, (e, g) in enumerate(zip(expected, got, strict=False)):
        if e != g:
            return f"fixture {label!r}: log line {i} differs\n  expected: {e}\n  got:      {g}"
    if len(expected) != len(got):
        return f"fixture {label!r}: log length {len(got)} != expected {len(expected)}"
    return f"fixture {label!r}: logs differ"


class ConformanceMismatch(AssertionError):
    """A fixture's replay diverged from its stored canonical log."""


# ---------------------------------------------------------------------------
# On-disk corpus
# ---------------------------------------------------------------------------


def corpus_paths() -> list[Path]:
    """Every committed fixture file, in sorted (stable) order."""
    return sorted(_CORPUS_DIR.glob("*.json"))


def load_fixture(path: Path) -> dict[str, Any]:
    return json.loads(path.read_text())


def write_fixture(fixture: dict[str, Any], index: int) -> Path:
    _CORPUS_DIR.mkdir(parents=True, exist_ok=True)
    path = _CORPUS_DIR / f"{index:03d}_{fixture['label']}.json"
    path.write_text(json.dumps(fixture, indent=2, sort_keys=True) + "\n")
    return path


def write_corpus(fixtures: list[dict[str, Any]], *, clean: bool = True) -> list[Path]:
    """(Re)write a whole corpus (1-indexed, sorted by generation order). With
    ``clean`` (default) any stale fixture files are removed first, so the on-disk
    corpus exactly matches ``fixtures``. The generating *specs* live test-side
    (they use the synthetic demo decks); the package stays consumer-agnostic."""
    if clean:
        for path in corpus_paths():
            path.unlink()
    return [write_fixture(fixture, i) for i, fixture in enumerate(fixtures, start=1)]
