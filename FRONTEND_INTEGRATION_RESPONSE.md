# srg_sim — response to the Run It Back frontend integration brief

Landed in commit `da736ee` (task #124). Everything below is on `main`, `invoke
check` green.

## Version string

`srg info` (CLI) and `version()` (WASM) both emit:

```json
{ "engine": "0.1.0", "commit": "<git short hash>",
  "schemas": { "effect_ir": 69, "game_log": 1, "observable_state": 1 },
  "policies": ["random","heuristic","aggressive","smart","newbie"] }
```

**No-skew check:** assert the three `schemas` versions match between the backend
`srg` binary and the vendored `web/src/pkg`. Those are the hard contract (the
enriched-`Deck` shape is driven by `effect_ir`). `commit` is provenance for a
matched (binary, pkg) pair — see the build command. A committed `web/src/pkg`
carries the stamp of the commit it was built *from* (its parent), so don't
diff `commit` for equality across the two artifacts; diff the schema versions.

## Build / refresh command

```bash
invoke release-web
```

Builds the `srg` **release binary** (`target/release/srg`) **and** the WASM pkg
(`web/src/pkg`) from the same working tree, then prints the stamp. Commit the
refreshed `web/src/pkg` and deploy *that* `target/release/srg` (the two are a
matched pair). `invoke wasm` alone still rebuilds just the pkg.

`web/src/pkg` is now committed (previously git-ignored) so you can vendor it with
no Rust toolchain. Refreshed `web/src/sample/deck{A,B}.json` (Bull/Fae) to the
current schema — they double as the no-panic test fixtures, so they can't silently
drift.

## Deliverables checklist

- **P1.1 no-panic** — `tests/frontend_no_panic.rs` plays Bull vs Fae from
  `open`→`done` across 40 remote-driven seeds (the exact `submit()` wire path,
  varied choices to walk many `legal[]` branches) + 60 local-policy seeds (incl.
  `random`). Asserts no panic + terminal result. All green.
- **P1.2 version** — above.
- **P1.3 committed pkg** — done.
- **P2.4 self-describing decisions** — documented (labels not added; the doc lets
  you map buttons): `schemas/v1/decision_protocol.md` catalogs `Step`, every
  decision `point`, each `legal[]` option shape, and `observable_state`. Note:
  card-target options carry `number` + `card` (uuid) but **no card name** — join
  the uuid against the full card objects in `observable_state.players[*].in_play`/
  `discard`/`hand` (each has `name`) to render a label.
- **P2.5 observable_state schema** — pinned in
  `schemas/v1/observable_state.schema.json`; the projection now carries
  `schema_version`.
- **P2.6 policies** — `policies()` (WASM) and the `policies` field of `srg info`.

## Test-deck cards with gameplay-affecting Unsupported clauses

Matches complete correctly (turn rolls, card play, stops, breakouts, finishes,
count-out all work), but with ~63% main-deck clause coverage many card *riders*
silently no-op. For Bull vs Fae specifically: **26 of the numbered main-deck cards
carry at least one Unsupported clause.** Of those clauses, only **4 are inert** in
a standard (no-stipulation) match — the `"If this is a <Ring of Fire/Steel Cage/
Liger's Den/Lumberjack> match…"` conditionals and the stipulation-gated entrance —
so they'd no-op even at full fidelity. The other **28 clauses do change play** and
are silently dropped. Highest-impact examples to warn on:

- **Card selection / advantage:** #23 *2 Birds, 1 Stone* (search 3 Grapples +
  distribute), #26 *Alpaca Slam* (tutor a Grapple to top), #22 *360 Lariat* (recur
  2 + draw 2), #28 *Cake Smash* (reveal 7, add 3) / *9th Rule of Villainy!* (mill
  the opponent's top 6), #7 *Back Elbow*, #16 *A Diminished Flock*, #27 *A Small
  Fortune*.
- **Crowd-meter (engine feature not yet built, task #97):** #24 *$1,000,000
  Dreamer* (Crowd Meter +1; draw = Crowd Meter).
- **Choice / "or" clauses:** #1 *American Double Punch*, #5 *American Triple Skull
  Slam* ("add bottom card to hand **or** stop …"), #4 *A Boss Photobomb*.
- **Conditional buffs / combo riders:** #11 *2 Handed Slam*, #12 *1/4 Twist* ("if
  you have <named card> in play: +1 …"), #29 *Pig Tail Swing* / *The Thorn Forest*
  ("if stopped …"), #30 *Curse To Sleep*.

Regenerate the full list anytime from the enriched decks:

```bash
python3 - <<'PY'
import json
for p in ['web/src/sample/deckA.json','web/src/sample/deckB.json']:
    d=json.load(open(p)); acc=[]
    def w(n):
        if isinstance(n,dict):
            if n.get('@type')=='Unsupported': acc.append(n['raw_text'])
            [w(v) for v in n.values()]
        elif isinstance(n,list): [w(v) for v in n]
    for c in d['cards']: 
        acc=[]; w(c.get('effects',[]))
        for cl in acc: print(f"#{c['number']:>2} {c['name']}: {cl}")
PY
```

Raising coverage for these is deferred (brief item 7). None of them crash — the
no-panic test exercises the branches that reach them; they degrade to no-ops. If you
want an in-UI "partial rules" badge, the presence of any `Unsupported` node in a
card's `effects` (visible in `observable_state`) is the flag to key off.
