#!/usr/bin/env python3
"""Emit ``cards.ir.json`` — the parser-parity corpus (task 75, substrate-split.md §6).

The **Python engine is the parity oracle** (see CLAUDE.md). This script drives the
oracle's rules parser over the whole card export and writes, per parseable record, the
input rules text alongside the Effect IR the oracle compiles it to. The Rust port then
re-parses each record's text and asserts value-identical IR (``tests/parser_parity.rs``),
so any grammar divergence between the two ports surfaces as a failing conformance run.

The oracle is a separate checkout of the ``python`` branch (``~/data/srg_sim_python``,
overridable via ``$SRG_PY``); its ``cards.yaml`` snapshot and ``overrides.yaml`` are the
same sources the oracle itself consumes. Records with empty rules text, and the two
out-of-scope types (Spectacle, CrowdMeter), are skipped — mirroring what the engine
actually parses. Output is deterministic (records in db_uuid order, sorted JSON keys),
so a regenerated corpus diffs cleanly against the committed one.
"""

from __future__ import annotations

import json
import os
import sys
from pathlib import Path

PY_ORACLE = Path(os.environ.get("SRG_PY", Path.home() / "data" / "srg_sim_python"))
sys.path.insert(0, str(PY_ORACLE))

from srg_sim.effects import EffectSource  # noqa: E402
from srg_sim.loader import DEFAULT_CARDS_YAML, CardIndex, _rules_text  # noqa: E402
from srg_sim.rules_parser import load_overrides, parse_text  # noqa: E402

# card_type -> the EffectSource the engine parses that record's rules text as.
# Spectacle / CrowdMeter are out of scope and carry no parsed match effects.
_SOURCE = {
    "MainDeckCard": EffectSource.CARD,
    "SingleCompetitorCard": EffectSource.GIMMICK,
    "TornadoCompetitorCard": EffectSource.GIMMICK,
    "TrioCompetitorCard": EffectSource.GIMMICK,
    "EntranceCard": EffectSource.ENTRANCE,
}


def build_corpus(cards_yaml: Path, overrides_yaml: Path) -> list[dict]:
    index = CardIndex.from_yaml(cards_yaml)
    overrides = load_overrides(overrides_yaml)
    rows: list[dict] = []
    for rec in index.records:
        source = _SOURCE.get(rec.get("card_type"))
        if source is None:
            continue
        text = _rules_text(rec)
        if not text.strip():
            continue
        uuid = rec["db_uuid"]
        effects = parse_text(text, source, uuid, overrides)
        rows.append(
            {
                "db_uuid": uuid,
                "card_type": rec["card_type"],
                "source": source.value,
                "rules_text": text,
                "effects": [e.to_dict() for e in effects],
            }
        )
    rows.sort(key=lambda r: r["db_uuid"])
    return rows


def main(argv: list[str]) -> int:
    out = Path(argv[0]) if argv else Path("fixtures/cards.ir.json")
    cards_yaml = Path(os.environ.get("SRG_CARDS", DEFAULT_CARDS_YAML))
    overrides_yaml = PY_ORACLE / "overrides.yaml"
    rows = build_corpus(cards_yaml, overrides_yaml)
    out.parent.mkdir(parents=True, exist_ok=True)
    # A JSON array with one compact record per line: small on disk, yet every
    # record is its own git-diffable line so a parser change reads as a clean diff.
    lines = [json.dumps(r, sort_keys=True, separators=(",", ":")) for r in rows]
    out.write_text("[\n" + ",\n".join(lines) + "\n]\n")
    parsed = sum(1 for r in rows if r["effects"])
    print(f"{out}: {len(rows)} records ({parsed} with effects) from {cards_yaml}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
