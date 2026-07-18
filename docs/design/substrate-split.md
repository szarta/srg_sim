# Substrate Split & Rust Migration — design (review artifact)

**Status:** proposal for review. This is a companion to [`DESIGN.md`](../../DESIGN.md)
and is governed by the same gate: the **Effect IR (§3)** and the **game-log schema
(§8)** are expensive-to-change contracts. This document does **not** alter their
semantics — it re-homes them as language-neutral JSON contracts that two engines
round-trip. Any change to §3/§8 is still proposed against `DESIGN.md`, not here.

This doc transitions `srg_sim` from a single Python program into **one authoritative
rules engine** with **many consumers on top**, and records the decision to make that
engine **Rust**.

---

## 0. Decisions locked (this session)

| # | Decision | Choice |
|---|----------|--------|
| Pausability | how the engine suspends at a decision point | **Continuation engine** — a resumable state machine, `resume(response) -> Step` |
| Language | where the authoritative engine lives | **Rust**, compiled to every target (native + WASM) |
| Scope | how much becomes Rust | **Everything below the line + the parser** — full single-language stack |
| Python's fate | after Rust is live | **Transitional oracle**, then **deprecated**; Rust regresses against a frozen golden-log corpus |
| RNG | cross-engine determinism | **Portable PRNG** (splitmix64/PCG) in both languages → byte-identical logs |

**Consumer target order:** (1) **console + MCP**, (2) **web presentation layer**
(WASM), (3) **mobile offline play** (later).

> **Scope clarification.** "Everything in Rust" = the rules engine, the rules
> parser/grammar, and the state/session runtime. The matchup **`report/` PDF/HTML
> generator is a *consumer*** (presentation, not rules), so it is out of the engine
> crate. **There are no consumers of either app yet**, so nothing is owed backward
> compatibility: `report/` (and the rest of the Python CLI surface) is free to be
> rebuilt in the web presentation layer or dropped — it is not preserved for compat,
> only used opportunistically while it still helps.

---

## 1. Why split, and why Rust

The sim is already shaped like a server-authoritative engine: a pure
`Engine(decks, policies, seed).play()` produces a deterministic `GameLog`; the
`Policy` ABC is the "who decides" seam; `GameState.observable(viewer)` is the
info/anti-cheat boundary; the Effect IR is the single source of rules truth. The
split just **draws a hard line** between that core and everything that consumes it,
and gives the core a wire-facing session API.

The original plan (see `substrate-split-design` memory) was *Python authoritative on
the server + a risky hand-written client port for offline vs-AI*. **Rust inverts
that risk.** Instead of one source-of-truth language plus a second, drift-prone port,
there is **one implementation compiled to every target**:

- **console + MCP** → native Rust binary / Rust core with a thin MCP shell.
- **web** → the same crate → **WASM**; TS/JS is purely presentational.
- **mobile offline** → the same crate → native lib via **uniffi** (or WASM).

The "language split" delta (below, §8.4) stops being *"how do we trust a second
engine"* and becomes *"compile the one engine N ways."* Rust also fits the
**continuation-engine** choice better than Python: the idiomatic form is a
**resumable state machine** — no async runtime, deterministic, snapshot-friendly,
and a one-to-one map onto the decision protocol (§4).

---

## 2. The substrate boundary

**Below the line — `srg-core` (Rust crate): the authoritative rules engine, zero
knowledge of any consumer.**

| Responsibility | Current Python module | End-state (Rust) |
|---|---|---|
| Domain model, enums | `cards.py` | `cards` |
| Card DB / decklist load | `loader.py` | `loader` |
| **Effect IR (§3)** | `effects.py`, `conditions.py` | `ir` (serde structs, JSON round-trip) |
| **Text → IR + coverage** | `rules_parser.py` + `overrides.yaml` | `parser` (emits `cards.ir.json`) |
| Game state + `observable` | `state.py` | `state` |
| Turn loop / executor / finish sequence | `engine.py` | `engine` (**resumable state machine**) |
| Ported finish/breakout math | `finish.py` | `finish` |
| Ported skill-stop math | `stops.py` | `stops` |
| Seeded RNG | `rng.py` | `rng` (**portable PRNG**, §5) |
| **Game-log schema (§8)** | `gamelog.py` | `gamelog` (serde structs) |
| "Who decides" seam | `policy.py` (ABC + Random/Heuristic/Replay/profiles) | `policy` (trait + impls) |
| **Wire session + protocol** | *(new)* | `session` (§4) |

