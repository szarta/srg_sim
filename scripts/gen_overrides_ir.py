#!/usr/bin/env python3
"""Regenerate `overrides.ir.json` from `overrides.yaml` (the source of truth, on main).

The override table is hand-authored in **this repo's** `overrides.yaml` (repo root;
`$SRG_OVERRIDES` to point elsewhere) — the machine-read Rust form is the pre-expanded
`overrides.ir.json` (defaults filled) the parser loads strictly. This is the coverage-
growth loop: model a card/competitor gimmick in `overrides.yaml`, run this script,
rebuild. The Rust engine embeds the result via `include_str!`.

Expansion uses the relocated in-repo IR tooling (`scripts/srg_ir/effects.py`'s
`from_dict` → `to_dict`) to validate and canonicalize each entry. That tooling is a
self-contained copy of the frozen Effect-IR dataclasses, lifted out of the retired
`srg_sim_python` oracle (Phase 2 / task #79) so this script no longer reaches into that
archived checkout. Output is deterministic (keys and JSON sorted) so a regenerated table
diffs cleanly against the committed one.
"""

from __future__ import annotations

import json
import os
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
# The IR tooling lives beside this script (scripts/srg_ir/); put scripts/ on the path so
# `import srg_ir` resolves it. The override *data* is read from this repo's overrides.yaml.
sys.path.insert(0, str(Path(__file__).resolve().parent))

import yaml  # noqa: E402

from srg_ir.effects import from_dict  # noqa: E402


def expand(overrides_yaml: Path) -> dict:
    raw = yaml.safe_load(overrides_yaml.read_text()) or {}
    return {uuid: [from_dict(e).to_dict() for e in entries] for uuid, entries in raw.items()}


def main(argv: list[str]) -> int:
    out = Path(argv[0]) if argv else Path("overrides.ir.json")
    overrides_yaml = Path(os.environ.get("SRG_OVERRIDES", REPO_ROOT / "overrides.yaml"))
    table = expand(overrides_yaml)
    out.write_text(json.dumps(table, indent=2, sort_keys=True) + "\n")
    print(f"{out}: {len(table)} override entries from {overrides_yaml}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
