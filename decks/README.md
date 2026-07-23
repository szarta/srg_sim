# Decklists (yaml). Format in DESIGN.md §2.

Each file is one side: a `competitor`, an `entrance`, and exactly 30 numbered
main-deck cards (`{number, name}`), resolved against the card DB at load. See
`bull.yaml` / `warehouse.yaml` for fully-modeled examples.

## Adding and testing a new deck

Drop in `decks/<name>.yaml`, then **audit it** — the one-command deck-testing
harness that reports modeling gaps and shakes out crashes over many games:

```bash
srg audit decks/<name>.yaml decks/bull.yaml --games 30
```

It prints, for the matchup:

- **deck coverage** — each deck's *unmodeled* clauses (cards whose rules text
  the parser leaves `Unsupported`). Model these with grammar (recurring shapes,
  DB-wide) or an override in `overrides.yaml` (bespoke, keyed by `db_uuid`); a
  card modeled either way is covered in **every** deck that uses it.
- **playtest** — N seeded games, reporting decisive vs turn-cap endings, any
  **crashed** seeds (isolated per game), and the **runtime `Unsupported` no-ops**
  that actually fired in play (with counts) — so you see which gaps change play
  versus which are inert (e.g. match-stipulation clauses that never fire in a
  standard match).

A clean deck reads `0 unmodeled clause(s)`, all games `decisive`, `crashed: 0`,
and `runtime Unsupported no-ops: none`.

### Banking a regression golden

Once a matchup is modeled and stable, capture a game into the conformance corpus
so any future change that alters its behavior fails the byte-for-byte replay
(`tests/engine_conformance.rs` + `tests/session.rs`):

```bash
srg audit decks/<a>.yaml decks/<b>.yaml --games 10 \
  --capture fixtures/conformance/00N_<label>.json
```

The captured fixture (decks + per-player decisions + canonical log) is picked up
automatically by the directory-scanning replay tests — run `invoke check` to
confirm it replays.
