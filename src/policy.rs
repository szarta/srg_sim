//! Policies — where "player skill" lives (DESIGN.md §7).
//!
//! A [`Policy`] is handed a **decision point**, the **legal option set**, and the
//! live [`GameState`], and returns one option (or `None` to suspend, for a replay
//! that has run dry). The engine logs every consulted call as a `decision` event
//! (`point` + `legal` + `chosen` + `policy`), so the imitation-learning dataset
//! (§7, M4) falls out for free.
//!
//! Options are plain JSON values (a `kind` tag plus fields the engine maps back to
//! a card), so `legal`/`chosen` serialize straight into the log. The shipped set:
//!
//! * [`RandomPolicy`] — uniform over the legal set, drawn from the engine's one
//!   seeded stream (`state.rng`), so a random game is still reproducible by seed.
//! * [`HeuristicPolicy`] — small, transparent, playstyle-aware rules (build one
//!   chain while hoarding stops; spend stops on Finishes); the M1 baseline. Its
//!   [`Profile`] selects the validated player profiles — `Aggressive` (greedy
//!   builder), `Smart` (hoards stops, builds only holding a Finish), `Newbie`
//!   (greedy, misplays the stop/discard economy).
//! * [`ReplayPolicy`] — replays one side's recorded `chosen` options in order, so
//!   any recorded match (sim or human) reconstructs deterministically.
//!
//! [`Policies`] pairs a per-player policy for A and B and implements the engine's
//! [`Decider`] seam, routing each decision to the acting player's policy.

use crate::cards::Card;
use crate::conditions;
use crate::engine::Decider;
use crate::ir::{Action, AtkType, PlayOrder, Skill, Trigger};
use crate::state::GameState;
use serde_json::Value;
use std::cmp::Reverse;
use std::collections::VecDeque;

// ---------------------------------------------------------------------------
// The Policy trait + the Decider adapter
// ---------------------------------------------------------------------------

/// Chooses one legal option at each decision point (DESIGN.md §7). `None` means
/// the policy cannot answer (a replay run dry), which suspends the engine.
pub trait Policy {
    /// Return one element of `legal` for player `key` at `point`, or `None` to
    /// suspend. Only ever consulted when more than one option is legal.
    fn choose(
        &mut self,
        point: &str,
        legal: &[Value],
        state: &mut GameState,
        key: &str,
    ) -> Option<Value>;

    /// The policy name, recorded on the header and every `decision` event.
    fn name(&self) -> &str;
}

/// Pairs a per-player policy for A and B and adapts them to the engine's
/// [`Decider`] seam — the live-play driver behind the batch [`Engine::play`].
///
/// [`Engine::play`]: crate::engine::Engine::play
pub struct Policies {
    a: Box<dyn Policy>,
    b: Box<dyn Policy>,
}

impl Policies {
    /// Build from a policy for each side.
    pub fn new(a: Box<dyn Policy>, b: Box<dyn Policy>) -> Self {
        Self { a, b }
    }

    fn side(&mut self, viewer: &str) -> &mut Box<dyn Policy> {
        if viewer == "A" {
            &mut self.a
        } else {
            &mut self.b
        }
    }
}

impl Decider for Policies {
    fn decide(
        &mut self,
        point: &str,
        viewer: &str,
        legal: &[Value],
        state: &mut GameState,
    ) -> Option<Value> {
        self.side(viewer).choose(point, legal, state, viewer)
    }

    fn policy_name(&self, viewer: &str) -> String {
        if viewer == "A" {
            self.a.name()
        } else {
            self.b.name()
        }
        .to_owned()
    }
}

/// Build a named policy (`random` | `heuristic` | `aggressive` | `smart` |
/// `newbie`), or `None` for an unknown name.
/// Every AI policy name [`build_policy`] accepts — the opponent roster the console
/// (`srg`) and web client offer. Keep in lockstep with the `build_policy` match.
pub const POLICIES: &[&str] = &[
    "random",
    "heuristic",
    "aggressive",
    "smart",
    "newbie",
    "killshot",
];

