//! Turn loop, effect executor, stop resolution, finish sequence (DESIGN.md §6),
//! as a **resumable state machine** (`docs/design/substrate-split.md` §3.3/§4).
//!
//! The Python engine calls `policy.choose(...)` synchronously at each decision
//! point (`engine.py::_decide`). Here that one call becomes a **yield point**:
//! [`Engine::decide`] consults a [`Decider`]; if the decider has an answer the
//! engine continues, otherwise it returns `Err(`[`Yield`]`)` carrying a
//! [`DecisionRequest`], which propagates up through every `?` to the driver.
//!
//! Driven by a [`ReplayDecider`] over a recorded `decisions[]` list, this is the
//! **replay-from-seed** engine: deterministic, WASM-safe (no threads/coroutines),
//! and snapshot = `(seed, decisions[])`. The turn loop, executor, and finish
//! sequence land in sibling sub-modules; this file is the scaffold + leaf layer.
//!
//! (Construction in progress — task 72a of the split; `#![allow(dead_code)]`
//! stays until the loop is wired end-to-end in 72e.)
#![allow(dead_code)]

use crate::cards::{Card, Deck};
use crate::conditions::{self, RollContext};
use crate::gamelog::{CardMovement, Event, GameLog, Header, PlayerInfo};
use crate::ir::{
    Action, CardFilter, ChoiceOption, Condition, DeckEnd, Dest, Duration, Effect, LoseKind,
    RollWhen, Skill, Trigger, Who,
};
use crate::rng::SeededRNG;
use crate::skills::Skills;
use crate::state::{GameState, PlayerState};
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub const OPENING_HAND: usize = 3;
pub const HAND_CAP: i64 = 10;
pub const BREAKOUT_ATTEMPTS: usize = 3;
pub const TURN_CAP: i64 = 400;
pub const MAX_TIE_REROLLS: i64 = 64;

// ---------------------------------------------------------------------------
// Result / decision-protocol types
// ---------------------------------------------------------------------------

/// The match outcome (DESIGN.md §6). `winner` is a player key or `"draw"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameResult {
    pub winner: String,
    pub reason: String, // finish | count_out | disqualification | pinfall | turn_cap
    pub turns: i64,
}

/// Server → client: the engine has suspended awaiting one player's choice
/// (`docs/design/substrate-split.md` §4). Its `point`/`legal`/`chosen` fields are
/// the §8 `decision` event; `observable_state` is `GameState::observable(viewer)`.
#[derive(Debug, Clone, PartialEq)]
pub struct DecisionRequest {
    pub request_id: String,
    pub seq: u64,
    pub viewer: String,
    pub point: String,
    pub legal: Vec<Value>,
    pub observable_state: Value,
}

/// Client → server: the player's choice (one element of `legal`).
#[derive(Debug, Clone, PartialEq)]
pub struct DecisionResponse {
    pub request_id: String,
    pub chosen: Value,
}

/// One step of the resumable engine.
#[derive(Debug, Clone)]
pub enum Step {
    /// The engine is parked, awaiting a decision.
    Decision(DecisionRequest),
    /// The match finished.
    Done(GameResult),
}

/// The internal suspension signal: propagated up through `?` when the decider
/// has no answer for the outstanding decision, so the driver can surface it. The
/// request is boxed to keep the `Err` variant of [`Eng`] small.
#[derive(Debug, Clone)]
pub struct Yield(pub Box<DecisionRequest>);

/// Every engine method that can reach a decision point returns this: `Ok(value)`
/// to continue, `Err(Yield)` to suspend.
pub type Eng<T> = Result<T, Yield>;

// ---------------------------------------------------------------------------
// The "who decides" seam
// ---------------------------------------------------------------------------

/// The decision provider — the externalized `_decide` (task 73 supplies policy
/// impls; 72e drives it as a replay-from-seed loop).
pub trait Decider {
    /// The chosen option for a multi-option decision point, or `None` to suspend
    /// (the driver then yields a [`DecisionRequest`] and resumes on `submit`).
    fn decide(&mut self, point: &str, viewer: &str, legal: &[Value]) -> Option<Value>;

    /// The policy name recorded on the §8 `decision` event for `viewer`.
    fn policy_name(&self, viewer: &str) -> String;
}

/// Replays a recorded `decisions[]` list (per player), suspending when it runs
/// dry — the replay-from-seed driver behind [`Step`]/`submit`.
#[derive(Debug, Clone, Default)]
pub struct ReplayDecider {
    /// Per-player queue of recorded choices (front = next).
    decisions: BTreeMap<String, std::collections::VecDeque<Value>>,
    /// Per-player policy name (for the `decision` event's `policy` field).
    policies: BTreeMap<String, String>,
}

impl ReplayDecider {
    /// Build from `{player: [chosen, …]}` decisions and `{player: policy_name}`.
    pub fn new(
        decisions: BTreeMap<String, Vec<Value>>,
        policies: BTreeMap<String, String>,
    ) -> Self {
        Self {
            decisions: decisions
                .into_iter()
                .map(|(k, v)| (k, v.into_iter().collect()))
                .collect(),
            policies,
        }
    }
}

impl Decider for ReplayDecider {
    fn decide(&mut self, _point: &str, viewer: &str, _legal: &[Value]) -> Option<Value> {
        self.decisions.get_mut(viewer).and_then(|q| q.pop_front())
    }

