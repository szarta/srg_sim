# Decision-protocol wire contract (v1)

Frontend integration reference for the **Run It Back** play screen. This documents
every `Step`, decision `point` type, `legal[]` option shape, and the
`observable_state` projection the engine surfaces. Authoritative source:
`src/engine.rs` (`Step::to_json`, `Engine::decide`, the option builders) and
`src/state.rs` (`GameState::observable`). Pinned alongside
[`observable_state.schema.json`](observable_state.schema.json).

For **storing, publishing, and replaying** a match — including importing one the
engine did not run — see [`match_record.md`](match_record.md), the record/frame
interchange format.

The engine version stamp is `srg info` (CLI) / `version()` (WASM), e.g.:

```json
{ "engine": "0.1.0", "commit": "5de3c23",
  "schemas": { "effect_ir": 70, "game_log": 1, "observable_state": 1, "match_record": 1 },
  "policies": ["random","heuristic","aggressive","smart","newbie"] }
```

**No-skew check:** assert the **schema versions** match between the backend `srg`
binary and the vendored `web/src/pkg` — those are the hard compatibility contract
(the enriched-deck shape is driven by `effect_ir`). `commit` is provenance for a
matched (binary, pkg) pair produced by one `invoke release-web` run.

## Step

`WasmSession.step()/submit()` return one of:

```jsonc
// awaiting a choice
{ "kind": "decision", "request": {
    "request_id": "<turn_no>:<decision_index>",  // echo back on submit (idempotent)
    "seq": <int>,                                 // monotonic decision counter
    "viewer": "A" | "B",                          // the deciding seat
    "point": "<point string>",                    // see catalog below
    "legal": [ /* option objects */ ],            // submit the INDEX of the pick
    "observable_state": { /* see below */ }
} }

// match over
{ "kind": "done", "result": {
    "winner": "A" | "B" | "draw",
    "reason": "finish" | "count_out" | "disqualification" | "pinfall" | "turn_cap",
    "turns": <int>
} }
```

The client submits the **array index** into `legal[]` (`WasmSession.submit(i)`), not
the option object. A single-option decision is auto-resolved by the engine and never
surfaces. Every option object carries a `"kind"` discriminator — but `kind` is not
unique across points and can even differ within one point, so **branch on `point`
first, then `kind`, and drive selection by index.**

## Shared card-option shapes

Three builders produce the card options (`src/engine.rs`):

```jsonc
// play-style (in-play / hand playables)
{ "kind": "play",    "number": <i64>, "card": "<db_uuid>", "order": "<PlayOrder>", "atk_type": "<AtkType>" }
// stop-window candidate
{ "kind": "stop",    "number": <i64>, "card": "<db_uuid>", "order": "<PlayOrder>", "atk_type": "<AtkType>" }
// discard/bury/pile pick (NO atk_type)
{ "kind": "discard", "number": <i64>, "card": "<db_uuid>", "order": "<PlayOrder>" }
```

- `number` = the 1–30 main-deck card number. `card` = the card's `db_uuid`.
- `order` ∈ `Lead | Followup | Finish | None`; `atk_type` ∈ `Strike | Grapple | Submission | None` (PascalCase).
- **Card options carry no card name.** To render a readable label, join `card`
  (uuid) against the full card objects in `observable_state.players[*].in_play` /
  `discard` / own `hand` (each has a `name`), or an external card DB.
- Cross-board options add an `"owner": "A"|"B"` key telling you whose pile the uuid
  is in (marked below).

## Point catalog

