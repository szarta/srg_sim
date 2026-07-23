# CLAUDE.md — srg-core

Headless, deterministic **SRG Supershow** match engine, in **Rust**. Read this
first, then [`DESIGN.md`](DESIGN.md) and
[`docs/design/substrate-split.rst`](docs/design/substrate-split.rst) (the Rust
migration plan). This crate (`srg-core`, lib `srg_core`, bin `srg`) is the
authoritative rules core; consumers (console, MCP, WASM/web, mobile) sit on top.

**Migration status.** The port from the Python reference to Rust is **complete**:
Rust (`srg-core`) is the sole authoritative engine. The Python engine — a
**transitional parity oracle** during the migration — was **retired at Phase 2**
(task #79) once Rust reached 100% top-96 rules coverage. Its old checkout
(`~/data/srg_sim_python`, the `python` branch) is archival only; nothing in the
build, CI, or authoring loop consults it. Rust is validated against two committed,
language-neutral contracts frozen during M-R0 and cross-validated against the oracle
before its retirement: the JSON Schemas in [`schemas/v1/`](schemas/) and the golden
corpora — whole-engine logs in [`fixtures/conformance/`](fixtures/) (replayed byte-for-byte
by `tests/engine_conformance.rs`) and the whole-DB parser golden
[`fixtures/parser/cards.ir.json`](fixtures/) (`tests/parser_parity.rs`). Both run
Python-free inside `cargo test`.

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
  `~/data/srg_card_search_website/backend/app` (snapshot: `backend/app/cards.yaml`),
  built on get-diced.com and synced to consumers. The Rust parser consumes it directly
  (`CardIndex::from_yaml`); `srg cards-ir` freezes the parsed corpus into the parser
  golden. Not vendored.

## Commands

Development tasks run through `invoke` (`tasks.py`), wrapping `cargo`:

```bash
invoke check          # pre-commit (fmt + clippy + knots) + cargo test  (the CI gate)
invoke test           # cargo test
invoke build          # cargo build (--release for optimized)
invoke overrides      # regen overrides.ir.json from ./overrides.yaml (single source; self-contained)
invoke cards-ir       # regen the parser golden fixtures/parser/cards.ir.json (Rust parser)
invoke bump-version   # bump the crate version in Cargo.toml (dry-run with no args)
```

Install hooks once: `~/data/stars/venv/bin/pre-commit install`.
The **knots** hook gates code complexity — keep functions small. `cargo fmt` and
`cargo clippy -D warnings` run as pre-commit hooks (part of `invoke check`).

## Decks: authoring, testing, playing

A deck is `decks/<name>.yaml` (competitor + entrance + 30 numbered cards). The
loop when adding or modeling one (full guide: [`decks/README.md`](decks/README.md)):

```bash
srg audit decks/<a>.yaml decks/<b>.yaml --games 30   # coverage gaps + crash/anomaly playtest
srg repl  decks/<a>.yaml decks/<b>.yaml --human A     # interactive play (frontend's decision protocol);
                                                      #   --transcript FILE for a Claude-observable feed
srg audit ... --capture fixtures/conformance/NN_x.json  # bank a byte-for-byte regression golden
```

**Model an unmodeled clause** two ways (a card modeled either way is covered in
*every* deck that uses it, keyed by `db_uuid`): add **grammar** in `src/parser.rs`
for a recurring shape (DB-wide), or a bespoke **override** in `overrides.yaml`
(then `invoke overrides`). `bull.yaml` / `warehouse.yaml` are fully modeled
references. See [`docs/development/coverage-grind.rst`](docs/development/coverage-grind.rst)
for the modeling procedure and traps.

## Tasks

Tracked with `todo-sqlite-cli` (`.todo-sqlite-cli` marker → `todo-sqlite-cli.db`):

```bash
todo-sqlite-cli next            # what to work on
todo-sqlite-cli start <id>      # in-progress
todo-sqlite-cli done  <id>      # complete
```

Before committing: `invoke check` must be green.