**Above the line — consumers (depend only on `srg-core`'s public API):**

| Consumer | Today | After |
|---|---|---|
| Console CLI | `cli.py` | Rust CLI over `srg-core` |
| MCP server | *(new)* | Rust binary wrapping `session` |
| Interactive human play | `interactive.py` | a `HumanPolicy` consumer of `session` |
| Post-game review | `review.py` | replay consumer of `gamelog` |
| Batch analysis | `analysis.py` | drives `Engine::play` in parallel |
| Matchup report (PDF/HTML) | `report/` | Python tooling (or → web layer) |

The line is enforceable: **`srg-core` must never import a consumer.** In the Python
phase this is guarded by `import-linter` in `invoke check`; in Rust it is guarded by
the crate graph (consumers are separate crates / binaries depending on `srg-core`,
never the reverse).

---

## 3. Public API surface

Three layers, in increasing interactivity. Signatures below are Rust sketches; the
Python oracle mirrors them 1:1 so the conformance harness (§6) can pair calls.

### 3.1 Load / build

```rust
fn load_index(cards: &Path) -> CardIndex;
fn resolve_deck(list: &DeckList, index: &CardIndex) -> Result<Deck, DeckError>;
fn validate_deck(deck: &Deck) -> Vec<Unsupported>;   // the fidelity gate (§8.1)
```

`validate_deck` returns every clause the parser could not map. Consumers that require
tournament fidelity (any real match, PvP or vs-AI) treat a non-empty result as
**fail-closed** — the session refuses to open. This is the hard deck-build constraint
the memory flagged, enforced by the same call the deck-builder UI uses.

### 3.2 Batch / pure (unchanged shape from today)

```rust
let result = Engine::new(decks, policies, seed).play();   // GameResult; log on state
```

For AI-vs-AI, analysis batches, and tests. Deterministic under `seed`. This is the
form the conformance harness runs (§6).

### 3.3 Session / interactive (new — the wire-facing engine)

```rust
enum Step {
    Decision(DecisionRequest),   // engine is suspended, awaiting one player's choice
    Done(GameResult),
}

impl Session {
    fn open(seed: u64, decks: Decks, viewers: Viewers) -> (Session, Step);
    fn submit(&mut self, response: DecisionResponse) -> Step;
    fn snapshot(&self) -> SessionSnapshot;                 // state.to_dict() + rng state
    fn restore(snap: SessionSnapshot) -> Session;          // resume without replay
}
```

`Session` is the **continuation engine**: internally the turn loop advances until it
needs a choice from a player, then *yields* a `DecisionRequest` and parks. `submit`
feeds the chosen option back in and resumes exactly where it parked. No re-run per
decision (the difference from the replay-from-seed PoC), but the same determinism —
see §4.

---

## 4. The decision protocol — `_decide` over the wire

Today the engine calls `self.policies[key].choose(point, legal, state, key)`
synchronously (`engine.py::_decide`). The protocol **externalizes that one call**.
The public shapes are language-neutral JSON so §8's `decision` event and the wire
message are the *same object*.

```jsonc
// server → client : the engine has suspended awaiting a choice
DecisionRequest {
  "request_id": "…",        // = hash(turn_no, decision_index): replay-stable + idempotent
  "seq": 17,                // monotonic; == count of decisions already submitted
  "viewer": "A",            // whose choice this is
  "point": "turn_action",   // §7 decision point — UNCHANGED set
  "legal": [ … ],           // exactly the engine's legal option set
  "observable_state": { … } // = state.observable(viewer) — the ONLY state that leaves the server
}

// client → server : the player's choice
DecisionResponse {
  "request_id": "…",        // must match the outstanding request
  "chosen": { … }           // one element of `legal` (or its index)
}
```

**Relationship to the engine call.** `point`, `legal`, and `chosen` are byte-for-byte
the fields of the §8 `decision` event. `observable_state` is exactly
`GameState.observable(viewer)`. So the protocol introduces **no new schema** — it is
`_decide` with the state projection attached and the answer round-tripped.

**Idempotency & ordering.** `request_id = hash(turn_no, decision_index)` is stable
under replay and unique per decision; `seq` equals the number of decisions already
accepted. A duplicate or stale `DecisionResponse` (client resend, reconnect) is a
no-op because its `request_id`/`seq` no longer matches the outstanding request. There
is never more than one outstanding request per session.

**Anti-cheat falls out.** Only `observable_state` crosses the wire. The **seed and
all hidden zones (decks, the opponent's hand) never leave the server**, so a client
cannot predict its own draws. This is the existing `observable`/`hidden`-flag boundary
(§7) reused verbatim as the network trust boundary.

**Continuation vs replay — two drivers, one protocol.** The chosen engine is the
continuation state machine (live in-memory game state on the server; `submit` resumes
in place). But the *protocol is identical* to a replay-from-seed driver that stores
`(seed, decisions[])` and re-runs to the next decision. That equivalence is not
academic — it is the backbone of two facilities:

- **Crash / reconnect recovery.** A continuation engine holds mutable state, so
  recovery is not free the way it is for pure replay. `Session::snapshot` serializes
  `state.to_dict()` + RNG state at each decision boundary; on restart the server
  `restore`s the last snapshot (cheap) or, as a fallback, **replays `(seed,
  decisions[])`** — the same `decisions[]` the log already stores — to rebuild state.
  Both paths must land on byte-identical state; that is a conformance assertion (§6).
- **Determinism guarantee.** `resume(response)` is a pure function of
  `(prior state, response)`; the whole session is a pure function of
  `(seed, decisions[])`. Same inputs → byte-identical `GameLog`, whether produced by
  the live continuation engine or by a cold replay. This is what the conformance
  harness pins.

---

## 5. Determinism & the portable PRNG

Determinism is a **product requirement independent of Python**: replay, snapshots, and
netcode all need the roll/shuffle stream reproducible across Rust targets (native vs
`wasm32`) and across a snapshot/restore. Byte-identical Python↔Rust logs (the port aid
in §6) are a *second* beneficiary, not the sole reason. Today `rng.py` wraps CPython's
`random.Random` (MT19937 with `choice`/`shuffle` rejection sampling), which is neither
portable across Rust targets nor cheaply reproducible outside CPython.

**Change:** replace the generator with a **portable algorithm — splitmix64** (or a
small PCG) — implemented identically in both languages. It is a few lines each, has
no platform/tempering subtleties, and makes the three primitives (`roll`, `shuffle`,
`reveal`) reproducible across engines.

- **Contract.** `roll()` maps the next 64-bit draw to one of six skill faces by a
  fixed reduction; `shuffle` is a Fisher–Yates driven by the same stream; `reveal` /
  `randint` share it. The face order (`SKILL_FACES`) and the reduction are part of the
  contract and identical in both engines.
- **Snapshot.** The RNG state is now just the 64-bit (or 128-bit) internal word, so
  `snapshot`/`restore` and the §8 hidden-state handling get *simpler*, not harder.
- **Cost.** This **invalidates existing golden logs/seeds** — they are re-generated
  once under the new PRNG. It touches **neither §3 nor §8**; it is a §6-adjacent
  determinism change, formalized as a `DESIGN.md` delta (§9 below).

---

## 6. The conformance harness — the guard on the rewrite

A second implementation is a drift risk. The harness converts that risk into a
**continuously-proven equivalence** *during the port*. With **no consumers depending on
Python**, this is a purely internal **build-time de-risking aid** — the cheapest way to
catch subtle port bugs in the effects executor, stop resolution, and finish sequence,
where the existing Python code already encodes hundreds of correct behaviors that
`fae_comp` + closed-form checks alone do not cover. It is kept only as long as it pulls
its weight; nothing external forces its longevity. Two phases:

**Phase 1 — Python as live oracle (differential testing).** For a growing corpus of
`(seed, decisions[])` fixtures spanning the top-96 cards and every engine branch:

1. `python_log = PythonEngine(seed, decisions)` and
   `rust_log = RustEngine(seed, decisions)`.
2. Assert **byte-identical** logs after canonical normalization (stable key order,
   fixed float formatting — though the engine is integer-valued).
3. Assert **parser parity** on the IR artifact: `python_parse(card) == rust_parse(card)`
   for every card in the DB, so `cards.ir.json` is engine-agnostic.
4. Assert **snapshot/replay parity**: `restore(snapshot)` state ==
   `replay(seed, decisions)` state at every decision boundary (§4).

The Python engine's 640 tests and the verbatim-ported `fae_comp` finish/stop math are
what make the oracle *trustworthy*; Rust inherits that validation by matching it.

> **Realized scope (task 75, `invoke conformance`).** Steps 3–4 run live against the
> oracle at `~/data/srg_sim_python` (`tests/parser_parity.rs` over the whole DB, and
> `tests/session.rs::snapshot_restores_at_every_boundary`). Step 2 (whole-log parity)
> is **not** re-run against that oracle: the `python` branch kept `random.Random`
> (MT19937) rather than adopting the §5 splitmix64 stream, an accepted split — so the
> two engines' logs cannot be byte-identical at a shared seed. Log parity is therefore
> owned Phase-2-style by the frozen splitmix64 corpus, which Rust reproduces in
> `tests/engine_conformance.rs`. The parser is RNG-independent, so step 3 is unaffected
> and is the genuine cross-language log-free check.

**Phase 2 — freeze & deprecate.** Once Rust passes Phase 1 across the top-96, the
Python engine is retired. Its outputs are **frozen into a golden-log corpus** (the
fixtures + their canonical logs). Rust then regresses against the frozen corpus in CI.

> **Known consequence of "deprecate after cutover" (accepted):** after Phase 2 there
> is **no live differential oracle for newly-added cards** — a new card's IR and
> behavior are validated by Rust unit tests + a hand-checked golden log, not by a
> second engine. The frozen corpus still catches regressions on everything it covers.
> If this bites, the escape hatch is to keep the Python parser alive as an IR oracle
> only (cheaper than the full engine).

---

## 7. Migration phases

Each phase ends green (conformance for engine phases; a working consumer for the rest).

- **M-R0 — contract freeze.** Pin §3 IR and §8 gamelog as JSON schemas both languages
  validate against — *done*: generated from the frozen dataclasses by
  `srg_sim/schema.py`, committed under `schemas/v1/`, with a drift-guard + conformance
  test (`tests/test_schema.py`) that fails on any un-versioned §3/§8 change. Swap Python
  `rng.py` to splitmix64 — *done*: canonical splitmix64 (matches the published reference
  vectors), the cross-engine draw-stream contract. Seed the conformance corpus — *done*:
  `srg_sim/conformance.py` + `tests/conformance_corpus.py` generate self-contained
  `(seed, decks, decisions[]) → canonical log` fixtures under `fixtures/conformance/`
  (Heuristic-family policies so replay is byte-exact), guarded by `tests/test_conformance.py`
  (byte-identical replay + a golden-log drift guard). This is the exact target the Rust
  engine (M-R1) must reproduce. (No consumers depend on Python, so no deprecation runway —
  the only cost is reseeding fixtures.)
- **M-R1 — Rust core + parity (console + MCP).** Port `ir`, `state`, `finish`,
  `stops`, `rng`, `gamelog`, `engine` (as the resumable state machine), `parser`,
  and the `policy` trait. Stand up the **conformance harness** (§6). Ship the **Rust
  console CLI** and the **MCP server** over `session`. Python engine still runs as the
  live oracle.
- **M-R2 — web presentation (WASM).** Compile `srg-core` to WASM; build the web
  presentation layer on `session` + `observable_state`. Server-authoritative PvP runs
  the same crate natively.
- **M-R3 — cutover & deprecate.** Rust passes conformance on the top-96 → freeze the
  golden corpus, retire the Python engine (§6 Phase 2). Report tooling stays Python or
  migrates into the web layer.
- **M-R4 — mobile offline (later).** Bundle `srg-core` via uniffi/WASM for on-device
  vs-AI. No new engine — the same crate, same conformance guarantee.

---

## 8. Sim → real-game deltas (honest accounting)

The sim was built to *analyze* matchups; a real game must be *tournament-exact,
pausable, and two-sided*. Each delta and how the design closes it:

### 8.1 Fidelity bar jumps to tournament-exact
The sim logs `Unsupported` and plays on; a real match cannot. **Close:**
`validate_deck` (§3.1) is **fail-closed** at `Session::open` — a deck with any
`Unsupported` clause cannot enter a real game. Coverage-clean becomes a hard
deck-build constraint, enforced by the same substrate call the deck-builder uses.
Simplifications the sim shipped (a modeled "generally-best" mode of an A-or-B choice,
skipped errata riders) must each become a real decision point before their card is
tournament-legal — tracked in the coverage report, now a *release gate* not a metric.

### 8.2 Engine must pause at `_decide`
**Close:** the continuation `Session` (§3.3, §4). The batch `Engine::play` path
(§3.2) remains for AI-vs-AI and tests by driving the same machine with
auto-responding policies — one engine, two drivers.

### 8.3 Timing / priority made explicit
The sim resolves linearly; two real players need agreed **response windows**
(stop windows), **simultaneous-trigger ordering**, and **priority passing**. **Close:**
model each as an **additional decision point in the same protocol** — the stop window
is already the `stop` decision surfaced to the defender; simultaneous triggers become
an `order_triggers` decision to the controlling player; a priority pass becomes a
`pass_priority` decision. These are **§7 additions (new decision points), not §3/§8
changes.** *Staged:* reserved in the protocol now, specified in full in a follow-up —
PvP is target #2–3, behind console/MCP and offline.

### 8.4 Language split
**Close:** resolved by the Rust decision — one engine, compiled native + WASM, reaching
every consumer (§1). The conformance harness (§6) is the guard that a *port* (should
one ever be added for a platform Rust can't reach) stays honest; in the meantime there
is no second implementation to distrust.

### 8.5 Decision logs as training corpus
Already free: every `decision` event (sim, human, or PvP) is a
`(observable_state, legal, chosen)` tuple in the §8 schema. Human/PvP sessions log the
same schema, so real-player data for `LearnedPolicy` (DESIGN §7 M4) accrues with no
change. Unaffected by the Rust move — the corpus is JSON.

---

## 9. Proposed `DESIGN.md` deltas (additive — §3/§8 preserved)

All additive; none alters IR or gamelog semantics. Proposed against the gate, per
CLAUDE.md:

1. **§5/§6 (RNG) — determinism note.** Record that the seeded RNG is a **portable
   splitmix64** (identical across engines), replacing the `random.Random` wrapper, to
   permit byte-identical cross-engine logs. Note that this reseeds existing golden
   logs and touches neither §3 nor §8.
2. **§7 (policy interface) — protocol + timing points.** Add the wire form of a
   decision (`DecisionRequest`/`DecisionResponse`) as the transport of `_decide`, and
   **reserve** the `order_triggers` / `pass_priority` decision points for explicit
   timing (§8.3), to be specified in a follow-up.
3. **§9 (module layout) — boundary + language.** Record the substrate line (`srg-core`
   below; consumers above) and the Rust end-state, with the Python engine as a
   **transitional oracle** (this doc, §2/§6/§7).
4. **New §13 — substrate boundary, session API, and conformance.** Point to this doc
   as the authoritative expansion; capture the boundary rule ("`srg-core` never imports
   a consumer"), the three-layer public API (§3), and the conformance harness (§6) as
   the migration's safety rail.

§3 and §8 are **explicitly unchanged** — that they need no change is the evidence the
split is clean.

---

## 10. Open questions (flag, don't guess)

- **Parser in Rust vs. IR-oracle-only Python.** "Everything in Rust" ports the
  regex/grammar parser (`rules_parser.py`) to Rust. The grammar iterates fastest in
  Python; the fallback in §6 (keep the Python *parser* alive as an IR oracle even
  after the engine is dropped) is cheap insurance. Decide at M-R1 whether the Rust
  parser ships from day one or Python emits `cards.ir.json` until the Rust parser
  reaches parity.
- **MCP server surface.** Which tools the MCP server exposes (open session, submit
  decision, observe, analyze, coverage) — a small spec of its own, drafted at M-R1.
- **Timing/priority full spec (§8.3).** The response-window / simultaneous-trigger /
  priority-pass model — deferred to a follow-up doc; only the decision points are
  reserved now.
- **WASM RNG/word-size.** Confirm splitmix64 (64-bit) behaves identically under
  `wasm32` — expected, but pinned by a conformance fixture.