| `point` | Player is choosing | `legal[]` element shape | Ready label? |
|---|---|---|---|
| `turn_action` | Which card to play this turn, or pass | `play`-option per playable, then `{"kind":"pass"}` | no (number/uuid/order/atk_type) |
| `stop` | A stopper vs. the just-played attack, or decline | `{"kind":"none","vs_order":"<PlayOrder>","vs_type":"<AtkType>"}` first, then `stop`-option per candidate | no |
| `optional` | Accept/decline a "you may" effect | `[{"kind":"yes","clause":"<rules text>"},{"kind":"no","clause":...}]` — or, from bare yes/no windows, `[{"kind":"yes"},{"kind":"no"}]` (no `clause`) | **yes** (`clause` when present) |
| `optional_swap` | Accept/decline a hand↔discard swap grant | `[{"kind":"yes","clause":"switch a hand card with a discard card"},{"kind":"no",...}]` | **yes** (`clause`) |
| `elect_bump` | Elect the same-skill roll bump, or not | `[{"kind":"yes","point":"elect_bump","losing":<bool>},{"kind":"no",...}]` | hint only (`losing`) |
| `choice` | One branch of a "Choose 1: …" effect | `{"kind":"choice","index":<usize>,"label":"<branch text>"}` per branch | **yes** (`label`) |
| `name` | Bind one of several literal names (Raven) | `{"kind":"name","name":"<string>"}` per option | **yes** (`name`) |
| `mulligan` | First-turn redraw offer | `[{"kind":"redraw"},{"kind":"keep"}]` | no |
| `mulligan_draw` | How many cards to redraw (up to N) | `{"kind":"draw","n":<usize>}` per count, N→0 | **yes** (`n`) |
| `mulligan_bury` | Next card to place at deck bottom (one at a time) | `discard`-option per remaining card | no |
| `target` | A card to move (recur / return / search / swap / discard-from-play) | `discard`-option per pile card; via `pick_optional_from` a trailing `{"kind":"none"}`. The in-play-discard site (`decide("target")`) yields `play`-options **+ `owner`** | no |
| `return_to_hand` | An in-play card to bounce to hand (either board) | `play`-option **+ `owner`** per in-play card | no |
| `bury` | (do_pass) recycle a discard card to deck bottom — `play`-options; **or** (bury_from_discard) bury from a discard pile — `discard`-options **+ `owner`** | see note (two shapes) | no |
| `discard` | A hand card to discard (own hand) | `discard`-option per hand card | no |
| `discard_opp_hand` | A card in the **opponent's** hand to force-discard | `discard`-option (same wire shape as `discard`) | no |
| `bury_hand` | A card from your own hand to deck bottom | `discard`-option | no |
| `bury_opp_hand` | A card from the opponent's hand to deck bottom | `discard`-option | no |
| `reshuffle_target` | Which seat's discard-into-deck reshuffle to trigger | `[{"kind":"seat","seat":"A"},{"kind":"seat","seat":"B"}]` | seat token |
| `reroll_target` | Whose roll to re-roll (Grim Librarian) | `[{"kind":"reroll_target","target":"OPP"},{"kind":"reroll_target","target":"SELF"}]` | side token |

Notes:
- `target` and `bury` each have **two `legal` shapes** depending on the effect that
  raised them; detect by inspecting `kind` (`discard` vs `play`) and the presence of
  `owner`. Match on index regardless.
- `optional`-family options: the decision is "yes" iff the chosen option's `kind` is
  `"yes"`. `stop`/`mulligan` similarly gate on `kind == "none"` / `"redraw"`.
- There is **no** standalone `skill` or `order` decision point; card ordering during a
  mulligan is `mulligan_bury`, and `order` appears only as a field inside card options.

## observable_state

Per-viewer projection (`GameState::observable`); pinned in
[`observable_state.schema.json`](observable_state.schema.json). Lossy: seed, RNG,
`flags`, and hidden zones are excluded.

```jsonc
{
  "schema_version": 1,
  "viewer": "A",                 // seat this projection is scoped to
  "crowd_meter": <int>,
  "active": "A",                 // seat whose turn it is
  "turn_no": <int>,
  "players": {
    "A": {
      "competitor": { "db_uuid","name","division","stats":{Power,Agility,Technique,Submission,Grapple,Strike}, ... },
      "entrance":   { "db_uuid","name", ... },
      "in_play":    [ <Card>, ... ],   // public
      "discard":    [ <Card>, ... ],   // public
      "gimmick_blanked": <bool>,
      "deck_size":  <int>,             // count only; order hidden
      "hand":       [ <Card>, ... ]    // OWN seat, or a Peek-revealed opponent
      // "hand_size": <int>            // instead of "hand", for a hidden opponent
    },
    "B": { ... }
  }
}
```

Exactly one of `hand` (own seat / Peek-revealed) or `hand_size` (hidden opponent) is
present per player. A `<Card>` is `{ db_uuid, name, number, atk_type, play_order,
finish_bonuses, tags, raw_text, effects }` (`name` is the human title — the join key
for card-option labels).
