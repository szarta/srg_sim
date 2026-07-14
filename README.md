# srg_sim — SRG Supershow Match Simulator

A headless, deterministic Python simulator that plays two 30-card **SRG
Supershow** decks against each other, emits a fully-serialized **replayable
game log**, and serves as an analysis bench for finding strengths and
weaknesses in a matchup or a deck build.

Player skill is modeled as a **pluggable decision policy**. The game-log schema
is designed so that *real human matches* can be recorded in the exact same
format and later used to fit human-like policies.

## Status

✅ **M1 complete.** Two decks play a full legal game end-to-end under seeded RNG;
the JSONL log replays byte-for-byte; the rules parser compiles card text to the
Effect IR with a coverage report. [`DESIGN.md`](DESIGN.md) remains the review
gate for the Effect IR and game-log schema.

Roadmap (see [`DESIGN.md`](DESIGN.md) §10):

- **M1** ✅ — rules-correct engine + serialized log (two decks play a full legal
  game; deterministic under a seed; validation suite green).
- **M2** — batch analysis harness (win-rate / finish / stop stats per matchup).
- **M3** — broaden `rules_text` → Effect coverage; drive `Unsupported` to zero
  across the top-96 competitors.
- **M4** — ingest real match logs; fit a human-like policy.

## Usage

The `srg-sim` CLI plays matches, reports rules coverage, and verifies replays.
It resolves cards against the DB export (`--cards`, defaulting to the snapshot):

```bash
srg-sim play decks/bull.yaml decks/fae.yaml --seed 7 --out game.jsonl
srg-sim replay game.jsonl          # re-run from the header seed; verify it matches
srg-sim coverage --top96           # grammar / override / unsupported clause tally
```

`--policy-a` / `--policy-b` select `random` or `heuristic`. A decklist names a
competitor, an entrance, and 30 cards (see [`decks/`](decks/) and DESIGN.md §2).

## Getting started

This repo uses the **shared virtualenv** at `~/data/stars/venv` — do **not**
create a new one. Install the package and dev tooling into it:

```bash
~/data/stars/venv/bin/pip install -e ".[dev]"
```

Development tasks are driven by [`invoke`](https://www.pyinvoke.org/)
(`tasks.py`); run them from the venv:

```bash
invoke check          # pre-commit hooks + type check + tests — the CI gate
invoke test           # run the test suite
invoke build          # build the sdist and wheel into dist/
invoke docs           # build the Sphinx developer docs -> docs/_build/html
invoke bump-version   # bump the version across all files (dry-run with no args)
invoke --list         # list all tasks
```

Install the git hooks once per clone:

```bash
~/data/stars/venv/bin/pre-commit install
```

Developer documentation (environment, workflow, agent helpers, design notes)
lives in [`docs/`](docs/) and builds with Sphinx.

## Card data — source of authority

Card data is **not vendored** in this repo. The source of authority is the
PostgreSQL database that backs the SRG card-search website and mobile app:

- **Repo / DB:** `~/data/srg_card_search_website/backend/app`
  (`postgresql://…@localhost/srg_cards`, see `backend/app/database.py`). It is
  updated often as cards are added and corrected.
- **Snapshot:** `backend/app/cards.yaml` is a read-only YAML export regenerated
  from that database — the convenient form the loader consumes.

> **Assumption:** anyone using `srg_sim` also has a checkout of the
> `srg_card_search_website` repo and access to that database.

## Authoritative sources (do not re-derive the math)

- Canonical ruleset: `/home/brandon/fae_comp/SUPERSHOW_MECHANICS.md`
- Validated finish/breakout math: `/home/brandon/fae_comp/supershow.py` (mirror
  of the frontend `FinishCalculator.jsx`)
- Validated skill-stop logic: `/home/brandon/fae_comp/skill_stops.py`
- Turn-roll model + self-check numbers: `/home/brandon/fae_comp/tournament_turnsim.py`

## Task tracking

Tasks live in a `todo-sqlite-cli` database (`todo-sqlite-cli.db`, resolved via
the `.todo-sqlite-cli` marker):

```bash
todo-sqlite-cli list    # active work
todo-sqlite-cli next    # the single next task
```
