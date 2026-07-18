# srg_sim — Design (review gate)

This document is the **review artifact** before the engine is implemented. It pins the two
expensive-to-change decisions — the **Effect IR** and the **game-log schema** — plus the
phase-0 scope, module layout, and turn loop. Nothing in `srg_sim/` has engine logic yet.

Read alongside the authoritative sources in [`README.md`](README.md). The finish/breakout
math and skill-stop logic are **ported verbatim** from the validated `fae_comp` modules; we
do not re-derive them.

---

## 1. Scope — phase 0

A **game** is a singles match: each side is `{1 SingleCompetitorCard, 30-card main deck,
1 Entrance card}`. The simulator takes **two decklists** and plays them; card-pool *formats*
(Worlds / Hardcore / Old School / Super Lucha) only restrict deck *building* and are
therefore irrelevant to the engine.

**In scope**
- SingleCompetitor games (one competitor per side).
- 30-card main deck (one printing each of `deck_card_number` 1–30).
- One **Entrance card** per side, declared at start; its effect is modeled.
- **Standard crowd meter** baseline: starts 0; adds +N to the finish roll at level N; +1 on
  each breakout.
- The full turn duel, card ordering chain, stops (RPS + skill-stops), finish + breakout.

**Out of scope (phase 0)** — each represented as an explicit `Unsupported`/ignored flag in
the log, never silently modeled:
- Trio and Tornado competitors (separate formats).
- Tag games (multiple competitors per side) and card text keyed to tag play.
- Spectacle cards (one-per-game Newman/Valiant pick).
- CrowdMeter **card types** and their rule modifications (max handsize, no-DQ, count-outs…).
- Deck-build legality (format pools; the `skill-requirement cards ≤ 2 / deck` rule) — handled
  later by an optional offline validator, not the engine.

**Priority signal.** Competitor cards carry a `division` field. **Worlds** (top 64) +
**Underworld** (next 32) = the **top-96** most-played/competitive comps, set quarterly and
mostly stable. Gimmick (`rules_text`) coverage and validation are prioritized on these.

---

## 2. Domain model (`cards.py`)

Immutable, hashable, serializable.

- `AtkType` = {Strike, Grapple, Submission, None}. RPS: Strike ▷ Grapple ▷ Submission ▷ Strike.
- `PlayOrder` = {Lead, Followup, Finish, None}.
- `Card`: `db_uuid, name, number (1–30 for main deck), atk_type, play_order,
  finish_bonuses:{skill:int}, tags:[str], raw_text:str, effects:[Effect]`.
  - `type_from_number(n) = [Submission, Strike, Grapple][n % 3]` (n≡1 Strike, 2 Grapple, 0 Sub).
    (Cross-checked against `atk_type` at load; mismatches logged.)
- `Competitor`: `name, division, stats:{6 skills}, gimmick_text, effects:[Effect],
  related_finishes:[db_uuid]`.
- `EntranceCard`: `name, raw_text, effects:[Effect]` (play_order None, no atk_type).
- `Deck`: `competitor, entrance, cards:[Card]` (exactly 30) + integrity checks
  (`len==30`, numbers present). Format legality is **not** enforced here.

**Card data source.** The **source of authority** is the PostgreSQL database
behind the SRG card-search website/app (`~/data/srg_card_search_website/backend/app`,
`postgresql://…@localhost/srg_cards`), updated often. `loader.py` consumes the
read-only YAML export (`backend/app/cards.yaml`) regenerated from it. Card data
is **not vendored** here; the assumption is every user has that repo + DB access.

**Decklist file** (`decks/*.yaml`) — the sim's input:
```yaml
competitor: "The Bull"          # resolved by name (+ optional variant/version)
entrance: "Calling in Kanik"
cards:                          # 30 entries; each a db_uuid OR name (+set to disambiguate)
  - {number: 1, name: "American Double Punch"}
  - {number: 2, db_uuid: "..."}
  # ...
```

---

## 3. Effect IR (`effects.py`) — the linchpin

Cards, competitor gimmicks, and Entrance effects **all** compile to one typed IR. The engine
executes **only IR**, never raw text. An `Effect` is a `(trigger, condition, actions[])`
triple. Everything below is `@dataclass(frozen=True)` and round-trips to JSON.