pub fn build_policy(name: &str) -> Option<Box<dyn Policy>> {
    match name {
        "random" => Some(Box::new(RandomPolicy::new())),
        "heuristic" => Some(Box::new(HeuristicPolicy::heuristic())),
        "aggressive" => Some(Box::new(HeuristicPolicy::aggressive())),
        "smart" => Some(Box::new(HeuristicPolicy::smart())),
        "newbie" => Some(Box::new(HeuristicPolicy::newbie())),
        "killshot" => Some(Box::new(HeuristicPolicy::killshot())),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// RandomPolicy
// ---------------------------------------------------------------------------

/// Uniform choice over the legal set, using the engine's seeded RNG (so a random
/// game is still reproducible by seed).
pub struct RandomPolicy {
    name: String,
}

impl RandomPolicy {
    pub fn new() -> Self {
        Self {
            name: "random".to_owned(),
        }
    }
}

impl Default for RandomPolicy {
    fn default() -> Self {
        Self::new()
    }
}

impl Policy for RandomPolicy {
    fn choose(
        &mut self,
        _point: &str,
        legal: &[Value],
        state: &mut GameState,
        _key: &str,
    ) -> Option<Value> {
        state.rng.reveal(legal).cloned()
    }

    fn name(&self) -> &str {
        &self.name
    }
}

// ---------------------------------------------------------------------------
// ReplayPolicy
// ---------------------------------------------------------------------------

/// Replays one side's recorded `chosen` options in order (DESIGN.md §8). The
/// engine consults a policy only when more than one option is legal — the same
/// predicate that gates whether a `decision` event is logged — so the recorded
/// list lines up one-for-one with the calls. Returns `None` once exhausted, which
/// suspends the engine (the recorded stream and the re-run engine have diverged).
pub struct ReplayPolicy {
    decisions: VecDeque<Value>,
    name: String,
}

impl ReplayPolicy {
    pub fn new(decisions: Vec<Value>) -> Self {
        Self {
            decisions: decisions.into_iter().collect(),
            name: "replay".to_owned(),
        }
    }
}

impl Policy for ReplayPolicy {
    fn choose(
        &mut self,
        _point: &str,
        _legal: &[Value],
        _state: &mut GameState,
        _key: &str,
    ) -> Option<Value> {
        self.decisions.pop_front()
    }

    fn name(&self) -> &str {
        &self.name
    }
}

// ---------------------------------------------------------------------------
// HeuristicPolicy + player profiles
// ---------------------------------------------------------------------------

/// Which validated player profile a [`HeuristicPolicy`] plays (todo #32).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// Builds one chain greedily onto whatever board it has (the M1 baseline).
    Aggressive,
    /// Hoards stops via pass+bury; builds only when holding a Finish.
    Smart,
    /// Greedy, no pass/bury game, misplays the stop/discard economy.
    Newbie,
    /// Plays Mastermind's **Kill Shot line** (Olympics pod). The line keeps the board
    /// **Grapple-free** — it builds only with Strikes and Submissions — so the
    /// opponent's see-1 Grapple stop ("if your opponent has another Grapple in play,
    /// stop any Grapple", deck #19) has no trigger, leaving a lone Grapple finish
    /// unstoppable (its keyed card-13 stop is also skill-offline vs most of the
    /// field). It fires ONLY the Grapple finish (Kill Shot) — declining the Strike/Sub
    /// finishes, which the SAME Strike+Sub board makes see-1-stoppable — and each
    /// un-stopped attempt clears the boards and raises the Crowd Meter, climbing the
    /// ladder toward auto-success. Selected by name `killshot`; never the default, so
    /// the M1 conformance goldens (which never name it) are untouched.
    KillShot,
}

/// A transparent, playstyle-aware baseline. Offense: win when a Finish is playable;
/// else build one chain minimally, committing the least valuable card and holding
/// online stops back — then pass to gather stops. Defense: spend a stop on the real
/// threat (a Finish) and let Leads / Follow Ups resolve.
pub struct HeuristicPolicy {
    name: String,
    profile: Profile,
}

impl HeuristicPolicy {
    /// The base heuristic (name `heuristic`, the `Aggressive` profile).
    pub fn heuristic() -> Self {
        Self::profile("heuristic", Profile::Aggressive)
    }

    /// The validated aggressive builder (== the M1 baseline).
    pub fn aggressive() -> Self {
        Self::profile("aggressive", Profile::Aggressive)
    }

    /// The smart passer — hoards stops, builds only when holding a Finish.
    pub fn smart() -> Self {
        Self::profile("smart", Profile::Smart)
    }

    /// The newbie — greedy, misplays the stop/discard economy.
    pub fn newbie() -> Self {
        Self::profile("newbie", Profile::Newbie)
    }

    /// The Kill Shot pilot — commits to the Grapple finish line (todo: Olympics pod).
    pub fn killshot() -> Self {
        Self::profile("killshot", Profile::KillShot)
    }

    fn profile(name: &str, profile: Profile) -> Self {
        Self {
            name: name.to_owned(),
            profile,
        }
    }

    // -- decision points ---------------------------------------------------

    /// Keep a hand that can open (has a Lead); otherwise redraw once.
    fn at_mulligan(&self, legal: &[Value], state: &GameState, key: &str) -> Value {
        let has_lead = state.players[key]
            .hand
            .iter()
            .any(|c| c.play_order == PlayOrder::Lead);
        or_first(
            by_kind(legal, if has_lead { "keep" } else { "redraw" }),
            legal,
        )
    }

    fn at_turn_action(&self, legal: &[Value], state: &GameState, key: &str) -> Value {
        // Finish selection. KillShot fires ONLY the Grapple finish (Kill Shot): on its
        // Grapple-free board it is the one finish the see-1 stop field can't touch, and
        // each un-stopped attempt climbs the Crowd Meter ladder. It declines the
        // Strike/Sub finishes (their see-1 stops ARE armed by our own Strike+Sub
        // board). Every other profile takes any playable Finish — the M1 baseline,
        // byte-for-byte unchanged.
        let finish = match self.profile {
            Profile::KillShot => find_grapple_finish(legal, state, key),
            _ => find_play(legal, "Finish"),
        };
        if let Some(finish) = finish {
            return finish.clone(); // go for the win
        }
        match self.profile {
            Profile::Smart if !holds_finish(state, key) => {}
            Profile::Newbie => {
                if let Some(need) = next_build_order(&state.players[key].in_play) {
                    if let Some(b) = first_nonstop(legal, need, state, key) {
                        return b.clone(); // play a card just to play it — no board read
                    }
                }
            }
            _ => {
                if let Some(candidate) = self.build_candidate(legal, state, key) {
                    return candidate;
                }
            }
        }
        or_first(by_kind(legal, "pass"), legal) // hold stops, pass to gather more
    }

    /// The cheapest builder for the chain's next needed link, if any. The `KillShot`
    /// line keeps the board **Grapple-free** (Strikes/Submissions only) so a lone
    /// Kill Shot can't be see-1-stopped — Grapple builders are excluded even at the
    /// cost of passing a turn when only Grapples are on offer.
    fn build_candidate(&self, legal: &[Value], state: &GameState, key: &str) -> Option<Value> {
        let need = next_build_order(&state.players[key].in_play)?;
        let grapple_free = self.profile == Profile::KillShot;
        let opts: Vec<Value> = legal
            .iter()
            .filter(|o| okind(o) == "play" && oorder(o) == need)
            .filter(|o| {
                !grapple_free || hand_card(state, key, ocard(o)).atk_type != AtkType::Grapple
            })
            .cloned()
            .collect();
        cheapest_builder(&opts, state, key)
    }

    fn at_stop(&self, legal: &[Value], state: &GameState, key: &str) -> Value {
        let stops: Vec<&Value> = legal.iter().filter(|o| okind(o) == "stop").collect();
        if stops.is_empty() {
            return legal[0].clone();
        }
        if self.profile == Profile::Newbie {
            return stops[0].clone(); // panics: stops the first threat, wastes it
        }
        let incoming_finish = ovs_order(&legal[0]) == "Finish";
        if self.profile == Profile::KillShot {
            return self.at_stop_killshot(&stops, incoming_finish, state, key, legal);
        }
        if incoming_finish {
            return stops[0].clone(); // the real threat — spend a stop
        }
        legal[0].clone() // let a Lead / Follow Up resolve; save the stop
    }

    /// The Kill Shot line's defensive stop economy: the finish-answering stops (#13–15
    /// finish-stops, #25–27 bury-stops) are MM's ONLY answer to the opponent's
    /// finishes, so they are reserved for finishes — never spent on a Lead/Follow Up.
    /// A Lead/Follow Up is instead disrupted with a **cheap, non-Grapple** stop (keeps
    /// the board Grapple-free so a lone Kill Shot stays see-1-proof); if none is
    /// available, the attack is let through rather than burning a reserved stop.
    fn at_stop_killshot(
        &self,
        stops: &[&Value],
        incoming_finish: bool,
        state: &GameState,
        key: &str,
        legal: &[Value],
    ) -> Value {
        if incoming_finish {
            return stops[0].clone(); // the real threat — spend the reserved stop
        }
        stops
            .iter()
            .find(|o| {
                let c = hand_card(state, key, ocard(o));
                !is_reserved_stop(c.number) && c.atk_type != AtkType::Grapple
            })
            .map_or_else(|| legal[0].clone(), |o| (*o).clone())
    }

    fn at_bury(&self, legal: &[Value], state: &GameState, key: &str) -> Value {
        if self.profile == Profile::Newbie {
            return legal[0].clone(); // recycles carelessly — no plan
        }
        // Recycle the most valuable discard card (Finish > stop > dead); on a tie,
        // the earliest option (Python `max` keeps the first maximum). The pool may span
        // the opponent's discard ("bury N in your opponent's discard") or either pile
        // (Cherry Glamazon), so each card is looked up in its OWN pile — the option's
        // `owner`, defaulting to `key` for the own-discard pass (`do_pass`).
        let killshot = self.profile == Profile::KillShot;
        let best = (0..legal.len())
            .max_by_key(|&i| {
                let owner = oowner(&legal[i]).unwrap_or(key);
                let card = discard_card(state, owner, ocard(&legal[i]));
                // KillShot recurs its Grapple finish ahead of any other finish; a
                // constant `false` for other profiles leaves their ordering intact.
                let grapple_fin = killshot && is_grapple_finish(card);
                (recycle_value(card), grapple_fin, Reverse(i))
            })
            .unwrap();
        legal[best].clone()
    }

    fn at_discard(&self, legal: &[Value], state: &GameState, key: &str) -> Value {
        if self.profile == Profile::Newbie {
            return legal[0].clone(); // sheds carelessly (leftmost) — may pitch a Finish
        }
        // Shed the least valuable card; on a tie, the earliest option (Python `min`
        // keeps the first minimum, which `min_by_key` also does). KillShot breaks ties
        // to keep its Grapple finish longest (a constant `false` for other profiles
        // leaves their ordering intact).
        let killshot = self.profile == Profile::KillShot;
        let worst = (0..legal.len())
            .min_by_key(|&i| {
                let card = hand_card(state, key, ocard(&legal[i]));
                let grapple_fin = killshot && is_grapple_finish(card);
                (discard_keep_value(card, state, key), grapple_fin)
            })
            .unwrap();
        legal[worst].clone()
    }

    /// Dismantler's "discard any number of cards from your hand": keep shedding the least
    /// valuable card until the opponent's hand would be stripped — the count
    /// `D* = opp_hand - offset` the engine used to force (D-1 ≥ opp_hand empties them for
    /// offset -1) — then stop. Each option carries `discarded`/`opp_hand`/`offset`. A
    /// no-op-to-continue past D* keeps the actor's remaining cards.
    fn at_coupled_discard(&self, legal: &[Value], state: &GameState, key: &str) -> Value {
        let hint = |k: &str| legal[0].get(k).and_then(Value::as_i64).unwrap_or(0);
        let discarded = hint("discarded");
        let target = (hint("opp_hand") - hint("offset")).max(0);
        if discarded >= target {
            return or_first(by_kind(legal, "none"), legal); // stop — keep the rest
        }
        // Shed the least valuable card (mirrors at_discard); never the "none" option.
        let worst = (0..legal.len())
            .filter(|&i| okind(&legal[i]) != "none")
            .min_by_key(|&i| {
                discard_keep_value(hand_card(state, key, ocard(&legal[i])), state, key)
            })
            .unwrap();
        legal[worst].clone()
    }

    /// The effect owner burying the OPPONENT's hand (The Man from I.T.): disrupt the
    /// most valuable card, looked up in the opponent's hand (the pool owner). Negated
    /// `min_by_key` keeps the FIRST maximum on a tie, matching Python `max`.
    fn at_bury_opp_hand(&self, legal: &[Value], state: &GameState, key: &str) -> Value {
        let owner = state.opponent_of(key);
        let best = (0..legal.len())
            .min_by_key(|&i| {
                -discard_keep_value(hand_card(state, &owner, ocard(&legal[i])), state, &owner)
            })
            .unwrap();
        legal[best].clone()
    }

    fn at_optional(&self, legal: &[Value]) -> Value {
        or_first(by_kind(legal, "yes"), legal) // take optional edges (reroll / buff)
    }

    /// Spend an elective same-skill bump (Ringside Ruckus / "The Floats") only when
    /// behind on the roll — EXCEPT never when the opponent sign-flips our gimmick
    /// (Cassandra): a bump fires our OnBump gimmick, and flipped it turns Mastermind's
    /// "opponent's next roll -2" into a +2 for them, so the elective bump backfires.
    fn at_elect_bump(&self, legal: &[Value], state: &GameState, key: &str) -> Value {
        if self.profile == Profile::KillShot && opp_flips_gimmick_signs(state, key) {
            return or_first(by_kind(legal, "no"), legal); // don't Float into a sign-flip
        }
        let losing = legal.iter().any(|o| {
            okind(o) == "yes" && o.get("losing").and_then(Value::as_bool).unwrap_or(false)
        });
        or_first(by_kind(legal, if losing { "yes" } else { "no" }), legal)
    }

    /// Take Pretty Paul's once-per-match bump replacement whenever it is offered — a
    /// tie is about to force a bump either way, and the replacement strictly draws an
    /// extra card, denies the opponent their bump draw, and skips both `OnBump`
    /// gimmicks. A finesse "save it for the sign-flip" read is left to a human pilot.
    fn at_bump_replace(&self, legal: &[Value]) -> Value {
        or_first(by_kind(legal, "yes"), legal)
    }
}

impl Policy for HeuristicPolicy {
    fn choose(
        &mut self,
        point: &str,
        legal: &[Value],
        state: &mut GameState,
        key: &str,
    ) -> Option<Value> {
        let chosen = match point {
            "mulligan" => self.at_mulligan(legal, state, key),
            "turn_action" => self.at_turn_action(legal, state, key),
            "stop" => self.at_stop(legal, state, key),
            "bury" => self.at_bury(legal, state, key),
            // Burying from hand is the affected player shedding a hand card (to the
            // deck bottom) — same "drop your least valuable" read as a discard.
            "bury_hand" => self.at_discard(legal, state, key),
            "bury_opp_hand" => self.at_bury_opp_hand(legal, state, key),
            "skill" => at_choose_skill(legal, state, key),
            "discard" => self.at_discard(legal, state, key),
            "coupled_discard" => self.at_coupled_discard(legal, state, key),
            "reveal" => at_reveal(legal, state, key),
            "optional" => self.at_optional(legal),
            "elect_bump" => self.at_elect_bump(legal, state, key),
            "bump_replace" => self.at_bump_replace(legal),
            _ => legal[0].clone(),
        };
        Some(chosen)
    }

    fn name(&self) -> &str {
        &self.name
    }
}

// ---------------------------------------------------------------------------
// Free helpers — option fields, card lookups, valuations
// ---------------------------------------------------------------------------

fn okind(o: &Value) -> &str {
    o.get("kind").and_then(Value::as_str).unwrap_or("")
}

fn oorder(o: &Value) -> &str {
    o.get("order").and_then(Value::as_str).unwrap_or("")
}

fn ocard(o: &Value) -> &str {
    o.get("card").and_then(Value::as_str).unwrap_or("")
}

/// The pile owner an option carries (`bury_from_discard` tags each candidate), or
/// `None` for own-pile options (`do_pass`).
fn oowner(o: &Value) -> Option<&str> {
    o.get("owner").and_then(Value::as_str)
}

fn ovs_order(o: &Value) -> &str {
    o.get("vs_order").and_then(Value::as_str).unwrap_or("")
}

fn by_kind<'a>(legal: &'a [Value], kind: &str) -> Option<&'a Value> {
    legal.iter().find(|o| okind(o) == kind)
}

