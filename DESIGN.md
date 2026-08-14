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
- Deck-build legality (format pools) — an optional offline validator, not the engine. The
  `skill-requirement cards ≤ 2 / deck` rule is implemented (`Deck::format_problems`, surfaced
  by `srg audit`); the rest of the card-pool format rules remain out of scope.

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
OnStop(dir=YOURS|THEIRS, order?)  # when a stop happens; order (schema v31) gates on the
                             #   STOPPED card's play order — "when your opponent stops your
                             #   Finish" (La Fenix Super Lucha); None = any stopped card
OnHit(atk_type?, name_contains?, text_contains?, on_any=False, order?, who=SELF, from_hand=False)  # when a matching card RESOLVES into play (schema v6);
                             # on_any=True (schema v23) = a standing "when you hit a card" gimmick, fires on EVERY hit
                             # (Bartholomew Hooke) — override-only, so a bare parser OnHit fragment stays inert
                             #   gimmicks gate on the hit card's attack type and/or its
                             #   title/text ("when you hit a card with 'X' in the name",
                             #   case-insensitive OR-substring); see "hit" below.
                             # from_hand=True (schema v128) dispatches from the owner's HAND, not the board — a
                             #   "reveal this card from your hand when your opponent hits <X>" reactive (Mailman);
                             #   hand_self_triggers scans hand cards and binds self_card for a ShuffleSelfIntoDeck body
OnBump                       # when the owner bumps (a tied roll: both draw + re-roll).
                             #   Both sides bump on a tie, so each owner's OnBump fires;
                             #   bump-punish gimmicks (Mastermind: "opp next roll -2") live
                             #   here. Gate repeats with a once-per-turn frequency.
OnBury(who, from_hand_only?, also_discard?)  # when an EFFECT/Gimmick causes the owner to bury cards
                             #   (Cyclone V1) — fires after act_bury/act_discard only, NOT the
                             #   mechanical pass-and-recycle or hand-cap trim (they bypass those
                             #   paths). from_hand_only limits to hand buries; also_discard also fires
                             #   on an effect-caused hand discard (Tommy "bury or discard"). schema v15
StartOfTurn / StartOfMatch
OnBreakout(who?)             # after a breakout: who=None any ("after a breakout" — Copy Kat); SELF you broke out; OPP your opponent broke out. Fires BEFORE the boards clear so a card-based recur still sees its card. schema v29
OnBreakoutRoll(who, attempts?)  # fires on EACH of who's breakout rolls (run_on_breakout_roll, reads roll value/skill via RollContext) — "if your opponent rolls N for their breakout roll, you lose"; "each time your opponent rolls for a breakout roll, …". attempts = 1-based ordinal gate ("their 1st or 2nd breakout roll" -> [1,2]; "their 3rd" -> [3]); empty = every roll. schema v128 for attempts
OnReroll(who)                # when who re-rolls their TURN roll (at the roll-off, after the die lands; run_on_reroll). who=SELF "when you re-roll"; OPP "when your opponent re-rolls". A roll-mod body ("their roll is -1") adjusts the re-rolled value; draw / shuffle-self resolve normally. schema v104
OnShuffle(who)               # when who's deck is shuffled by a card/gimmick EFFECT (search/tutor/shuffle-into-deck/hand-into-deck or explicit "shuffle your deck") — NOT the setup shuffle. who=OPP "when your opponent shuffles their deck" (Memes Dealer V2). schema v32
OnDraw(who)                  # right after who DRAWS 1+ cards (run_on_draw at the draw chokepoint). Used by a WhileInDiscard recur gated on DrewThisTurn — "when this card is in your discard pile, if you drew 1+ cards this turn, you may add it to your hand" (The Gobstopper); self_card bound so AddSelfToHand resurrects the source. schema v129
OnFlip(who, count?)          # when who flips cards (Flip mills deck→discard). count = exact-size gate ("when you flip exactly 3 cards" — Evee); None = any flip. Fired by run_on_flip. schema v84
OnDiscardMove(who)           # when one or more cards LEAVE who's discard pile via a card/gimmick EFFECT (recur-to-hand / shuffle-into-deck / recur-to-deck-top /
                             # hand-discard swap / effect-caused pile bury) — NOT the mechanical pass-and-recycle. Fires ONCE per action, not per card.
                             # who = owner of the PILE (OPP = "when your opponent moves any number of cards from their discard pile" — Brumeister V2). schema v34
OnCrowdMeterIncrease         # whenever the (shared) Crowd Meter goes UP — the per-turn +1 after a breakout OR an effect-driven CrowdMeter swing (a decrease never fires it).
                             # Both players' standing effects carrying it fire (global meter). "When the Crowd Meter increases, <body>" (Khloe Mai). schema v131
OnTurnStart                  # once at the START of every turn, BEFORE the roll-off (both players' entrance + in-play effects), so a MultiTurnRollBonus armed here lands on this
                             # turn's roll and last-turn gates read the just-ended turn (Impact is Family V1's stall-punish rider). Distinct from StartOfTurn (post-roll, winner only). schema v140
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
WHILE_IN_DISCARD     # active ONLY while the source card sits in its owner's discard pile
                     #   ("when this card is in your discard pile, …"). Scanned from the
                     #   discard zone; inert while the card is in play. Honored by
                     #   is_text_blanked (the in-discard Spotlight blanks). schema v30

# --- TIMED durations (schema v35). Unlike every While* duration above, these are NOT
# re-derived from a zone on each stats read: the buff is granted IMPERATIVELY when its
# effect fires and lives in PlayerState.timed_buffs until its sweep. Folded into the
# derived stats at the one chokepoint, so a timed buff feeds turn rolls, Finish rolls
# and breakout rolls alike (a stop that becomes a Finish can roll on the opponent's
# turn, while the buff is still live). BuffSkill.cap changes meaning under these: it
# bounds the ACCUMULATED total, so repeat firings of the same clause stack and clamp
# ("(Max +5 to each)"), one entry per (clause, skill, expiry).
UNTIL_END_OF_TURN    # "until the end of the turn" (~81 cards). Swept with the other
                     #   per-turn resets at the top of the following turn.