**Trigger** — *when* an effect fires:
```
OnPlay                       # when this card is played
OnRoll(skill?, who=SELF)     # after `who` makes a turn roll (skill=None => any skill);
                             #   outcome-agnostic — roll-value gimmicks (Bull) live here
OnWinTurn / OnLoseTurn(by=?) # after the turn roll resolves (outcome-specific)
OnStop(dir=YOURS|THEIRS)     # when a stop happens
OnHit(atk_type?, name_contains?, text_contains?)  # when a matching card RESOLVES into play (schema v6)
                             #   gimmicks gate on the hit card's attack type and/or its
                             #   title/text ("when you hit a card with 'X' in the name",
                             #   case-insensitive OR-substring); see "hit" below
OnBump                       # when the owner bumps (a tied roll: both draw + re-roll).
                             #   Both sides bump on a tie, so each owner's OnBump fires;
                             #   bump-punish gimmicks (Mastermind: "opp next roll -2") live
                             #   here. Gate repeats with a once-per-turn frequency.
StartOfTurn / StartOfMatch
Static                       # always-on passive (e.g. "+1 to Power"); duration-scoped, see below
```
**"Hit" = a card resolving into play.** A card is hit either (a) when you play it and it is
*not* stopped, or (b) when a **stop** you play resolves into play (your stop is itself "hit").
`OnHit` fires on both.

Frequency guards: `once_per_turn`, `once_per_match`, `n_times_per_match(k)`.

**Duration** — how long a `Static`/buff effect stays active (a first-class field on Effect):
```
WHILE_IN_PLAY        # card-sourced buffs: active while the SOURCE CARD is in play.
                     #   Finishes buff this way -> on breakout all in-play cards are
                     #   discarded, so their buffs end automatically.
WHILE_GIMMICK_ACTIVE # competitor-gimmick buffs (e.g. Tytan +1 Power): active while the
                     #   competitor's Gimmick is NOT blanked.
INSTANT              # one-shot mutation (draw, bury, ±roll), no lasting state.
```
**Gimmick blanking** is itself `WHILE_IN_PLAY`: a blanker card sets `gimmick_blanked` on the
target while the blanker is in play; when the blanker leaves play the Gimmick un-blanks and
its buffs return. Blanked gimmicks contribute no effects (incl. no `Static` buffs) while blanked.

**Condition** — a predicate on `GameState` (composable via And/Or/Not):
```
SkillCompare(skill, who=SELF, cmp=>|>=|=|<, vs=OPP_SAME|VALUE, value?, vs_skill?)  # vs_skill: compare to a DIFFERENT opponent skill ("your Strike > opp Agility")
HandSizeCompare(cmp, vs=OPP|VALUE, value?)
CrowdMeterCompare(cmp, value)
HasInPlay(who, filter, count=1, cmp=>=) / HasInDiscard(...)
RollWasSkill(skill) / RollGapExactly(k) / RollGapAtLeast(k)   # gap = opp - self, positive = self rolled lower
RollLeadAtLeast(k)           # self rolled >= k HIGHER than opp (gap <= -k) — mirror of RollGapAtLeast (YamatoHama). schema v2.
RollValue(cmp, value)        # the actual number rolled this turn, read via the trigger's `who` (Mrs. Apocalypse, Numer01)
OppWonLastRoll               # the opponent won the PREVIOUS turn's roll-off (GameState.last_roll_winner); false on turn 1 (Dunn re-roll). schema v3.
Always
```

