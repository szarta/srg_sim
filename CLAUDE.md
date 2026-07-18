# CLAUDE.md — srg-core

Headless, deterministic **SRG Supershow** match engine, in **Rust**. Read this
first, then [`DESIGN.md`](DESIGN.md) and
[`docs/design/substrate-split.md`](docs/design/substrate-split.md) (the Rust
migration plan). This crate (`srg-core`, lib `srg_core`, bin `srg`) is the
authoritative rules core; consumers (console, MCP, WASM/web, mobile) sit on top.

**Migration status.** `main` is being ported from the Python reference to Rust
(the M-R1 tasks; see `todo-sqlite-cli`). The **Python engine is the parity
oracle** and lives in a separate checkout of the `python` branch (cloned to
`~/data/srg_sim_python`) — not on `main`. Rust is validated against two committed,
language-neutral contracts frozen during M-R0: the JSON Schemas in
[`schemas/v1/`](schemas/) and the golden conformance corpus in
[`fixtures/conformance/`](fixtures/).

## Ground rules

- **`DESIGN.md` is the review gate.** The Effect IR (§3) and game-log schema
  (§8) are the two expensive-to-change decisions, now **cross-language contracts**
  (`schemas/v1/`). Do not re-derive or quietly alter them; a change breaks both
  engines and must be proposed against the doc **and** bump the schema version.
- **Do not re-derive the math.** Finish/breakout (`finish`) and skill-stop logic
  (`stops`) trace *verbatim* to the validated `fae_comp` modules (via the Python
  port). Carry their self-checks across; the conformance corpus guards the result.
- **Never silently drop a rule.** Anything the parser can't map becomes an
  explicit `Unsupported` IR node that surfaces in the coverage report.
- **The substrate never imports a consumer.** The `srg_core` lib must not depend
  on the `srg` bin (or future consumer crates). The crate graph enforces this.

## Environment

- **Rust toolchain** is pinned by [`rust-toolchain.toml`](rust-toolchain.toml)
  (`stable` + clippy/rustfmt). Build with `cargo`.
- **`invoke` / `pre-commit`** run through the shared venv `~/data/stars/venv` — do
  **not** create another. (`tasks.py` is a thin Python wrapper over `cargo`.)
- **Card data:** source of authority is the Postgres DB at
  `~/data/srg_card_search_website/backend/app` (snapshot: `backend/app/cards.yaml`).
  Consumed by the parser (→ `cards.ir.json`) and the Python oracle; not vendored.

## Commands

Development tasks run through `invoke` (`tasks.py`), wrapping `cargo`:

```bash
invoke check          # pre-commit (fmt + clippy + knots) + cargo test  (the CI gate)
invoke test           # cargo test
invoke build          # cargo build (--release for optimized)
invoke conformance    # cross-language harness vs the Python oracle (parser + snapshot parity)
invoke bump-version   # bump the crate version in Cargo.toml (dry-run with no args)
```

Install hooks once: `~/data/stars/venv/bin/pre-commit install`.
The **knots** hook gates code complexity — keep functions small. `cargo fmt` and
`cargo clippy -D warnings` run as pre-commit hooks (part of `invoke check`).

## Tasks

Tracked with `todo-sqlite-cli` (`.todo-sqlite-cli` marker → `todo-sqlite-cli.db`):

```bash
todo-sqlite-cli next            # what to work on
todo-sqlite-cli start <id>      # in-progress
todo-sqlite-cli done  <id>      # complete
```

Before committing: `invoke check` must be green.
