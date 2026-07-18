#!/usr/bin/env python3
"""Regenerate `overrides.ir.json` from the Python `overrides.yaml` (the source of truth).

The override table is hand-authored in the **Python parity oracle**'s `overrides.yaml`
(`~/data/srg_sim_python`; `$SRG_PY` to override) — the machine-read Rust form is the
pre-expanded `overrides.ir.json` (defaults filled) the parser loads strictly. This is
the M-R3 coverage-growth loop: model a card/competitor gimmick in `overrides.yaml`, run
this script, and both engines pick it up from one source — so parser parity
(`tests/parser_parity.rs`) holds structurally, no dual-authoring.

Expansion is exactly what the parser does with an override entry: `from_dict(entry)`
(fills every dataclass default) then `to_dict()`. Output is deterministic (keys and JSON
sorted) so a regenerated table diffs cleanly against the committed one.
"""

from __future__ import annotations

import json
import os
import sys
from pathlib import Path

PY_ORACLE = Path(os.environ.get("SRG_PY", Path.home() / "data" / "srg_sim_python"))
sys.path.insert(0, str(PY_ORACLE))

import yaml  # noqa: E402

from srg_sim.effects import from_dict  # noqa: E402


def expand(overrides_yaml: Path) -> dict:
    raw = yaml.safe_load(overrides_yaml.read_text()) or {}
    return {uuid: [from_dict(e).to_dict() for e in entries] for uuid, entries in raw.items()}


def main(argv: list[str]) -> int:
    out = Path(argv[0]) if argv else Path("overrides.ir.json")
    overrides_yaml = Path(os.environ.get("SRG_OVERRIDES", PY_ORACLE / "overrides.yaml"))
    table = expand(overrides_yaml)
    out.write_text(json.dumps(table, indent=2, sort_keys=True) + "\n")
    print(f"{out}: {len(table)} override entries from {overrides_yaml}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
