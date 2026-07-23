# srg_sim — record / replay / interop: the answers (brief items 11–13)

Answers to the "design-lock now, build later" section of
[`FRONTEND_INTEGRATION_BRIEF.md`](FRONTEND_INTEGRATION_BRIEF.md). Short version: the
observable-frame and observer-record schemas are **pinned, versioned, and validating**,
so the historical-archive import is unblocked. Reference material, in the order you'll
want it:

- [`schemas/v1/match_record.md`](schemas/v1/match_record.md) — the format guide (write
  your importer against this)
- [`schemas/v1/match_record.schema.json`](schemas/v1/match_record.schema.json) — the pinned schema, `version: 1`
- [`fixtures/records/observer_example.json`](fixtures/records/observer_example.json) — a complete hand-authored observer archive
- `DESIGN.md` §8.1, `src/record.rs`, `tests/record.rs`

Version stamp is now:

```json
{ "engine": "0.1.0", "commit": "<git short hash>",
  "schemas": { "effect_ir": 70, "game_log": 1, "observable_state": 1, "match_record": 1 },
  "policies": ["random","heuristic","aggressive","smart","newbie"] }
```

---

## 11. Canonical serialization format — **not** the raw game log

You proposed adopting `gamelog.schema.json` as the portable interchange format. I'd
push back on that specific choice, and I've built the alternative rather than just
asserting it. Three reasons the JSONL log is the wrong public artifact:

1. **It leaks hidden state.** Every `decision` event carries the deciding player's
   `legal` list — which, at a `turn_action` point, enumerates their entire hand. A
   published log hands a spectator information no player had. That's fatal for
   "some recorded games will be made public."
2. **It can't carry an engine stamp.** You asked that every record embed the engine
   version it was produced with. The log header deliberately can't: the conformance
   goldens (`fixtures/conformance/`) compare headers byte-for-byte, so putting a commit
   hash in there would break every golden on every commit.
3. **It isn't authorable.** An imported real-life match has no seed, no deck order, no
   ground-truth card ids for hidden moves. The log schema assumes all three.

The interchange format is instead a **match record**
(`schemas/v1/match_record.schema.json`, `schema_version: 1`), which *contains* the
frame sequence and, for engine-run games, the compact replay seed. The game log stays
exactly what it is — the engine's own loss-less stream, still versioned, still
byte-for-byte replayable by `srg replay` — but it is an internal/analysis artifact, not
the thing you publish.

**Store both, and here's the split:** for a site-run game the frames are *derivable*
from the replay seed (`seed + decks + seats + decisions`, a few KB) — so persist the
seed as the source of truth and rehydrate frames on demand; persist the frames too if
you want the match to survive an engine bump that changes replay (a rules fix will
change what re-simulation produces; frames are what actually happened). For an imported
game the frames **are** the record. Both carry the engine stamp when one applies.

## 12. Browser replay affordance — yes, two ways

- **Frames (what a replay viewer should use).** `WasmSession.frames()` returns the
  ordered observable-frame sequence — forward, back, jump, scrub, no engine
  re-execution — and it works identically for imported records, which can't
  re-simulate at all. `frames_from(n)` is the incremental read while a match is live
  (the sequence is re-derived on every step, so don't re-serialize the whole thing each
  time), `frame_count()` for the cheap check.
- **Re-simulation (when you want live engine state).** Your instinct was right:
  `restore` + re-submitting recorded decisions does yield every intermediate `Step` in
  order. Trimming the last *k* answers from `decisions.<seat>` in a snapshot rewinds
  exactly *k* decisions. That's now guarded by
  `tests/record.rs::truncated_snapshot_rewinds_to_the_same_steps`, so it won't silently
  regress. Caveat: with more than one remote seat, trim per seat — the snapshot stores
  answers per seat, not as one interleaved list.

Use the second only when you need legal moves / alternate lines. For playback the
frames are cheaper and universal.

## 13. Two information shapes — one schema

