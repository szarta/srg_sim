"""The conformance corpus *specs* — which decks, policies, and seeds make up the
committed fixtures under ``fixtures/conformance/`` (DESIGN.md §6 harness).

This lives test-side because it builds the synthetic demo decks; the package's
:mod:`srg_sim.conformance` holds only the deck-agnostic format machinery. Run

    python -m tests.conformance_corpus

to (re)generate the corpus after a deliberate engine-behaviour change — the same
regeneration :mod:`tests.test_conformance` guards. The seeds/policies are fixed so
generation is fully deterministic. This seed corpus (gimmick turn-roll comebacks,
lowest-wins, the stop/finish board war, the vanilla ≈50/50 baseline) grows toward
the top-96 by M-R3.
"""

from __future__ import annotations

from typing import Any

from srg_sim import conformance
from srg_sim.policy import AggressiveBuilder, HeuristicPolicy, Newbie, Policy, SmartPasser

from tests import demo_decks as dd

# (label, competitor A, competitor B, policy A, policy B, seed). Heuristic-family
# policies only (RNG-free decisions -> byte-exact replay; see srg_sim.conformance).
_SPECS: tuple[tuple[str, Any, Any, type[Policy], type[Policy], int], ...] = (
    ("heuristic_bull_fae_s7", dd.bull_gimmick, dd.fae_gimmick, HeuristicPolicy, HeuristicPolicy, 7),
    (
        "heuristic_bull_fae_s42",
        dd.bull_gimmick,
        dd.fae_gimmick,
        HeuristicPolicy,
        HeuristicPolicy,
        42,
    ),
    ("aggressive_vs_smart_s1", dd.bull_gimmick, dd.fae_gimmick, AggressiveBuilder, SmartPasser, 1),
    ("newbie_vs_smart_s13", dd.bull_gimmick, dd.fae_gimmick, Newbie, SmartPasser, 13),
    ("smart_mirror_bull_fae_s99", dd.bull_gimmick, dd.fae_gimmick, SmartPasser, SmartPasser, 99),
    ("heuristic_vanilla_s7", dd.vanilla, dd.vanilla, HeuristicPolicy, HeuristicPolicy, 7),
)


def fixtures() -> list[dict[str, Any]]:
    """Generate every corpus fixture, deterministically, in declared order."""
    out = []
    for label, make_a, make_b, pol_a, pol_b, seed in _SPECS:
        deck_a = dd.make_deck("A", make_a())
        deck_b = dd.make_deck("B", make_b())
        out.append(conformance.generate_fixture(label, deck_a, deck_b, pol_a(), pol_b(), seed))
    return out


def main() -> None:
    for path in conformance.write_corpus(fixtures()):
        print(f"wrote {path}")


if __name__ == "__main__":
    main()
