# srg_sim — brief for enabling initial Run It Back frontend gameplay tests

You are working in `~/data/srg_sim` (the Rust `srg-core` engine). A new
login-gated section, **Run It Back**, is being built in
`~/data/srg_card_search_website` where a user plays a full game of Supershow in
the browser against an AI opponent, driven by this engine compiled to **WASM**.

The backend + auth + deck storage + enrichment are already done and verified.
What's next on the website side is the in-browser **play screen**. Your job is
to make sure the engine gives that screen everything it needs to run a real
match end-to-end — and nothing more for now. Optimize for "a human can play one
full, non-crashing match of our test decks in the browser and see sensible
choices," not for full rules coverage.

## How the frontend uses you (the integration contract we depend on)

- The frontend **vendors** `web/src/pkg/{srg_core.js, srg_core_bg.wasm}` and calls:
  `init()` → `WasmSession.open(deckA_json, deckB_json, seats_json, BigInt(seed))`
  → `.step()` / `.submit(choiceIndex)` → `.snapshot()` / `.restore()`, all
  returning JSON strings. Seats are `{A:'remote', B:'<policy>'}` (human is A;
  AI seat B resolves locally and never suspends).
- **Enriched decks come from the backend**, which shells
  `srg session open <a.yaml> <b.yaml> --cards <cards.yaml>` and takes
  `snapshot.deck_a` / `snapshot.deck_b`. Those enriched `Deck` JSON objects are
  handed straight to `WasmSession.open`. So the CLI-produced enriched deck and
  the WASM-consumed deck **must be the same schema** (see version-skew below).
- The play screen renders `Step`:
  - decision: `{kind:'decision', request:{request_id, seq, viewer, point, legal:[...], observable_state:{turn_no, crowd_meter, active, players:{A,B}}}}`
  - done: `{kind:'done', result:{winner, reason, turns}}`
  The user clicks one element of `legal[]`; the frontend submits its index.

## Test decks we will use first

Single-competitor decks only (the CLI loader currently hardcodes
`SingleCompetitorCard`): **The Bull** (`decks/bull.yaml`) vs **Fae**
(`decks/fae.yaml`), seat B policy `heuristic`, a few fixed seeds. Please treat
these as the golden path — if something must work, it's these two decks.

## Prioritized asks

**P1 — must have for the first playable test**

1. **Panic-free matches with partial coverage.** A full match of the test decks
   must play from `open` to `{kind:'done'}` in WASM **without ever panicking**,
   even though ~36% of main-deck clauses currently parse to `Unsupported`
   (`srg coverage` main deck ≈ 63.5%). Unsupported clauses must degrade to
   no-ops, never `unwrap()`/`panic!` — a WASM panic poisons the module and
   kills the session mid-game, which is the single worst failure mode for the
   UI. If any code path can still panic mid-match, convert it to a graceful
   no-op or a structured error step. Please add/point to a test that plays these
   decks across several seeds and asserts no panic + a terminal result.

2. **Same-version CLI binary and WASM pkg, with a version stamp.** The backend's
   `srg` release binary and the vendored `web/src/pkg` must be built from the
   **same commit** so the enriched-deck schema matches. Provide a single
   command/`invoke` target that builds both from the current tree, and expose an
   engine/schema version the frontend can read (e.g. a `WasmSession`/module
   `version()` accessor mirroring `srg info`) so we can assert no skew at load.

3. **Committed, current WASM build.** Refresh `web/src/pkg` from the current
   engine and commit it, so the frontend can vendor a known-good artifact
   without a local Rust toolchain. Document the refresh command.

**P2 — makes the UI good, not just functional**

4. **Self-describing decisions.** For each `legal[]` option and each `point`
   (decision type), give the frontend enough to render a button label without
   re-deriving rules — ideally a short human label and the card(s)/uuids
   involved per option, not just an opaque enum/index. If adding labels is too
   invasive now, instead **document the JSON schema of every `point` type and
   every `legal` option variant** so we can map them to UI ourselves.

5. **Pin & version the observable_state schema.** Document/pin the full
   `observable_state` shape (top-level fields, and each `players.{A,B}` sub-shape:
   hand cards vs. hand count, in-play, discard, crowd meter, etc.) in
   `schemas/v1`, and bump a `schema_version` the client can assert against.

