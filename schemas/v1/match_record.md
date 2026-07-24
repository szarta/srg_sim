# Match records (v1) — the replay / import interchange format

How to **store**, **publish**, **replay**, and **import** a match. Pinned alongside
[`match_record.schema.json`](match_record.schema.json); the engine side is
`src/record.rs` (DESIGN.md §8.1). Companion to the live-play wire contract in
[`decision_protocol.md`](decision_protocol.md).

A record is one JSON object. Its centre is **`frames`**: the ordered sequence of
observable states a replay viewer steps through. Everything else is envelope.

```jsonc
{
  "schema_version": 1,
  "kind": "full",                  // "full" (engine ran it) | "observer" (transcribed)
  "engine": { "engine": "0.1.0", "commit": "…",
              "schemas": { "effect_ir": 70, "game_log": 1,
                           "observable_state": 1, "match_record": 1 },
              "policies": ["random","heuristic","aggressive","smart","newbie"] },
  "meta": { "created": "2026-07-19T20:15:00Z",
            "source": "in person — Saturday locals",
            "match_type": "standard", "notes": "" },
  "players": {
    "A": { "player": "Brandon",
           "competitor": { "card": "<db_uuid>", "name": "The Bull" },
           "entrance":   { "card": "<db_uuid>", "name": "Calling in Kanik" },
           "deck": [ { "card": "…", "name": "…", "number": 1 }, … ] },   // optional
    "B": { … }
  },
  "frames": [ … ],
  "result": { "winner": "B", "reason": "finish", "turns": 2 },
  "replay": { "seed": 7, "deck_a": {…}, "deck_b": {…},
              "seats": {…}, "decisions": {…} }                           // "full" only
}
```

## The two kinds

| | `full` | `observer` |
|---|---|---|
| produced by | this engine (`srg record`, `WasmSession.record()`) | a human transcribing a real-life / other-platform match |
| `replay` seed | present → re-simulatable, frames are derivable | **absent** → playback only |
| `engine` stamp | present (replay fidelity is only guaranteed against a matching stamp) | absent |
| hidden zones | never (counts only) | never |
| authored by hand | no | **yes — this is the import format** |

Both kinds replay identically in a viewer, because a viewer only ever walks `frames`.

**Storage advice.** For a `full` record the frames are derivable from `replay`, so the
compact thing to persist is the seed (a few KB) and rehydrate frames on demand
(`WasmSession.restore(snapshot)` → `frames()`); persist the frames too if you want the
match to survive an engine version bump that changes replay. For an `observer` record
the frames **are** the record. Raw frame sequences are repetitive JSON — expect a
20-turn match around half a megabyte uncompressed and a small fraction of that gzipped.

## Frames

```jsonc
{
  "seq": 12,                 // 0-based, dense, chronological
  "turn_no": 2,              // never decreases
  "active": "B",             // seat whose turn it is
  "crowd_meter": 1,
  "action": { "type": "play", … },   // what produced this frame
  "players": {
    "A": { "in_play": [<CardRef>…], "discard": [<CardRef>…],
           "hand_size": 3, "deck_size": 24, "gimmick_blanked": false },
    "B": { … }
  }
}
```

- Frame `0` is the opening position with `action: {"type":"start"}`; the **last** frame
  is `{"type":"result", …}` and must agree with the record's `result`.
- State is captured **as of** the action, not after everything it triggers settles. A
  played card is still resolving through the stop window, so it appears on `in_play` in
  a *later* frame — or never, if it gets stopped.
- `hand_size` / `deck_size` / `gimmick_blanked` are optional for importers who did not
  record them (the validator warns); the engine always fills them.
- A **`CardRef`** is `{card: "<db_uuid>", name?, number?}`. `card` is the join key
  against the card DB; `name`/`number` are display hints the engine always fills. An
  importer who cannot identify a card may use `""` (validator warning).

### Action vocabulary

