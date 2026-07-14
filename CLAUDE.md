# CLAUDE.md — srg_sim

Headless, deterministic **SRG Supershow** match simulator. Read this first, then
[`DESIGN.md`](DESIGN.md). Longer developer/agent docs live in [`docs/`](docs/).

## Ground rules

- **`DESIGN.md` is the review gate.** The Effect IR (§3) and game-log schema
  (§8) are the two expensive-to-change decisions — do not re-derive or quietly
  alter them; propose changes against the doc. All `srg_sim/*.py` are docstring
  stubs until the design is signed off.
- **Do not re-derive the math.** Finish/breakout (`finish.py`) and skill-stop
  logic (`stops.py`) are ported *verbatim* from the validated `fae_comp`
  modules — see the sources in `README.md`. Port with their self-checks.
- **Never silently drop a rule.** Anything the parser can't map becomes an
  explicit `Unsupported` sentinel that surfaces in the coverage report.

## Environment

- **Shared venv:** `~/data/stars/venv` — do **not** create another. `make dev`
  installs `srg_sim[dev]` into it.
- **Card data:** source of authority is the Postgres DB at
  `~/data/srg_card_search_website/backend/app` (snapshot: `backend/app/cards.yaml`).
  Not vendored here; assume every user has that repo + DB.

## Commands

```bash
make check        # ruff lint + mypy + pytest  (the CI gate)
make fmt          # auto-format + ruff --fix
make docs         # build Sphinx docs
make precommit    # run all pre-commit hooks
```

Install hooks once: `~/data/stars/venv/bin/pre-commit install`.
The **knots** hook gates code complexity — keep functions small.

## Tasks

Tracked with `todo-sqlite-cli` (`.todo-sqlite-cli` marker → `todo-sqlite-cli.db`):

```bash
todo-sqlite-cli next            # what to work on
todo-sqlite-cli start <id>      # in-progress
todo-sqlite-cli done  <id>      # complete
```

Before committing: `make check` and `make precommit` must be green.