/// The chosen option, or the first legal option as a fallback (mirrors Python's
/// `_by_kind(...) or legal[0]`).
fn or_first(chosen: Option<&Value>, legal: &[Value]) -> Value {
    chosen.unwrap_or(&legal[0]).clone()
}

fn find_play<'a>(legal: &'a [Value], order: &str) -> Option<&'a Value> {
    legal
        .iter()
        .find(|o| okind(o) == "play" && oorder(o) == order)
}

/// A playable **Grapple** Finish option — Mastermind's Kill Shot. The `KillShot`
/// profile fires only this: on a Grapple-free board it is the finish the see-1 stop
/// field can't touch. `None` if no Grapple finish is playable (then the line holds).
fn find_grapple_finish<'a>(legal: &'a [Value], state: &GameState, key: &str) -> Option<&'a Value> {
    legal.iter().find(|o| {
        okind(o) == "play"
            && oorder(o) == "Finish"
            && hand_card(state, key, ocard(o)).atk_type == AtkType::Grapple
    })
}

/// True iff `card` is a Grapple finish (the Kill Shot line's payload).
fn is_grapple_finish(card: &Card) -> bool {
    card.play_order == PlayOrder::Finish && card.atk_type == AtkType::Grapple
}

/// Choose which hand card to REVEAL (fog-of-war "each player reveals N in their
/// hand"): expose the card that leaks the LEAST information. See the reveal-decision
/// heuristic — lower [`reveal_rank`] wins, ties keep the earliest option.
fn at_reveal(legal: &[Value], state: &GameState, key: &str) -> Value {
    // Empty deck: the opponent can already infer the whole hand from public zones
    // (discard + board), so revealing anything leaks nothing new — take the first.
    if state.players[key].deck.is_empty() {
        return legal[0].clone();
    }
    let best = (0..legal.len())
        .min_by_key(|&i| reveal_rank(hand_card(state, key, ocard(&legal[i])), state, key))
        .unwrap();
    legal[best].clone()
}