UNTIL_START_OF_YOUR_NEXT_TURN
                     # "until the start of your next turn" (Snake Pitt Super Lucha,
                     #   Arcade Addict Aaron, Caveman V1). A turn is SHARED and its
                     #   active player is only known once the turn roll resolves, so the
                     #   sweep runs immediately AFTER that roll: the buff still feeds the
                     #   roll that makes the turn yours, then dies. It therefore survives
                     #   every turn on which its owner is not the active player.
                     #   Hand-adjudicated 2026-07-20 (stacking + expiry + roll timing).
UNTIL_TARGET_HITS_CARD
                     # "until they hit a card" (Sleep Paralysis): an EVENT-swept blank
                     #   poison, latched on the target (PlayerState.blank_until_hit) and
                     #   lifted the instant that player next LANDS A HIT (record_landed_
                     #   hit), so it can span several turns. schema v127
```
**Gimmick blanking** is itself `WHILE_IN_PLAY`: a blanker card sets `gimmick_blanked` on the
target while the blanker is in play; when the blanker leaves play the Gimmick un-blanks and
its buffs return. Blanked gimmicks contribute no effects (incl. no `Static` buffs) while blanked.

**Condition** — a predicate on `GameState` (composable via And/Or/Not):
```
SkillCompare(skill, who=SELF, cmp=>|>=|=|<, vs=OPP_SAME|VALUE|SELF_OTHER, value?, vs_skill?)  # vs_skill: compare to a DIFFERENT skill; OPP_SAME/OPP=vs opponent ("your Strike > opp Agility"), SELF_OTHER=two of your OWN skills ("your Agility > your Strike", the #13/#14/#15 equal-8 stops)
HandSizeCompare(cmp, vs=OPP|VALUE, value?)
CrowdMeterCompare(cmp, value)
DeckSizeCompare(cmp, value, who=SELF)  # who's remaining deck size vs value — "if you have 0 cards in your deck" (Foxworthy V3). schema v82
MatchHasNoDisqualifications  # the match currently has no disqualifications (neither player can be DQ'd; GameState.match_has_no_dq) — Cardona's Pizza Cutter. schema v83
IsMatchType(types)  # the match stipulation is one of `types` ("if this is a Steel Cage or Liger's Den Match, …"); disjunction over GameState.match_type. 156-clause gate family. schema v92
HasInPlay(who, filter, count=1, cmp=>=) / HasInDiscard(who, filter, count=1)  # HasInDiscard count: "if you have N <X> in your discard pile" (Fortress's Tower of Strength, count 2). Default 1 = "has >=1". schema v136
ChosenNameIs(name, who)      # who's ChooseName binding == name; the gate that resolves "that" name into one concrete effect per
                             # option (Raven). False until a choice is bound. schema v37
InPlayCompare(filter, cmp, who, vs_who)  # cross-board: who's count of filter in play `cmp` vs_who's count ("target has more
                             # Strikes in play than you" — Snake Pitt V3: who=OPP, vs_who=SELF, cmp=>). Honors CountsAsInPlay. schema v33
RollWasSkill(skill, who?) / RollGapExactly(k) / RollGapAtLeast(k)   # gap = opp - self, positive = self rolled lower.
                             # RollWasSkill who: SELF (default) reads the owner's rolled skill, OPP the other side's
                             # (roll ctx opp_skill); under And/Or = "both/either players rolled X for turn roll". schema v75
RollLeadAtLeast(k)           # self rolled >= k HIGHER than opp (gap <= -k) — mirror of RollGapAtLeast (YamatoHama). schema v2.
RollValue(cmp, value, who=SELF)  # the turn-roll VALUE (die+stat+mods) `who` rolled: SELF (the trigger's roller) or OPP — "your opponent's turn roll is N" (Scott Prime's The Loaded Glove); the opp value is derived from the actor's roll ctx as value+gap. schema v130
PrintedRollValue(who, value) # the rolled skill's PRINTED (base, unbuffed) stat on the who-side's competitor == value
                             # ("rolls their printed 8 skill" — Collin); needs a roll ctx, who follows the trigger. schema v17
SameRolledSkill              # you and your target rolled the SAME skill this turn-roll (Hex, Nic Nemeth):
                             # RollContext.skill == .opp_skill (new field carrying the other side's rolled skill,
                             # set only in the post-roll/pair contexts). Needs a roll ctx. schema v18
FirstTurn                    # this is the first turn of the game (GameState.turn_no <= 1). Gates "if this is the first
                             # turn of the game, this card is also a <order> / cannot be stopped". schema v119
StoppedCardNoLogoNoReq       # the owner's most recently stopped card had neither a competitor logo nor a skill
                             # requirement (Logoless tag AND no SkillRequirement; flags["stopped_card_no_logo_no_req"],
                             # stamped by apply_stop). Gates "if the stopped card did not have a competitor logo or
                             # skill requirement, this card is also a Finish". schema v144
BrokeOutLastTurn{who}        # `who` broke out on the PREVIOUS turn (flags["broke_out_turn"] == turn_no-1, stamped by
                             # `breakout` on success). Gates "if you broke out last turn, …"; "either/any player" -> Or. schema v120
StoppedCard{who,last_turn}   # `who` performed a stop last turn (last_turn) or this turn (flags["stopped_card_turn"], stamped
                             # by `apply_stop` for the stopping side). Gates "if you stopped a card last turn, …". schema v121
OppWonLastRoll               # the opponent won the PREVIOUS turn's roll-off (GameState.last_roll_winner); false on turn 1 (Dunn re-roll). schema v3.
EndedTurnNoPlay{who}         # `who` ended the PREVIOUS turn without playing a card — roll-off winner on turn_no-1 who
                             # passed (flags["last_pass_turn"]); false on turn 1 / after a play or lost roll-off. `who` defaults
                             # SELF (skip-when-self; The SRG Boss); OPP gates Impact is Family V1. schema v78; who added v140