    fn policy_name(&self, viewer: &str) -> String {
        self.policies.get(viewer).cloned().unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// Sign-flip transform (Cassandra's FlipGimmickSigns)
// ---------------------------------------------------------------------------

/// Negate the printed +/- modifier on one action, recursing into a `Choice`'s
/// branches; anything without a signed delta is returned unchanged.
fn negate_action(action: &Action) -> Action {
    match action {
        Action::Choice { options } => Action::Choice {
            options: options
                .iter()
                .map(|o| ChoiceOption {
                    node_type: o.node_type,
                    label: o.label.clone(),
                    actions: o.actions.iter().map(negate_action).collect(),
                })
                .collect(),
        },
        Action::ModifyRoll {
            who,
            delta,
            when,
            per,
            per_who,
        } => Action::ModifyRoll {
            who: *who,
            delta: -*delta,
            when: *when,
            per: per.clone(),
            per_who: *per_who,
        },
        Action::BuffSkill {
            skill,
            delta,
            who,
            duration,
            target_highest,
            per_crowd,
            cap,
        } => Action::BuffSkill {
            skill: *skill,
            delta: -*delta,
            who: *who,
            duration: *duration,
            target_highest: *target_highest,
            per_crowd: *per_crowd,
            cap: *cap,
        },
        Action::CrowdMeter { delta } => Action::CrowdMeter { delta: -*delta },
        Action::MaxHandSize {
            delta,
            who,
            duration,
        } => Action::MaxHandSize {
            delta: -*delta,
            who: *who,
            duration: *duration,
        },
        Action::FinishBonus { skill, delta } => Action::FinishBonus {
            skill: *skill,
            delta: -*delta,
        },
        Action::FinishRollBonus {
            delta,
            when_skill,
            either,
        } => Action::FinishRollBonus {
            delta: -*delta,
            when_skill: *when_skill,
            either: *either,
        },
        Action::BreakoutModifier { delta, attempts } => Action::BreakoutModifier {
            delta: -*delta,
            attempts: *attempts,
        },
        other => other.clone(),
    }
}

/// A copy of `effect` with every printed +/- modifier negated — the transform
/// Cassandra's `FlipGimmickSigns` applies to the opponent's gimmick.
fn flip_signs(effect: &Effect) -> Effect {
    let mut out = effect.clone();
    out.actions = effect.actions.iter().map(negate_action).collect();
    out
}

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

/// Plays a single match to completion (DESIGN.md §6 turn loop), driven by a
/// [`Decider`].
pub struct Engine {
    decks: BTreeMap<String, Deck>,
    kind: String,
    pub state: GameState,
    pub log: GameLog,
    result: Option<GameResult>,
    pending_loss: Option<(String, String)>,
    roll_ctx: BTreeMap<String, RollContext>,
    turn_bumped: bool,
    decider: Box<dyn Decider>,
    /// Monotonic counter of decisions offered, for `request_id`/`seq`.
    decision_index: u64,
}

impl Engine {
    /// Construct an engine over two decks and a decider. The log header is built
    /// immediately (players, seed, kind); `play`/`setup` land in later sub-tasks.
    pub fn new(
        deck_a: Deck,
        deck_b: Deck,
        decider: Box<dyn Decider>,
        seed: u64,
        created: String,
        kind: String,
    ) -> Self {
        let mut decks = BTreeMap::new();
        decks.insert("A".to_owned(), deck_a);
        decks.insert("B".to_owned(), deck_b);
        let players = decks
            .iter()
            .map(|(k, d)| {
                (
                    k.clone(),
                    PlayerState {
                        competitor: d.competitor.clone(),
                        entrance: d.entrance.clone(),
                        deck: d.cards.clone(),
                        hand: Vec::new(),
                        discard: Vec::new(),
                        in_play: Vec::new(),
                        pending_roll_mods: Default::default(),
                        freq_counters: BTreeMap::new(),
                        gimmick_blanked: false,
                        gimmick_flipped: false,
                        flags: serde_json::Map::new(),
                    },
                )
            })
            .collect();
        let header = Self::build_header(&decks, &*decider, seed, &kind, &created);
        let state = GameState::new(players, SeededRNG::new(seed));
        Self {
            decks,
            kind,
            state,
            log: GameLog::new(header),
            result: None,
            pending_loss: None,
            roll_ctx: BTreeMap::new(),
            turn_bumped: false,
            decider,
            decision_index: 0,
        }
    }

    fn build_header(
        decks: &BTreeMap<String, Deck>,
        decider: &dyn Decider,
        seed: u64,
        kind: &str,
        created: &str,
    ) -> Header {
        let players = decks
            .iter()
            .map(|(k, d)| {
                (
                    k.clone(),
                    PlayerInfo {
                        competitor: d.competitor.name.clone(),
                        entrance: d.entrance.name.clone(),
                        deck: d.cards.iter().map(|c| c.db_uuid.clone()).collect(),
                        policy: decider.policy_name(k),
                    },
                )
            })
            .collect();
        Header {
            seed,
            kind: kind.to_owned(),
            created: created.to_owned(),
            players,
            schema: crate::gamelog::SCHEMA_VERSION,
        }
    }

    // -- the decision seam (yield point) -----------------------------------

    /// The externalized `_decide`: a single-option point is auto-taken (no
    /// decision event); a multi-option point consults the decider, logs the §8
    /// `decision` event, and returns the choice — or suspends via [`Yield`].
    fn decide(&mut self, point: &str, key: &str, legal: Vec<Value>) -> Eng<Value> {
        if legal.len() == 1 {
            return Ok(legal.into_iter().next().unwrap());
        }
        self.decision_index += 1;
        match self.decider.decide(point, key, &legal) {
            Some(chosen) => {
                let policy = self.decider.policy_name(key);
                self.log(Event::Decision {
                    t: self.state.turn_no,
                    player: key.to_owned(),
                    point: point.to_owned(),
                    legal,
                    chosen: chosen.clone(),
                    policy,
                });
                Ok(chosen)
            }
            None => Err(Yield(Box::new(self.build_request(point, key, legal)))),
        }
    }

    fn build_request(&self, point: &str, key: &str, legal: Vec<Value>) -> DecisionRequest {
        DecisionRequest {
            request_id: format!("{}:{}", self.state.turn_no, self.decision_index),
            seq: self.decision_index - 1,
            viewer: key.to_owned(),
            point: point.to_owned(),
            legal,
            observable_state: self.state.observable(key),
        }
    }

    // -- logging -----------------------------------------------------------

    fn log(&mut self, event: Event) {
        self.log.append(event);
    }

    fn log_effect(&mut self, src: &str, action: &str, target: Option<&str>, detail: Value) {
        let t = self.state.turn_no;
        self.log(Event::EffectApplied {
            t,
            src: src.to_owned(),
            action: action.to_owned(),
            target: target.map(str::to_owned),
            detail,
        });
    }

    fn log_unsupported(&mut self, owner: &str, raw: &str, reason: &str) {
        let t = self.state.turn_no;
        self.log(Event::Unsupported {
            t,
            owner: owner.to_owned(),
            raw: raw.to_owned(),
            reason: reason.to_owned(),
            card: None,
            gimmick: None,
        });
    }

    // -- derived stats (live condition evaluation) -------------------------

    fn stats(&self, key: &str) -> Skills {
        let state = &self.state;
        state.effective_stats(
            key,
            Some(&|c: &Condition| conditions::holds(c, state, key, None)),
        )
    }

    fn stat(&self, key: &str, skill: Skill) -> i64 {
        self.stats(key).get(skill)
    }

    // -- standing effects --------------------------------------------------

    /// All effects currently able to fire for `key`: gimmick (unless blanked;
    /// sign-flipped by an opposing Cassandra), entrance, and in-play cards.
    fn standing_effects(&self, key: &str) -> Vec<Effect> {
        let player = &self.state.players[key];
        let mut out = Vec::new();
        if !self.state.is_gimmick_blanked(key) {
            if self.gimmick_signs_flipped(key) {
                out.extend(player.competitor.effects.iter().map(flip_signs));
            } else {
                out.extend(player.competitor.effects.iter().cloned());
            }
        }
        out.extend(player.entrance.effects.iter().cloned());
        for card in &player.in_play {
            out.extend(card.effects.iter().cloned());
        }
        out
    }

    /// True iff `key`'s opponent has an active `Static` `FlipGimmickSigns`
    /// (Cassandra negating every printed +/- on `key`'s gimmick).
    fn gimmick_signs_flipped(&self, key: &str) -> bool {
        let opp = self.state.opponent_of(key);
        if self.state.is_gimmick_blanked(&opp) {
            return false;
        }
        self.state.players[&opp]
            .competitor
            .effects
            .iter()
            .any(|eff| {
                matches!(eff.trigger, Trigger::Static)
                    && eff
                        .actions
                        .iter()
                        .any(|a| matches!(a, Action::FlipGimmickSigns { .. }))
            })
    }

    // -- draw / hand cap ---------------------------------------------------

    /// Draw up to `n` cards from `key`'s deck (top, or bottom for `Bottom`),
    /// logging the hidden move and enforcing the hand cap immediately.
    fn draw(&mut self, key: &str, n: usize, source: DeckEnd) -> Eng<()> {
        let player = self.state.players.get_mut(key).unwrap();
        if source == DeckEnd::Bottom {
            player.deck.reverse();
        }
        let drawn = player.draw(n);
        if source == DeckEnd::Bottom {
            self.state.players.get_mut(key).unwrap().deck.reverse();
        }
        if !drawn.is_empty() {
            let cards = drawn.iter().map(|c| c.db_uuid.clone()).collect();
            let t = self.state.turn_no;
            self.log(Event::Draw(CardMovement {
                t,
                player: key.to_owned(),
                cards,
                source: Some(deck_end_str(source).to_owned()),
                hidden: true,
            }));
            self.hand_cap(key)?;
        }
        Ok(())
    }

    /// Enforce the derived hand cap for `key` right now (a draw or an opponent's
    /// cap-lowering card can put them over — they discard down immediately).
    fn hand_cap(&mut self, key: &str) -> Eng<()> {
        let state = &self.state;
        let cap = state.effective_hand_cap(
            key,
            HAND_CAP,
            Some(&|c: &Condition| conditions::holds(c, state, key, None)),
        );
        let excess = self.state.players[key].hand.len() as i64 - cap;
        if excess > 0 {
            self.discard_from_hand(key, excess as usize, false, None)?;
        }
        Ok(())
    }

    fn enforce_hand_caps(&mut self) -> Eng<()> {
        for key in ["A", "B"] {
            self.hand_cap(key)?;
        }
        Ok(())
    }

    // -- discard / bury ----------------------------------------------------

    /// Discard `count` cards from `key`'s hand matching `selector` (`None` = any).
    /// The owner chooses which (via the `discard` point) unless `random`.
    fn discard_from_hand(
        &mut self,
        key: &str,
        count: usize,
        random: bool,
        selector: Option<&crate::ir::CardFilter>,
    ) -> Eng<()> {
        let filt = selector.cloned().unwrap_or_default();
        let mut dropped: Vec<Card> = Vec::new();
        for _ in 0..count {
            let pool: Vec<Card> = self.state.players[key]
                .hand
                .iter()
                .filter(|c| conditions::card_matches(c, &filt))
                .cloned()
                .collect();
            if pool.is_empty() {
                break;
            }
            let card = if random {
                self.state.rng.reveal(&pool).cloned().unwrap()
            } else {
                self.choose_discard(key, &pool)?
            };
            let hand = &mut self.state.players.get_mut(key).unwrap().hand;
            if let Some(pos) = hand.iter().position(|c| c.db_uuid == card.db_uuid) {
                hand.remove(pos);
            }
            dropped.push(card);
        }
        if !dropped.is_empty() {
            let cards = dropped.iter().map(|c| c.db_uuid.clone()).collect();
            self.state
                .players
                .get_mut(key)
                .unwrap()
                .discard
                .extend(dropped);
            let t = self.state.turn_no;
            self.log(Event::Discard(CardMovement {
                t,
                player: key.to_owned(),
                cards,
                source: None,
                hidden: false,
            }));
        }
        Ok(())
    }

    fn choose_discard(&mut self, key: &str, pool: &[Card]) -> Eng<Card> {
        let legal = pool.iter().map(discard_option).collect();
        let chosen = self.decide("discard", key, legal)?;
        Ok(find_by_uuid(pool, &chosen))
    }

    /// Move `cards` from `key`'s discard to the bottom of the deck.
    fn bury_cards(&mut self, key: &str, cards: &[Card]) {
        let player = self.state.players.get_mut(key).unwrap();
        for card in cards {
            if let Some(pos) = player
                .discard
                .iter()
                .position(|c| c.db_uuid == card.db_uuid)
            {
                player.discard.remove(pos);
            }
            player.deck.push(card.clone());
        }
        let uuids = cards.iter().map(|c| c.db_uuid.clone()).collect();
        let t = self.state.turn_no;
        self.log(Event::Bury(CardMovement {
            t,
            player: key.to_owned(),
            cards: uuids,
            source: Some("discard".to_owned()),
            hidden: false,
        }));
    }

    fn discard_in_play(&mut self, key: &str) {
        let player = self.state.players.get_mut(key).unwrap();
        if player.in_play.is_empty() {
            return;
        }
        let cards: Vec<Card> = std::mem::take(&mut player.in_play);
        let uuids = cards.iter().map(|c| c.db_uuid.clone()).collect();
        player.discard.extend(cards);
        let t = self.state.turn_no;
        self.log(Event::Discard(CardMovement {
            t,
            player: key.to_owned(),
            cards: uuids,
            source: None,
            hidden: false,
        }));
    }

    fn take_from_hand(&mut self, key: &str, number: i64) -> Card {
        let hand = &mut self.state.players.get_mut(key).unwrap().hand;
        let pos = hand.iter().position(|c| c.number == number).unwrap();
        hand.remove(pos)
    }

    // -- outcome bookkeeping ----------------------------------------------

    fn win(&mut self, winner: &str, reason: &str) {
        if self.result.is_none() {
            self.result = Some(GameResult {
                winner: winner.to_owned(),
                reason: reason.to_owned(),
                turns: self.state.turn_no,
            });
        }
    }

    fn ended(&self) -> bool {
        self.result.is_some()
    }

    fn resolve_pending(&mut self) -> bool {
        let Some((loser, reason)) = self.pending_loss.take() else {
            return false;
        };
        let winner = self.state.opponent_of(&loser);
        self.win(&winner, &reason);
        true
    }

    // -- frequency guards --------------------------------------------------

    fn may_fire(&self, eff: &Effect, key: &str) -> bool {
        if eff.frequency.kind == crate::ir::Frequency::Unlimited {
            return true;
        }
        !self.state.players[key]
            .freq_counters
            .contains_key(&freq_key(eff))
    }

    fn mark_fired(&mut self, eff: &Effect, key: &str) {
        if eff.frequency.kind != crate::ir::Frequency::Unlimited {
            self.state
                .players
                .get_mut(key)
                .unwrap()
                .freq_counters
                .insert(freq_key(eff), 1);
        }
    }

    fn clear_turn_freq(&mut self) {
        for player in self.state.players.values_mut() {
            player.freq_counters.retain(|k, _| !k.starts_with("turn:"));
        }
    }

    // -- effect execution --------------------------------------------------

    /// `SELF` resolves to the acting player, `OPP` to their opponent.
    fn target(&self, who: Who, key: &str) -> String {
        if who == Who::SelfSide {
            key.to_owned()
        } else {
            self.state.opponent_of(key)
        }
    }

    /// Fire every effect whose trigger matches `trigger` (by class name), whose
    /// condition holds, and whose frequency guard permits (DESIGN.md §3). `roll`
    /// supplies the roll context so `RollGap*`/`RollWasSkill` conditions resolve on
    /// turn-roll triggers; it is `None` (those conditions fail) elsewhere.
    fn run_effects(
        &mut self,
        effects: &[Effect],
        trigger: &str,
        key: &str,
        roll: Option<&RollContext>,
    ) -> Eng<()> {
        for eff in effects {
            if trigger_name(&eff.trigger) == trigger {
                self.fire_if_ready(eff, key, roll)?;
            }
        }
        Ok(())
    }

    /// Fire one effect if its frequency guard permits and its condition holds (the
    /// trigger is matched by the caller). Shared by trigger dispatch and the
    /// skill/who-matched OnRoll path so both honour condition + frequency alike.
    fn fire_if_ready(&mut self, eff: &Effect, key: &str, roll: Option<&RollContext>) -> Eng<()> {
        if !(self.may_fire(eff, key) && conditions::holds(&eff.condition, &self.state, key, roll)) {
            return Ok(());
        }
        if eff.optional && !self.take_optional(eff, key)? {
            return Ok(()); // declined "you may" — leaves the freq guard unspent
        }
        self.mark_fired(eff, key);
        self.apply_actions(eff, key)
    }

    /// Offer a "you may" effect to its owner (DESIGN.md §3 `Effect.optional`); the
    /// card controller decides (a close approximation for the rare opponent-decides
    /// rider, noted in its clause).
    fn take_optional(&mut self, eff: &Effect, key: &str) -> Eng<bool> {
        let legal = vec![
            json!({"kind": "yes", "clause": eff.raw_clause}),
            json!({"kind": "no", "clause": eff.raw_clause}),
        ];
        Ok(self.decide("optional", key, legal)?["kind"] == "yes")
    }

    fn apply_actions(&mut self, eff: &Effect, key: &str) -> Eng<()> {
        for action in &eff.actions {
            self.apply_action(action, key)?;
            if self.resolve_pending() {
                return Ok(());
            }
        }
        Ok(())
    }

    /// The action dispatch (Python `_ACTIONS`). Passive markers read elsewhere are
    /// no-ops; anything not modeled as an executed mutation surfaces as an
    /// `unsupported` log event (never silently dropped, DESIGN.md ground rules).
    fn apply_action(&mut self, action: &Action, key: &str) -> Eng<()> {
        match action {
            Action::Draw {
                n,
                source,
                who,
                per,
                per_who,
            } => self.act_draw(*n, *source, *who, per.as_ref(), *per_who, key)?,
            Action::Bury {
                selector,
                count,
                who,
                random,
            } => self.act_bury(selector, *count, *who, *random, key),
            Action::Flip { n, who } => self.act_flip(*n, *who, key),
            Action::Discard {
                selector,
                count,
                who,
                random,
                per,
                per_who,
            } => self.act_discard(selector, *count, *who, *random, per.as_ref(), *per_who, key)?,
            Action::Search {
                filter,
                dest,
                count,
            } => self.act_search(filter, *dest, *count, key)?,
            Action::ShuffleDeck { who } => self.act_shuffle_deck(*who, key),
            Action::ShuffleIntoDeck { selector } => self.act_shuffle_into_deck(selector, key)?,
            Action::AddFromDiscard { filter } => self.act_add_from_discard(filter, key)?,
            Action::RecurToDeckTop { selector, count } => {
                self.act_recur_to_deck_top(selector, *count, key)?
            }
            Action::RemoveFromPlay {
                selector,
                who,
                count,
            } => self.act_remove_from_play(selector, *who, *count, key)?,
            Action::RevealAndDiscard { count, who } => {
                self.act_reveal_and_discard(*count, *who, key)
            }
            Action::Peek { who } => self.act_peek(*who, key),
            Action::ModifyRoll {
                who,
                delta,
                when,
                per,
                per_who,
            } => self.act_modify_roll(*who, *delta, *when, per.as_ref(), *per_who, key),
            Action::CrowdMeter { delta } => self.act_crowd(*delta, key),
            Action::WinTie { who } => self.act_win_tie(*who, key),
            Action::BlankGimmick { who, duration } => self.act_blank_gimmick(*who, *duration, key),
            Action::FlipGimmick { who } => self.act_flip_gimmick(*who, key),
            Action::LoseBy { kind, who } => self.act_lose_by(*kind, *who, key),
            Action::PlayExtraCard { .. } => self.act_play_extra_card(key),
            Action::Choice { options } => self.act_choice(options, key)?,
            // Passive markers, read where they matter (roll-off, finish, hand-cap,
            // count_in_play), never executed as a mutation — a no-op, not Unsupported.
            Action::LowestRollWins
            | Action::FlipGimmickSigns { .. }
            | Action::CountsAsInPlay { .. }
            | Action::ElectBumpOnSameSkill { .. }
            | Action::Unstoppable { .. }
            | Action::AlsoLead { .. }
            | Action::DoubleFinishIfBumped
            | Action::MaxHandSize { .. } => {}
            other => {
                let raw = format!("{other:?}");
                self.log_unsupported(
                    key,
                    &raw,
                    &format!("action {} not modeled", action_name(other)),
                );
            }
        }
        Ok(())
    }

    /// Count of `per`-matching cards on `per_who`'s board (honoring
    /// `CountsAsInPlay`) — the scale for a per-count Draw/Discard/ModifyRoll.
    fn per_multiplier(&self, per: &CardFilter, per_who: Who, key: &str) -> i64 {
        let counter = self.target(per_who, key);
        conditions::count_in_play(&self.state.players[&counter].in_play, per, None)
    }

    /// Let `key`'s policy pick one of `cards` (a recur/tutor selection); the owner
    /// chooses which to recover. Auto-taken (unlogged) when only one card matches.
    fn pick_from(&mut self, key: &str, cards: &[Card], point: &str) -> Eng<Card> {
        let legal = cards.iter().map(discard_option).collect();
        let chosen = self.decide(point, key, legal)?;
        Ok(find_by_uuid(cards, &chosen))
    }

    /// Like [`pick_from`](Self::pick_from) but "up to": a trailing `none` option
    /// lets the owner stop early. `None` = declined.
    fn pick_optional_from(&mut self, key: &str, cards: &[Card], point: &str) -> Eng<Option<Card>> {
        let mut legal: Vec<Value> = cards.iter().map(discard_option).collect();
        legal.push(json!({"kind": "none"}));
        let chosen = self.decide(point, key, legal)?;
        if chosen["kind"] == "none" {
            return Ok(None);
        }
        Ok(Some(find_by_uuid(cards, &chosen)))
    }

    fn act_draw(
        &mut self,
        n: i64,
        source: DeckEnd,
        who: Who,
        per: Option<&CardFilter>,
        per_who: Who,
        key: &str,
    ) -> Eng<()> {
        let target = self.target(who, key);
        let mut n = n;
        if let Some(per) = per {
            n *= self.per_multiplier(per, per_who, key);
        }
        if n != 0 {
            self.draw(&target, n as usize, source)?;
        }
        Ok(())
    }

    fn act_shuffle_deck(&mut self, who: Who, key: &str) {
        let target = self.target(who, key);
        let deck = &mut self.state.players.get_mut(&target).unwrap().deck;
        self.state.rng.shuffle(deck);
        self.log_effect(key, "ShuffleDeck", Some(&target), Value::Null);
    }

    fn act_bury(&mut self, _selector: &CardFilter, count: i64, who: Who, random: bool, key: &str) {
        let target = self.target(who, key);
        let mut cards: Vec<Card> = self.state.players[&target]
            .discard
            .iter()
            .take(count.max(0) as usize)
            .cloned()
            .collect();
        if random {
            self.state.rng.shuffle(&mut cards);
        }
        if !cards.is_empty() {
            self.bury_cards(&target, &cards);
        }
    }

    fn act_flip(&mut self, n: i64, who: Who, key: &str) {
        let target = self.target(who, key);
        let flipped: Vec<Card> = {
            let deck = &mut self.state.players.get_mut(&target).unwrap().deck;
            let take = (n.max(0) as usize).min(deck.len());
            deck.drain(..take).collect()
        };
        if flipped.is_empty() {
            return;
        }
        let uuids = flipped.iter().map(|c| c.db_uuid.clone()).collect();
        let player = self.state.players.get_mut(&target).unwrap();
        player.discard.extend(flipped);
        let t = self.state.turn_no;
        self.log(Event::Discard(CardMovement {
            t,
            player: target,
            cards: uuids,
            source: Some("deck".to_owned()),
            hidden: false,
        }));
    }

    #[allow(clippy::too_many_arguments)]
    fn act_discard(
        &mut self,
        selector: &CardFilter,
        count: i64,
        who: Who,
        random: bool,
        per: Option<&CardFilter>,
        per_who: Who,
        key: &str,
    ) -> Eng<()> {
        let target = self.target(who, key);
        let mut count = count;
        if let Some(per) = per {
            count *= self.per_multiplier(per, per_who, key);
        }
        if count != 0 {
            self.discard_from_hand(&target, count.max(0) as usize, random, Some(selector))?;
        }
        Ok(())
    }

    fn act_search(&mut self, filter: &CardFilter, dest: Dest, count: i64, key: &str) -> Eng<()> {
        if dest == Dest::Discard {
            return self.search_to_discard(filter, count, key);
        }
        let matches: Vec<Card> = self.state.players[key]
            .deck
            .iter()
            .filter(|c| conditions::card_matches(c, filter))
            .cloned()
            .collect();
        if !matches.is_empty() {
            let card = self.pick_from(key, &matches, "target")?;
            {
                let player = self.state.players.get_mut(key).unwrap();
                if let Some(pos) = player.deck.iter().position(|c| c.db_uuid == card.db_uuid) {
                    player.deck.remove(pos);
                }
                player.hand.push(card.clone());
            }
            let t = self.state.turn_no;
            self.log(Event::Search(CardMovement {
                t,
                player: key.to_owned(),
                cards: vec![card.db_uuid],
                source: Some("deck".to_owned()),
                hidden: true, // deck -> hand: both private, opponent sees only counts
            }));
        }
        let deck = &mut self.state.players.get_mut(key).unwrap().deck;
        self.state.rng.shuffle(deck);
        self.hand_cap(key)
    }

    /// "Search your deck for up to N cards and put them into your discard pile":
    /// the owner picks which (and how many) to bin, a face-up (public) move. The
    /// deck is disturbed, so it shuffles afterwards (DESIGN.md §3, #49).
    fn search_to_discard(&mut self, filter: &CardFilter, count: i64, key: &str) -> Eng<()> {
        for _ in 0..count.max(0) {
            let matches: Vec<Card> = self.state.players[key]
                .deck
                .iter()
                .filter(|c| conditions::card_matches(c, filter))
                .cloned()
                .collect();
            if matches.is_empty() {
                break;
            }
            let Some(card) = self.pick_optional_from(key, &matches, "search")? else {
                break; // "up to" — the owner may stop early
            };
            {
                let player = self.state.players.get_mut(key).unwrap();
                if let Some(pos) = player.deck.iter().position(|c| c.db_uuid == card.db_uuid) {
                    player.deck.remove(pos);
                }
                player.discard.push(card.clone());
            }
            let t = self.state.turn_no;
            self.log(Event::Discard(CardMovement {
                t,
                player: key.to_owned(),
                cards: vec![card.db_uuid],
                source: Some("deck".to_owned()),
                hidden: false, // deck -> discard: the binned card is public in discard
            }));
        }
        let deck = &mut self.state.players.get_mut(key).unwrap().deck;
        self.state.rng.shuffle(deck);
        Ok(())
    }

    /// Recur one matching card from discard into the deck, then shuffle ("shuffle N
    /// cards" is authored as repeated actions; DESIGN.md §3 review gate).
    fn act_shuffle_into_deck(&mut self, selector: &CardFilter, key: &str) -> Eng<()> {
        let matches: Vec<Card> = self.state.players[key]
            .discard
            .iter()
            .filter(|c| conditions::card_matches(c, selector))
            .cloned()
            .collect();
        if !matches.is_empty() {
            let card = self.pick_from(key, &matches, "target")?;
            {
                let player = self.state.players.get_mut(key).unwrap();
                if let Some(pos) = player
                    .discard
                    .iter()
                    .position(|c| c.db_uuid == card.db_uuid)
                {
                    player.discard.remove(pos);
                }
                player.deck.push(card.clone());
            }
            let t = self.state.turn_no;
            self.log(Event::Bury(CardMovement {
                t,
                player: key.to_owned(),
                cards: vec![card.db_uuid],
                source: Some("discard".to_owned()),
                hidden: false,
            }));
        }
        let deck = &mut self.state.players.get_mut(key).unwrap().deck;
        self.state.rng.shuffle(deck);
        Ok(())
    }

    /// Recur a matching card from discard to hand ("add 1 <type> from your discard
    /// pile to your hand"); the owner chooses which (DESIGN.md §7).
    fn act_add_from_discard(&mut self, filter: &CardFilter, key: &str) -> Eng<()> {
        let matches: Vec<Card> = self.state.players[key]
            .discard
            .iter()
            .filter(|c| conditions::card_matches(c, filter))
            .cloned()
            .collect();
        if matches.is_empty() {
            return Ok(());
        }
        let card = self.pick_from(key, &matches, "target")?;
        {
            let player = self.state.players.get_mut(key).unwrap();
            if let Some(pos) = player
                .discard
                .iter()
                .position(|c| c.db_uuid == card.db_uuid)
            {
                player.discard.remove(pos);
            }
            player.hand.push(card.clone());
        }
        let t = self.state.turn_no;
        self.log(Event::Search(CardMovement {
            t,
            player: key.to_owned(),
            cards: vec![card.db_uuid],
            source: Some("discard".to_owned()),
            hidden: false, // discard (public) -> hand: which card left discard is visible
        }));
        self.hand_cap(key)
    }

    /// Put up to `count` matching cards from discard on top of the deck; the owner
    /// picks how many and which (DESIGN.md §7).
    fn act_recur_to_deck_top(&mut self, selector: &CardFilter, count: i64, key: &str) -> Eng<()> {
        for _ in 0..count.max(0) {
            let matches: Vec<Card> = self.state.players[key]
                .discard
                .iter()
                .filter(|c| conditions::card_matches(c, selector))
                .cloned()
                .collect();
            if matches.is_empty() {
                return Ok(());
            }
            let Some(card) = self.pick_optional_from(key, &matches, "target")? else {
                return Ok(()); // owner declined to recur more ("up to")
            };
            {
                let player = self.state.players.get_mut(key).unwrap();
                if let Some(pos) = player
                    .discard
                    .iter()
                    .position(|c| c.db_uuid == card.db_uuid)
                {
                    player.discard.remove(pos);
                }
                player.deck.insert(0, card.clone()); // top of deck (redraw next turn)
            }
            let t = self.state.turn_no;
            self.log(Event::Bury(CardMovement {
                t,
                player: key.to_owned(),
                cards: vec![card.db_uuid],
                source: Some("discard".to_owned()),
                hidden: false,
            }));
        }
        Ok(())
    }

    /// Board disruption: the actor sends up to `count` cards the target has in play
    /// to the target's discard, aiming via the `target` decision point (a visible
    /// removal — both endpoints public).
    fn act_remove_from_play(
        &mut self,
        selector: &CardFilter,
        who: Who,
        count: i64,
        key: &str,
    ) -> Eng<()> {
        let target = self.target(who, key);
        for _ in 0..count.max(0) {
            let matches: Vec<Card> = self.state.players[&target]
                .in_play
                .iter()
                .filter(|c| conditions::card_matches(c, selector))
                .cloned()
                .collect();
            if matches.is_empty() {
                return Ok(());
            }
            let card = self.pick_from(key, &matches, "target")?;
            {
                let player = self.state.players.get_mut(&target).unwrap();
                if let Some(pos) = player
                    .in_play
                    .iter()
                    .position(|c| c.db_uuid == card.db_uuid)
                {
                    player.in_play.remove(pos);
                }
                player.discard.push(card.clone());
            }
            let t = self.state.turn_no;
            self.log(Event::Discard(CardMovement {
                t,
                player: target.clone(),
                cards: vec![card.db_uuid],
                source: Some("in_play".to_owned()),
                hidden: false,
            }));
        }
        Ok(())
    }

    /// Reveal `count` random cards from the target's hand; discard the Stops among
    /// them (Spin Wheel Kick). 0..count leave, so it is not a fixed-count discard.
    fn act_reveal_and_discard(&mut self, count: i64, who: Who, key: &str) {
        let target = self.target(who, key);
        let mut pool: Vec<Card> = self.state.players[&target].hand.clone();
        let reveals = (count.max(0) as usize).min(pool.len());
        let mut revealed: Vec<Card> = Vec::new();
        for _ in 0..reveals {
            let card = self.state.rng.reveal(&pool).cloned().unwrap();
            let pos = pool.iter().position(|c| c.db_uuid == card.db_uuid).unwrap();
            pool.remove(pos);
            revealed.push(card);
        }
        let dropped: Vec<Card> = revealed.into_iter().filter(is_stop_card).collect();
        if dropped.is_empty() {
            return;
        }
        let uuids: Vec<String> = dropped.iter().map(|c| c.db_uuid.clone()).collect();
        {
            let player = self.state.players.get_mut(&target).unwrap();
            for card in &dropped {
                if let Some(pos) = player.hand.iter().position(|c| c.db_uuid == card.db_uuid) {
                    player.hand.remove(pos);
                }
            }
            player.discard.extend(dropped);
        }
        let t = self.state.turn_no;
        self.log(Event::Discard(CardMovement {
            t,
            player: target,
            cards: uuids,
            source: None,
            hidden: false,
        }));
    }

    fn act_crowd(&mut self, delta: i64, key: &str) {
        let _ = key;
        self.state.crowd_meter += delta;
        let t = self.state.turn_no;
        let value = self.state.crowd_meter;
        self.log(Event::CrowdMeter { t, delta, value });
    }

    fn act_modify_roll(
        &mut self,
        who: Who,
        delta: i64,
        when: RollWhen,
        per: Option<&CardFilter>,
        per_who: Who,
        key: &str,
    ) {
        let target = self.target(who, key);
        let mut delta = delta;
        if let Some(per) = per {
            delta *= self.per_multiplier(per, per_who, key);
        }
        {
            let mods = &mut self
                .state
                .players
                .get_mut(&target)
                .unwrap()
                .pending_roll_mods;
            match when {
                RollWhen::This => mods.this_turn += delta,
                RollWhen::Next => mods.next_turn += delta,
            }
        }
        let slot = if when == RollWhen::This {
            "this"
        } else {
            "next"
        };
        self.log_effect(
            key,
            "ModifyRoll",
            Some(&target),
            json!({"delta": delta, "when": slot}),
        );
    }

    /// Executed (one-shot) blank: latch the flag on the target. A while-in-play
    /// blank is authored Static and read via `is_gimmick_blanked`; this covers an
    /// `OnHit` "blank the gimmick" that fires once.
    fn act_blank_gimmick(&mut self, who: Who, duration: Duration, key: &str) {
        let target = self.target(who, key);
        self.state.players.get_mut(&target).unwrap().gimmick_blanked = true;
        let detail = json!({"duration": serde_json::to_value(duration).unwrap()});
        self.log_effect(key, "BlankGimmick", Some(&target), detail);
    }

    /// Turn a competitor to its back side (Copy Kat V2): one-way and idempotent —
    /// latch the flip so the front's effects switch off and the back's on.
    fn act_flip_gimmick(&mut self, who: Who, key: &str) {
        let target = self.target(who, key);
        {
            let player = self.state.players.get_mut(&target).unwrap();
            if player.gimmick_flipped {
                return;
            }
            player.gimmick_flipped = true;
        }
        self.log_effect(key, "FlipGimmick", Some(&target), Value::Null);
    }

    /// Pure information: grant `key` a look at `target`'s hand for the rest of this
    /// turn (no zone changes; `observable` reads the peek flag). Peeking your own
    /// hand is a no-op.
    fn act_peek(&mut self, who: Who, key: &str) {
        let target = self.target(who, key);
        if target == key {
            return;
        }
        let turn = self.state.turn_no;
        let hand_size = self.state.players[&target].hand.len();
        {
            let mut peek = serde_json::Map::new();
            peek.insert(target.clone(), json!(turn));
            self.state
                .players
                .get_mut(key)
                .unwrap()
                .flags
                .insert("peek".to_owned(), Value::Object(peek));
        }
        self.log_effect(key, "Peek", Some(&target), json!({"hand_size": hand_size}));
    }

    fn act_choice(&mut self, options: &[ChoiceOption], key: &str) -> Eng<()> {
        if options.is_empty() {
            return Ok(());
        }
        let legal: Vec<Value> = options
            .iter()
            .enumerate()
            .map(|(i, opt)| json!({"kind": "choice", "index": i, "label": opt.label}))
            .collect();
        let chosen = self.decide("choice", key, legal)?;
        let idx = chosen["index"].as_u64().unwrap() as usize;
        let actions = options[idx].actions.clone();
        for action in &actions {
            self.apply_action(action, key)?;
            if self.resolve_pending() {
                return Ok(());
            }
        }
        Ok(())
    }

    fn act_win_tie(&mut self, who: Who, key: &str) {
        let target = self.target(who, key);
        self.state
            .players
            .get_mut(&target)
            .unwrap()
            .flags
            .insert("win_tie".to_owned(), json!(true));
        self.log_effect(key, "WinTie", Some(&target), Value::Null);
    }

    fn act_lose_by(&mut self, kind: LoseKind, who: Who, key: &str) {
        let loser = self.target(who, key);
        let kind_str = serde_json::to_value(kind).unwrap();
        let kind_name = kind_str.as_str().unwrap().to_owned();
        self.pending_loss = Some((loser.clone(), kind_name.to_lowercase()));
        self.log_effect(key, "LoseBy", Some(&loser), json!({"kind": kind_name}));
    }

    /// Grant one more turn action this turn ("you may play an additional card");
    /// consumed by the turn loop, reset each turn.
    fn act_play_extra_card(&mut self, key: &str) {
        let flags = &mut self.state.players.get_mut(key).unwrap().flags;
        let cur = flags
            .get("extra_plays")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        flags.insert("extra_plays".to_owned(), json!(cur + 1));
    }
}

// ---------------------------------------------------------------------------
// Free helpers
// ---------------------------------------------------------------------------

fn deck_end_str(source: DeckEnd) -> &'static str {
    match source {
        DeckEnd::Top => "TOP",
        DeckEnd::Bottom => "BOTTOM",
    }
}

/// The per-effect frequency-counter key (`turn:`/`match:` + clause + trigger).
fn freq_key(eff: &Effect) -> String {
    let prefix = if eff.frequency.kind == crate::ir::Frequency::OncePerTurn {
        "turn:"
    } else {
        "match:"
    };
    format!("{prefix}{}|{}", eff.raw_clause, trigger_name(&eff.trigger))
}

/// The Python class name of a trigger — part of the freq-counter key, so it must
/// match `type(eff.trigger).__name__` exactly.
fn trigger_name(trigger: &Trigger) -> &'static str {
    match trigger {
        Trigger::OnPlay => "OnPlay",
        Trigger::OnRoll { .. } => "OnRoll",
        Trigger::InRoll { .. } => "InRoll",
        Trigger::OnRollBoost { .. } => "OnRollBoost",
        Trigger::OnWinTurn => "OnWinTurn",
        Trigger::OnLoseTurn { .. } => "OnLoseTurn",
        Trigger::OnStop { .. } => "OnStop",
        Trigger::OnHit { .. } => "OnHit",
        Trigger::OnBump => "OnBump",
        Trigger::StartOfTurn => "StartOfTurn",
        Trigger::StartOfMatch => "StartOfMatch",
        Trigger::OnBreakout => "OnBreakout",
        Trigger::Static => "Static",
    }
}