/// Reveal priority for one card (1 = most preferred to reveal … 8 = least): already
/// known > dead equal-8 stop > non-stop Lead > non-stop Follow Up > Lead Stop >
/// Finish > Follow Up Stop > anything else.
fn reveal_rank(card: &Card, state: &GameState, key: &str) -> i64 {
    if state.players[key].revealed_hand.contains(&card.db_uuid) {
        return 1; // already shown — zero new information
    }
    // A "dead" equal-8 stop (#13-15): its skill-requirement stat is below the
    // opponent's, so the stop cannot win — nothing lost by showing it.
    if (13..=15).contains(&card.number) {
        if let Some(req) = card.skill_requirements.first() {
            let opp = state.opponent_of(key);
            if state.effective_stat(key, req.skill, None)
                < state.effective_stat(&opp, req.skill, None)
            {
                return 2;
            }
        }
    }
    let is_stop = card_is_stop(card);
    match card.play_order {
        PlayOrder::Lead if !is_stop => 3,
        PlayOrder::Followup if !is_stop => 4,
        PlayOrder::Lead => 5,     // Lead Stop
        PlayOrder::Finish => 6,   // Finish
        PlayOrder::Followup => 7, // Follow Up Stop
        PlayOrder::None => 8,
    }
}

/// Whether `card` carries a Stop action.
fn card_is_stop(card: &Card) -> bool {
    card.effects
        .iter()
        .any(|e| e.actions.iter().any(|a| matches!(a, Action::Stop { .. })))
}

