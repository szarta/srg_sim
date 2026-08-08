# srg_sim — SRG Supershow Match Engine

A headless, deterministic **Rust** engine (`srg-core`) that plays two 30-card
**SRG Supershow** decks against each other, emits a fully-serialized
**replayable game log**, and serves as an analysis bench for finding strengths
and weaknesses in a matchup or a deck build.

Player skill is modeled as a **pluggable decision policy**. The record schema is
designed so that *real human matches* can be transcribed in the exact same
format and later used to fit human-like policies.

## Status

The port from the original Python reference to Rust is **complete**: the Rust
crate `srg-core` (lib `srg_core`, bin `srg`) is the sole authoritative engine.
The Python engine was a transitional parity oracle and was **retired** once Rust
reached 100% top-96 rules coverage; the language-neutral contracts it was
validated against — the JSON Schemas in [`schemas/v1/`](schemas/) and the golden
corpora — are frozen and replayed Python-free inside `cargo test`. The one
surviving Python file is `scripts/srg_ir/effects.py`, the override→IR expander.
[`DESIGN.md`](DESIGN.md) is the review gate for the Effect IR and game-log schema.

Two decks play a full legal game end-to-end under seeded RNG; the log replays
byte-for-byte; the parser compiles card `rules_text` to the Effect IR with a
coverage report. Current focus is the **whole-DB coverage grind** — top-96
competitors are 100% modeled, and the remaining long tail of card clauses is
mapped to grammar (or overrides) family by family, driving the whole-DB
`Unsupported` count down. See [`docs/development/coverage-grind.rst`](docs/development/coverage-grind.rst).

Roadmap (see [`DESIGN.md`](DESIGN.md) §10):

- **M1** ✅ — rules-correct engine + serialized log (deterministic under a seed;
  validation suite green).
- **M2** — batch analysis harness (win-rate / finish / stop stats per matchup).
- **M3** — broaden `rules_text` → Effect coverage across the whole card DB.
- **M4** — ingest real match records; fit a human-like policy.

## Usage

The `srg` CLI (`cargo run --bin srg -- <cmd>`, or the built binary) plays
matches, reports rules coverage, verifies replays, and drives interactive and
recorded play. It resolves cards against the DB snapshot (`--cards`, defaulting
to `cards.yaml`):

```bash
srg play decks/bull.yaml decks/fae.yaml --seed 7 --out game.jsonl
srg replay game.jsonl                                  # re-run from the header seed; verify byte-for-byte
srg coverage                                           # grammar / override / unsupported clause tally
srg analyze decks/bull.yaml decks/fae.yaml --games 500 # batch win-rate / finish / length summary
srg audit decks/<a>.yaml decks/<b>.yaml --games 30     # coverage gaps + crash/anomaly playtest for a new deck
srg repl  decks/<a>.yaml decks/<b>.yaml --human A      # interactive terminal match vs a local AI
srg record decks/<a>.yaml decks/<b>.yaml --out rec.json --seed 7   # portable match record (frames + replay seed)
srg validate-record rec.json --cards cards.yaml        # gate an imported / hand-authored archive
```

A decklist names a competitor, an entrance, and 30 cards (see [`decks/`](decks/)
and DESIGN.md §2). `record`/`validate-record` are the portable **match-record**
interchange format consumers store and replay (a viewer walks the same
observable-frame sequence for engine-run and real-life-transcribed matches; see
[`schemas/v1/match_record.md`](schemas/v1/match_record.md)). `cards-ir` freezes
the parsed corpus into the parser golden. See the
[analysis](docs/development/analysis.rst), [report](docs/development/reports.rst),
and [playing/review](docs/development/playing.rst) docs.

## Getting started

The engine is **Rust**. The toolchain is pinned by
[`rust-toolchain.toml`](rust-toolchain.toml) (`stable` + clippy/rustfmt); build
and test with `cargo`:

```bash
cargo build --release
cargo test
```

Development tasks wrap `cargo` behind [`invoke`](https://www.pyinvoke.org/)
(`tasks.py`), which runs through the **shared virtualenv** at `~/data/stars/venv`
— do **not** create a new one. `invoke` / `pre-commit` are the only Python
tooling; install them into that venv if it lacks them (`pip install invoke
pre-commit`). Run the tasks through it:

```bash
invoke check          # pre-commit hooks only (fmt + clippy + knots) — the fast gate
invoke test           # cargo test — the suite (separate from check)
invoke build          # cargo build (--release for optimized)
invoke bump-version   # bump the crate version in Cargo.toml (dry-run with no args)
invoke --list         # list all tasks
# Full pre-commit gate: invoke check && invoke test
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
