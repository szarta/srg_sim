"""CLI entry: ``srg-sim play|coverage|analyze|replay|review|export|report`` (DESIGN.md §9).

The user-facing entry point that ties the pipeline together:

* ``play A.yaml B.yaml`` — resolve two decklists against the card index, compile
  their rules to IR (:func:`~srg_sim.rules_parser.enrich_deck`), play one seeded
  match, print the result, and optionally write the JSONL game log. With
  ``--policy-a human`` (or ``-b``) a person plays that side from the terminal
  against the engine; the log is marked ``kind:"real"`` for later ``review``.
* ``review LOG.jsonl`` — replay a recorded match (sim or real) and reconstruct, at
  each decision, both the player's observable view and the full oracle state, for
  post-game critique (:mod:`~srg_sim.review`, DESIGN.md §7/§10 M4).
* ``coverage`` — build the index and print the rules-parser coverage report
  (grammar / override / unsupported) over the whole DB and, with ``--top96``,
  over the top-96 competitive subset (DESIGN.md §4).
* ``analyze A.yaml B.yaml --games N`` — batch N seeded games for the matchup
  (:mod:`~srg_sim.analysis`), print the aggregate :class:`MatchupReport`, and
  optionally export it as JSON (``--json``) or long-format CSV (``--csv``) for a
  downstream notebook (DESIGN.md §10 M2).
* ``replay LOG.jsonl`` — re-run a recorded *sim* log from its ``header.seed`` and
  decks/policies, then diff the produced stream against the recording to verify
  determinism (DESIGN.md §8).
* ``export LOG.jsonl…`` — flatten one or more logs' ``decision`` events to
  imitation-learning NDJSON (``{observable_state, legal, chosen, policy, point}``),
  the honest per-seat training signal ``LearnedPolicy`` consumes (DESIGN.md §10 M4).
* ``report A B`` — build a 2-competitor matchup report (turn-roll odds, CM1–5 finish
  odds with card art, skill stops, skill-requirement payoffs) as a self-contained
  Sphinx project rendered to HTML and, with ``--pdf``, a xelatex PDF.

``--cards`` overrides the card-export path (defaults to the snapshot), so every
command runs against any ``cards.yaml`` — real or a test fixture.
"""

from __future__ import annotations

import argparse
import csv
import json
from collections.abc import Callable
from pathlib import Path
from typing import Any

from srg_sim import review as rv
from srg_sim import rules_parser as rp
from srg_sim.analysis import Matchup, MatchupReport, run_batch, seed_range
from srg_sim.cards import Deck
from srg_sim.engine import Engine
from srg_sim.gamelog import GameLog, diff
from srg_sim.interactive import HumanPolicy
from srg_sim.loader import DEFAULT_CARDS_YAML, CardIndex, LoaderError, load_deck
from srg_sim.policy import (
    AggressiveBuilder,
    HeuristicPolicy,
    Newbie,
    Policy,
    RandomPolicy,
    SmartPasser,
)

_POLICY_NAMES = "random|heuristic|aggressive|smart|newbie|human"

_POLICIES: dict[str, Callable[[], Policy]] = {
    "random": RandomPolicy,
    "heuristic": HeuristicPolicy,
    "aggressive": AggressiveBuilder,
    "smart": SmartPasser,
    "newbie": Newbie,
    "human": HumanPolicy,
}

Overrides = dict[str, list[dict[str, object]]]


def _policy_factory(name: str) -> Callable[[], Policy]:
    if name not in _POLICIES:
        raise SystemExit(f"unknown policy {name!r}; choose from {sorted(_POLICIES)}")
    return _POLICIES[name]