**Action** — the *what* (mutations); each names a `target` (SELF/OPP/a card/skill):
```
Draw(n, from=TOP|BOTTOM, who, per?, per_who=SELF)  Bury(selector, count)   Discard(selector, count, who, per?, per_who=SELF)
Flip(n, who=SELF)             Search(filter, dest=HAND|DISCARD, count=1)  ShuffleIntoDeck(selector)
                              # dest=DISCARD: "search your deck for up to `count` cards, put them in
                              # discard" — owner chooses which/how many (a `search` decision), then shuffles
                              # Draw/Discard `per`: n/count scales by the count of `per` cards in play,
                              # exactly like ModifyRoll (authored OnPlay for "for each OTHER … in play")
ShuffleDeck(who)              # shuffle a whole deck ("Shuffle your deck")
AddFromDiscard(filter)        RemoveFromPlay(selector, who=OPP, count=1)  # board disruption -> discard
RecurToDeckTop(selector, count=1)  # "up to N" discard -> TOP of deck (redraw next turn)
RevealAndDiscard(count, who=OPP)   # reveal `count` random cards, discard the Stops among them (0..count)
CountsAsInPlay(selector, count=2)  # Static self-decl: this card counts as `count` cards matching `selector`
ModifyRoll(who, delta, when=THIS|NEXT, per?, per_who=OPP)  # delta scales by count of `per` cards in play
BuffSkill(skill, delta, who, duration=WHILE_IN_PLAY, target_highest?, per_crowd?, cap?, per?, per_zone=IN_PLAY)
                                                 # per=CardFilter -> bonus = delta * (count of the target's cards
                                                 # in per_zone {IN_PLAY|DISCARD} matching per), clamped to cap
                                                 # ("+1 for each card in play with 'Chin' in the name, Max +3"); schema v7
MaxHandSize(delta, who, duration=WHILE_IN_PLAY)  # Static: signed cap modifier, folds into the derived hand cap
Reroll(who, once=True)        WinTie(who)                   Bump(who)
ElectBumpOnSameSkill(uses=2)  # Static roll-off grant: owner MAY bump on a same-skill roll, N times/match
Stop(order?, atk_type?, source_is_skillreq?)   BlankGimmick(who, duration=WHILE_IN_PLAY)
Unstoppable(by_order?)        # Static self-decl: cannot be stopped by stops of `by_order` (None = anything)
AlsoLead(condition)           # Static self-decl: also playable as a Lead while `condition` holds
BlankText(card, until=END_OF_TURN)             LoseBy(kind=DISQUALIFICATION|PINFALL, who)
CrowdMeter(delta)             PlayExtraCard(order?)         SetFinishRoll(value, condition)
FinishBonus(skill, delta)     BreakoutModifier(delta, attempts?)
FinishRollBonus(delta, when_skill?, either=False)  # +delta to a Finish roll; when_skill gates on the rolled skill
DoubleFinishIfBumped          # Static self-decl: double THIS card's Finish bonuses if the finisher bumped
LowestRollWins                # Static marker (Fae): the roll-off is won by the lowest roll
```
`Bury(selector, count, who, random, source)` moves `count` cards to the **bottom of the
deck** (schema v4). `source=DISCARD` (default) recycles the top `count` of the **discard
pile** (the pass-and-recycle bury); `source=HAND` is the card-text bury — "bury N cards in
[your/their] hand" — where the **hand owner chooses which** unless `random`. `Flip(n)` moves
the **top `n` cards of the deck to the discard pile** (there is no "buried" zone — see §5). `RemoveFromPlay(selector, who, count)` moves up to `count` cards from
a player's **`in_play` board to their discard** ("Discard 1 card your opponent has in play");
the **acting** player chooses which matching card(s) — an aimed disruption, not random — and a
no-match board is a no-op. `RecurToDeckTop(selector, count)` puts **up to** `count` matching
cards from the **discard pile onto the top of the deck** (the owner picks how many and which);
it is the redraw-next-turn recycle, distinct from `ShuffleIntoDeck` (bottom + reshuffle).
`PlayExtraCard` grants the active player one more turn action this turn (consumed by the turn
loop, reset each turn). `BuffSkill` applies to the **unified derived-stats view** — i.e. it
affects turn rolls, stops, *and* breakout rolls alike; there is no per-context scope, only
`duration`. `MaxHandSize` is the derived-hand-cap analogue of a `Static` `BuffSkill`: it is
read on demand (`GameState.effective_hand_cap` = base + active mods, clamped at 0), never
stored, so raising your own cap or lowering an opponent's folds in and out with the card.
`LoseBy` is how
cards trigger the DQ / pinfall loss conditions (§6). Count-out is engine-driven, not an action.
The **static self-declaration** family — `CountsAsInPlay`, `Unstoppable`, `AlsoLead`,
`DoubleFinishIfBumped` — carries no mutation: each is a `Static` marker the engine *reads
structurally* (in-play counting, the stop check, the playability check, the finish sequence)
and never executes, so it dispatches to a no-op like `LowestRollWins`. `CountsAsInPlay` lifts
every "in play" tally its `selector` *implies* (a Lead-Strike declaration raises the Lead, the
Strike, and the Lead-Strike counts alike), feeding per-count `ModifyRoll`/`Draw`/`Discard`
scaling and `HasInPlay` gates.

**Unsupported sentinel** — any clause the parser can't confidently map:
```
Unsupported(raw_text, reason)      # engine ignores it BUT logs a `unsupported` event
```
So coverage is always measurable and no gimmick is ever silently mis-played.

`Effect = {trigger, condition: Condition = Always, actions: [Action|Unsupported],
raw_clause: str, source: card|gimmick|entrance, optional: bool = False}`. `optional`
marks a "you may" effect: when it would fire, the card controller is offered an
`optional` decision (take it / skip); declining leaves the frequency guard unspent.

**Executor** (in `engine.py`): at each trigger point the engine collects every active
`Effect` whose `trigger` matches and `condition` holds, respecting frequency guards, and
applies its actions in text order. Static effects fold into a `derived-stats` view used by
rolls/stops. Optional effects (reroll, self-buff, "you may…") are surfaced to the **policy**
as choices, not auto-applied.

---

## 4. rules_text → Effect pipeline (`rules_parser.py`)

Data-driven, three layers, tried in order:
1. **Pattern grammar.** A small library of regexes/templates for the recurring shapes:
   `+N to <skill>`, `draw N card(s)`, `bury N`, `when you roll <skill>`, `your (next )?turn
   roll is +N`, `stop any <order?> <type>`, `if your <skill> is greater than your opponent's
   <skill>`, `once (per|a) (turn|match)`, trigger clauses `When … :`. Splits `rules_text`
   into clauses (newlines / sentences) and maps each to `(trigger, condition, actions)`.
2. **Curated override table** (`overrides.yaml`, keyed by `db_uuid`): hand-authored IR for
   cards the grammar can't parse. This is where top-96 gimmicks land first.
3. **`Unsupported(raw_clause, reason)`** for anything left over.