BuriedSpotlightLastTurn{who} # `who` buried a Spotlight card on the PREVIOUS turn (flags["buried_spotlight_turn"] == turn_no-1,
                             # stamped when a Bury moves a Spotlight-tagged card). Gates Impact is Family V1's stall-punish rider. schema v140
RerolledTurnRoll             # the owner re-rolled their turn roll THIS turn (any turn die re-rolled at the roll-off;
                             # PlayerState.flags["rerolled_turn"]). Gates King Brian Cage's finish riders, OR'd with
                             # RollWasSkill{Power}. schema v80
HitCard(filter, who?, last_turn?)  # who hit a card matching `filter` this turn (last_turn=False) or the previous turn
                             # (True) — "if you hit a Grapple last turn". Reads PlayerState.hit_this_turn/hit_last_turn
                             # (by-card, rotated at turn start); empty filter = any hit. Filtered sibling of HitThisTurn. schema v91
DrewThisTurn(who?, at_least)  # who has DRAWN at least `at_least` cards this turn — "if you drew 1 or more cards this turn"
                             # (Gobstopper recur; Brotherly Love "drew 3+ → also a Lead"). Reads PlayerState.drew_this_turn
LostTurnRollsInARow(who?, at_least)  # who has LOST at least `at_least` turn rolls IN A ROW — "if you lose 2 Turn Rolls in a row"
                             # (Me Against the World's discard recur, via a WhileInDiscard OnLoseTurn). Reads PlayerState.turn_losses_in_a_row
                             # (incremented on a loss, reset on a win). schema v134
                             # (incremented at the draw chokepoint, reset at turn start). schema v129
DuringTurn(who)              # it is currently who's turn (GameState.active == who-side) — gates a continuous
                             # effect to a turn phase ("during your opponent's turn: …" — La Fenix). schema v19
FlippedForGimmick            # the flip now resolving was caused by a Gimmick-source effect ("flipped for your
                             # Gimmick"). Reads GameState.flip_provenance; only meaningful on a flipped card's OnFlip{SELF}. schema v87
FlippedByName(names)         # the flip now resolving was caused by a card whose name contains one of `names` (CI
                             # OR-substring; the Set-Up-the-Ladder cards). Reads flip_provenance.source_name. schema v87
Always
```

**Action** — the *what* (mutations); each names a `target` (SELF/OPP/a card/skill):
```
Draw(n, from=TOP|BOTTOM, who, per?, per_who=SELF, cap?, per_excludes_trigger=False, from_crowd=False)  # from_crowd -> count = Crowd Meter + n (n is the signed offset), clamped to cap, floored at 0 — "draw cards equal to the Crowd Meter +1 (Max +5)"; mutually exclusive with per (schema v108)
Bury(selector, count, per?, per_who=SELF, per_zone=IN_PLAY, all=False)   Discard(selector, count, who, per?, per_who=SELF, all=False)
                              # Bury/Discard `all` (schema v90): shed EVERY hand card matching `selector`, ignoring
                              # count/per ("they bury/discard all Strike cards"); dispatch derives count from hand size.
                              # Bury per_zone (schema v130): the zone `per` counts — IN_PLAY (Cardona) or FLIPPED_THIS_TURN
                              # ("opp buries 1 per Strike flipped", Scott's Five Star Heart Punch; fires OnHit, after the Flip)
Flip(n, who=SELF, per?, per_who=SELF, until?, until_to_hand=False)  Search(filter, dest=HAND|DISCARD|DECK_TOP, count=1, source=DECK|DECK_OR_DISCARD)  ShuffleIntoDeck(selector, source=DISCARD|IN_PLAY|HAND, who=SELF, all=False, then_draw=False, then_bury=False)  # then_bury: bury `count` (the shuffled number) from hand ("… then bury the same number from your hand", Double Leg Death Lock); HAND source shuffles from hand ("… any number from your hand …, then draw the same number", The Dudebuster); who=OPP/each-player recurs that player's own zone into their own deck ("each player shuffles 1 Grapple from their discard pile into their deck"). schema v143
                              # until (schema v68): flip-until — ignore n, mill one card at a time until a flipped card
                              # matches `until`; that card -> hand if until_to_hand, else discard ("Flip cards until you flip a Submission[, add it to your hand]")
                              # dest=DECK_TOP (schema v22): search, shuffle the deck, put the card on TOP (Heartache Kid)
                              # dest=DISCARD: "search your deck for up to `count` cards, put them in
                              # discard" — owner chooses which/how many (a `search` decision), then shuffles
                              # Draw/Discard `per`: n/count scales by the count of `per` cards in play,
                              # exactly like ModifyRoll (authored OnPlay for "for each OTHER … in play")
ShuffleDeck(who)              # shuffle a whole deck ("Shuffle your deck")
AddFromDiscard(filter)        RemoveFromPlay(selector, who=OPP, count=1, to_deck=false, all=false)  # board disruption -> discard (to_deck=true = "bury it", to the owner's deck bottom; JT Dunn). all=true clears EVERY match of the who-side at once, no per-card pick ("Discard all cards in play", Apocalypse). schema v133 (all: v135)
RedirectBoardEffect(actions)  # the per-player halves of an "each player …" board effect (Apocalypse's board clear, Rejected!'s
                             # discard-bury, Derailed's hand cycle), wrapped so a competitor with a matching RedirectAuthority (Emo Mam)
                             # may choose which players they affect (both / controller / opponent / neither). Absent an active authority
                             # every half applies — byte-identical to a plain each-player effect — so wrapping is safe DB-wide. schema v135
RedirectAuthority(groups)    # Static gimmick marker (Emo Mam): "when you or your opponent hit one of `groups`, you may choose who it
                             # affects." Read by RedirectBoardEffect via the RESOLVING card's name (trailing-'!'/case-insensitive, so the
                             # gimmick's "Rejected" matches the card "Rejected!"), so only the listed cards are ever redirected. schema v135
