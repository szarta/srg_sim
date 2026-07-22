# Coverage-tail audit (task #117)

**Date:** 2026-07-22 · **Snapshot:** `fixtures/parser/cards.ir.json` (6386 records) +
`overrides.ir.json` (186 entries) · **Baseline:** main-deck parser coverage **56.7%**
(5,304 unsupported clauses; 12,256 total).

Top-96 **competitors** are 100% modeled. This audit targets the remaining gap — the full
card pool — so we grind the mechanics real decks actually use instead of brute-forcing
4,700+ distinct clause shapes.

## Method

Every `Unsupported` node in the frozen parser corpus carries its `raw_text` at clause
granularity. We normalize each to a *shape* (digits → `N`, skill names → `<S>` — the same
normalization `srg coverage` uses), then classify by shared mechanic (first-match-wins
keyword rules) and tally by card type. **7,062** unsupported clauses total
(main 5,305 / competitor 1,450 / entrance 307); only **16** are sub-nodes inside
already-overridden cards (deeper fidelity) — the rest are the true coverage gap.

**Caveat:** counts are *DB frequency* (how many cards print a mechanic), a proxy for how
often it's encountered — not competitive-play frequency. If get-diced exposes deck/usage
data later, re-weight by real play. The 2,277-clause long tail (1,613 one-off shapes) is
genuinely miscellaneous — diminishing returns.

## Mechanic buckets (ranked; cum% of all 7,062)

| # | Mechanic bucket | Clauses | main / comp / ent | Cum% | Task |
|---|---|--:|---|--:|---|
| 1 | **Hand disruption** — bury/discard/peek opponent's hand | **1,451** | 1130 / 301 / 20 | 21% | **#39** (P5→bump) |
| 2 | Draw riders (conditional/per-count draws) | 416 | 252 / 141 / 23 | 26% | #49 |
| 3 | Flip cards | 353 | 272 / 73 / 8 | 31% | *new* |
| 4 | Crowd Meter (value / gate / +N) | 338 | 283 / 48 / 7 | 36% | #97 |
| 5 | Stop-eligibility mods (cannot-be-stopped / stop-any) | 312 | 295 / 13 / 4 | 40% | *new* |
| 6 | In-discard (`this card in your discard pile:` …) | 283 | 282 / 1 / 0 | 44% | #115 |
| 7 | In-play removal (discard/choose/bury opp in play) | 252 | 211 / 38 / 3 | 48% | *new* |
| 8 | Finish-roll riders | 210 | 190 / 17 / 3 | 51% | #49 |
| 9 | Recur from discard (add/shuffle → hand/deck) | 204 | 164 / 34 / 6 | 54% | *new* |
| 10 | Timing/trigger block headers (`During your turn:` …) | 192 | 34 / 119 / 39 | 57% | #81-ish |
| 11 | Match-type stipulation (Steel Cage / Triad / Liger's Den) | 130 | 124 / 1 / 5 | 58% | #97 |
| 12 | Reveal hand/deck (generic) | 104 | 71 / 25 / 8 | 60% | *new* |
| 13 | Deck manip (top/bottom draw/discard/reveal) | 98 | 71 / 26 / 1 | 61% | *new* |
| 14 | Recur/shuffle deck (generic) | 96 | 73 / 13 / 10 | 62% | *new* |
| 15 | Skill-conditional buff (`<S> skill +N`, FU/Finish gate) | 90 | 72 / 16 / 2 | 63% | *new* |
| 16 | Also-Follow-Up (rolled-skill / crowd-meter) | 88 | 87 / 1 / 0 | 65% | *new* |
| 17 | Gimmick blank | 61 | 50 / 11 / 0 | 65% | #94/new |
| 18 | Reveal-if-named (Training With / Barbed Wire / …) | 52 | 43 / 6 / 3 | 66% | *new* |
| 19 | DQ-cause / lose-via-DQ | 36 | 33 / 3 / 0 | 66% | #94 |
| 20 | Spotlight family | 19 | 10 / 6 / 3 | 67% | #109–116 |
| — | **Long tail** (1,613 distinct one-off shapes) | 2,277 | — | 100% | defer |

## Findings

1. **Hand disruption is the single biggest lever by far — 1,451 clauses (~21%), 1,130
   main-deck** — yet it's task **#39 at P5**. This is mispriced; it should lead the grind.
   (Related: `#39` opponent hand-bury is a subset; the bucket also covers hand-discard and
   hand-peek.)
2. **Pareto holds:** the top **6** buckets ≈ **44%** of the gap; the top **10** ≈ **57%**;
   the top **~16** ≈ **65%**. Knocking out ~8–10 mechanic families would plausibly lift
   main-deck coverage from 56.7% toward ~80%. The remaining 2,277-clause tail is 1,613
   distinct one-offs — low ROI, defer.
3. **Big buckets with no task yet** (`new`): Flip (353), Stop-eligibility (312), In-play
   removal (252), Recur-from-discard (204), Reveal (104), Deck-manip (98), Also-Follow-Up
   (88). These, not the Spotlight cluster (only 19 clauses), are where the untracked volume
   is. Spotlight is a *thin* gap by count — its priority is about specific high-value cards
   (#110 text-copy), not raw coverage.
4. **The "riders" tasks (#49)** are actually two large buckets — Draw riders (416) +
   Finish-roll riders (210) = **626 clauses**. Bigger than it reads.
5. **Crowd Meter (#97)** is larger than expected — CM value/gate (338) + match-type
   stipulations (130) = **468 clauses**. The GM Calace swap covered the *entry* mechanic;
   the bulk is main-deck cards that *read* the meter / gate on match type.
6. **Timing/trigger headers (192, mostly competitor)** are a *structural* gap — multi-line
   trigger blocks (`During your turn:` / `Once per turn roll:`) whose following clause is
   the real effect. Relates to the timing spec (#81); may unlock competitor clauses cheaply.

## Recommended roadmap (grind order)

By leverage (clause count × how self-contained the mechanic is):

1. **Hand disruption (#39)** — biggest lever; mostly a family of `Bury/Discard{who:OPP,
   from:Hand, random?}` + hand-peek. Bump to **P2**.
2. **Draw + Finish-roll riders (#49)** — 626 clauses; largely extending existing
   `Draw`/`FinishRollBonus` with per-count/cap/conditions already half-present.
3. **In-play removal + Recur-from-discard** (*new*, 456) — targeted board/discard
   manipulation; overlaps existing `RemoveFromPlay` / `AddFromDiscard` nodes.
4. **Flip (353)** + **Stop-eligibility (312)** (*new*) — self-contained families.
5. **Crowd Meter reads + match-type gates (#97, 468)** — now that the swap subsystem exists.
6. **In-discard (#115, 283)** — one infra lever (`WHILE_IN_DISCARD` scan) unlocks ~282.
7. Defer the long tail and the thin Spotlight-by-count cluster (chase #110 text-copy only
   for its specific high-value cards).

## Proposed tracker actions

- **Bump #39 → P2**, and broaden its title/scope to "hand disruption" (bury + discard +
  peek), not just bury.
- **Split/retitle #49** to name the two rider buckets explicitly (Draw riders; Finish-roll
  riders), ~626 clauses.
- **Create tasks** for the untracked big buckets: Flip, Stop-eligibility, In-play removal,
  Recur-from-discard.
- **Re-scope #97** to include the *reader/gate* side (CM value gates + match-type gates),
  not just the swap.
- Leave Spotlight (#109–116) where it is — value is card-specific, not volume.
