# Handoff: build the "Run It Back" in-browser play screen

You are the web frontend agent working in `~/data/srg_card_search_website`. The
`srg-core` Rust engine (in `~/data/srg_sim`) is **ready** and everything the play
screen needs is committed. This brief is self-contained; the deeper references are
listed at the end. Your goal, per the original brief: **a human plays one full,
non-crashing match of the test decks in the browser against a local AI, and sees
sensible choices** — not full rules coverage.

## What to vendor (no Rust toolchain needed)

From `~/data/srg_sim`, copy into your app:

- `web/src/pkg/{srg_core.js, srg_core_bg.wasm}` — the WASM engine, **current**
  (effect_ir schema **70**). Committed; vendor as-is.
- `web/src/sample/deck{A,B}.json` — Bull vs Fae enriched decks, valid against the
  current schema. Use as your UI fixture / first playable target.

## The API (this is the whole surface)

```js
import init, { WasmSession, version, policies } from './pkg/srg_core.js'

await init()

// Version stamp — assert no skew vs the backend `srg` binary (see below).
const info = JSON.parse(version())
// { engine:"0.1.0", commit:"…", schemas:{effect_ir:70, game_log:1, observable_state:1}, policies:[…] }
const opponents = JSON.parse(policies())          // ["random","heuristic","aggressive","smart","newbie"]

// Open a match. Decks are enriched-Deck JSON (from the backend, or the sample files).
// Seats: the human is the "remote" seat; the AI seat is a policy name.
const s = WasmSession.open(
  JSON.stringify(deckA), JSON.stringify(deckB),
  JSON.stringify({ A: "remote", B: "heuristic" }),
  BigInt(seed)                                     // u64
)                                                  // throws a JS Error on bad deck/seat/policy

let step = JSON.parse(s.step())                    // current step (also what submit returns)
// … render step …
// when the user clicks legal[i]:
step = JSON.parse(s.submit(i))                     // advance; returns the next step JSON

// Persist / resume the whole match:
const snap = s.snapshot()                          // string
const s2 = WasmSession.restore(snap)               // rebuilds to the same step
```

`WasmSession.open`, `.submit`, `.restore` throw a JS `Error` on bad input; wrap
them. The AI seat resolves locally and never suspends — you only ever render/answer
the **remote** (human) seat's decisions.

## Rendering a `Step`

```jsonc
// awaiting a choice:
{ "kind":"decision", "request": {
    "request_id":"…", "seq":N, "viewer":"A", "point":"<point>",
    "legal":[ /* option objects */ ], "observable_state":{ … } } }
// match over:
{ "kind":"done", "result": { "winner":"A"|"B"|"draw", "reason":"finish|count_out|disqualification|pinfall|turn_cap", "turns":N } }
```

The user picks one element of `legal[]`; you submit its **array index**. Branch on
`point` first, then `kind` (the `kind` discriminator is not unique across points).
**Full catalog of every `point` and `legal` shape: `schemas/v1/decision_protocol.md`.**

**Payload fields to render on buttons/prompts (don't re-derive rules):**

| point(s) | render from the option/first-option |
|---|---|
| card picks (`turn_action`, `stop`, `bury`, `discard`, `target`, …) | `number` + `card` (uuid) + `order`/`atk_type`. **No card name** — join `card` against the card objects in `observable_state` (`in_play`/`discard`/own `hand`), each has `name`. Cross-board picks add `owner:"A"|"B"`. |
| `optional`, `optional_swap` | the effect's rules text in `clause` (e.g. *"When you roll Submission you may look at your opponent's hand."*) |
| `stop` | the first option `{kind:"none", vs_order, vs_type}` = what's being defended |
| `choice` | each option's `label` (branch text) |
| `name` | each option's `name` |
| `mulligan_draw` | each option's `n` (count) |
| `elect_bump` | the `losing` boolean hint |

## `observable_state`

Per-viewer projection, pinned in `schemas/v1/observable_state.schema.json`:

```jsonc
{ "schema_version":1, "viewer":"A", "crowd_meter":N, "active":"A", "turn_no":N,
  "players": { "A": {…}, "B": {…} } }
```

Each player: `competitor`, `entrance`, `in_play:[Card]`, `discard:[Card]`,
`gimmick_blanked`, `deck_size`, and **exactly one of** `hand:[Card]` (your seat, or
a Peek-revealed opponent) **or** `hand_size:N` (hidden opponent). A `Card` is
`{ db_uuid, name, number, atk_type, play_order, … }` — `name` is your join key for
option labels. This is the anti-cheat boundary: never assume the opponent's hand is
present.

## No-skew check (do this at load)

Assert the three `schemas` versions from WASM `version()` **equal** what the backend
`srg` binary reports (`srg info`). Compare the **schema versions, not `commit`** — a
committed pkg carries its parent commit's stamp by construction. Current contract:
`effect_ir:70, game_log:1, observable_state:1`.

## Two things on your/backend side (required)

1. **Rebuild & deploy the backend `srg` binary from the same commit as the vendored
   pkg** — they're a matched pair (both must report effect_ir 70). Easiest: in
   `~/data/srg_sim` run `invoke release-web` (builds the `srg` release binary **and**
   the pkg from one tree), commit the pkg, ship that binary.
2. **Enriched decks come from the backend** (`srg session open <a.yaml> <b.yaml>
   --cards <cards.yaml>` → take `snapshot.deck_a`/`deck_b`), handed straight to
   `WasmSession.open`. Don't parse YAML/enrich in the browser.

## Coverage caveat (partial fidelity by design)

Bull & Warehouse decks are **fully modeled**; other decks (e.g. Fae) have some
clauses that safely **no-op** — matches complete correctly, but some card text won't
fire. This never crashes. If you want a "partial rules" badge, flag any card whose
`effects` (visible in `observable_state`) contains an `Unsupported` node.

## Not yet exposed — ask if you want it

`WasmSession` has no `log()` accessor yet, so there's no in-browser **play-by-play
event feed** (rolls, plays, stops, finishes). The engine `Session` already produces
it (`srg session` emits `log`); exposing it on WASM is a ~5-line add — say the word.
For the first playable test you don't need it: render state from `observable_state`
(board, hands, crowd, turn) and end on the `done` result.

## First target (golden path)

Bull (`deckA.json`) vs Fae (`deckB.json`), seats `{A:"remote", B:"heuristic"}`, a
fixed seed → one full, non-crashing match, playable end to end with sensible
choices. Then generalize the opponent picker to `policies()`.

## References (all in `~/data/srg_sim`)

- `schemas/v1/decision_protocol.md` — every decision `point` + `legal` option shape
- `schemas/v1/observable_state.schema.json` — the state contract
- `FRONTEND_INTEGRATION_RESPONSE.md` — the full response to your original brief
- `src/wasm.rs` — the WASM binding source (the API above)
- `decks/README.md` — deck testing (`srg audit`) if you add/model new decks