/// The `play` option for a card (the §7 `turn_action` legal shape).
fn card_option(card: &Card) -> Value {
    json!({
        "kind": "play",
        "number": card.number,
        "card": card.db_uuid,
        "order": card.play_order.name(),
        "atk_type": card.atk_type.name(),
    })
}

/// The `stop` option for a candidate stopper.
fn stop_option(card: &Card) -> Value {
    json!({
        "kind": "stop",
        "number": card.number,
        "card": card.db_uuid,
        "order": card.play_order.name(),
        "atk_type": card.atk_type.name(),
    })
}

/// The `discard` option for a card (also used for bury/target picks).
fn discard_option(card: &Card) -> Value {
    json!({
        "kind": "discard",
        "number": card.number,
        "card": card.db_uuid,
        "order": card.play_order.name(),
    })
}

/// Whether `card` can act as a Stop — carries at least one `Stop` action (its
/// online condition is not checked; a revealed Stop is discarded regardless).
fn is_stop_card(card: &Card) -> bool {
    card.effects
        .iter()
        .any(|eff| eff.actions.iter().any(|a| matches!(a, Action::Stop { .. })))
}

/// The action's Python class name — the tail of an `unsupported` event's reason
/// when an action reaches the executor without a modeled handler.
fn action_name(action: &Action) -> &'static str {
    match action {
        Action::Draw { .. } => "Draw",
        Action::Bury { .. } => "Bury",
        Action::Flip { .. } => "Flip",
        Action::Discard { .. } => "Discard",
        Action::Search { .. } => "Search",
        Action::ShuffleDeck { .. } => "ShuffleDeck",
        Action::ShuffleIntoDeck { .. } => "ShuffleIntoDeck",
        Action::AddFromDiscard { .. } => "AddFromDiscard",
        Action::RecurToDeckTop { .. } => "RecurToDeckTop",
        Action::CountsAsInPlay { .. } => "CountsAsInPlay",
        Action::RemoveFromPlay { .. } => "RemoveFromPlay",
        Action::RevealAndDiscard { .. } => "RevealAndDiscard",
        Action::Peek { .. } => "Peek",
        Action::ModifyRoll { .. } => "ModifyRoll",
        Action::BuffSkill { .. } => "BuffSkill",
        Action::MaxHandSize { .. } => "MaxHandSize",
        Action::Reroll { .. } => "Reroll",
        Action::WinTie { .. } => "WinTie",
        Action::Bump { .. } => "Bump",
        Action::ElectBumpOnSameSkill { .. } => "ElectBumpOnSameSkill",
        Action::Stop { .. } => "Stop",
        Action::BlankGimmick { .. } => "BlankGimmick",
        Action::FlipGimmick { .. } => "FlipGimmick",
        Action::BlankText { .. } => "BlankText",
        Action::LoseBy { .. } => "LoseBy",
        Action::CrowdMeter { .. } => "CrowdMeter",
        Action::PlayExtraCard { .. } => "PlayExtraCard",
        Action::SetFinishRoll { .. } => "SetFinishRoll",
        Action::FinishBonus { .. } => "FinishBonus",
        Action::FinishRollBonus { .. } => "FinishRollBonus",
        Action::BreakoutModifier { .. } => "BreakoutModifier",
        Action::LowestRollWins => "LowestRollWins",
        Action::FlipGimmickSigns { .. } => "FlipGimmickSigns",
        Action::Unstoppable { .. } => "Unstoppable",
        Action::AlsoLead { .. } => "AlsoLead",
        Action::DoubleFinishIfBumped => "DoubleFinishIfBumped",
        Action::Choice { .. } => "Choice",
        Action::Unsupported { .. } => "Unsupported",
    }
}

/// The card in `pool` whose `db_uuid` matches the chosen option's `card` field.
fn find_by_uuid(pool: &[Card], chosen: &Value) -> Card {
    let uuid = chosen["card"].as_str().unwrap();
    pool.iter()
        .find(|c| c.db_uuid == uuid)
        .expect("chosen card is in the pool")
        .clone()
}