AddFlippedToHand(count?, filter)  # "add N of the flipped cards to your hand" / "add all flipped Strikes …": move `count`
                             # (None = all) matching cards from the turn's flip pool (flipped_this_turn ∩ discard) to hand;
                             # owner picks on a choice. Flip-pool-scoped sibling of AddFromDiscard. schema v88
ReturnToHand(selector, who, count=1, choose=False)  # bounce matching in-play cards to their OWNER's hand;
                             # choose=True picks from EITHER board ("any player has in play" — Fox Assassin V2). schema v20
SwapHandDiscard               # "switch 1 card in your hand with 1 in your discard" (Collin, Mr. Rey): 1 hand card
                             # out (-> discard, shed point) + 1 discard card in (-> hand, tutor point); no-op if a zone is empty. schema v17
AddSelfToHand                 # "If this card is flipped, [you may] add it to your hand": moves the just-flipped
                             # referent (Engine::flipped_card, bound per-card in run_self_flips) from discard -> its owner's
                             # hand. Paired with an OnFlip{SELF} trigger; "you may" -> Effect.optional. schema v85
ShuffleSelfIntoDeck           # "If this card is flipped, [you may] shuffle it [back] into your deck": flipped referent
                             # discard -> deck, then shuffle (fires OnShuffle). Sibling of AddSelfToHand. schema v86
PutSelfOnDeckTop              # "[you may] put this card on top of your deck": self referent (self_card, or stopped_card for
                             # the "If stopped, …" family) moves discard/hand -> deck FRONT (drawn next), unshuffled. schema v141
PutFromHandOnDeckTop{count}   # "put N card(s) from your hand on top of your deck": owner picks which; hand -> deck FRONT.
                             # Standalone (on hit) or the tail of a PutSelfOnDeckTop recycle ("…, then put 1 from hand on top"). schema v142
PlaySelf                      # "If this card is flipped, [you may] play it[ as an additional card this turn]": flipped
                             # referent discard -> resolve_play by its owner (stop window, OnPlay/OnHit), a bonus play. schema v86
RecurToDeckTop(selector, count=1)  # "up to N" discard -> TOP of deck (redraw next turn)
Reveal(who, count, whole_hand=False)  # fog-of-war: `who` reveals `count` hand card(s) of their choice to the opponent (join revealed_hand, no zone change). whole_hand=True -> expose the ENTIRE hand at once, ignoring count ("Reveal your hand to your opponent", Bermuda Triangle). schema v127
RevealAndDiscard(count, who=OPP)   # reveal `count` random cards, discard the Stops among them (0..count)
RevealForDraw(who=OPP, count=1, draw=2, match_on=STOP|ROLLED_SKILL)  # reveal `count` random from hand; actor draws `draw` per revealed card matched by `match_on` — a Stop (Bartholomew) or one whose move type == the actor's just-rolled skill (The Winning Ticket, reads roll ctx). schema v24
Scry(deck, top=0, bottom=0, reveal=False, to_hand=0, bury=0, rest=RETURN|CHOOSE|FLIP|MAY_FLIP)  # schema v11: look
                              # at/reveal `top`+`bottom` cards of `deck`'s deck; the effect owner (actor) takes `to_hand`
                              # best -> deck owner's hand, buries `bury` (worst on own deck, best on an opponent's
                              # = sabotage), rest RETURN (reorder on top) or CHOOSE (keep good on top, bury junk) or
                              # FLIP (mill leftovers to discard — "add M to your hand and flip the others"; schema v69)
                              # or MAY_FLIP (optional single-card flip — mill only what's worth denying/shedding, leave
                              # the rest on top; "look at the top card of your opponent's deck, you may flip it"; v96).
                              # to_hand_filter (opt): restrict which revealed cards `to_hand` may take — "add 1 STOP to
                              # your hand and bury the others" (Fortress); only a match qualifies, the rest fall through
                              # to bury/rest. None = take the `to_hand` best regardless of kind. schema v136
                              # reveal=public (logged) vs private "look at". Perfect Assistant/Split/Ricky Riot/Oracle
RevealRoute(deck, match_atk, on_match, on_fail, fail_optional=False, reveal=False, reveal_from=TOP, match_parity=None)
                              # schema v12/v13: reveal 1 from `reveal_from` (TOP|BOTTOM|CHOOSE, blind->top); if the
                              # predicate holds -> on_match, else on_fail (an optional "you may flip/bury it" is
                              # taken only when worth it — shed junk on your own deck, disrupt a valuable card on
                              # an opponent's). Predicate = revealed.atk_type==match_atk, OR match_parity (Some(true)
                              # = even) for the odd/even guess. Dest {HAND, FLIP=mill, BURY=bottom, LEAVE=on top}.
                              # Candy MaM / Flame Fighter (atk, per rolled skill) / Smart Mark (parity)
RevealThen(reveal_from, count, filter, take_matched=False, then=[], then_optional=False)  # schema v95
                              # Reveal `count` card(s) from reveal_from (DECK_TOP|DECK_BOTTOM peek, non-destructive;
                              # HAND_RANDOM = uniformly-random hand card); if a revealed card matches `filter` (name
                              # substring / atk type) run the consequence: take that card to hand when take_matched
                              # ("add that card to your hand", mandatory), then apply `then` (extra parsed actions —
                              # draw / roll bonus / re-roll / …), the whole `then` gated by then_optional ("… and you
                              # may re-roll"). Non-match leaves every card in place. "Reveal the top card of your deck:
                              # if it has 'X' in the name, add that card to your hand"; "Randomly reveal 1 card in your
                              # hand: if it has 'X', draw 1 card". Owner is always the effect owner (your deck/hand).
ShuffleHandDraw(who, count, choose=False)  # schema v13: shuffle a player's hand into their deck, shuffle, draw
                              # `count`; choose=actor picks the player ("either player"). Cyclone V2, on a bump