6. **Enumerate available policies.** Expose the list of AI policies
   (`random`, `heuristic`, `aggressive`, `smart`, `newbie`) from the engine
   (CLI subcommand or WASM accessor) so the opponent picker isn't hardcoded.

**Nice to have / explicitly LATER (do not spend time now unless trivial)**

7. Raising rules coverage for cards specifically in the test decks (share of
   `Unsupported` clauses that actually change play). Fine to defer; note which
   test-deck cards have gameplay-affecting Unsupported clauses so we can warn users.
8. WASM-side deck enrichment (parse decklist + card DB in the browser). **Not
   needed** — the backend enriches. Skip.
9. Tornado/Trio competitor support. Out of scope for initial tests.
10. A deterministic scripted-match fixture (seed + decks + fixed choice sequence
    → expected step sequence) the frontend can snapshot-test against, and keep
    `web/src/sample/deck{A,B}.json` valid against the current schema as an
    engine-independent UI fixture.

## Record / replay / interop (design-lock now, build later)

This is the deeper point of "Run It Back": the website will **record every game
as a portable, replayable artifact**, and — the headline feature — **import
detailed logs of games played in person or on other platforms** and replay them
on our site. We are not building the replay UI in the first pass, but the
decisions below are yours and affect the schema/version work above, so please
lock them now so we don't persist records in a throwaway shape.

11. **Confirm the canonical serialization format.** We intend to adopt your
    pinned **`schemas/v1/gamelog.schema.json`** (the JSONL event log that
    `srg replay` reproduces byte-for-byte) as the portable interchange format
    for both our own games and imported ones. Please confirm this is the right,
    stable choice, keep it versioned, and make sure every log/record embeds the
    **engine version** it was produced with (replay fidelity depends on it —
    same `version()` from ask #2). If a snapshot (seed+decks+seats+decisions) is
    a better compact replay seed than the full log, tell us and we'll store both.

12. **Browser replay affordance.** For the replay viewer, the frontend needs to
    reconstruct and **step through the full ordered sequence of steps** of a
    finished game from a stored record (snapshot or gamelog) — forward/back,
    not just jump to the end. Confirm whether `WasmSession.restore` + re-running
    recorded decisions gives us each intermediate `Step` in order; if not,
    please add a WASM replay API that yields the step sequence.

13. **Two information shapes — full vs observer.** Records come in two shapes and
    the schema must serve both:
    - **full** (site-run games): complete data — hidden zones, deck contents,
      seed. Engine-authoritative. This is the normal log/snapshot.
    - **observer** (imported real-life / other-platform games): only what a
      *spectator* could see. Hidden hands and deck contents are **absent** and
      cannot be reconstructed; there is no seed and the game is **not
      re-simulatable**. The website will render these as a **playback of an
      ordered sequence of observable frames** (per-step public state + the action
      that occurred), not an engine re-run.
    So the frontend's replay is built around an **observable-frame sequence** that
    works for both shapes. Two asks:
    (a) Confirm the exact schema of one observable frame (this is the
    `observable_state` shape from ask #5, per step, plus a description of the
    action taken) and that a **full game's log yields this ordered frame
    sequence**.
    (b) Confirm/define an **observer-level record schema** the user can author to
    directly — a standalone sequence of observable frames (+ participants +
    result), with **no hidden state or seed required** — and, if feasible,
    validate such a record. Do NOT build an authoring tool; the user (a capable
    dev, possibly with Claude) produces the archive data in your documented
    format. We just need that format nailed down and versioned.

Note: some recorded games will be made **public** and replayable by anyone on our
site, so the observable-frame/observer schema is effectively a public interchange
format — keep it clean and stable.

These are **not** blockers for the first playable test (P1 above stays: no-panic
match + version + fresh pkg). They're here so the gamelog / observable-frame
schema stays the stable, version-stamped interchange format we build persistence
on.

## Deliverables

- A no-panic multi-seed match test over `decks/bull.yaml` vs `decks/fae.yaml`.
- One command that builds the `srg` release binary **and** the WASM pkg from the
  same commit, plus a readable engine/schema version from both CLI and WASM.
- A refreshed, committed `web/src/pkg`.
- Docs (in `DESIGN.md` or a short `schemas/` note) for the `point` types,
  `legal` option variants, and `observable_state` shape, with a `schema_version`.

Please report back the version string, the build/refresh command, and any
test-deck cards whose Unsupported clauses materially affect play.