**Non-effect metadata** (e.g. `Skill Requirement: <skill> N+`, a deck-BUILD constraint, not a
match effect) is recognized and skipped like a frequency-guard header — neither compiled to an
effect nor counted as a clause in coverage. A grammar builder may also **decline** (return
None) on a shape it can't faithfully model — e.g. a "stop any … even if it cannot be stopped"
target — so the clause falls through to `Unsupported` rather than dropping the qualifier.

A **coverage report** (`srg-sim coverage`) prints, over the whole DB and over the top-96
subset: % clauses parsed by grammar / by override / unsupported, and the most-common
unparsed phrasings — this drives M3 work. Target: unsupported → 0 across the top-96.

---

## 5. Game state (`state.py`)

There are exactly **five regions** per side: the `competitor`+`entrance` (fixed), and
four card zones — `deck`, `hand`, `discard`, `in_play`. **Visibility:** `discard` and
`in_play` are public; `hand` is private to its owner; `deck` is hidden to everyone (though
as a deck shrinks, remaining hand cards become inferable from public info). Policies never
read hidden zones (opponent `hand`/either `deck`) unless an effect reveals them.

`PlayerState`: `competitor, entrance, hand[], deck[], discard[], in_play[],
pending_roll_mods{this,next}, freq_counters, gimmick_blanked:bool, flags`. `GameState`:
`players[A,B], crowd_meter, active, turn_no, rng, log`. All snapshottable
(`to_dict`/`from_dict`) so any state is reproducible and diffable. `deck` order matters;
shuffles/searches go through the seeded RNG. **The seeded RNG is a portable
`splitmix64`** (identical stream in Python and the Rust engine), not a `random.Random`
wrapper, so cross-engine logs are byte-identical (see the substrate-split doc,
`docs/design/substrate-split.md` §5); this reseeds existing golden logs and touches
neither §3 nor §8. **Bury** = move a card from `discard` to the
**bottom of `deck`**; **Flip** = move the top of `deck` to `discard` (there is no separate
"buried" zone — a buried card lives in the deck).

**Derived stats.** There is no stored `static_buffs`; a player's effective skills are
*computed on demand* = base competitor stats + every active `BuffSkill` whose source is
still present: cards in `in_play` (`WHILE_IN_PLAY`) and the competitor gimmick if
`not gimmick_blanked` (`WHILE_GIMMICK_ACTIVE`). This single derived-stats view feeds turn
rolls, stop checks, and breakout rolls, so buffs/blanks are always consistent and reversible
(a card leaving play or a gimmick being blanked simply drops out of the recomputation).
The **maximum hand size** is derived the same way (`effective_hand_cap` = base 10 + active
`Static` `MaxHandSize` deltas, clamped at 0), so an opponent's cap-lowering card folds in and
out with the same recomputation and is enforced continuously (§6).

---

## 6. Turn loop (`engine.py`) — pseudocode