CountsAsInPlay(selector, count=2)  # Static self-decl: this card counts as `count` cards matching `selector`
ModifyRoll(who, delta, when=THIS|NEXT, per?, per_who=OPP, per_zone=IN_PLAY)  # delta scales by count of `per` cards
                              # in `per_zone` (IN_PLAY, or DISCARD for "+2 for each Finish in your discard pile"; schema v70)
RollDraw(who, skill, count)   # one-shot roll-conditional draw: "if your [opponent's] next turn roll is <S>, draw N". `who`
                              # = whose NEXT turn roll to WATCH (SELF|OPP); the owner draws `count` if that roll resolves to
                              # `skill`. Armed on hit, fires-or-fizzles on that one turn roll and is consumed (schema v109)
NextRollSkillBonus(who, skills, delta)  # one-turn skill-gated turn-roll bonus: "+N to <S>, <S> during your next turn roll" /
                              # "if your [opponent's] next turn roll is <S>, it is +N". `delta` applies to `who`'s (SELF|OPP)
                              # IMMEDIATELY-next turn roll if it comes up one of `skills`, then the queue is drained (one-turn
                              # window, non-match fizzles). Unlike ModifyRoll{on_skill}, does NOT wait indefinitely (schema v110)
MultiTurnRollBonus(who, rolls, delta)  # multi-turn turn-roll bonus: "your [opponent's] next N turn rolls are +/-N". `delta`
                              # applies to each of `who`'s (SELF|OPP) next `rolls` turn rolls, decrementing per roll-off until
                              # exhausted. Skill-agnostic and self-expiring, unlike the standing TurnRollBonus (schema v111)
BuffSkill(skill, delta, who, duration=WHILE_IN_PLAY, target_highest?, target_lowest?, per_crowd?, cap?, per?, per_zone=IN_PLAY, per_excludes_self=False)
                                                 # target_lowest -> retarget to the LOWEST base skill ("+N to your lowest skill"); mirrors target_highest. schema v93
                                                 # per=CardFilter -> bonus = delta * (count of the target's cards
                                                 # in per_zone {IN_PLAY|DISCARD|FLIPPED_THIS_TURN} matching per), clamped to cap
                                                 # per_excludes_self -> drop the SOURCE card from that count ("for each OTHER <X> you have in play"); skip-when-false additive. schema v105
                                                 # ("+1 for each card in play with 'Chin' in the name, Max +3"; FLIPPED_THIS_TURN
                                                 # = cards flipped this turn, "for each Strike flipped: +1 to Strike"); schema v7/v74
MaxHandSize(delta, who, duration=WHILE_IN_PLAY, set=None)  # Static cap mod: signed delta, or absolute set ("handsize is N", lowest wins, overrides base)
AddText(name_contains=[...], effects=[Effect...])  # Static gimmick: the owner's played cards whose title matches (case-insensitive OR) gain `effects` (their own triggers, usually OnPlay), injected at play time alongside the card's own. El Super Santa/Sabu. schema v25
Reroll(who, once=True, choose=False, when=THIS, cost?, finish=False, breakout=False)  # who=SELF/OPP die; choose=owner picks
                              # a player; when=NEXT grants a one-shot re-roll for the owner's next turn roll
                              # (schema v9 choose, v10 when). Structural read in the roll-off. finish=True (v76): a
                              # FINISH-roll re-roll (offer_finish_reroll); breakout=True (v102): a BREAKOUT-roll re-roll
                              # (offer_breakout_reroll) — "re-roll your Breakout roll" / "force your opponent to re-roll…".
                              # cost? = RerollCost{kind, count?, filter?} (v103): SHUFFLE_IN_PLAY (shuffle a matching
                              # in-play card away, Mr. Hyde) | BURY_FROM_HAND | DISCARD_FROM_HAND ("bury/discard N cards
                              # from your hand to re-roll"); offered only while affordable, paid on election.
CoupledDiscard(offset)        # "discard any number from your hand, opp discards that number `offset`" (Dismantler,
                              # offset -1). Actor's count N is an engine heuristic (min(self_hand, opp_hand+1)); the
                              # self-discard fires OnBury; opp then sheds max(0, N+offset). schema v76
SwitchRolledSkill(from_skill, to)  # "when you roll from_skill for your turn/Finish roll, you may switch it
                              # to `to`" (Scott Prime; schema v14). Structural read in BOTH roll paths; the
                              # "you may" lives on Effect.optional. Turn die keeps its mods (value recomputed
                              # on `to`'s stat); a switched Finish die recomputes base+combo from `to`.
WinTie(who)                   Bump(who)
ElectBumpOnSameSkill(uses=2)  # Static roll-off grant: owner MAY bump on a same-skill roll, N times/match
Stop(order?, atk_type?, source_is_skillreq?, even_unstoppable=False, target?, also_order=[])   # also_order (v146): the stopped attack must ALSO count as one of these orders via an AlsoLead whose condition holds — "Stop any Finish X that is also a Lead[ or a Follow Up]" (a multi-order attack). AND-ed in stop_matches_for.  BlankGimmick(who, duration=WHILE_IN_PLAY)
StopRequiresTag(tag)          # marker paired with a sibling Stop in the same effect: the stop is legal only vs an attacker carrying `tag` — "Stop any Grapple with a Spotlight" (read by card_can_stop). schema v26
Unstoppable(by_order?)        # Static self-decl: cannot be stopped by stops of `by_order` (None = anything)
AlsoLead(condition, order=Lead)  # Static self-decl: also playable in `order`'s slot while `condition` holds
                              # (order=Followup -> "also a Follow Up", playable when a Lead is in play; schema v70)
BlankText(selector, who, discard_only=False)   LoseBy(kind=DISQUALIFICATION|PINFALL, who)  # discard_only: blank only cards in the target's discard pile
  # Static decl: `who`'s cards matching `selector` fire no text & cannot stop while the source is in play ("your opponent's Spotlights are blank" — is_text_blanked). schema v27
BlankTextPermanent(selector, who)  # REST-OF-MATCH ("poison") blank — "Blank all Spotlights for the rest of the match" (ee0defe5).
  # EXECUTED once when the effect fires: resolves `who` to the absolute target and stamps a persistent (selector, owner) into GameState.permanent_blanks (is_text_blanked checks it) — survives the source leaving play & catches cards played later. "All" (both boards) = who=SELF + who=OPP. schema v139