/// True iff a stop's deck number is a finish-answering stop the Kill Shot line
/// reserves for finishes: the #13–15 keyed finish-stops and the #25–27 bury-stops.
fn is_reserved_stop(number: i64) -> bool {
    (13..=15).contains(&number) || (25..=27).contains(&number)
}

/// True iff `key`'s opponent has an active `Static` `FlipGimmickSigns` (Cassandra
/// negating every printed +/- on `key`'s gimmick) — mirrors the engine's
/// `gimmick_signs_flipped`. When set, `key`'s OnBump gimmick works AGAINST them, so
/// the Kill Shot pilot declines the elective (Floats) bump.
fn opp_flips_gimmick_signs(state: &GameState, key: &str) -> bool {
    let opp = state.opponent_of(key);
    if state.is_gimmick_blanked(&opp) {
        return false;
    }
    state.players[&opp].competitor.effects.iter().any(|eff| {
        matches!(eff.trigger, Trigger::Static)
            && eff
                .actions
                .iter()
                .any(|a| matches!(a, Action::FlipGimmickSigns { .. }))
    })
}

/// "Choose a skill" (Catch These Hands): pick the opponent's strongest skill, so the -1
/// debuff bites hardest — and, since the sibling "next time you roll that skill, draw"
/// keys off the same choice, we can steer a turn roll to it. Ties keep the first legal
/// option (canonical skill order).
fn at_choose_skill(legal: &[Value], state: &GameState, key: &str) -> Value {
    let opp = state.opponent_of(key);
    let stats = state.players[&opp].competitor.stats;
    let stat_of = |o: &Value| -> i64 {
        let name = o.get("skill").and_then(Value::as_str).unwrap_or("");
        Skill::ALL
            .iter()
            .find(|s| s.name() == name)
            .map_or(i64::MIN, |s| stats.get(*s))
    };
    // Highest opponent stat; `Reverse(i)` breaks ties toward the earliest legal option.
    (0..legal.len())
        .max_by_key(|&i| (stat_of(&legal[i]), Reverse(i)))
        .map(|i| legal[i].clone())
        .unwrap_or_else(|| legal[0].clone())
}

