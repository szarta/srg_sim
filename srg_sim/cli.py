"""CLI entry: ``srg-sim play | coverage | replay`` (DESIGN.md §9).

The user-facing entry point that ties the pipeline together:

* ``play A.yaml B.yaml`` — resolve two decklists against the card index, compile
  their rules to IR (:func:`~srg_sim.rules_parser.enrich_deck`), play one seeded
  match, print the result, and optionally write the JSONL game log.
* ``coverage`` — build the index and print the rules-parser coverage report
  (grammar / override / unsupported) over the whole DB and, with ``--top96``,
  over the top-96 competitive subset (DESIGN.md §4).
* ``replay LOG.jsonl`` — re-run a recorded *sim* log from its ``header.seed`` and
  decks/policies, then diff the produced stream against the recording to verify
  determinism (DESIGN.md §8).

``--cards`` overrides the card-export path (defaults to the snapshot), so every
command runs against any ``cards.yaml`` — real or a test fixture.
"""

from __future__ import annotations

import argparse
from collections.abc import Callable
from pathlib import Path

from srg_sim import rules_parser as rp
from srg_sim.cards import Deck
from srg_sim.engine import Engine
from srg_sim.gamelog import GameLog, PlayerInfo, diff
from srg_sim.loader import DEFAULT_CARDS_YAML, CardIndex, LoaderError, load_deck
from srg_sim.policy import (
    AggressiveBuilder,
    HeuristicPolicy,
    Newbie,
    Policy,
    RandomPolicy,
    SmartPasser,
)

_POLICIES: dict[str, Callable[[], Policy]] = {
    "random": RandomPolicy,
    "heuristic": HeuristicPolicy,
    "aggressive": AggressiveBuilder,
    "smart": SmartPasser,
    "newbie": Newbie,
}

Overrides = dict[str, list[dict[str, object]]]


def _make_policy(name: str) -> Policy:
    if name not in _POLICIES:
        raise SystemExit(f"unknown policy {name!r}; choose from {sorted(_POLICIES)}")
    return _POLICIES[name]()


def _index(path: str) -> CardIndex:
    if not Path(path).exists():
        raise SystemExit(f"card export not found: {path}")
    return CardIndex.from_yaml(path)


def _load_playable(ref: str, index: CardIndex, overrides: Overrides) -> Deck:
    try:
        return rp.enrich_deck(load_deck(ref, index).deck, overrides)
    except LoaderError as exc:
        raise SystemExit(f"could not load deck {ref}: {exc}") from exc


# ---------------------------------------------------------------------------
# play
# ---------------------------------------------------------------------------


def _cmd_play(args: argparse.Namespace) -> int:
    index = _index(args.cards)
    overrides = rp.load_overrides()
    deck_a = _load_playable(args.deck_a, index, overrides)
    deck_b = _load_playable(args.deck_b, index, overrides)
    engine = Engine(
        deck_a,
        deck_b,
        _make_policy(args.policy_a),
        _make_policy(args.policy_b),
        seed=args.seed,
        created=args.created,
    )
    result = engine.play()
    print(
        f"seed {args.seed}: {deck_a.competitor.name} ({args.policy_a}) "
        f"vs {deck_b.competitor.name} ({args.policy_b})"
    )
    print(f"result: {result.winner} wins by {result.reason} in {result.turns} turns")
    log = engine.state.log
    assert log is not None
    if args.out:
        log.write(args.out)
        print(f"log: {args.out} ({len(log.events)} events)")
    return 0


# ---------------------------------------------------------------------------
# coverage
# ---------------------------------------------------------------------------


def _cmd_coverage(args: argparse.Namespace) -> int:
    index = _index(args.cards)
    overrides = rp.load_overrides()
    main = [r for r in index.records if r.get("card_type") == "MainDeckCard"]
    _print_coverage("main deck", rp.coverage(main, overrides))
    if args.top96:
        top = [r for r in index.records if rp.is_top96(r)]
        _print_coverage("top-96 competitors", rp.coverage(top, overrides))
    return 0