```
setup: build both decks; apply StartOfMatch effects (incl. Entrance/gimmick);
       shuffle (seeded); each player draws 3 (opening hand) before the first roll.
loop until a player loses or a turn cap:
  # first-turn redraw (per player, ONCE): on the first won turn a player would take
  #   an action, if they have NO Leads in hand they MAY reveal the whole hand
  #   (public), bury it to the bottom of the deck IN AN ORDER THEY CHOOSE, then draw
  #   UP TO that many. Marked spent whether taken or not — a player who bumps/loses
  #   the early rolls still gets it exactly once (NOT a setup step).
  # --- turn roll ---
  rollA = roll(playerA); rollB = roll(playerB)      # roll = uniform skill face -> derived stat
  apply pending_roll_mods, static buffs, OnRoll effects
  if tie: BOTH players bump (each draws 1), then re-roll — until it breaks
          (WinTie / anti-bump can win the tie instead of bumping; reroll via policy)
  winner = higher value (or lower, if a "lowest wins" effect is active)
  fire OnWinTurn/OnLoseTurn effects; decrement/refresh freq guards
  # --- active player's action (plays exactly ONE card, or passes) ---
  active = winner
  if active must draw and active.deck empty and active.hand empty:
      -> active WINS by COUNT-OUT (deck+hand exhausted on a won turn)   # win condition
  active.draw(1)
  action = policy(active).choose_turn_action(legal_actions)   # play 1 card OR pass+bury 1
  # on pass: bury 1 (recycle a discard card to the bottom of the deck; no-op if discard empty)
  if play: ordering chain is ORDER-ONLY vs your OWN persistent in-play board — a Lead is
           always playable (you may stack another), a Follow Up needs a Lead in play, a
           Finish needs a Follow Up in play (type is irrelevant to the chain).
           The played card resolves ("is hit") unless the defender plays ONE valid stop:
           STOPS ARE TEXT-DRIVEN — a hand card can stop iff one of its parsed `Stop` effects
           matches the attack's order/type AND that effect's condition holds (skill stops,
           see-1, crowd-meter gates; §3/§4). The stop, if played, resolves onto the
           defender's in-play board and PERSISTS there (it is itself "hit"); only the
           stopped attack goes to the attacker's discard, and you cannot stop a stop.
           A Follow Up used as a stop enters play EVEN WITH NO LEAD beneath it — stopping
           bypasses the play-sequence gate — so a stop can build board state, arm see-1
           stops, feed combo/finish bonuses, and even enable a later Finish off the FU.
           Resolved cards PERSIST in `in_play` across turns (both sides); a Finish that
           resolves unstopped -> finish sequence. fire OnHit/OnStop effects.
           ORDER IS STRICT: the stop window opens BEFORE any of the attack's own text —
           a STOPPED card fires NONE of its text (no OnPlay, no OnHit). So OnPlay/OnHit
           resolve only for an unstopped attack (OnPlay as it resolves, before it lands
           on the board; OnHit once it is in play). See srg-rules-confirmed.
  any LoseBy(DQ|Pinfall) triggered by a resolved/stopped card ends the game immediately
  # the hand cap is CONTINUOUS (base 10 + Static MaxHandSize mods, per player) — enforced the
  # moment a player exceeds it, never batched: after every draw (turn/bump/effect, todo #28)
  # AND after any board change that lowers a cap. A card entering play that drops the
  # opponent's max forces them to discard down right then, with no draw of their own
  # (_enforce_hand_caps runs both sides after a play resolves). Over-cap sheds by policy
  # choice (§3, todo #28/#37).
finish sequence:
  finisher makes ONE finish roll = derived stat(rolled skill)                     # base + all-roll BuffSkills
                                  + SUM finish_bonus(rolled skill) over the WHOLE  # combo numbers, finish-only,
                                        in-play sequence (Lead + Follow Up + …)    # summed across the combo
                                  + flat FinishRollBonus (any-skill "+N to Finish rolls", finisher only)
                                  + crowd_meter
    # Two distinct "+N" channels: a bare "+N to <skill>" is a per-skill combo bonus (finish-only,
    # via finish_bonuses/bonus_for); "Your <skill> is +N" is a persistent BuffSkill folded into the
    # derived stat (so it also lifts turn + breakout rolls). Do NOT route combo numbers through
    # derived stats — that would inflate turn rolls by the whole board.
  auto-success rule + CM0-10-always rule (ported from supershow.finish_odds semantics)
  defender takes up to 3 breakout rolls (own derived stats, own penalties); success if >= finish value
  any success -> discard ALL in-play on BOTH sides (their WHILE_IN_PLAY buffs end),
                 crowd_meter += 1, the turn ends, play resumes;
  all fail -> defender LOSES by finish
```

The in-play board persists across turns, so the strategic spine is a card-economy war: build a
chain toward a Finish while the defender holds stops to spend on it (a stop is worth more held
in hand than played as a weak attack). Stops are **text-driven per printing** — the 30-card
number-map (§4) is the *typical* pattern, but each card's actual stop ability comes from its
parsed `Stop` effect(s); a card with no Stop effect cannot stop.

**Win/loss conditions** (a `GameResult{winner, reason}`):
- `finish` — defender fails all breakout rolls.
- `count_out` — the **active** player wins a turn and must draw with **both deck and hand
  empty** → that player **wins** (running yourself out on a won turn is a win, not a deck-out loss).