fn holds_finish(state: &GameState, key: &str) -> bool {
    state.players[key]
        .hand
        .iter()
        .any(|c| c.play_order == PlayOrder::Finish)
}

/// The next chain link to commit: a Lead if none in play, then a Follow Up; `None`
/// once the chain is Lead+Follow Up (wait to draw a Finish).
fn next_build_order(board: &[Card]) -> Option<&'static str> {
    if !board.iter().any(|c| c.play_order == PlayOrder::Lead) {
        return Some("Lead");
    }
    if !board.iter().any(|c| c.play_order == PlayOrder::Followup) {
        return Some("Followup");
    }
    None
}

/// The card we'd most willingly commit to build a chain: the least valuable
/// (non-stop < offline stop). `None` if the only options are ONLINE stops — never
/// spent offensively.
fn cheapest_builder(opts: &[Value], state: &GameState, key: &str) -> Option<Value> {
    if opts.is_empty() {
        return None;
    }
    let best = (0..opts.len())
        .min_by_key(|&i| play_value(hand_card(state, key, ocard(&opts[i])), state, key))
        .unwrap();
    let card = hand_card(state, key, ocard(&opts[best]));
    (play_value(card, state, key) < 2).then(|| opts[best].clone())
}

/// The first playable builder of the needed order that is NOT a stop (a newbie
/// never plays a stop offensively, but plays any non-stop card just to play it).
fn first_nonstop<'a>(
    legal: &'a [Value],
    need: &str,
    state: &GameState,
    key: &str,
) -> Option<&'a Value> {
    legal.iter().find(|o| {
        okind(o) == "play" && oorder(o) == need && !has_stop_effect(hand_card(state, key, ocard(o)))
    })
}