def _print_coverage(label: str, report: rp.CoverageReport) -> None:
    print(f"\n{label}: {report.total} clauses ({report.rate:.1%} parsed)")
    print(f"  grammar      {report.grammar:6}")
    print(f"  override     {report.override:6}")
    print(f"  unsupported  {report.unsupported:6}")
    if report.top_unparsed:
        print("  top unparsed shapes:")
        for shape, count in report.top_unparsed[:15]:
            print(f"    {count:5}  {shape}")


# ---------------------------------------------------------------------------
# replay
# ---------------------------------------------------------------------------


def _cmd_replay(args: argparse.Namespace) -> int:
    recorded = GameLog.read(args.log)
    header = recorded.header
    if header.kind != "sim":
        raise SystemExit(f"replay supports only sim logs, got kind={header.kind!r}")
    index = _index(args.cards)
    overrides = rp.load_overrides()
    engine = Engine(
        _deck_from_header(header.players["A"], index, overrides),
        _deck_from_header(header.players["B"], index, overrides),
        _make_policy(header.players["A"].policy),
        _make_policy(header.players["B"].policy),
        seed=header.seed,
        created=header.created,
    )
    engine.play()
    produced = engine.state.log
    assert produced is not None
    problems = diff(recorded, produced)
    if not problems:
        print(f"replay OK: {len(recorded.events)} events reproduced byte-identically")
        return 0
    print(f"replay MISMATCH: {len(problems)} problem(s)")
    for problem in problems[:5]:
        print(f"  {problem}")
    return 1


def _deck_from_header(info: PlayerInfo, index: CardIndex, overrides: Overrides) -> Deck:
    """Rebuild a player's deck from the log header (competitor/entrance by name,
    cards by db_uuid in recorded order), then compile its rules to IR."""
    deck = Deck(
        competitor=index.competitor(info.competitor),
        entrance=index.entrance(info.entrance),
        cards=tuple(index.main_card({"db_uuid": uuid}) for uuid in info.deck),
    )
    return rp.enrich_deck(deck, overrides)


# ---------------------------------------------------------------------------
# argument parsing
# ---------------------------------------------------------------------------


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="srg-sim", description="SRG Supershow match simulator")
    sub = parser.add_subparsers(dest="command", required=True)

    play = sub.add_parser("play", help="play a seeded match between two decklists")
    play.add_argument("deck_a", help="decklist YAML for side A")
    play.add_argument("deck_b", help="decklist YAML for side B")
    play.add_argument("--seed", type=int, default=0)
    play.add_argument(
        "--policy-a", default="heuristic", help="random|heuristic|aggressive|smart|newbie"
    )
    play.add_argument(
        "--policy-b", default="heuristic", help="random|heuristic|aggressive|smart|newbie"
    )
    play.add_argument("--created", default="", help="header timestamp (kept out of the engine)")
    play.add_argument("--out", help="write the JSONL game log here")
    _add_cards_arg(play)
    play.set_defaults(func=_cmd_play)

    coverage = sub.add_parser("coverage", help="rules-parser coverage report (DESIGN.md §4)")
    coverage.add_argument("--top96", action="store_true", help="also report the top-96 subset")
    _add_cards_arg(coverage)
    coverage.set_defaults(func=_cmd_coverage)

    replay = sub.add_parser("replay", help="re-run a sim log and verify it reproduces")
    replay.add_argument("log", help="recorded JSONL game log")
    _add_cards_arg(replay)
    replay.set_defaults(func=_cmd_replay)

    return parser


def _add_cards_arg(parser: argparse.ArgumentParser) -> None:
    parser.add_argument(
        "--cards", default=str(DEFAULT_CARDS_YAML), help="path to the cards.yaml export"
    )


def main(argv: list[str] | None = None) -> int:
    args = _build_parser().parse_args(argv)
    func = args.func
    result: int = func(args)
    return result


if __name__ == "__main__":
    raise SystemExit(main())