Unblank(selector, who)  # "Un-blank your Finishes." — the inverse: RESTORES `who`'s cards matching `selector`, overriding every blank source for the rest of the match.
  # One-shot; records `selector` in PlayerState.text_unblank, which is_text_blanked checks FIRST (un-blank wins over a continuous BlankText or a stop's blanked_text). schema v117
CopyText(selector, who, zone=IN_PLAY|DISCARD, copy_tags)  # "this card copies the text of …" (Spotlight text-copy family: #2/#9/#16). Static decl read
                             # (never executed) by GameState.copied_effects, folded into standing_effects: the effects of every card matching
                             # `selector` in `who`'s `zone` are RE-HOMED onto the copier and fire for as long as this clause's own duration holds (a
                             # WHILE_IN_DISCARD copy projects them from the copier's discard), regardless of the source effect's original duration — the
                             # copier becomes the new `self`. Bounded against copy→copy recursion by copy_guard. "…then blanks them" (#2) is a paired
                             # BlankText, not a flag. copy_tags grafts the source's tags (its "Spotlight-ness"; #16, latched — designed, not yet modeled). schema v71
BlankStoppedText             # "the stopped card has blank text until the end of the turn" (21 cards). Blanks ONE card by IDENTITY into
                             # GameState.blanked_text (a selector scan cannot: the blanking stop card stays in play, so it would never end);
                             # cleared by the end-of-turn sweep. Resolved BEFORE the stopped card's own OnStop, so it suppresses that card's
                             # "If Stopped" text — the point of the family ("stop any X WITH 'If Stopped' in the text: …"). schema v36
BlankHitCard                 # "… that card has blank text" — blanks GameState.hit_card (the card that just triggered this OnHit) by identity,
                             # for as long as it stays IN PLAY (stamps blanked_in_play; is_text_blanked honours it only while a copy is in the
                             # owner's play area, so it self-expires on leave-play). Jax, Pet of the Year ("when your opponent hits a <named>
                             # card, that card has blank text and their next turn roll is -1"), on an OnHit{who:Opp, name_contains} gimmick. schema v148
FinishIfStop                 # FINISH-OFF-STOP: "if played as a Stop, this card is also a Finish". A passive marker read in apply_stop
                             # (maybe_finish_off_stop): when this stop lands SUCCESSFULLY it runs a full finish sequence off the stop — the
                             # stopper is the finisher, the stopped attacker the target (a breakout attempt that can end the match). Authored
                             # on an OnStop{Theirs} effect whose CONDITION carries the gate (Always for "if played as a Stop";
                             # StoppedCardNoLogoNoReq for "if the stopped card had no logo/skill requirement"), so a sibling CrowdMeter action
                             # ("the Crowd Meter is +N and …") is gated with it. Distinct from AlsoLead "also a Finish" (playability). schema v145
EndTurn                      # "… and end the current turn" (Boot Off the Apron / Capture Headlock / Take You for a Ride, on stopping a "Double
                             # Team" card). Executed: flags the ACTIVE player's turn_ended, which the turn loop's extra-play loop honours —
                             # cancelling any remaining PlayExtraCard grants (a stop already ends the turn otherwise). Authored on OnStop{Theirs}. schema v147
ChooseName(options)          # "Choose 1: <name>, <name>, or <name>" (Raven): bind one option for the match into PlayerState.chosen_name,
                             # authored under StartOfMatch. Read by the ChosenNameIs condition (§3), which resolves "that" name by gating one
                             # concrete effect per option, so exactly one is live. schema v37
DisqualificationRule(enabled, scope=SELF|MATCH)  # Static match-rule toggle (schema v8): enabled=false =
                                                 # "no disqualifications"; a DQ LoseBy is VOIDED when the loser
                                                 # is immune (self-scope owner, or any match-scope rule). In-play-
                                                 # scoped + condition-gated; last-played-order tie-break is task #93
ForceRandomDiscardMove(who)  # Static poison (schema v131): while the declaring card is in play/discard, the who-side (Bleeding Out:
                             # OPP = "an opponent") must resolve every card-/Gimmick-driven move out of their OWN discard RANDOMLY
                             # (no free choice of which to recur). Read at the discard-move choice sites via force_random_discard_move.
LockDiscard(who)             # Static poison (schema v132): an OPPONENT cannot move ANY card out of the who-side's discard pile
                             # (Split Personality: "your opponent cannot move other cards from your discard pile", who=SELF = the
                             # owner's own pile). Read at bury_from_discard via discard_move_locked. Stronger than ForceRandomDiscardMove.
ConsideredCompare(domain=SKILL|HAND, order=GREATER|LESS)  # Static meta-override (schema v16): the
                             # declaring player's vs-opponent SkillCompare (domain=SKILL) / HandSizeCompare
                             # (HAND) always resolves as `order` "for card effects", ignoring real values —
                             # strict (equality never holds). RaRa Perre (SKILL/GREATER), Theo V2 (HAND/LESS).
SuppressOpponentDraw         # Static decl (schema v21): "your opponent does not draw for your card effects"
                             # (Sami "The Draw") — a Draw(who=OPP) resolved by the declaring player is voided at act_draw.