def _make_policy(name: str) -> Policy:
    return _policy_factory(name)()


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
    human = "human" in (args.policy_a, args.policy_b)
    engine = Engine(
        deck_a,
        deck_b,
        _make_policy(args.policy_a),
        _make_policy(args.policy_b),
        seed=args.seed,
        created=args.created,
        kind="real" if human else "sim",  # a human took at least one decision (§8)
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
# analyze
# ---------------------------------------------------------------------------


def _cmd_analyze(args: argparse.Namespace) -> int:
    index = _index(args.cards)
    overrides = rp.load_overrides()
    deck_a = _load_playable(args.deck_a, index, overrides)
    deck_b = _load_playable(args.deck_b, index, overrides)
    matchup = Matchup(
        deck_a,
        deck_b,
        _policy_factory(args.policy_a),
        _policy_factory(args.policy_b),
        created=args.created,
    )
    outcomes = run_batch(
        matchup, seed_range(args.games, args.seed_start), keep_logs=True, jobs=args.jobs
    )
    report = MatchupReport.from_outcomes(outcomes)
    _print_analysis(report, deck_a, deck_b, args)
    if args.json:
        Path(args.json).write_text(json.dumps(report.to_dict(), indent=2) + "\n")
        print(f"json: {args.json}")
    if args.csv:
        _write_report_csv(args.csv, report)
        print(f"csv: {args.csv}")
    return 0


def _print_analysis(
    report: MatchupReport, deck_a: Deck, deck_b: Deck, args: argparse.Namespace
) -> None:
    header = (
        f"analyze: {deck_a.competitor.name} ({args.policy_a}) vs "
        f"{deck_b.competitor.name} ({args.policy_b}) — {report.games} games"
    )
    if report.games:
        header += f" (seeds {args.seed_start}-{args.seed_start + report.games - 1})"
    print(header)
    print(_wins_line(report))
    print(f"  reasons: {_counts(report.reasons)}")
    if report.finish_types:
        print(f"  finish types: {_counts(report.finish_types)}")
    length = report.length
    print(
        f"  length (turns): min {length['min']:.0f}  mean {length['mean']:.1f}  "
        f"median {length['median']:.0f}  max {length['max']:.0f}"
    )
    print(f"  stops/game: A {report.stops['A']:.2f}  B {report.stops['B']:.2f}")


def _wins_line(report: MatchupReport) -> str:
    parts = []
    for side in ("A", "B"):
        lo, hi = report.win_ci[side]
        parts.append(
            f"{side} {report.wins[side]} ({report.win_rate[side]:.1%}, CI {lo:.1%}-{hi:.1%})"
        )
    return f"  wins: {'  '.join(parts)}  draw {report.wins['draw']}"


def _counts(counter: dict[str, int]) -> str:
    """Render a count map as ``k n, ...`` ordered by count (desc), then name."""
    ordered = sorted(counter.items(), key=lambda kv: (-kv[1], kv[0]))
    return ", ".join(f"{name} {count}" for name, count in ordered) or "(none)"


def _write_report_csv(path: str, report: MatchupReport) -> None:
    """Long-format ``metric,value`` CSV: nested keys dot-joined, list indices too.

    Long format keeps the ragged crowd-meter curve and nested maps tidy for a
    notebook (``pd.read_csv(...).set_index("metric")``)."""
    with Path(path).open("w", newline="") as handle:
        writer = csv.writer(handle)
        writer.writerow(["metric", "value"])
        writer.writerows(_flatten(report.to_dict()))


def _flatten(data: dict[str, Any], prefix: str = "") -> list[tuple[str, Any]]:
    rows: list[tuple[str, Any]] = []
    for key, value in data.items():
        dotted = f"{prefix}{key}"
        if isinstance(value, dict):
            rows.extend(_flatten(value, f"{dotted}."))
        elif isinstance(value, list):
            rows.extend((f"{dotted}.{i}", item) for i, item in enumerate(value))
        else:
            rows.append((dotted, value))
    return rows


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
    decks = rv.rebuild_decks(header, index, overrides)
    engine = Engine(
        decks["A"],
        decks["B"],
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


# ---------------------------------------------------------------------------
# review
# ---------------------------------------------------------------------------


def _cmd_review(args: argparse.Namespace) -> int:
    log = GameLog.read(args.log)
    index = _index(args.cards)
    overrides = rp.load_overrides()
    recon = rv.reconstruct(log, index, overrides)
    records = recon.for_player(args.player) if args.player else recon.records
    result = recon.result
    print(
        f"review: {log.header.kind} log, {len(log.events)} events — "
        f"{result.winner} wins by {result.reason} in {result.turns} turns"
    )
    scope = f"player {args.player}" if args.player else "all players"
    print(f"  {len(records)} decision(s) reconstructed ({scope})")
    if args.ndjson:
        Path(args.ndjson).write_text(rv.records_to_ndjson(records))
        print(f"  ndjson: {args.ndjson}")
    return 0


# ---------------------------------------------------------------------------
# report
# ---------------------------------------------------------------------------


def _cmd_report(args: argparse.Namespace) -> int:
    from srg_sim.loader import LoaderError
    from srg_sim.report.build import build_report

    try:
        out = build_report(
            args.comp_a,
            args.comp_b,
            cards_path=args.cards,
            cms=_parse_cms(args.cm),
            mc_games=args.mc,
            seed=args.seed,
            out_root=args.out,
            html=not args.no_html,
            pdf=args.pdf,
        )
    except LoaderError as exc:
        raise SystemExit(f"could not build report: {exc}") from exc
    print(f"report: {out}")
    if not args.no_html:
        print(f"  html: {out / '_build' / 'html' / 'index.html'}")
    if args.pdf:
        print(f"  pdf:  {out / '_build' / 'latex' / 'matchup.pdf'}")
    return 0


def _parse_cms(spec: str) -> tuple[int, ...]:
    """Parse a Crowd-Meter spec: ``"1-5"`` range or ``"1,3,5"`` list."""
    if "-" in spec:
        lo, hi = (int(x) for x in spec.split("-", 1))
        return tuple(range(lo, hi + 1))
    return tuple(int(x) for x in spec.split(","))


# ---------------------------------------------------------------------------
# export
# ---------------------------------------------------------------------------


def _cmd_export(args: argparse.Namespace) -> int:
    index = _index(args.cards)
    overrides = rp.load_overrides()
    lines: list[str] = []
    for path in args.logs:
        recon = rv.reconstruct(GameLog.read(path), index, overrides)
        records = recon.for_player(args.player) if args.player else recon.records
        lines.append(rv.records_to_training_ndjson(records))
    ndjson = "".join(lines)
    count = ndjson.count("\n")
    if args.out:
        Path(args.out).write_text(ndjson)
        print(f"export: {count} decision(s) from {len(args.logs)} log(s) -> {args.out}")
    else:
        print(ndjson, end="")
    return 0


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
    play.add_argument("--policy-a", default="heuristic", help=_POLICY_NAMES)
    play.add_argument("--policy-b", default="heuristic", help=_POLICY_NAMES)
    play.add_argument("--created", default="", help="header timestamp (kept out of the engine)")
    play.add_argument("--out", help="write the JSONL game log here")
    _add_cards_arg(play)
    play.set_defaults(func=_cmd_play)

    coverage = sub.add_parser("coverage", help="rules-parser coverage report (DESIGN.md §4)")
    coverage.add_argument("--top96", action="store_true", help="also report the top-96 subset")
    _add_cards_arg(coverage)
    coverage.set_defaults(func=_cmd_coverage)

    analyze = sub.add_parser("analyze", help="batch N seeded games and report matchup metrics")
    analyze.add_argument("deck_a", help="decklist YAML for side A")
    analyze.add_argument("deck_b", help="decklist YAML for side B")
    analyze.add_argument("--games", type=int, default=100, help="number of games to run")
    analyze.add_argument(
        "--seed-start", type=int, default=0, help="first seed (games use S..S+N-1)"
    )
    analyze.add_argument(
        "--jobs",
        type=int,
        default=1,
        help="parallel worker processes (>1 fans games out; results stay seed-ordered)",
    )
    analyze.add_argument("--policy-a", default="heuristic", help=_POLICY_NAMES)
    analyze.add_argument("--policy-b", default="heuristic", help=_POLICY_NAMES)
    analyze.add_argument("--created", default="", help="header timestamp (kept out of the engine)")
    analyze.add_argument("--json", help="write the report as JSON here")
    analyze.add_argument("--csv", help="write the report as long-format CSV here")
    _add_cards_arg(analyze)
    analyze.set_defaults(func=_cmd_analyze)

    replay = sub.add_parser("replay", help="re-run a sim log and verify it reproduces")
    replay.add_argument("log", help="recorded JSONL game log")
    _add_cards_arg(replay)
    replay.set_defaults(func=_cmd_replay)

    review = sub.add_parser(
        "review", help="reconstruct each decision's player-view + oracle truth (§7)"
    )
    review.add_argument("log", help="recorded JSONL game log (sim or real)")
    review.add_argument("--player", help="restrict to one player's decisions (e.g. A)")
    review.add_argument("--ndjson", help="write the review records as NDJSON here")
    _add_cards_arg(review)
    review.set_defaults(func=_cmd_review)

    report = sub.add_parser(
        "report", help="build a 2-competitor matchup report (Sphinx HTML + xelatex PDF)"
    )
    report.add_argument("comp_a", help="first competitor (name, substring, or db_uuid)")
    report.add_argument("comp_b", help="second competitor (name, substring, or db_uuid)")
    report.add_argument("--cm", default="0-5", help="Crowd-Meter range/list for finish odds")
    report.add_argument("--mc", type=int, default=50000, help="Monte-Carlo rolls for turn odds")
    report.add_argument("--seed", type=int, default=11, help="turn-odds MC seed")
    report.add_argument("--out", default="docs/reports", help="output root dir")
    report.add_argument("--pdf", action="store_true", help="also build the xelatex PDF")
    report.add_argument("--no-html", action="store_true", help="skip the HTML build")
    _add_cards_arg(report)
    report.set_defaults(func=_cmd_report)

    export = sub.add_parser(
        "export", help="flatten one or more logs to imitation-learning NDJSON (§10 M4)"
    )
    export.add_argument("logs", nargs="+", help="recorded JSONL game log(s) to flatten")
    export.add_argument("--player", help="restrict to one player's decisions (e.g. A)")
    export.add_argument("--out", help="write decisions.ndjson here (default: stdout)")
    _add_cards_arg(export)
    export.set_defaults(func=_cmd_export)

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