**(a) The observable frame.** Confirmed, with one deliberate deviation from your
sketch. A frame is *not* the `observable_state` object from ask #5 verbatim: that shape
embeds whole `Card` objects with compiled Effect IR and raw rules text, which is
impossible to hand-author and enormous to store per step. A frame instead carries card
**references** (`{card: db_uuid, name?, number?}`) — join against your card DB for
everything else, which you already do for card art:

```jsonc
{ "seq": 12, "turn_no": 2, "active": "B", "crowd_meter": 1,
  "action": { "type": "play", "player": "B",
              "card": {"card": "<uuid>", "name": "2 Handed Slam", "number": 11},
              "order": "Lead", "atk_type": "Grapple" },
  "players": { "A": { "in_play": [<CardRef>…], "discard": [<CardRef>…],
                      "hand_size": 3, "deck_size": 24, "gimmick_blanked": false },
               "B": { … } } }
```

And yes — **a full game's log yields exactly this ordered frame sequence**. The engine
projects each log event to at most one frame as the match plays (`decision` and
`unsupported` are dropped, not redacted; a movement the log marks `hidden` becomes a
count with no card ids), so it's the same data path for a live match, a rehydrated
replay, and a `srg record` export. The frame vocabulary reuses the log's event type
names, so the two documents read against each other.

One semantic to build the viewer around: **state is captured as of the action, not
after it settles.** A played card is still resolving through the stop window when its
`play` frame is emitted, so it shows up on `in_play` in a *later* frame — or never, if
it gets stopped. That's faithful to how the table actually looks.

**(b) The observer record.** Defined, versioned, and validating — it's the same
envelope with `kind: "observer"`, no `replay` seed, no `engine` stamp. Everything a
transcriber can't know is optional (`hand_size`, `deck_size`, `gimmick_blanked`, the
decklists, a card's uuid), and there's a `note` action for anything the vocabulary
doesn't model, so nobody has to distort what they saw to make it fit. An observer
record carrying a replay seed is a hard validation **error** — an observed match is not
re-simulatable, and the schema refuses to pretend otherwise.

Validation exists (you asked "if feasible"):

```bash
srg validate-record archive.json                      # structure
srg validate-record archive.json --cards cards.yaml   # + every card uuid resolves
```

in the browser, `validate_record(json)` → `{"errors":[…],"warnings":[…]}`. Errors reject;
warnings flag thin archives (missing counts, unidentified cards) that still play back
fine. It's structural — it does **not** re-derive the rules, so it can't tell you an
imported match was *played* legally, only that the archive is well-formed. Building
that would mean running the rules engine over a game with no hidden state, which is a
different (and much later) project.

No authoring tool, as you asked. `fixtures/records/observer_example.json` is the
worked example to hand someone: two turns, real card uuids, exercising `start`, `roll`,
`turn_result`, `play`, `stop`, `discard`, `draw`, `note`, `crowd_meter`,
`finish_attempt`, `breakout`, `result`.

## What landed

- `src/record.rs` — the record/frame model, the log→frame projection, the validator.
- `schemas/v1/match_record.schema.json` + `match_record.md`; DESIGN.md §8.1.
- Engine/session: frames captured alongside the log; `Session::{frames, frames_from,
  record}`; `SessionSnapshot` gained `PartialEq`.
- WASM: `frames()`, `frames_from(n)`, `frame_count()`, `record(source)`, and the free
  function `validate_record(json)`.
- CLI: `srg record …  --out rec.json` and `srg validate-record rec.json [--cards …]`.
- `version_info()` gained `schemas.match_record`.
- `fixtures/records/observer_example.json`; `tests/record.rs` (11 tests) covering
  validity, ordering, the no-hidden-leak property, restore-determinism, the rewind
  recipe, and validator rejections; `tests/schema_version.rs` guards the constant.
- `build.rs` now re-runs when the branch ref moves, so the `commit` stamp a record
  embeds is actually the commit it was built from (it used to go stale between commits
  on the same branch).

Nothing here changed the live-play protocol, the Effect IR, or the game-log schema — no
skew with the pkg you've already vendored beyond the added APIs.