CrowdMeter(delta)             PlayExtraCard(order?)         SetFinishRoll(value, condition)
FinishBonus(skill, delta)     BreakoutModifier(delta, attempts?, when_skill?, who=SELF, either=False, per?, per_who=SELF, per_zone=IN_PLAY, per_divisor?, cap?, per_excludes_self=False)  # when_skill gates to the rolled breakout skill (v79); who=OPP -> "your opponent's breakout rolls …" (v94); either -> symmetric "if either player rolls <S> for their breakout roll, their roll is +/-N", applies to whoever defends from either board (v107). per set = delta * floor((count of per_who's cards in per_zone matching the filter) / per_divisor), clamped to cap, per_excludes_self drops the source card — "your opponent's breakout rolls are +1 for each Stop they have in play"; the BreakoutModifier parallel of FinishRollBonus.per, ordinal clauses ("1st and 2nd") emit one per attempt index (schema v112)
GrantBreakoutBonus(delta, who=SELF)  # TIMED imperative "+delta to who's breakout rolls until the end of the turn" — accumulates onto the target's breakout_bonus_eot store (added for the defender by breakout_bonus), swept at the next turn start. Unlike Static BreakoutModifier it survives the source card leaving play (Mailman shuffles itself away as it grants). who: SELF (Mailman); OPP = "your opponent's breakout rolls are -N" (Why So Serious?!?, revealed as a Strike). schema v128 (who: v132)
BreakoutAttempts(delta, set?, who=SELF, per?, per_who=SELF, per_zone=IN_PLAY, per_divisor?, cap?, per_excludes_self=False)  # modifies the NUMBER of breakout rolls (not a roll's value — that's BreakoutModifier): base BREAKOUT_ATTEMPTS(3), `set` overrides it ("your opponent gets 2 Breakout rolls this turn"), `delta` shifts it ("gets 1 additional/fewer Breakout roll"); who=OPP -> "your opponent gets …", SELF -> "you get …". Read by breakout_attempts_for, which sums both boards (SELF from the defender + OPP from the finisher), takes the smallest `set`, and clamps to [1, base+7]; `per` scales `delta` per counted card like BreakoutModifier.per (schema v113)
FinishRollBonus(delta, when_skill?, either=False, per?, per_who=SELF, per_zone=IN_PLAY, per_divisor?, cap?, per_excludes_self=False, per_crowd=False)  # +delta to a Finish roll; when_skill gates on the rolled skill. either -> symmetric "if either player rolls <S> for their Finish roll", read from the opponent's board too (v107 consumes the field). `per` set = delta * floor((count of per_who's cards in per_zone matching the filter) / per_divisor) — "+1 per Spotlight in your opponent's discard" (schema v28); per_zone=FLIPPED_THIS_TURN for "+1 for each Strike card flipped", per_divisor=3 for "+1 for every 3 Strikes in play" (schema v74); cap clamps the per-count product ("(Max +2)"), per_excludes_self drops the source card ("for each OTHER <X> you have in play") — read via the source-threaded fold (schema v106). per_crowd -> a SECOND live-Crowd-Meter addend (clamped to cap), on top of the Crowd Meter the finish math already folds into every roll — "Your Finish roll is + the Crowd Meter (Max +N)" (schema v123)
TurnRollBonus(skill, delta, who=SelfSide, either=False, per_crowd=False, cap=None)   # Static self-decl: +delta to a TURN roll when the rolled skill == `skill` — "Your Power is +N during turn rolls". Read by turn_roll_bonus in the roll-off (parallel of FinishRollBonus/BreakoutModifier); phase-scoped, so it never touches finish rolls / stops / skill comparisons the way a plain BuffSkill would (schema v97). who -> whose roll, from the owner's POV: SelfSide = the owner's own; Opp = the owner's opponent's ("your opponent's <S> is -N during their turn rolls"); turn_roll_bonus sums a roller's own SelfSide mods with their opponent's Opp mods (v122). either -> symmetric "if either player rolls <S> for their turn roll, their roll is +/-N", picked up from the opponent's board too (v107). per_crowd -> dynamic delta = Crowd Meter clamped to cap ("your <S> is + the Crowd Meter (Max +N) during your turn roll"; the "During your turn roll:" header re-scoping a per_crowd BuffSkill so it stays roll-off-only, v118)
DoubleFinishIfBumped          # Static self-decl: double THIS card's Finish bonuses if the finisher bumped
DoubleFinishIf(condition)     # conditional generalization: double THIS card's Finish bonuses when `condition`
                              # holds ("… if you have another Submission in play / rolled Power"). Read in
                              # card_finish_bonus against the owner's turn-roll ctx (so RollWasSkill resolves). schema v77
RequireStops(count)           # this card can only be stopped by `count` Stops at once — the defender must commit
                              # `count` legal stops or it lands (King Brian Cage). Read in offer_stop. schema v80
AlsoAtkType(atk_type)         # this card ALSO counts as attack type `atk_type` beyond its printed type ("also a
                              # Finish Grapple", King Brian Cage). Read via Card::counts_as_atk_type at every
                              # atk-type test (stop-matching, CardFilter, hit gimmicks). schema v81
FinishRequires(kind, count)   # DEFENDER declaration: the opponent needs `count` cards of `kind`
                              # (CARDS|LEADS|FOLLOW_UPS) in play to LAND a Finish against you (D3 V1's "needs 3
                              # cards in play"). Read in playable_options; Stops bypass it. On top of the built-in
                              # FOLLOW_UPS×1 default to land a Finish. schema v125
HandToDeckTop(who, selector)  # look at `who`'s hand, move one chosen `selector` card to the top of `who`'s
                              # deck (D3 V1's Claw, who=Opp) — tempo/info denial, the target redraws it. schema v126