- `disqualification` — a `LoseBy(DISQUALIFICATION)` action fires (e.g. "if this card is
  stopped, you lose by disqualification").
- `pinfall` — a `LoseBy(PINFALL)` action fires (e.g. one of Stung's finishers).

**Ported verbatim** (with their self-checks) into `finish.py` and `stops.py`:
- `finish.py` ← `fae_comp/supershow.py` finish/breakout math (uncapped value; CM0-10-always;
  ≥11-at-CM>0 auto-success; ≥ breaks out).
- `stops.py` ← `fae_comp/skill_stops.py` skill-stop online logic (beat-opp, equal-8,
  Colossal Smash). Cards 13/14/15 keyed to skill pairs partitioning the 6 skills.

Rolls use **actual seeded draws** (a roll picks one of 6 skills uniformly; value = that
derived stat). The closed-form `finish_odds`/`turn_odds` tools are used only in validation.

---

## 7. Policy interface (`policy.py`) — where "player skill" lives

`Policy` is handed the **observable** state + the **legal action set** at each decision point
and returns a choice. Decision points (the skill surface):
```
mulligan(hand)                         choose_turn_action(play-or-pass, which card, bury target)
respond_with_stop(valid_stops | none)  commit_finish?(given CM / stop risk)
choose_finish(which finish card)       use_optional?(reroll / self-buff / "you may")
choose_target(for a targeted effect)   breakout_choices(if any optional)
discard(which card to shed)            search(which deck card to bin next, "up to N" -> discard)
```
The `search` point fires per card of a `Search(dest=DISCARD)` "up to N": the owner
picks a deck card to bin (a trailing `none` stops early), then the deck shuffles.
`discard` fires whenever a hand must shed a card — over the max hand size (10),
enforced immediately on the draw that exceeds it, or forced by an effect
(`Discard N`, "your opponent discards N"). The
hand's **owner** always chooses which card, even on an opponent-forced discard,
*unless* the effect is random (`random=True` → seeded RNG picks). `HeuristicPolicy`
sheds the least valuable card: dead card → offline stop → online stop → needed chain
piece → Finish (protecting the line being pushed).
Ships `RandomPolicy` and `HeuristicPolicy` (M1). `LearnedPolicy` (M4) consumes exactly the
`(observable_state, legal_actions)` tuples the log already records → the training signal is
free. Policies never see hidden info (opponent hand/deck order) unless an effect reveals it.

**Observation model** (todo #34). `GameState.observable(viewer)` is the redacted view a
player at the table actually has — the honest input for M4 imitation learning. Public to both
sides: competitors, entrances, `in_play`, `discard`, gimmick-blank status, plus
`crowd_meter`/`active`/`turn_no`. Private: the opponent's `hand` shows only its **size**, and
**every** `deck` shows only its size (order is hidden from everyone, owner included — the
five-region rule); the viewer's own `hand` is full. RNG, `flags`, `freq_counters`, and
`pending_roll_mods` are engine bookkeeping, not table zones, so they are omitted. This gate
pairs with the log's `hidden` flag (§8): the engine keeps ground-truth ids for deterministic
replay, and `observable` is what decides what a given viewer is allowed to know.

**Decision protocol / wire form** (substrate split — `docs/design/substrate-split.md`
§4). The synchronous `_decide(point, key, legal)` call has a transport form for
remote/interactive play: server → `DecisionRequest{request_id, seq, viewer, point,
legal, observable_state}`; client → `DecisionResponse{request_id, chosen}`. `point`,
`legal`, `chosen` are exactly the `decision`-event fields (§8) and `observable_state`
is exactly `GameState.observable(viewer)`, so this introduces **no schema change** —
only `observable` crosses the wire, keeping seed + hidden zones server-side (anti-cheat).
**Reserved for explicit timing** (tournament play; deferred, see §12): two additional
decision points — `order_triggers` (controller orders simultaneous triggers) and
`pass_priority` (priority passing / response windows). These are §7 additions, **not**
§3/§8 changes, and are unspecified until the timing follow-up.

**Player-profile policies** (todo #32) subclass `HeuristicPolicy`, overriding only the
decision points that differ, so a matchup can pit skill levels against each other:
- `aggressive` (`AggressiveBuilder`) — the validated baseline; builds one chain greedily.
- `smart` (`SmartPasser`) — passes+buries to **hoard stops**, building only when it holds a
  Finish (then toward that combo); the strongest self-play profile.
- `newbie` (`Newbie`) — greedy (throws a Finish the moment it's playable, opens Leads/FUs
  just to play them), never plays stops offensively, but misplays the economy: stops eagerly
  (wastes a stop on the first threat) and discards/buries carelessly (leftmost).

Advanced, opponent-model-dependent play — baiting signature cards (Apocalypse/Rejected!) out
early, forcing a stop to land to arm a held see-1, see-1 type-avoidance, and the smart
passer's "build anyway vs a stop-poor opponent" exception — is **deferred** to todo #35
(needs an opponent-model input; profiles take it as an optional constructor arg then).

---

## 8. Game-log schema (`gamelog.py`) — one schema for SIM *and* REAL games

JSON Lines (one event per line) + a header. A recorded human match is the same schema with
`policy: "human"`. Enough to (a) deterministically replay a sim, (b) transcribe a real match,
(c) train a policy.

```jsonc
// header
{"schema": 1, "seed": 11, "kind": "sim|real", "created": "<passed-in>",
 "players": {"A": {"competitor": "...", "entrance": "...", "deck": [<card refs>],
                   "policy": "heuristic|random|human|learned:v1"},
             "B": {...}}}
// then an ordered stream of events, each: {"t": turn_no, "type": ..., ...}
roll        {player, skill, base, mods:[{src,delta}], value}
turn_result {winner, tie_bumps}
decision    {player, point:"turn_action|stop|finish|mulligan|target|optional|discard|bury",
             legal:[...], chosen:<idx|action>, policy}
play        {player, card, order, atk_type}
stop        {player, card, stopped, reason}
draw|bury|discard|search {player, cards:[...], from?, hidden?}
            // hidden=true iff both endpoints are private (hand/deck): a draw
            // (deck->hand) or a bury (hand->deck). The opponent sees the count,
            // not which cards. cards[] keeps ground-truth ids for replay; the
            // observable projection redacts them per viewer.
finish_attempt {player, finish, value, bonus:{...}, crowd_meter, auto_success}
breakout    {defender, rolls:[{skill, value, penalty, success}], broke_out}
crowd_meter {delta, value}
unsupported {owner, card|gimmick, raw, reason}
effect      {src, action, target, detail}          // executed IR (audit trail)
result      {winner, reason:"finish|count_out|disqualification|pinfall", turns}
```
`decision` events are the key export: `legal` + `chosen` + observable-state ref = the
imitation-learning dataset. Replay = re-run the engine with the header seed and assert the
event stream matches.

---

## 9. Module layout

```
srg_sim/
  cards.py        # Card, Competitor, EntranceCard, Deck, enums
  loader.py       # cards.yaml -> index; resolve decklist -> Deck (name/uuid/variant)
  effects.py      # Effect IR: Trigger, Condition, Action, Effect, Unsupported
  rules_parser.py # rules_text -> [Effect]; grammar + overrides.yaml + coverage report
  state.py        # GameState, PlayerState, snapshots
  engine.py       # turn loop, effect executor, stop resolution, finish sequence
  finish.py       # PORTED from fae_comp/supershow.py (finish/breakout) + self-checks
  stops.py        # PORTED from fae_comp/skill_stops.py (skill-stop online logic)
  rng.py          # seeded RNG wrapper; roll(), shuffle(), reveal()
  policy.py       # Policy ABC + RandomPolicy, HeuristicPolicy
  gamelog.py      # event dataclasses, JSONL read/write, replay/verify
  analysis.py     # M2: batch N seeded games for a matchup -> outcomes; aggregation;
                  #     Matchup/GameOutcome/MatchupReport, run_batch(jobs=N) parallel fan-out
  report/         # 2-competitor matchup scorecard -> Sphinx HTML + xelatex PDF:
                  #   carddb, images, turn (exact|MC), finishes, skillreqs, classify,
                  #   model, render (RST), build. Reuses finish.py/stops.py/engine.py.
  cli.py          # `srg-sim play|coverage|analyze|replay|review|export|report`
decks/            # example decklists (yaml)
overrides.yaml    # hand-authored IR for cards the grammar can't parse
tests/            # parity + regression (see §10)
DESIGN.md README.md pyproject.toml
```

**Substrate split & Rust end-state** (`docs/design/substrate-split.md`). The modules
above divide into a **substrate** — the authoritative rules engine (`cards`, `loader`,
`effects`/`conditions`, `rules_parser`+`overrides`, `state`, `engine`, `finish`,
`stops`, `rng`, `gamelog`, `policy`, plus a new `session` for the wire protocol) — and
**consumers** on top (`cli`, `interactive`, `review`, `report/`, `analysis`, a future
MCP server / web / mobile). The boundary rule: **the substrate never imports a
consumer** (guarded by `import-linter`, then by the Rust crate graph). The end-state
moves the substrate + parser to a single **Rust `srg-core` crate** compiled to every
target (native console/MCP, WASM web, native mobile lib); the Python engine serves as a
**transitional parity oracle**, then is deprecated in favor of a frozen golden-log
corpus. See §13.

---

## 10. Milestones

- **M1 — rules-correct engine + log.** Two decks play a full legal game end-to-end under
  `RandomPolicy`/`HeuristicPolicy`; deterministic under a seed; complete JSONL log; replay
  verifies. Effect IR + executor cover cards actually in the two demo decks; everything else
  flags `Unsupported`. Validation suite green.
- **M2 — analysis harness.** Batch N seeded games for a matchup; aggregate win-rate, finish
  type/rate, stop usage, crowd-meter curves, game length; A/B deck diff. *As built:*
  `analysis.run_batch` fans games across processes (`jobs=N`, seed-ordered, serial fallback);
  `MatchupReport.from_outcomes` computes the aggregates; `srg-sim analyze A.yaml B.yaml
  --games N [--jobs J] [--json|--csv]` prints and exports the report (`docs/development/analysis`).
- **M3 — coverage.** Grow grammar + overrides until `Unsupported == 0` over the top-96;
  coverage report tracked in CI.
- **M4 — player data.** Ingest recorded real matches (same schema); fit `LearnedPolicy`;
  compare to heuristics; expose per-decision divergence as a "how a human differs" analysis.

---

## 11. Validation (`tests/`)

Regression against the validated `fae_comp` tools:
- Gimmick-free turn duel → **≈50/50**; Monte-Carlo converges to closed-form `turn_odds` (CI).
- `finish.py` parity vs `supershow.finish_odds` / `FinishCalculator.jsx` on a case batch.
- `stops.py` coverage cases (Bull vs Fae; Colossal Smash always-on) — a deck-analysis tool
  (the engine's stops are text-driven; see §4).
- Text-driven stops **engage** under skilled play: a demo Bull-vs-Fae heuristic batch spends
  stops contesting Finishes across the persistent board (regression against a null-defense sim).
- Determinism: same seed + same decks + same policies → byte-identical log; replay verifies.
- `tournament_turnsim` self-checks — **reproduced** (todo #17/#31): Bull vs vanilla ≈54.1%,
  vs Fae ≈45.9% within tolerance, mirror ≈50%. The turn-roll gimmick layer threads per-side
  roll context (rolled skill + signed gap) into `OnRoll` firing. The **Bull** is roll-value
  keyed — its card reads "when your turn roll is exactly 3 less than your target's turn roll,
  your next roll is +1 (4 less → +2, 5+ less → +3)" — so it is three `OnRoll` effects gated by
  `RollGapExactly/AtLeast` → `ModifyRoll(SELF, +N, NEXT)`, firing whether the roll won *or*
  lost (**not** `OnLoseTurn`; the two coincide only when highest-wins). **Fae** carries a
  `Static` `LowestRollWins` marker flipping the roll-off to lowest-wins, which makes the Bull's
  own roll boost *backfire* — the mechanism behind the sub-50% result. An opt-in test guards
  the reference's own self-check numbers when `fae_comp` is checked out.

---

## 12. Open questions / deferred (flag, don't guess)

**Resolved (folded into the design):**
- ✅ Loss/win conditions — finish, count-out (a *win* on exhausting deck+hand on a won turn),
  disqualification, pinfall. See §6 / §8.
- ✅ "Hit a card" = a card resolving into play — an unstopped played card, or a stop entering
  play (the stop is itself hit). See §3.
- ✅ Buff duration — `WHILE_IN_PLAY` for card sources (Finishes' buffs die on breakout),
  `WHILE_GIMMICK_ACTIVE` for gimmicks (until blanked; blank lifts when the blanker leaves
  play). Buffs apply to the unified derived-stats view (turn rolls, stops, breakouts). §3/§5.
- ✅ Incremental-value cards (7–9, 10–12, 16–18, 22–24) — no longer a permanent gap; the
  **full card DB is parsed** during build-up, so these fill via grammar + overrides. Anything
  still unparsed flags `Unsupported` and shows in the coverage report.
- ✅ Board persistence & the chain — the in-play board **persists across turns** (both sides),
  one card played per won turn, order-only chain (§6). Cleared only on breakout (both sides).
  Any number of same-stage cards may stack. Replaces the earlier within-turn-combo model.
- ✅ Stops — **text-driven per printing** (a card's parsed `Stop` effects + conditions), not
  universal RPS; the 30-card number-map (§4) is the typical pattern. See §6.

**Resolved (folded into the design):**
- ✅ **Turn-roll gimmick layer** (todo #17/#31) — the engine threads per-side roll context
  (rolled skill + signed `gap` = opponent − self, so positive = rolled lower) into `OnRoll`
  firing, and the roll-off honours a `Static` `LowestRollWins`. Bull (gap comeback via `OnRoll`
  + `RollGap*` → `ModifyRoll(NEXT)`) and Fae (lowest-wins) reproduce the `tournament_turnsim`
  parity (§11). Pending roll bonuses apply to the first roll of a roll-off only — a bump is a
  *new* roll and drops them, matching the reference. Pending-debuff gimmicks (Grump: "when your
  opponent rolls 8/9…") reuse the same `OnRoll(who=OPP)` path but need a roll-*value* condition,
  still to be added when that competitor lands.

**Still open (confirm as we hit them):**
- Finish-bonus model — combo cards contributing to the finish via `BuffSkill`, plus flat
  "+N to your Finish rolls" (in progress).
- Exact interaction of some gimmicks with multi-roll breakouts (buffs that change mid-breakout,
  effects that add breakout attempts) and simultaneity when both players trigger on one event.

---

## 13. Substrate split & Rust migration

Full detail: [`docs/design/substrate-split.md`](docs/design/substrate-split.md) (a
review artifact of the same class as this document). Summary of what it pins:

- **The boundary.** A **substrate** (authoritative rules engine + parser + a new
  `session` wire layer) below the line; **consumers** (console, MCP, interactive,
  review, report, web, mobile) above it. The substrate never imports a consumer.
- **The public API** — three layers: load/build (`load_index`, `resolve_deck`,
  `validate_deck`), batch/pure (`Engine::play`), and session/interactive (`Session`,
  the pausable **continuation state machine** driving the decision protocol, §7).
- **The engine goes Rust**, one crate compiled to every target (native + WASM),
  resolving the language-split delta by compiling one implementation N ways rather than
  trusting a second one. The Python engine is a **transitional parity oracle**, then
  deprecated (frozen golden-log corpus).
- **The conformance harness** is the migration's safety rail: same `(seed,
  decisions[])` → Python and Rust must emit byte-identical `GameLog` (enabled by the
  portable `splitmix64` RNG, §5), plus parser-parity on `cards.ir.json`.
- **§3 and §8 are unchanged** — re-homed as language-neutral JSON contracts. Every
  delta this migration needs is additive (RNG note §5, protocol + reserved timing
  points §7, module/boundary note §9, this section).