Same type names as the game-log events (`gamelog.schema.json`), projected to what a
spectator could see. Log events an observer could *not* see are **dropped**, not
redacted: `decision` (its `legal` list enumerates the deciding player's hand) and
`unsupported` (an engine diagnostic). The one exception is a passed turn — a
`turn_action` decision whose choice was `pass` projects to `pass`, seat only.

| `type` | fields | notes |
|---|---|---|
| `start` | — | frame 0 only |
| `roll` | `player, skill, base, value, mods[]` | `mods` = `{src, delta}` |
| `play` | `player, card, order, atk_type` | `order`: Lead \| Followup \| Finish \| None |
| `stop` | `player, card, stopped, reason?` | `player` is the seat playing the stop |
| `pass` | `player` | the active seat passed; the card it recycles arrives as a separate `bury` |
| `turn_result` | `winner, tie_bumps` | the roll-off |
| `draw` | `player, count` | deck→hand is private both ends: **count only** |
| `discard` \| `bury` \| `search` | `player, count, cards[]?, from?` | `cards` present only when the move was publicly visible |
| `finish_attempt` | `player, finish, value, crowd_meter, auto_success` | |
| `breakout` | `defender, broke_out, rolls[]` | `rolls` = `{skill, value, penalty, success}` |
| `crowd_meter` | `delta, value` | |
| `effect` | `src, action, target?` | `src`/`target` are seats; `action` is the IR action name. The engine's `detail` payload is deliberately dropped (bookkeeping, and it can name hidden cards) |
| `note` | `text` | free text; **never emitted by the engine** — it exists so an importer never has to distort a real match |
| `result` | `winner, reason, turns` | final frame |

## Importing an observed match

Write the JSON yourself (there is no authoring tool by design), then gate it:

```bash
srg validate-record archive.json                      # structure
srg validate-record archive.json --cards cards.yaml   # + resolve every card uuid
```

or in the browser, `validate_record(json)` → `{"errors":[…],"warnings":[…]}`. Errors
reject the archive; warnings are advisory. What the validator checks:

- `schema_version` is supported; seats `A`/`B` are present; `kind` is consistent
  (**an observer record carrying a `replay` seed is an error** — an observed match
  cannot be re-derived);
- frames are non-empty, `seq` dense and 0-based, `turn_no` never going backwards, the
  last frame a `result` that agrees with `result`;
- seat keys on every action and every per-frame player map;
- non-negative counts; `winner` ∈ `A|B|draw`.

It is structural. It does **not** re-derive the rules, so it cannot tell you whether an
imported match was *played* legally — only whether the archive is well-formed.

[`fixtures/records/observer_example.json`](../../fixtures/records/observer_example.json)
is a complete, minimal, hand-authored observer archive (two turns, real card uuids) —
start from it.

## Producing and replaying engine records

```bash
srg record decks/bull.yaml decks/fae.yaml --out rec.json --seed 7 \
  --policy-a heuristic --policy-b smart --source "get-diced.com Run It Back"
```

In the browser (`web/src/pkg`):

```js
const s = WasmSession.open(deckA, deckB, seats, BigInt(seed));
// … play …
JSON.parse(s.frames())                 // every frame so far, in order
JSON.parse(s.frames_from(n))           // incremental tail (the sequence is re-derived each step)
s.frame_count()
JSON.parse(s.record("Run It Back"))    // the finished match as a full record
```

Two ways to scrub a `full` record:

1. **Frames** — walk `frames` forward and back. No engine involved; works for both
   kinds; this is what a replay viewer should do.
2. **Re-simulation** — `WasmSession.restore(snapshot)` over the record's `replay`.
   Trimming the last *k* answers from `decisions.<seat>` rewinds exactly *k* decisions,
   and re-submitting them walks forward through the identical ordered `Step`s (guarded
   by `tests/record.rs::truncated_snapshot_rewinds_to_the_same_steps`). Use this when
   you want live engine state — legal moves, alternate lines — not just playback. With
   more than one remote seat, trim per seat: the snapshot stores answers per seat, not
   as one interleaved list.

## Versioning

`schema_version` is `1` and mirrors `record::RECORD_SCHEMA_VERSION`, which is reported
in `srg info` / WASM `version()` under `schemas.match_record` (guarded by
`tests/schema_version.rs`). Assert it on import. Additive, back-compatible fields will
not bump it; any change that could break a reader will.