LowestRollWins                # Static marker (Fae): the roll-off is won by the lowest roll
```
`Bury(selector, count, who, random, source, per?, per_who=SELF)` moves `count` cards to the
**bottom of the deck** (schema v4). `source=DISCARD` (default) recycles the top `count` of the
**discard pile** (the pass-and-recycle bury); `source=HAND` is the card-text bury — "bury N cards
in [your/their] hand" — where the **hand owner chooses which** unless `random`. `per` scales
`count` by the count of `per_who`'s matching in-play cards ("bury 1 …for each Lead you have in
play" — Cardona; schema v83), mirroring `Draw`/`Discard`. `Flip(n)` moves
the **top `n` cards of the deck to the discard pile** (there is no "buried" zone — see §5). `RemoveFromPlay(selector, who, count)` moves up to `count` cards from
a player's **`in_play` board to their discard** ("Discard 1 card your opponent has in play");
the **acting** player chooses which matching card(s) — an aimed disruption, not random — and a
no-match board is a no-op. `RecurToDeckTop(selector, count)` puts **up to** `count` matching
cards from the **discard pile onto the top of the deck** (the owner picks how many and which);
it is the redraw-next-turn recycle, distinct from `ShuffleIntoDeck(selector, source)` which shuffles
one card into the deck from the **discard** (default) or, with `source=IN_PLAY`, from the owner's
**in-play** board ("shuffle 1 Follow Up you have in play into your deck" — Cardona; schema v83).
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
`docs/design/substrate-split.rst` §5); this reseeds existing golden logs and touches
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
  # ORDERING (srgpc.net): gimmicks that trigger DURING a turn roll resolve BEFORE the
  # winner is decided, and when both players have one, the player with the HIGHER TURN
  # ROLL resolves first (`Engine::roll_order`). Applies to the skill switch, the in-roll
  # boost, the re-roll offer and the post-roll OnRoll pass. An exact tie is undefined by
  # the rules, so the stable A-then-B order is kept (a tie bumps anyway).
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

**Decision protocol / wire form** (substrate split — `docs/design/substrate-split.rst`
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

### 8.1 Match record (`record.rs`) — the portable, publishable artifact

The log above is the **engine's** stream: loss-less, seeded, and internal. It is not
the thing a consumer ships. A `decision` event enumerates the deciding player's whole
hand in `legal`, so a published log leaks hidden state; the header is deliberately
free of an engine-version stamp (it would make the conformance goldens
commit-dependent); and it cannot be authored by hand.

A **match record** (`schemas/v1/match_record.schema.json`) is the interchange format
consumers persist, publish, and replay. One schema, two kinds:

```jsonc
{"schema_version": 1, "kind": "full|observer",
 "engine": {<version_info(): engine, commit, schemas, policies>},   // full only
 "meta": {created, source, match_type, notes},
 "players": {"A": {player, competitor, entrance, deck?}, "B": {...}},
 "frames": [ {seq, turn_no, active, crowd_meter, action, players:{A,B}} ],
 "result": {winner, reason, turns},
 "replay": {<session snapshot: seed + decks + seats + decisions>}}   // full only
```

A **frame** is one replay step: the *observable* (spectator) public state — both
boards, both discards, hand/deck **sizes** — plus the `action` that produced it, using
the same type names as the log events above. Frames are the projection of the log:
`decision` and `unsupported` events are dropped (not redacted), and a movement the log
marks `hidden` becomes a count with no card ids. The one decision an observer *can*
see is a passed turn, so a `turn_action` decision whose choice was `pass` projects to
a `pass` action carrying the seat alone (the bury it recycles is its own action). Nothing in a record, either kind,
carries a hidden zone's contents — that is what makes it publishable.

- **full** — engine-run. Carries `replay`, so the frames are *derivable*: a consumer
  may store the seed alone and rehydrate (`Session::restore` → `frames()`).
- **observer** — a real-life or other-platform match someone transcribed. Frames are
  the record; there is no seed and it is not re-simulatable. Optional per-frame fields
  (`hand_size`, `deck_size`, `gimmick_blanked`) and a `note` action exist so an
  importer never has to invent or distort what they saw.

A viewer walks `frames` and so plays both kinds identically. `MatchRecord::validate`
(`srg validate-record`, WASM `validate_record`) is the import gate: structural
consistency (dense `seq`, chronological turns, final frame agrees with `result`,
observer records carry no seed) with card-uuid resolution when a card DB is supplied.

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

**Substrate split & Rust end-state** (`docs/design/substrate-split.rst`). The modules
above divide into a **substrate** — the authoritative rules engine (`cards`, `loader`,
`effects`/`conditions`, `rules_parser`+`overrides`, `state`, `engine`, `finish`,
`stops`, `rng`, `gamelog`, `policy`, plus a new `session` for the wire protocol) — and
**consumers** on top (`cli`, `interactive`, `review`, `report/`, `analysis`, a future
MCP server / web / mobile). The boundary rule: **the substrate never imports a
consumer** (guarded by `import-linter`, then by the Rust crate graph). The end-state
moves the substrate + parser to a single **Rust `srg-core` crate** compiled to every
target (native console/MCP, WASM web, native mobile lib). The Python engine served as a
**transitional parity oracle** and has been **retired** (Phase 2, task #79) in favor of
frozen golden corpora — whole-engine logs plus the whole-DB parser golden — that Rust
reproduces Python-free. See §13.

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

Full detail: [`docs/design/substrate-split.rst`](docs/design/substrate-split.rst) (a
review artifact of the same class as this document). Summary of what it pins:

- **The boundary.** A **substrate** (authoritative rules engine + parser + a new
  `session` wire layer) below the line; **consumers** (console, MCP, interactive,
  review, report, web, mobile) above it. The substrate never imports a consumer.
- **The public API** — three layers: load/build (`load_index`, `resolve_deck`,
  `validate_deck`), batch/pure (`Engine::play`), and session/interactive (`Session`,
  the pausable **continuation state machine** driving the decision protocol, §7).
- **The engine goes Rust**, one crate compiled to every target (native + WASM),
  resolving the language-split delta by compiling one implementation N ways rather than
  trusting a second one. The Python engine was a **transitional parity oracle**, now
  **retired** (Phase 2, task #79) in favor of frozen golden corpora.
- **The conformance harness** was the migration's safety rail: same `(seed,
  decisions[])` → Python and Rust emitted byte-identical `GameLog` (enabled by the
  portable `splitmix64` RNG, §5), plus parser-parity on `cards.ir.json`. Post-retirement,
  Rust regresses against the **frozen** golden logs (`engine_conformance.rs`) and the
  **frozen** parser golden (`parser_parity.rs`), both in `cargo test` with no Python.
- **§3 and §8 are unchanged** — re-homed as language-neutral JSON contracts. Every
  delta this migration needs is additive (RNG note §5, protocol + reserved timing
  points §7, module/boundary note §9, this section).