fn hand_card<'a>(state: &'a GameState, key: &str, uuid: &str) -> &'a Card {
    state.players[key]
        .hand
        .iter()
        .find(|c| c.db_uuid == uuid)
        .expect("chosen card is in hand")
}

fn discard_card<'a>(state: &'a GameState, key: &str, uuid: &str) -> &'a Card {
    state.players[key]
        .discard
        .iter()
        .find(|c| c.db_uuid == uuid)
        .expect("chosen card is in discard")
}

fn has_stop_effect(card: &Card) -> bool {
    card.effects
        .iter()
        .any(|eff| eff.actions.iter().any(|a| matches!(a, Action::Stop { .. })))
}

fn stop_online(card: &Card, state: &GameState, key: &str) -> bool {
    card.effects.iter().any(|eff| {
        eff.actions.iter().any(|a| matches!(a, Action::Stop { .. }))
            && conditions::holds(&eff.condition, state, key, None)
    })
}

/// How reluctant we are to play a card (0 spend freely … 2 hold): a non-stop is 0,
/// an offline stop 1, an online stop 2 (keep it for defense).
fn play_value(card: &Card, state: &GameState, key: &str) -> i64 {
    if !has_stop_effect(card) {
        return 0;
    }
    if stop_online(card, state, key) {
        2
    } else {
        1
    }
}

/// How reluctant we are to discard a hand card (higher = keep longer): Finish >
/// needed chain piece > online stop > offline stop > dead card.
fn discard_keep_value(card: &Card, state: &GameState, key: &str) -> i64 {
    if card.play_order == PlayOrder::Finish {
        return 4;
    }
    if needed_piece(card, state, key) {
        return 3;
    }
    if has_stop_effect(card) {
        return if stop_online(card, state, key) { 2 } else { 1 };
    }
    0
}

/// A Lead / Follow Up whose order the board still needs to advance the chain.
fn needed_piece(card: &Card, state: &GameState, key: &str) -> bool {
    next_build_order(&state.players[key].in_play) == Some(card.play_order.name())
}

/// Priority for recycling a discard card back into the deck (higher = keep): a
/// Finish (re-attempt) over a stop (re-defend) over a dead card.
fn recycle_value(card: &Card) -> i64 {
    if card.play_order == PlayOrder::Finish {
        3
    } else if has_stop_effect(card) {
        2
    } else {
        1
    }
}
