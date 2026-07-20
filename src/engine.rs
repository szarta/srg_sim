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
//! and snapshot = `(seed, decisions[])`. The batch [`Engine::play`] driver and the
//! resumable [`Session`] driver share one decision protocol and produce a
//! byte-identical [`GameLog`] — the whole-engine parity pinned by the conformance
//! corpus (`tests/engine_conformance.rs`).

use crate::cards::{Card, Deck};
use crate::conditions::{self, RollContext};
use crate::gamelog::{BreakoutRoll, CardMovement, Event, GameLog, Header, PlayerInfo, RollMod};
use crate::ir::{
    Action, AtkType, BuryFrom, CardFilter, ChoiceOption, Condition, DeckEnd, Dest, Direction,
    DqScope, Duration, Effect, LoseKind, PlayOrder, RevealDest, RevealFrom, RevealMatch, RollWhen,
    ScryRest, Skill, Trigger, Who,
};
use crate::rng::SeededRNG;
use crate::skills::Skills;
use crate::state::{GameState, PlayerState, TimedBuff};
use serde_json::{json, Value};
use std::cmp::Reverse;
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

impl Step {
    /// The step as JSON — the single wire contract every consumer reads (`srg
    /// session`, the WASM bindings, and through them the MCP server and the web
    /// client). `Step`/`DecisionRequest`/`GameResult` don't derive `Serialize`, so
    /// this is the one place that shape is defined.
    ///
    /// `{"kind":"decision","request":{request_id, seq, viewer, point, legal,
    /// observable_state}}` or `{"kind":"done","result":{winner, reason, turns}}`.
    pub fn to_json(&self) -> Value {
        match self {
            Step::Decision(r) => serde_json::json!({
                "kind": "decision",
                "request": {
                    "request_id": r.request_id,
                    "seq": r.seq,
                    "viewer": r.viewer,
                    "point": r.point,
                    "legal": r.legal,
                    "observable_state": r.observable_state,
                },
            }),
            Step::Done(res) => serde_json::json!({
                "kind": "done",
                "result": { "winner": res.winner, "reason": res.reason, "turns": res.turns },
            }),
        }
    }
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

/// The decision provider — the externalized `_decide`. A live [`Policy`] reads
/// `state` (and `RandomPolicy` draws from `state.rng`) to choose; the
/// [`ReplayDecider`] ignores `state` and replays a recorded list, suspending when
/// it runs dry.
///
/// [`Policy`]: crate::policy::Policy
pub trait Decider {
    /// The chosen option for a multi-option decision point, or `None` to suspend
    /// (the driver then yields a [`DecisionRequest`] and resumes on `submit`). The
    /// live `state` is passed through so a policy can inspect the board and, for a
    /// random policy, consume the engine's seeded RNG.
    fn decide(
        &mut self,
        point: &str,
        viewer: &str,
        legal: &[Value],
        state: &mut GameState,
    ) -> Option<Value>;

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
    fn decide(
        &mut self,
        _point: &str,
        viewer: &str,
        _legal: &[Value],
        _state: &mut GameState,
    ) -> Option<Value> {
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
            per,
            per_zone,
        } => Action::BuffSkill {
            skill: *skill,
            delta: -*delta,
            who: *who,
            duration: *duration,
            target_highest: *target_highest,
            per_crowd: *per_crowd,
            cap: *cap,
            per: per.clone(),
            per_zone: *per_zone,
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
            per,
            per_who,
            per_zone,
        } => Action::FinishRollBonus {
            delta: -*delta,
            when_skill: *when_skill,
            either: *either,
            per: per.clone(),
            per_who: *per_who,
            per_zone: *per_zone,
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
/// The `Draw` action's payload, grouped so `act_draw` stays under the argument
/// limit as the per-count options grew (`cap`, `per_excludes_trigger`).
struct DrawSpec {
    n: i64,
    source: DeckEnd,
    who: Who,
    per: Option<CardFilter>,
    per_who: Who,
    cap: Option<i64>,
    per_excludes_trigger: bool,
}

pub struct Engine {
    pub state: GameState,
    pub log: GameLog,
    result: Option<GameResult>,
    pending_loss: Option<(String, String)>,
    roll_ctx: BTreeMap<String, RollContext>,
    turn_bumped: bool,
    /// `db_uuid` of the card currently being stopped, set for the duration of
    /// `apply_stop` so `BlankStoppedText` knows its referent. Transient, never
    /// serialized.
    stopped_card: Option<String>,
    /// `db_uuid` of the card whose hit is currently being resolved, set for the
    /// duration of `run_hit_gimmicks` so a `per_excludes_trigger` count can drop it.
    /// Transient, never serialized.
    hit_card: Option<String>,
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
                        reroll_grants: Default::default(),
                        timed_buffs: Vec::new(),
                        chosen_name: None,
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
            state,
            log: GameLog::new(header),
            result: None,
            pending_loss: None,
            roll_ctx: BTreeMap::new(),
            turn_bumped: false,
            stopped_card: None,
            hit_card: None,
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
        match self.decider.decide(point, key, &legal, &mut self.state) {
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
    /// The persistent standing effects that are *not* a played card: competitor
    /// gimmick (blank/flip-aware) + entrance. Fired for standing `OnStop` gimmicks
    /// in a stop exchange, where re-scanning in-play cards would re-fire the stop
    /// card that just entered play (`apply_stop`).
    fn gimmick_standing_effects(&self, key: &str) -> Vec<Effect> {
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
        out
    }

    fn standing_effects(&self, key: &str) -> Vec<Effect> {
        let mut out = self.gimmick_standing_effects(key);
        for card in &self.state.players[key].in_play {
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
    ) -> Eng<usize> {
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
        let n = dropped.len();
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
        Ok(n)
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
            self.apply_action(action, key, &eff.raw_clause)?;
            if self.resolve_pending() {
                return Ok(());
            }
        }
        Ok(())
    }

    /// The action dispatch (Python `_ACTIONS`). Passive markers read elsewhere are
    /// no-ops; anything not modeled as an executed mutation surfaces as an
    /// `unsupported` log event (never silently dropped, DESIGN.md ground rules).
    ///
    /// `source` is the granting effect's `raw_clause`, carried only so a TIMED
    /// `BuffSkill` can accumulate under a stable stacking identity (see [`TimedBuff`]).
    fn apply_action(&mut self, action: &Action, key: &str, source: &str) -> Eng<()> {
        match action {
            Action::Draw {
                n,
                source,
                who,
                per,
                per_who,
                cap,
                per_excludes_trigger,
            } => self.act_draw(
                DrawSpec {
                    n: *n,
                    source: *source,
                    who: *who,
                    per: per.clone(),
                    per_who: *per_who,
                    cap: *cap,
                    per_excludes_trigger: *per_excludes_trigger,
                },
                key,
            )?,
            Action::Bury {
                selector,
                count,
                who,
                random,
                source,
            } => self.act_bury(selector, *count, *who, *random, *source, key)?,
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
            Action::ShuffleDeck { who } => self.act_shuffle_deck(*who, key)?,
            Action::ShuffleIntoDeck { selector } => self.act_shuffle_into_deck(selector, key)?,
            Action::AddFromDiscard { filter } => self.act_add_from_discard(filter, key)?,
            Action::SwapHandDiscard => self.act_swap_hand_discard(key)?,
            Action::RecurToDeckTop { selector, count } => {
                self.act_recur_to_deck_top(selector, *count, key)?
            }
            Action::RemoveFromPlay {
                selector,
                who,
                count,
            } => self.act_remove_from_play(selector, *who, *count, key)?,
            Action::ReturnToHand {
                selector,
                who,
                count,
                choose,
            } => self.act_return_to_hand(selector, *who, *count, *choose, key)?,
            Action::RevealAndDiscard { count, who } => {
                self.act_reveal_and_discard(*count, *who, key)
            }
            Action::RevealForDraw {
                who,
                count,
                draw,
                match_on,
            } => self.act_reveal_for_draw(*who, *count, *draw, *match_on, key)?,
            Action::Peek { who } => self.act_peek(*who, key),
            Action::Scry {
                deck,
                top,
                bottom,
                reveal,
                to_hand,
                bury,
                rest,
            } => self.act_scry(*deck, *top, *bottom, *reveal, *to_hand, *bury, *rest, key)?,
            Action::RevealRoute {
                deck,
                match_atk,
                on_match,
                on_fail,
                fail_optional,
                reveal,
                reveal_from,
                match_parity,
            } => self.act_reveal_route(
                *deck,
                *match_atk,
                *on_match,
                *on_fail,
                *fail_optional,
                *reveal,
                *reveal_from,
                *match_parity,
                key,
            )?,
            Action::ShuffleHandDraw { who, count, choose } => {
                self.act_shuffle_hand_draw(*who, *count, *choose, key)?
            }
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
            Action::Choice { options } => self.act_choice(options, key, source)?,
            // Passive markers, read where they matter (roll-off, finish, hand-cap,
            // count_in_play), never executed as a mutation — a no-op, not Unsupported.
            Action::LowestRollWins
            | Action::FlipGimmickSigns { .. }
            | Action::CountsAsInPlay { .. }
            | Action::ElectBumpOnSameSkill { .. }
            | Action::Unstoppable { .. }
            | Action::AlsoLead { .. }
            | Action::DoubleFinishIfBumped
            | Action::DisqualificationRule { .. }
            | Action::ConsideredCompare { .. }
            | Action::SuppressOpponentDraw
            | Action::SwitchRolledSkill { .. }
            | Action::AddText { .. }
            | Action::StopRequiresTag { .. }
            | Action::BlankText { .. }
            | Action::MaxHandSize { .. } => {}
            Action::BlankStoppedText => self.act_blank_stopped_text(key),
            Action::ChooseName { options } => self.act_choose_name(options, key)?,
            // A TIMED BuffSkill is granted imperatively here and lives in
            // `timed_buffs` until its sweep; every other duration is continuous
            // (folded from the board by `fold_buffs`) and never fires as an action.
            Action::BuffSkill {
                skill,
                delta,
                who,
                duration: duration @ (Duration::UntilEndOfTurn | Duration::UntilStartOfYourNextTurn),
                cap,
                ..
            } => self.grant_timed_buff(
                TimedBuff {
                    skill: *skill,
                    delta: *delta,
                    until: *duration,
                    source: source.to_owned(),
                    cap: *cap,
                    granted_turn: 0, // filled in from the live turn counter
                },
                *who,
                key,
            ),
            // A `Next` re-roll grants a one-shot for the owner's next turn roll; a
            // `This` re-roll is structural (read in the roll-off), a no-op here.
            Action::Reroll { when, .. } => {
                if *when == RollWhen::Next {
                    self.state
                        .players
                        .get_mut(key)
                        .unwrap()
                        .reroll_grants
                        .next_turn += 1;
                }
            }
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
    /// Count of `per`-matching cards on `per_who`'s board, optionally dropping the
    /// card with `exclude`'s uuid ("for each OTHER … in play").
    fn per_multiplier(
        &self,
        per: &CardFilter,
        per_who: Who,
        key: &str,
        exclude: Option<&str>,
    ) -> i64 {
        let counter = self.target(per_who, key);
        let board = &self.state.players[&counter].in_play;
        let skip = exclude.and_then(|u| board.iter().find(|c| c.db_uuid == u));
        conditions::count_in_play(board, per, skip)
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

    fn act_draw(&mut self, spec: DrawSpec, key: &str) -> Eng<()> {
        let DrawSpec {
            source,
            who,
            per_who,
            cap,
            per_excludes_trigger,
            ..
        } = spec;
        let target = self.target(who, key);
        let mut n = spec.n;
        if let Some(per) = spec.per.as_ref() {
            let exclude = per_excludes_trigger
                .then(|| self.hit_card.clone())
                .flatten();
            n *= self.per_multiplier(per, per_who, key, exclude.as_deref());
            // "(Max 3)" clamps the per-count product, not the flat draw.
            if let Some(c) = cap {
                n = n.min(c);
            }
        }
        if n != 0 {
            // "Your opponent does not draw for your card effects" (Sami "The Draw"):
            // a draw this player's effect grants the opponent is voided.
            if who == Who::Opp && self.suppresses_opp_draw(key) {
                self.log_effect(key, "SuppressOpponentDraw", Some(&target), json!({"n": n}));
            } else {
                self.draw(&target, n as usize, source)?;
            }
        }
        Ok(())
    }

    /// Whether `key` holds an active "your opponent does not draw for your card
    /// effects" declaration (Sami "The Draw"): a Static `SuppressOpponentDraw` on
    /// `key`'s own gimmick (unless blanked), entrance, or in-play, whose condition
    /// holds. Read at `act_draw`.
    fn suppresses_opp_draw(&self, key: &str) -> bool {
        let player = &self.state.players[key];
        let gimmick = (
            &player.competitor.effects,
            !self.state.is_gimmick_blanked(key),
        );
        std::iter::once(gimmick)
            .chain(std::iter::once((&player.entrance.effects, true)))
            .chain(player.in_play.iter().map(|c| (&c.effects, true)))
            .any(|(effects, active)| active && self.declares_suppress(effects, key))
    }

    /// Any Static `SuppressOpponentDraw` among `effects` whose condition holds.
    fn declares_suppress(&self, effects: &[Effect], key: &str) -> bool {
        effects.iter().any(|eff| {
            matches!(eff.trigger, Trigger::Static)
                && eff
                    .actions
                    .iter()
                    .any(|a| matches!(a, Action::SuppressOpponentDraw))
                && conditions::holds(&eff.condition, &self.state, key, None)
        })
    }

    fn act_shuffle_deck(&mut self, who: Who, key: &str) -> Eng<()> {
        let target = self.target(who, key);
        self.log_effect(key, "ShuffleDeck", Some(&target), Value::Null);
        self.shuffle_deck(&target)
    }

    /// Shuffle `key`'s deck as an EFFECT-caused shuffle and fire any `OnShuffle`
    /// gimmicks. The match-start setup shuffle and the private bury-ordering shuffle
    /// deliberately bypass this (they are not a card/gimmick "shuffling your deck").
    fn shuffle_deck(&mut self, key: &str) -> Eng<()> {
        let deck = &mut self.state.players.get_mut(key).unwrap().deck;
        self.state.rng.shuffle(deck);
        self.run_on_shuffle(key)
    }

    /// Fire standing `OnShuffle` gimmicks after `shuffled`'s deck was shuffled by an
    /// effect. Scans BOTH players so a `who=OPP` ("when your opponent shuffles their
    /// deck" — Memes Dealer V2) variant works; fires once per shuffle.
    fn run_on_shuffle(&mut self, shuffled: &str) -> Eng<()> {
        let opp = self.state.opponent_of(shuffled);
        for owner in [shuffled.to_owned(), opp] {
            let effects = self.standing_effects(&owner);
            for eff in &effects {
                let Trigger::OnShuffle { who } = &eff.trigger else {
                    continue;
                };
                // SELF fires when the owner shuffled their own deck; OPP when the
                // shuffled deck belongs to the owner's opponent.
                let dir_ok = (*who == Who::SelfSide) == (owner.as_str() == shuffled);
                if dir_ok {
                    self.fire_if_ready(eff, &owner, None)?;
                }
            }
        }
        Ok(())
    }

    /// Fire standing `OnDiscardMove` gimmicks after an effect moved one or more cards
    /// OUT of `pile`'s discard pile. Scans BOTH players so a `who=OPP` variant ("when
    /// your opponent moves any number of cards from their discard pile" — Brumeister
    /// V2) works; fires once per action, however many cards moved.
    fn run_on_discard_move(&mut self, pile: &str) -> Eng<()> {
        let opp = self.state.opponent_of(pile);
        for owner in [pile.to_owned(), opp] {
            let effects = self.standing_effects(&owner);
            for eff in &effects {
                let Trigger::OnDiscardMove { who } = &eff.trigger else {
                    continue;
                };
                // SELF fires when the owner's own pile was drawn from; OPP when the
                // pile belongs to the owner's opponent.
                if (*who == Who::SelfSide) == (owner.as_str() == pile) {
                    self.fire_if_ready(eff, &owner, None)?;
                }
            }
        }
        Ok(())
    }

    fn act_bury(
        &mut self,
        selector: &CardFilter,
        count: i64,
        who: Who,
        random: bool,
        source: BuryFrom,
        key: &str,
    ) -> Eng<()> {
        let target = self.target(who, key);
        if source == BuryFrom::Hand {
            let n = self.bury_from_hand(&target, count.max(0) as usize, random, selector)?;
            if n > 0 {
                self.run_on_bury(&target, true, false)?; // effect-caused hand bury
            }
            return Ok(());
        }
        // Discard source: recycle the top `count` of the discard pile (optionally
        // randomized) to the bottom of the deck. Selector is ignored (the pass-and-
        // recycle bury never filters).
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
            self.run_on_bury(&target, false, false)?; // effect-caused discard-pile bury
            self.run_on_discard_move(&target)?;
        }
        Ok(())
    }

    /// "Bury N cards in [your/their] hand": move `count` cards from `key`'s hand to
    /// the bottom of their deck. The hand owner chooses which (their hidden hand)
    /// unless `random`. Mirrors [`discard_from_hand`](Self::discard_from_hand) but
    /// lands the cards on the deck bottom and logs a `Bury` from `hand`.
    fn bury_from_hand(
        &mut self,
        key: &str,
        count: usize,
        random: bool,
        selector: &CardFilter,
    ) -> Eng<usize> {
        let mut buried: Vec<Card> = Vec::new();
        for _ in 0..count {
            let pool: Vec<Card> = self.state.players[key]
                .hand
                .iter()
                .filter(|c| conditions::card_matches(c, selector))
                .cloned()
                .collect();
            if pool.is_empty() {
                break;
            }
            let card = if random {
                self.state.rng.reveal(&pool).cloned().unwrap()
            } else {
                self.pick_from(key, &pool, "bury_hand")?
            };
            let hand = &mut self.state.players.get_mut(key).unwrap().hand;
            if let Some(pos) = hand.iter().position(|c| c.db_uuid == card.db_uuid) {
                hand.remove(pos);
            }
            buried.push(card);
        }
        let n = buried.len();
        if !buried.is_empty() {
            let uuids = buried.iter().map(|c| c.db_uuid.clone()).collect();
            let player = self.state.players.get_mut(key).unwrap();
            for card in buried {
                player.deck.push(card);
            }
            let t = self.state.turn_no;
            self.log(Event::Bury(CardMovement {
                t,
                player: key.to_owned(),
                cards: uuids,
                source: Some("hand".to_owned()),
                hidden: false,
            }));
        }
        Ok(n)
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
            count *= self.per_multiplier(per, per_who, key, None);
        }
        if count != 0 {
            let n =
                self.discard_from_hand(&target, count.max(0) as usize, random, Some(selector))?;
            if n > 0 {
                self.run_on_bury(&target, true, true)?; // effect-caused hand discard (Tommy)
            }
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
        let picked = if matches.is_empty() {
            None
        } else {
            Some(self.pick_from(key, &matches, "target")?)
        };
        if let Some(card) = &picked {
            let player = self.state.players.get_mut(key).unwrap();
            if let Some(pos) = player.deck.iter().position(|c| c.db_uuid == card.db_uuid) {
                player.deck.remove(pos);
            }
        }
        // You looked through the deck — shuffle the remainder. The picked card is out
        // of the deck for the shuffle whether it lands in hand or back on top, so a
        // `Hand` search shuffles identically to before (byte-for-byte parity).
        self.shuffle_deck(key)?;
        if let Some(card) = picked {
            let player = self.state.players.get_mut(key).unwrap();
            match dest {
                Dest::Hand => player.hand.push(card.clone()),
                Dest::DeckTop => player.deck.insert(0, card.clone()), // top of deck
                Dest::Discard => unreachable!("handled above"),
            }
            let t = self.state.turn_no;
            self.log(Event::Search(CardMovement {
                t,
                player: key.to_owned(),
                cards: vec![card.db_uuid],
                source: Some("deck".to_owned()),
                hidden: true, // deck -> hand/deck: both private, opponent sees only counts
            }));
        }
        if dest == Dest::Hand {
            self.hand_cap(key)?;
        }
        Ok(())
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
        self.shuffle_deck(key)
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
            // The card has left the pile; fires ahead of the shuffle's own OnShuffle.
            self.run_on_discard_move(key)?;
        }
        self.shuffle_deck(key)
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
        self.run_on_discard_move(key)?;
        self.hand_cap(key)
    }

    /// "Switch 1 card in your hand with 1 card in your discard pile" (Collin, Mr. Rey):
    /// the owner picks one hand card out (→ discard, via the `discard`/shed point) and
    /// one discard card in (→ hand, via the `target`/tutor point). A no-op if either
    /// zone is empty. Even hand/discard sizes are preserved (a 1-for-1 swap).
    fn act_swap_hand_discard(&mut self, key: &str) -> Eng<()> {
        let hand: Vec<Card> = self.state.players[key].hand.clone();
        let discard: Vec<Card> = self.state.players[key].discard.clone();
        if hand.is_empty() || discard.is_empty() {
            return Ok(());
        }
        let out = self.pick_from(key, &hand, "discard")?; // hand card leaving
        let into = self.pick_from(key, &discard, "target")?; // discard card entering
        let player = self.state.players.get_mut(key).unwrap();
        if let Some(pos) = player.hand.iter().position(|c| c.db_uuid == out.db_uuid) {
            player.hand.remove(pos);
        }
        if let Some(pos) = player
            .discard
            .iter()
            .position(|c| c.db_uuid == into.db_uuid)
        {
            player.discard.remove(pos);
        }
        player.hand.push(into.clone());
        player.discard.push(out.clone());
        self.log_effect(
            key,
            "SwapHandDiscard",
            Some(key),
            json!({"hand_out": out.db_uuid, "discard_in": into.db_uuid}),
        );
        self.run_on_discard_move(key)
    }

    /// Put up to `count` matching cards from discard on top of the deck; the owner
    /// picks how many and which (DESIGN.md §7).
    fn act_recur_to_deck_top(&mut self, selector: &CardFilter, count: i64, key: &str) -> Eng<()> {
        let mut moved = 0;
        for _ in 0..count.max(0) {
            let matches: Vec<Card> = self.state.players[key]
                .discard
                .iter()
                .filter(|c| conditions::card_matches(c, selector))
                .cloned()
                .collect();
            if matches.is_empty() {
                break;
            }
            let Some(card) = self.pick_optional_from(key, &matches, "target")? else {
                break; // owner declined to recur more ("up to")
            };
            moved += 1;
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
        if moved > 0 {
            self.run_on_discard_move(key)?; // once per action, not per card
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

    /// "Add `count` card(s) in play to their hand" (Fox Assassin V2): bounce matching
    /// in-play cards back to their OWNER's hand. `choose` lets the actor pick from
    /// either board ("any player has in play"); otherwise the pick is over `who`'s.
    fn act_return_to_hand(
        &mut self,
        selector: &CardFilter,
        who: Who,
        count: i64,
        choose: bool,
        key: &str,
    ) -> Eng<()> {
        let boards: Vec<String> = if choose {
            vec![key.to_owned(), self.state.opponent_of(key)]
        } else {
            vec![self.target(who, key)]
        };
        for _ in 0..count.max(0) {
            let legal: Vec<Value> = boards
                .iter()
                .flat_map(|b| {
                    self.state.players[b]
                        .in_play
                        .iter()
                        .filter(|c| conditions::card_matches(c, selector))
                        .map(move |c| {
                            let mut opt = card_option(c);
                            opt["owner"] = json!(b);
                            opt
                        })
                })
                .collect();
            if legal.is_empty() {
                break;
            }
            let chosen = self.decide("return_to_hand", key, legal)?;
            let owner = chosen["owner"].as_str().unwrap().to_owned();
            let uuid = chosen["card"].as_str().unwrap().to_owned();
            let player = self.state.players.get_mut(&owner).unwrap();
            let Some(pos) = player.in_play.iter().position(|c| c.db_uuid == uuid) else {
                break;
            };
            let card = player.in_play.remove(pos);
            player.hand.push(card);
            let t = self.state.turn_no;
            self.log(Event::Search(CardMovement {
                t,
                player: owner,
                cards: vec![uuid],
                source: Some("in_play".to_owned()),
                hidden: false, // in-play (public) -> hand: which card left play is visible
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

    /// "Your opponent randomly reveals `count` card(s) in their hand: if it is a stop,
    /// draw `draw`" (Bartholomew Hooke). Reveals stay in hand (public); the actor draws
    /// `draw` for each revealed stop.
    fn act_reveal_for_draw(
        &mut self,
        who: Who,
        count: i64,
        draw: i64,
        match_on: RevealMatch,
        key: &str,
    ) -> Eng<()> {
        let target = self.target(who, key);
        // The actor's own just-rolled skill drives the `RolledSkill` predicate; it
        // is populated by `record_roll_ctx` before `OnRoll` fires (The Winning Ticket).
        let rolled = self.roll_ctx.get(key).and_then(|c| c.skill);
        let mut pool: Vec<Card> = self.state.players[&target].hand.clone();
        let reveals = (count.max(0) as usize).min(pool.len());
        let mut hits = 0i64;
        let mut revealed: Vec<String> = Vec::new();
        for _ in 0..reveals {
            let card = self.state.rng.reveal(&pool).cloned().unwrap();
            let pos = pool.iter().position(|c| c.db_uuid == card.db_uuid).unwrap();
            pool.remove(pos);
            if reveal_matches(&card, match_on, rolled) {
                hits += 1;
            }
            revealed.push(card.db_uuid);
        }
        if !revealed.is_empty() {
            self.log_effect(
                key,
                "RevealForDraw",
                Some(&target),
                json!({"revealed": revealed, "hits": hits}),
            );
        }
        if hits > 0 {
            self.draw(key, (hits * draw).max(0) as usize, DeckEnd::Top)?;
        }
        Ok(())
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
            delta *= self.per_multiplier(per, per_who, key, None);
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

    /// Look at / reveal cards from the top (and/or bottom) of `deck`'s deck and
    /// route them by value. The effect owner (`key`) is the actor: it takes the
    /// `to_hand` best cards to the deck owner's hand, buries `bury` of them to the
    /// deck bottom (the *worst* on its own deck, the *best* on an opponent's —
    /// sabotage), and disposes of the rest per `rest`. `reveal` makes the seen
    /// cards public (logged); a private "look at" logs only the count.
    #[allow(clippy::too_many_arguments)]
    fn act_scry(
        &mut self,
        deck: Who,
        top: i64,
        bottom: i64,
        reveal: bool,
        to_hand: i64,
        bury: i64,
        rest: ScryRest,
        key: &str,
    ) -> Eng<()> {
        let owner = self.target(deck, key);
        let sabotage = owner != key; // scrying an opponent's deck hurts, not helps

        // Pull the revealed window off the deck: `top` from the front, `bottom`
        // from the back (top = front of the Vec, the draw end).
        let mut revealed: Vec<Card> = Vec::new();
        {
            let d = &mut self.state.players.get_mut(&owner).unwrap().deck;
            let tn = (top.max(0) as usize).min(d.len());
            revealed.extend(d.drain(..tn));
            let bn = (bottom.max(0) as usize).min(d.len());
            let cut = d.len() - bn;
            revealed.extend(d.drain(cut..));
        }
        if revealed.is_empty() {
            return Ok(());
        }

        // Reveal (public) lists the card ids; a private "look at" logs only the
        // count — private info stays out of the log (the Peek convention).
        let seen = if reveal {
            json!(revealed
                .iter()
                .map(|c| c.db_uuid.clone())
                .collect::<Vec<_>>())
        } else {
            Value::Null
        };
        self.log_effect(
            key,
            "Scry",
            Some(&owner),
            json!({"count": revealed.len(), "revealed": seen, "public": reveal}),
        );

        // Rank by value (Finish > stop > other), best first.
        revealed.sort_by_key(|c| Reverse(scry_value(c)));

        // Take the `to_hand` best cards to the deck owner's hand.
        let take = (to_hand.max(0) as usize).min(revealed.len());
        if take > 0 {
            let taken: Vec<Card> = revealed.drain(..take).collect();
            let uuids: Vec<String> = taken.iter().map(|c| c.db_uuid.clone()).collect();
            self.state
                .players
                .get_mut(&owner)
                .unwrap()
                .hand
                .extend(taken);
            let t = self.state.turn_no;
            self.log(Event::Draw(CardMovement {
                t,
                player: owner.clone(),
                cards: uuids,
                source: Some("deck".to_owned()),
                hidden: !reveal,
            }));
        }

        // Bury `bury` cards to the deck bottom: the worst on your own deck, the
        // best on an opponent's.
        let bn = (bury.max(0) as usize).min(revealed.len());
        if bn > 0 {
            let buried: Vec<Card> = if sabotage {
                revealed.drain(..bn).collect()
            } else {
                let cut = revealed.len() - bn;
                revealed.drain(cut..).collect()
            };
            self.scry_to_bottom(&owner, &buried);
        }

        // Dispose of the leftovers, then re-cap the (possibly grown) hand.
        self.scry_dispose(&owner, revealed, rest, sabotage);
        self.hand_cap(&owner)
    }

    /// Route each scry leftover: `Return` puts them all back on top (best on top of
    /// your own deck, worst on top when sabotaging); `Choose` keeps the valuable
    /// ones on top and buries the junk (inverted when sabotaging).
    fn scry_dispose(&mut self, owner: &str, cards: Vec<Card>, rest: ScryRest, sabotage: bool) {
        if cards.is_empty() {
            return;
        }
        match rest {
            ScryRest::Return => {
                let mut ordered = cards;
                ordered.sort_by_key(|c| Reverse(scry_value(c))); // best first
                if sabotage {
                    ordered.reverse(); // feed the opponent their worst first
                }
                self.scry_to_top(owner, ordered);
            }
            ScryRest::Choose => {
                let (keep, bury): (Vec<Card>, Vec<Card>) = cards
                    .into_iter()
                    .partition(|c| (scry_value(c) >= 2) != sabotage);
                if !bury.is_empty() {
                    self.scry_to_bottom(owner, &bury);
                }
                self.scry_to_top(owner, keep);
            }
        }
    }

    /// Put `cards` back on top of `owner`'s deck, `cards[0]` ending up topmost.
    fn scry_to_top(&mut self, owner: &str, cards: Vec<Card>) {
        if cards.is_empty() {
            return;
        }
        let d = &mut self.state.players.get_mut(owner).unwrap().deck;
        for card in cards.into_iter().rev() {
            d.insert(0, card);
        }
    }

    /// Send `cards` to the bottom of `owner`'s deck and log the bury.
    fn scry_to_bottom(&mut self, owner: &str, cards: &[Card]) {
        if cards.is_empty() {
            return;
        }
        self.state
            .players
            .get_mut(owner)
            .unwrap()
            .deck
            .extend(cards.iter().cloned());
        let t = self.state.turn_no;
        self.log(Event::Bury(CardMovement {
            t,
            player: owner.to_owned(),
            cards: cards.iter().map(|c| c.db_uuid.clone()).collect(),
            source: Some("deck".to_owned()),
            hidden: false,
        }));
    }

    /// Reveal the top card of `deck`'s deck and route it by a runtime predicate: if
    /// its `atk_type` equals `match_atk` it goes to `on_match`, else to `on_fail`.
    /// A `fail_optional` fail branch ("you may flip/bury it") is taken only when
    /// worthwhile — shed junk on your own deck, disrupt a valuable card on an
    /// opponent's — otherwise the card is left on top.
    #[allow(clippy::too_many_arguments)]
    fn act_reveal_route(
        &mut self,
        deck: Who,
        match_atk: AtkType,
        on_match: RevealDest,
        on_fail: RevealDest,
        fail_optional: bool,
        reveal: bool,
        reveal_from: RevealFrom,
        match_parity: Option<bool>,
        key: &str,
    ) -> Eng<()> {
        let owner = self.target(deck, key);
        let sabotage = owner != key;
        let card = {
            let d = &mut self.state.players.get_mut(&owner).unwrap().deck;
            if d.is_empty() {
                return Ok(());
            }
            // `Choose` (top or bottom) is a blind pick — resolve it to the top.
            match reveal_from {
                RevealFrom::Bottom => d.pop().unwrap(),
                _ => d.remove(0),
            }
        };
        // Parity predicate (Smart Mark's odd/even guess) overrides the atk_type one.
        let matched = match match_parity {
            Some(even) => (card.number % 2 == 0) == even,
            None => card.atk_type == match_atk,
        };
        self.log_effect(
            key,
            "RevealRoute",
            Some(&owner),
            json!({"card": if reveal { json!(card.db_uuid) } else { Value::Null },
                   "matched": matched}),
        );
        let dest = if matched {
            on_match
        } else if fail_optional {
            // Take the "you may" only when it helps: dump a low-value card off your
            // own deck to dig; push a high-value card down an opponent's.
            let worth = if sabotage {
                scry_value(&card) >= 2
            } else {
                scry_value(&card) < 2
            };
            if worth {
                on_fail
            } else {
                RevealDest::Leave
            }
        } else {
            on_fail
        };
        self.route_revealed(&owner, card, dest)
    }

    /// Land a single revealed card in its chosen destination and log the move.
    fn route_revealed(&mut self, owner: &str, card: Card, dest: RevealDest) -> Eng<()> {
        let uuid = card.db_uuid.clone();
        let t = self.state.turn_no;
        let player = self.state.players.get_mut(owner).unwrap();
        match dest {
            RevealDest::Hand => {
                player.hand.push(card);
                self.log(Event::Draw(CardMovement {
                    t,
                    player: owner.to_owned(),
                    cards: vec![uuid],
                    source: Some("deck".to_owned()),
                    hidden: false,
                }));
                return self.hand_cap(owner);
            }
            RevealDest::Flip => {
                player.discard.push(card);
                self.log(Event::Discard(CardMovement {
                    t,
                    player: owner.to_owned(),
                    cards: vec![uuid],
                    source: Some("deck".to_owned()),
                    hidden: false,
                }));
            }
            RevealDest::Bury => {
                player.deck.push(card); // bottom
                self.log(Event::Bury(CardMovement {
                    t,
                    player: owner.to_owned(),
                    cards: vec![uuid],
                    source: Some("deck".to_owned()),
                    hidden: false,
                }));
            }
            RevealDest::Leave => player.deck.insert(0, card), // back on top
        }
        Ok(())
    }

    /// Shuffle a player's hand back into their deck, shuffle, then draw `count` — a
    /// mid-match hand refresh (Cyclone V2, on a bump). `choose` lets the actor pick
    /// which player ("either player"); the default policy picks itself.
    fn act_shuffle_hand_draw(&mut self, who: Who, count: i64, choose: bool, key: &str) -> Eng<()> {
        let target = if choose {
            self.decide_reshuffle_target(key)?
        } else {
            self.target(who, key)
        };
        let hand: Vec<Card> =
            std::mem::take(&mut self.state.players.get_mut(&target).unwrap().hand);
        if !hand.is_empty() {
            let uuids: Vec<String> = hand.iter().map(|c| c.db_uuid.clone()).collect();
            let t = self.state.turn_no;
            self.state
                .players
                .get_mut(&target)
                .unwrap()
                .deck
                .extend(hand);
            self.log(Event::Bury(CardMovement {
                t,
                player: target.clone(),
                cards: uuids,
                source: Some("hand".to_owned()),
                hidden: false,
            }));
        }
        self.shuffle_deck(&target)?;
        self.draw(&target, count.max(0) as usize, DeckEnd::Top)
    }

    /// "Either player" pick for [`Self::act_shuffle_hand_draw`] — the actor chooses
    /// itself or its opponent; the default policy takes the first (itself).
    fn decide_reshuffle_target(&mut self, key: &str) -> Eng<String> {
        let opp = self.state.opponent_of(key);
        let legal = vec![
            json!({"kind": "seat", "seat": key}),
            json!({"kind": "seat", "seat": opp}),
        ];
        let chosen = self.decide("reshuffle_target", key, legal)?;
        Ok(chosen["seat"].as_str().unwrap().to_owned())
    }

    /// "Choose 1: <name>, <name>, or <name>" (Raven): bind one option for the rest of
    /// the match. The owner decides (a `name` decision point); the binding is read by
    /// `ChosenNameIs`, which gates the sibling effects referencing "that" name.
    fn act_choose_name(&mut self, options: &[String], key: &str) -> Eng<()> {
        if options.is_empty() {
            return Ok(());
        }
        let legal: Vec<Value> = options
            .iter()
            .map(|n| json!({"kind": "name", "name": n}))
            .collect();
        let chosen = self.decide("name", key, legal)?;
        let name = chosen["name"].as_str().unwrap_or_default().to_owned();
        self.state.players.get_mut(key).unwrap().chosen_name = Some(name.clone());
        self.log_effect(key, "ChooseName", Some(key), json!({"name": name}));
        Ok(())
    }

    /// "The stopped card has blank text until the end of the turn": blank the card
    /// currently being stopped, by identity, for the rest of the turn. A no-op outside
    /// a stop exchange (no referent).
    fn act_blank_stopped_text(&mut self, key: &str) {
        let Some(uuid) = self.stopped_card.clone() else {
            return;
        };
        self.state.blanked_text.insert(uuid.clone());
        self.log_effect(key, "BlankStoppedText", None, json!({"card": uuid}));
    }

    /// Drop everything scoped "until the end of the turn" by the turn just finished:
    /// timed buffs under `UntilEndOfTurn` and the per-card text blanks from
    /// `BlankStoppedText`. Runs with the other per-turn resets at the top of the
    /// following turn.
    fn sweep_end_of_turn(&mut self) {
        for player in self.state.players.values_mut() {
            player
                .timed_buffs
                .retain(|b| b.until != Duration::UntilEndOfTurn);
        }
        self.state.blanked_text.clear();
    }

    /// Sweep "until the start of your next turn" buffs now that the turn roll has
    /// named `winner` the active player.
    ///
    /// A turn is shared and its active player is only known once the roll resolves, so
    /// this cannot run before the roll — the buff therefore still feeds the roll that
    /// makes the turn yours, and dies immediately after (hand-adjudicated 2026-07-20).
    /// `granted_turn < turn_no` keeps a buff granted on THIS turn's roll alive; buffs
    /// on the non-active player are untouched, which is what lets one survive across
    /// every turn its owner does not win.
    fn sweep_next_turn_buffs(&mut self, winner: &str) {
        let turn = self.state.turn_no;
        let player = self.state.players.get_mut(winner).unwrap();
        player
            .timed_buffs
            .retain(|b| b.until != Duration::UntilStartOfYourNextTurn || b.granted_turn >= turn);
    }

    /// Grant (or accumulate into) a TIMED skill buff on `who`'s side.
    ///
    /// The buff is stored on the TARGET, so the derived-stats fold needs no owner
    /// bookkeeping. Re-firing the same clause for the same skill and expiry
    /// accumulates into the existing entry and clamps to `cap` — "(Max +5 to each)"
    /// is a ceiling across repeat triggers, not per firing (hand-adjudicated).
    /// `grant` carries the per-firing increment in `delta`; `granted_turn` is filled
    /// in here from the live turn counter.
    fn grant_timed_buff(&mut self, grant: TimedBuff, who: Who, key: &str) {
        let target = self.target(who, key);
        let turn = self.state.turn_no;
        let (skill, until, cap, step) = (grant.skill, grant.until, grant.cap, grant.delta);
        let clamp = |v: i64| cap.map_or(v, |c| v.min(c));
        let player = self.state.players.get_mut(&target).unwrap();
        let total = match player
            .timed_buffs
            .iter_mut()
            .find(|b| b.source == grant.source && b.skill == skill && b.until == until)
        {
            Some(existing) => {
                existing.delta = clamp(existing.delta + step);
                existing.delta
            }
            None => {
                let d = clamp(step);
                player.timed_buffs.push(TimedBuff {
                    delta: d,
                    granted_turn: turn,
                    ..grant
                });
                d
            }
        };
        self.log_effect(
            key,
            "BuffSkill",
            Some(&target),
            json!({"skill": skill, "delta": step, "total": total, "until": until}),
        );
    }

    fn act_choice(&mut self, options: &[ChoiceOption], key: &str, source: &str) -> Eng<()> {
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
            self.apply_action(action, key, source)?;
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
        if kind == LoseKind::Disqualification && self.is_dq_immune(&loser) {
            // "no disqualifications" / "you cannot be disqualified": the loss is
            // voided and play continues (the triggering effect still fired).
            self.log_effect(
                key,
                "LoseByVoided",
                Some(&loser),
                json!({"kind": kind_name}),
            );
            return;
        }
        self.pending_loss = Some((loser.clone(), kind_name.to_lowercase()));
        self.log_effect(key, "LoseBy", Some(&loser), json!({"kind": kind_name}));
    }

    /// True iff `loser` is currently immune to a disqualification loss: some active
    /// `DisqualificationRule` disables DQ for them and none re-enables it. A rule
    /// applies to `loser` when its scope is `Match` (any owner) or `SelfSide` (owner
    /// == loser). Effects are in-play-scoped and condition-gated. NOTE: last-played-
    /// order tie-break between a disable and a re-enable is task #93 (needs a global
    /// play sequence); with no re-enable card modeled yet this is exact.
    fn is_dq_immune(&self, loser: &str) -> bool {
        let mut disabled = false;
        for (owner, player) in &self.state.players {
            let sources = std::iter::once(&player.competitor.effects)
                .chain(std::iter::once(&player.entrance.effects))
                .chain(player.in_play.iter().map(|c| &c.effects));
            for effects in sources {
                for eff in effects {
                    if !matches!(eff.trigger, Trigger::Static) {
                        continue;
                    }
                    for action in &eff.actions {
                        let Action::DisqualificationRule { enabled, scope } = action else {
                            continue;
                        };
                        let applies = *scope == DqScope::Match || owner == loser;
                        if !applies || !conditions::holds(&eff.condition, &self.state, owner, None)
                        {
                            continue;
                        }
                        if *enabled {
                            return false; // an active rule re-enables DQ
                        }
                        disabled = true;
                    }
                }
            }
        }
        disabled
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

    // -- turn loop ---------------------------------------------------------

    /// One full turn: bump the counter, clear per-turn state, resolve the roll-off,
    /// then the winner draws and takes their play action(s) (DESIGN.md §6). The
    /// board persists across turns; a `PlayExtraCard` grant loops another action.
    fn turn(&mut self) -> Eng<()> {
        self.state.turn_no += 1;
        self.clear_turn_freq();
        for player in self.state.players.values_mut() {
            player.flags.remove("extra_plays"); // "additional card this turn" is per-turn
                                                // Promote a "re-roll your next turn roll" grant to this turn (SET, not
                                                // accumulate); an unused grant expires.
            player.reroll_grants.this_turn = player.reroll_grants.next_turn;
            player.reroll_grants.next_turn = 0;
        }
        self.sweep_end_of_turn();
        let winner = self.turn_roll()?;
        self.sweep_next_turn_buffs(&winner);
        if self.ended() || !self.draw_for_turn(&winner)? {
            return Ok(());
        }
        self.first_turn_option(&winner)?; // the once-per-player first-turn redraw (§6)
        self.take_turn_action(&winner)?; // play ONE card (or pass+bury)
        while !self.ended() && self.consume_extra_play(&winner) {
            self.take_turn_action(&winner)?; // a PlayExtraCard granted another action
        }
        Ok(())
    }

    /// Spend one pending "additional card this turn" grant, if any.
    fn consume_extra_play(&mut self, key: &str) -> bool {
        let flags = &mut self.state.players.get_mut(key).unwrap().flags;
        let cur = flags
            .get("extra_plays")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        if cur <= 0 {
            return false;
        }
        flags.insert("extra_plays".to_owned(), json!(cur - 1));
        true
    }

    // -- top-level driver --------------------------------------------------

    /// Run the match to a result (the log is on `self.log`). The batch driver: with
    /// a fully-recorded [`ReplayDecider`] no decision suspends, so this returns
    /// `Ok`; the [`Session`] driver shares the exact same body but resumes on each
    /// `Yield`. A match that hits [`TURN_CAP`] is a `turn_cap` draw.
    pub fn play(&mut self) -> Eng<GameResult> {
        self.setup()?;
        while self.result.is_none() && self.state.turn_no < TURN_CAP {
            self.turn()?;
        }
        if self.result.is_none() {
            self.result = Some(GameResult {
                winner: "draw".to_owned(),
                reason: "turn_cap".to_owned(),
                turns: self.state.turn_no,
            });
        }
        let event = self.result_event();
        self.log(event);
        Ok(self.result.clone().unwrap())
    }

    fn result_event(&self) -> Event {
        let r = self.result.as_ref().unwrap();
        Event::Result {
            t: self.state.turn_no,
            winner: r.winner.clone(),
            reason: r.reason.clone(),
            turns: r.turns,
        }
    }

    // -- setup / mulligan --------------------------------------------------

    /// Match setup: StartOfMatch effects, shuffle, opening hands. The first-turn
    /// redraw is NOT done here — it belongs to each player's own first won turn
    /// (DESIGN.md §6), fired from the turn loop.
    pub fn setup(&mut self) -> Eng<()> {
        for key in ["A", "B"] {
            let effects = self.standing_effects(key);
            self.run_effects(&effects, "StartOfMatch", key, None)?;
        }
        for key in ["A", "B"] {
            let deck = &mut self.state.players.get_mut(key).unwrap().deck;
            self.state.rng.shuffle(deck);
        }
        for key in ["A", "B"] {
            self.draw(key, OPENING_HAND, DeckEnd::Top)?;
        }
        Ok(())
    }

    /// Offer the first-turn redraw once per player, on the first won turn they would
    /// take an action (DESIGN.md §6). Marked spent whether or not it fires, so a
    /// player who bumps/loses the early rolls still gets it exactly once.
    fn first_turn_option(&mut self, key: &str) -> Eng<()> {
        if self.state.players[key]
            .flags
            .get("had_first_turn")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Ok(());
        }
        self.state
            .players
            .get_mut(key)
            .unwrap()
            .flags
            .insert("had_first_turn".to_owned(), json!(true));
        self.mulligan(key)
    }

    /// First-turn redraw (DESIGN.md §6): only with NO Leads in hand, a player MAY
    /// reveal the whole hand, bury it to the bottom of the deck in an order they
    /// choose, then draw UP TO that many. With a Lead in hand it is not offered.
    fn mulligan(&mut self, key: &str) -> Eng<()> {
        let hand = &self.state.players[key].hand;
        if hand.is_empty() || hand.iter().any(|c| c.play_order == PlayOrder::Lead) {
            return Ok(());
        }
        let legal = vec![json!({"kind": "redraw"}), json!({"kind": "keep"})];
        if self.decide("mulligan", key, legal)?["kind"] != "redraw" {
            return Ok(());
        }
        let revealed = std::mem::take(&mut self.state.players.get_mut(key).unwrap().hand);
        let n = revealed.len();
        let ordered = self.order_bury(key, revealed)?; // player picks the bury order
        let uuids: Vec<String> = ordered.iter().map(|c| c.db_uuid.clone()).collect();
        self.state
            .players
            .get_mut(key)
            .unwrap()
            .deck
            .extend(ordered); // to the bottom
        let t = self.state.turn_no;
        self.log(Event::Bury(CardMovement {
            t,
            player: key.to_owned(),
            cards: uuids,
            source: Some("hand".to_owned()),
            hidden: false, // the hand was REVEALED, so the moved cards are public
        }));
        let draw_n = self.mulligan_draw_count(key, n)?; // draw UP TO N
        self.draw(key, draw_n, DeckEnd::Top)
    }

    /// Return `cards` in the owner's chosen bury order (last card forced).
    fn order_bury(&mut self, key: &str, cards: Vec<Card>) -> Eng<Vec<Card>> {
        let mut remaining = cards;
        let mut ordered: Vec<Card> = Vec::new();
        while remaining.len() > 1 {
            let legal: Vec<Value> = remaining.iter().map(discard_option).collect();
            let chosen = self.decide("mulligan_bury", key, legal)?;
            let card = find_by_uuid(&remaining, &chosen);
            let pos = remaining
                .iter()
                .position(|c| c.db_uuid == card.db_uuid)
                .unwrap();
            remaining.remove(pos);
            ordered.push(card);
        }
        ordered.extend(remaining);
        Ok(ordered)
    }

    /// How many to redraw: up to `n` (default policy takes the max — listed first).
    fn mulligan_draw_count(&mut self, key: &str, n: usize) -> Eng<usize> {
        let legal: Vec<Value> = (0..=n)
            .rev()
            .map(|i| json!({"kind": "draw", "n": i}))
            .collect();
        let chosen = self.decide("mulligan_draw", key, legal)?;
        Ok(chosen["n"].as_u64().unwrap() as usize)
    }

    // -- attack sequence ---------------------------------------------------

    /// Play ONE card advancing the persistent chain, or pass+bury (DESIGN.md §6).
    /// Cards resolve into `in_play` and stay there across turns; an unstopped Finish
    /// triggers the finish sequence.
    fn take_turn_action(&mut self, active: &str) -> Eng<()> {
        let defender = self.state.opponent_of(active);
        let mut legal = self.playable_options(active);
        legal.push(json!({"kind": "pass"}));
        let choice = self.decide("turn_action", active, legal)?;
        if choice["kind"] == "pass" {
            return self.do_pass(active);
        }
        let number = choice["number"].as_i64().unwrap();
        let card = self.take_from_hand(active, number);
        let landed = self.resolve_play(active, &defender, card.clone())?;
        if landed && card.play_order == PlayOrder::Finish {
            self.finish_sequence(active, &defender, &card)?;
        }
        Ok(())
    }

    /// Passing recycles one card from discard to the bottom of the deck (§6).
    fn do_pass(&mut self, active: &str) -> Eng<()> {
        let pool: Vec<Card> = self.state.players[active].discard.clone();
        if pool.is_empty() {
            return Ok(());
        }
        let legal: Vec<Value> = pool.iter().map(card_option).collect();
        let chosen = self.decide("bury", active, legal)?;
        let card = find_by_uuid(&pool, &chosen);
        self.bury_cards(active, &[card]);
        Ok(())
    }

    /// Playable cards: those advancing the owner's own chain, plus any self-declaring
    /// an `AlsoLead` whose condition currently holds.
    fn playable_options(&self, key: &str) -> Vec<Value> {
        let chain = &self.state.players[key].in_play;
        self.state.players[key]
            .hand
            .iter()
            .filter(|&c| playable(chain, c) || self.also_lead_now(key, c))
            .map(card_option)
            .collect()
    }

    /// Whether `card` may be played as a Lead this instant via an `AlsoLead`
    /// self-declaration whose condition currently holds.
    fn also_lead_now(&self, key: &str, card: &Card) -> bool {
        card.effects.iter().any(|eff| {
            eff.actions.iter().any(|a| {
                matches!(a, Action::AlsoLead { condition }
                    if conditions::holds(condition, &self.state, key, None))
            })
        })
    }

    // -- play resolution + stops ------------------------------------------

    /// Resolve a played card: log it, offer the stop window FIRST (a stopped card
    /// fires none of its text), then OnPlay, land it, OnHit + type-gated hit
    /// gimmicks, and re-check hand caps. `Ok(true)` iff the card landed and the
    /// match is still live.
    fn resolve_play(&mut self, active: &str, defender: &str, card: Card) -> Eng<bool> {
        let t = self.state.turn_no;
        self.log(Event::Play {
            t,
            player: active.to_owned(),
            card: card.db_uuid.clone(),
            order: card.play_order.name().to_owned(),
            atk_type: card.atk_type.name().to_owned(),
        });
        if let Some(stop) = self.offer_stop(defender, &card)? {
            self.apply_stop(active, defender, card, stop)?;
            return Ok(false);
        }
        // The card's own effects plus any "added text" its owner's active gimmick
        // grants to cards of this name (El Super Santa / Sabu). Injected effects
        // carry their own triggers (OnPlay/OnHit) and dispatch identically. A
        // text-blanked card (opponent's "your Spotlights are blank") fires nothing.
        let effects = if self.state.is_text_blanked(&card, active) {
            Vec::new()
        } else {
            let mut e = card.effects.clone();
            e.extend(self.injected_text(active, &card));
            e
        };
        self.run_effects(&effects, "OnPlay", active, None)?;
        if self.ended() {
            return Ok(false);
        }
        self.state
            .players
            .get_mut(active)
            .unwrap()
            .in_play
            .push(card.clone());
        self.run_effects(&effects, "OnHit", active, None)?; // the card's own "when this hits"
        self.run_hit_gimmicks(&card, active)?; // owner gimmick "when you hit a <type>" (D1)
        self.enforce_hand_caps()?; // a new Static max-handsize mod may force a discard
        Ok(!self.ended())
    }

    /// Fire `key`'s standing type-gated `OnHit` gimmicks for a card of `card`'s
    /// attack type that just hit (D1). A card's own untyped OnHit already resolved
    /// via `run_effects`, so it is not re-fired.
    fn run_hit_gimmicks(&mut self, card: &Card, key: &str) -> Eng<()> {
        // The hit card is already on the board here, so a "for each OTHER … in play"
        // count must drop it (`Draw.per_excludes_trigger`).
        self.hit_card = Some(card.db_uuid.clone());
        let result = self.run_hit_gimmicks_inner(card, key);
        self.hit_card = None;
        result
    }

    fn run_hit_gimmicks_inner(&mut self, card: &Card, key: &str) -> Eng<()> {
        let effects = self.standing_effects(key);
        for eff in &effects {
            let Trigger::OnHit {
                atk_type,
                name_contains,
                text_contains,
                on_any,
                order,
            } = &eff.trigger
            else {
                continue;
            };
            // A bare OnHit (no gate) is the card's OWN "when this hits", already fired
            // via `run_effects` — skipped here UNLESS it explicitly sets `on_any` ("when
            // you hit a card" — Bartholomew Hooke), which fires on every hit. `on_any`
            // is override-only, so parser fragments that produce a bare OnHit stay inert.
            let has_name_gate = !name_contains.is_empty() || !text_contains.is_empty();
            if atk_type.is_none() && !has_name_gate && order.is_none() && !on_any {
                continue;
            }
            let type_ok = atk_type.is_none_or(|want| want == card.atk_type);
            // "When you hit a Lead" — the play-order gate on the HIT card (ANDed).
            let order_ok = order.is_none_or(|want| want == card.play_order);
            let name_gate = CardFilter {
                name_contains: name_contains.clone(),
                text_contains: text_contains.clone(),
                ..Default::default()
            };
            if type_ok && order_ok && conditions::card_matches(card, &name_gate) {
                self.fire_if_ready(eff, key, None)?;
            }
        }
        Ok(())
    }

    /// "Added text" effects `key`'s active gimmicks grant to `card` (El Super Santa:
    /// cards with "Super" in the name gain "Draw 2"). Collects `AddText` actions from
    /// `key`'s standing Static effects whose condition holds and whose `name_contains`
    /// matches the card's title (case-insensitive OR), returning the effects to run
    /// alongside the card's own. Empty when no gimmick text applies.
    fn injected_text(&self, key: &str, card: &Card) -> Vec<Effect> {
        let mut out = Vec::new();
        for eff in self.standing_effects(key) {
            if !matches!(eff.trigger, Trigger::Static)
                || !conditions::holds(&eff.condition, &self.state, key, None)
            {
                continue;
            }
            for action in &eff.actions {
                if let Action::AddText {
                    name_contains,
                    effects,
                } = action
                {
                    let gate = CardFilter {
                        name_contains: name_contains.clone(),
                        ..Default::default()
                    };
                    if conditions::card_matches(card, &gate) {
                        out.extend(effects.iter().cloned());
                    }
                }
            }
        }
        out
    }

    /// Offer `defender` the stop window for `card`; return the chosen stopper (taken
    /// from hand) or `None`. The `none` option carries what is being defended so a
    /// policy can reserve stops for the real threat.
    fn offer_stop(&mut self, defender: &str, card: &Card) -> Eng<Option<Card>> {
        let stops = self.legal_stops(defender, card);
        if stops.is_empty() {
            return Ok(None);
        }
        let mut legal = vec![json!({
            "kind": "none",
            "vs_order": card.play_order.name(),
            "vs_type": card.atk_type.name(),
        })];
        legal.extend(stops.iter().map(stop_option));
        let choice = self.decide("stop", defender, legal)?;
        if choice["kind"] == "none" {
            return Ok(None);
        }
        let number = choice["number"].as_i64().unwrap();
        Ok(Some(self.take_from_hand(defender, number)))
    }

    fn legal_stops(&self, defender: &str, attack: &Card) -> Vec<Card> {
        self.state.players[defender]
            .hand
            .iter()
            .filter(|c| self.card_can_stop(defender, c, attack))
            .cloned()
            .collect()
    }

    /// Text-driven stop (DESIGN.md §6): a card can stop `attack` iff one of its
    /// parsed `Stop` effects matches the attack's order/type and that effect's
    /// condition holds from the defender's view. An attack `Unstoppable` by the
    /// stopper's play order cannot be stopped by it.
    fn card_can_stop(&self, defender: &str, stopper: &Card, attack: &Card) -> bool {
        if is_unstoppable_by(attack, stopper) {
            return false;
        }
        if self.state.is_text_blanked(stopper, defender) {
            return false; // a text-blanked stop card cannot stop
        }
        stopper.effects.iter().any(|eff| {
            conditions::holds(&eff.condition, &self.state, defender, None)
                && attacker_meets_tag_gates(eff, attack)
                && eff.actions.iter().any(|action| {
                    matches!(action, Action::Stop { .. }) && stop_matches(action, attack)
                })
        })
    }

    /// Apply a stop: the stopped ATTACK goes to the attacker's discard; the stopping
    /// card enters the defender's board and persists (bypassing the play-sequence
    /// gate). Fires the stop's OnHit + hit gimmicks, then both sides' OnStop.
    fn apply_stop(&mut self, active: &str, defender: &str, attack: Card, stop: Card) -> Eng<()> {
        self.state
            .players
            .get_mut(active)
            .unwrap()
            .discard
            .push(attack.clone());
        self.state
            .players
            .get_mut(defender)
            .unwrap()
            .in_play
            .push(stop.clone());
        let t = self.state.turn_no;
        self.log(Event::Stop {
            t,
            player: defender.to_owned(),
            card: stop.db_uuid.clone(),
            stopped: attack.db_uuid.clone(),
            reason: format!("{} stops {}", stop.atk_type.name(), attack.atk_type.name()),
        });
        let stop_effects = stop.effects.clone();
        let attack_effects = attack.effects.clone();
        self.run_effects(&stop_effects, "OnHit", defender, None)?;
        self.run_hit_gimmicks(&stop, defender)?; // a stop entering play is itself a hit
                                                 // "The stopped card has blank text until the end of the turn" must resolve
                                                 // BEFORE the stopped card's own OnStop: the whole point of that family is to
                                                 // suppress the stopped card's "If Stopped" text, several members reading
                                                 // "stop any card WITH 'If Stopped' in the text: that card has blank text …".
                                                 // Split so those effects land first and the rest keep their original order.
        self.stopped_card = Some(attack.db_uuid.clone());
        let (blanking, rest): (Vec<Effect>, Vec<Effect>) =
            stop_effects.into_iter().partition(|e| {
                e.actions
                    .iter()
                    .any(|a| matches!(a, Action::BlankStoppedText))
            });
        self.run_effects(&blanking, "OnStop", defender, None)?;
        // A blanked card fires nothing — the same rule `play_card` and `card_can_stop`
        // already apply to a text-blanked card.
        if !self.state.is_text_blanked(&attack, active) {
            self.run_effects(&attack_effects, "OnStop", active, None)?; // "if this is stopped"
        }
        self.run_effects(&rest, "OnStop", defender, None)?; // stop card: "when this stops"
        self.stopped_card = None;
        // Standing competitor/entrance OnStop, dir-aware from each owner's POV: the
        // attacker's card was stopped (YOURS), the defender stopped a card (THEIRS =
        // "when you Stop a card", e.g. Gia).
        let stopped = attack.play_order;
        self.run_on_stop_gimmicks(active, Direction::Yours, stopped)?;
        self.run_on_stop_gimmicks(defender, Direction::Theirs, stopped)?;
        Ok(())
    }

    /// Fire `key`'s standing (gimmick/entrance) `OnStop` effects whose direction
    /// matches `dir` — THEIRS for the stopper, YOURS for the stopped attacker — and
    /// whose optional `order` gate matches the **stopped** card's play order (`None`
    /// = any). Unlike `run_effects` (trigger-name match only), this consults both
    /// `OnStop.dir` and `OnStop.order`.
    fn run_on_stop_gimmicks(&mut self, key: &str, dir: Direction, stopped: PlayOrder) -> Eng<()> {
        let effects = self.gimmick_standing_effects(key);
        for eff in &effects {
            if matches!(eff.trigger, Trigger::OnStop { dir: d, order }
                if d == dir && order.is_none_or(|o| o == stopped))
            {
                self.fire_if_ready(eff, key, None)?;
            }
        }
        Ok(())
    }

    /// Fire standing `OnBury` gimmicks after an EFFECT-caused bury/discard landed on
    /// `buried_by` (The Cyclone V1, Tommy Stillwell). `from_hand` = the cards left the
    /// hand (vs the discard pile); `is_discard` = the event was a discard (vs a bury).
    /// Scans BOTH players so a `who=OPP` ("when your opponent buries") variant works;
    /// fires once per event. The mechanical pass-and-recycle bury and the hand-cap trim
    /// bypass `act_bury`/`act_discard`, so they never reach here (DESIGN.md §3).
    fn run_on_bury(&mut self, buried_by: &str, from_hand: bool, is_discard: bool) -> Eng<()> {
        let opp = self.state.opponent_of(buried_by);
        for owner in [buried_by.to_owned(), opp] {
            let effects = self.standing_effects(&owner);
            for eff in &effects {
                let Trigger::OnBury {
                    who,
                    from_hand_only,
                    also_discard,
                } = &eff.trigger
                else {
                    continue;
                };
                // SELF fires when the effect's owner is the burier; OPP when the
                // burier is the owner's opponent.
                let dir_ok = (*who == Who::SelfSide) == (owner.as_str() == buried_by);
                if !dir_ok {
                    continue;
                }
                if is_discard && !*also_discard {
                    continue; // a discard only fires the "bury or discard" variant
                }
                if *from_hand_only && !from_hand {
                    continue; // hand-only variant ignores discard-pile buries
                }
                self.fire_if_ready(eff, &owner, None)?;
            }
        }
        Ok(())
    }

    // -- finish sequence + breakout ---------------------------------------

    /// The finish roll: base stat + the whole in-play combo's printed bonuses for the
    /// rolled skill + flat Finish-roll bonuses + crowd meter. Auto-success, else the
    /// defender's breakout attempt decides win vs. resume (DESIGN.md §5/§6).
    fn finish_sequence(&mut self, finisher: &str, defender: &str, card: &Card) -> Eng<()> {
        let mut skill = self.state.rng.roll();
        // Switch-rolled-skill also applies to the Finish roll (Scott Prime): switch
        // before base/combo are computed so they recompute from the new skill.
        if let Some(to) = self.find_switch(finisher, skill)? {
            self.log_effect(
                finisher,
                "SwitchRolledSkill",
                Some(finisher),
                json!({"from": skill.name(), "to": to.name(), "roll": "finish"}),
            );
            skill = to;
        }
        let base = self.stat(finisher, skill);
        let combo: i64 = {
            let in_play = &self.state.players[finisher].in_play;
            in_play
                .iter()
                .map(|c| self.card_finish_bonus(c, skill))
                .sum()
        };
        let bonus = combo + self.finish_roll_bonus(finisher, skill);
        let cm = self.state.crowd_meter;
        let value = base + bonus + cm;
        let auto = crate::finish::is_auto_success(value, cm);
        self.log_finish_attempt(finisher, card, skill, bonus, value, cm, auto);
        if !auto && self.breakout(defender, value) {
            self.on_broken_out(finisher)?; // defender broke out; the match resumes
            return Ok(());
        }
        self.win(finisher, "finish");
        Ok(())
    }

    /// A single in-play card's Finish-roll combo bonus for `skill`, doubled when the
    /// card declares `DoubleFinishIfBumped` and this turn's roll-off bumped.
    fn card_finish_bonus(&self, card: &Card, skill: Skill) -> i64 {
        let mut bonus = card.bonus_for(skill);
        if self.turn_bumped
            && card.effects.iter().any(|eff| {
                eff.actions
                    .iter()
                    .any(|a| matches!(a, Action::DoubleFinishIfBumped))
            })
        {
            bonus *= 2;
        }
        bonus
    }

    /// "+N to your Finish rolls" from the finisher's live effects (in-play combo,
    /// gimmick, entrance), each gated by its condition and by its `when_skill`.
    fn finish_roll_bonus(&self, key: &str, skill: Skill) -> i64 {
        let mut total = 0;
        for eff in self.standing_effects(key) {
            if !conditions::holds(&eff.condition, &self.state, key, None) {
                continue;
            }
            for a in &eff.actions {
                if let Action::FinishRollBonus {
                    delta,
                    when_skill,
                    per,
                    per_who,
                    per_zone,
                    ..
                } = a
                {
                    if when_skill.is_none() || *when_skill == Some(skill) {
                        // Flat `delta`, or `delta * (count of `per_who`'s cards in
                        // `per_zone` matching the filter)` — "+1 per Spotlight in play".
                        let mult = match per {
                            Some(f) => {
                                let who = self.target(*per_who, key);
                                self.state.count_in_zone(f, *per_zone, &who)
                            }
                            None => 1,
                        };
                        total += *delta * mult;
                    }
                }
            }
        }
        total
    }

    #[allow(clippy::too_many_arguments)]
    fn log_finish_attempt(
        &mut self,
        finisher: &str,
        card: &Card,
        skill: Skill,
        bonus: i64,
        value: i64,
        cm: i64,
        auto: bool,
    ) {
        let mut bonus_map = BTreeMap::new();
        if bonus != 0 {
            bonus_map.insert(skill.name().to_owned(), bonus);
        }
        let t = self.state.turn_no;
        self.log(Event::FinishAttempt {
            t,
            player: finisher.to_owned(),
            finish: card.db_uuid.clone(),
            value,
            crowd_meter: cm,
            auto_success: auto,
            bonus: bonus_map,
        });
    }

    /// Total breakout-roll modifier for `defender`'s attempt number `attempt_no`
    /// (1-indexed): the sum of active `BreakoutModifier` deltas from the defender's
    /// own standing effects (gimmick/entrance/in-play combo), each gated by its
    /// condition. An `attempts` gate restricts a modifier to a single attempt
    /// ("your 3rd breakout roll each turn is +2"); `None` applies to every attempt
    /// ("your breakout rolls are +1"). Scans the same standing set as
    /// [`finish_roll_bonus`](Self::finish_roll_bonus).
    fn breakout_bonus(&self, defender: &str, attempt_no: i64) -> i64 {
        let mut total = 0;
        for eff in self.standing_effects(defender) {
            if !conditions::holds(&eff.condition, &self.state, defender, None) {
                continue;
            }
            for a in &eff.actions {
                if let Action::BreakoutModifier { delta, attempts } = a {
                    if attempts.is_none() || *attempts == Some(attempt_no) {
                        total += *delta;
                    }
                }
            }
        }
        total
    }

    /// Up to `BREAKOUT_ATTEMPTS` defender rolls; the first that beats the finish
    /// value breaks out. Returns whether the defender broke out.
    fn breakout(&mut self, defender: &str, finish_value: i64) -> bool {
        let cm = self.state.crowd_meter;
        let mut rolls: Vec<BreakoutRoll> = Vec::new();
        let mut broke = false;
        for i in 0..BREAKOUT_ATTEMPTS {
            let skill = self.state.rng.roll();
            let val = self.stat(defender, skill);
            // A `BreakoutModifier{delta}` raises the roll by `delta`; passing it as a
            // NEGATIVE `penalty` keeps the raw-10-always-breaks rule on the unboosted
            // value (a boosted 8->10 is not a "raw 10"). No modifier -> penalty 0 ->
            // byte-identical to before (the frozen corpus has none).
            let penalty = -self.breakout_bonus(defender, i as i64 + 1);
            let success = crate::finish::stat_breaks_out(val, finish_value, penalty, cm);
            rolls.push(BreakoutRoll {
                skill: skill.name().to_owned(),
                value: val,
                penalty,
                success,
            });
            if success {
                broke = true;
                break;
            }
        }
        let t = self.state.turn_no;
        self.log(Event::Breakout {
            t,
            defender: defender.to_owned(),
            broke_out: broke,
            rolls,
        });
        broke
    }

    /// Breakout aftermath: ALL cards in play on BOTH sides clear to discard (§5),
    /// crowd meter +1, then both players' `OnBreakout` gimmicks fire.
    fn on_broken_out(&mut self, finisher: &str) -> Eng<()> {
        // OnBreakout fires FIRST, while sources are still in play — a card-based recur
        // ("if your opponent breaks out, shuffle Spotlights…") needs its card present
        // before the boards clear. `who` selects whose breakout fires it (None = any);
        // the defender is the breaker. A no-op for decks without OnBreakout, so the
        // frozen corpus (which has none) is byte-identical.
        let breaker = self.state.opponent_of(finisher);
        for key in ["A", "B"] {
            for eff in self.standing_effects(key) {
                let Trigger::OnBreakout { who } = &eff.trigger else {
                    continue;
                };
                if who.is_none_or(|w| self.target(w, key) == breaker) {
                    self.fire_if_ready(&eff, key, None)?;
                }
            }
        }
        for key in ["A", "B"] {
            self.discard_in_play(key);
        }
        self.state.crowd_meter += 1;
        let t = self.state.turn_no;
        let value = self.state.crowd_meter;
        self.log(Event::CrowdMeter { t, delta: 1, value });
        Ok(())
    }

    // -- roll-off ----------------------------------------------------------

    /// Resolve the roll-off, set the active player, and fire the turn-roll gimmicks
    /// (OnWinTurn/OnLoseTurn for the outcome, OnRoll for each side's roll — the
    /// latter outcome-agnostic, DESIGN.md §6/§11).
    fn turn_roll(&mut self) -> Eng<String> {
        let winner = self.roll_off()?;
        self.state.active = winner.clone();
        let loser = self.state.opponent_of(&winner);
        let ctx_w = self.roll_ctx.get(&winner).cloned().unwrap_or_default();
        let ctx_l = self.roll_ctx.get(&loser).cloned().unwrap_or_default();
        let eff_w = self.standing_effects(&winner);
        self.run_effects(&eff_w, "OnWinTurn", &winner, Some(&ctx_w))?;
        let eff_l = self.standing_effects(&loser);
        self.run_effects(&eff_l, "OnLoseTurn", &loser, Some(&ctx_l))?;
        self.run_on_roll("A")?;
        self.run_on_roll("B")?;
        self.state.last_roll_winner = Some(winner.clone()); // "last turn roll" next turn (Dunn)
        Ok(winner)
    }

    /// Fire both players' `OnBump` effects for a bump just taken (a once-per-turn
    /// guard keeps a bump-punish gimmick firing once even across repeated ties).
    fn run_on_bump(&mut self) -> Eng<()> {
        for key in ["A", "B"] {
            let effects = self.standing_effects(key);
            self.run_effects(&effects, "OnBump", key, None)?;
        }
        Ok(())
    }

    /// Fire `key`'s `OnRoll` effects for the deciding roll: matched by the roller's
    /// skill (`None` = any) and gated by the roller's roll context.
    fn run_on_roll(&mut self, key: &str) -> Eng<()> {
        let opp = self.state.opponent_of(key);
        let effects = self.standing_effects(key);
        for eff in &effects {
            let Trigger::OnRoll { skill, who } = &eff.trigger else {
                continue;
            };
            let ctx_key = if *who == Who::SelfSide {
                key
            } else {
                opp.as_str()
            };
            let ctx = self.roll_ctx.get(ctx_key).cloned().unwrap_or_default();
            if skill.is_none() || ctx.skill == *skill {
                self.fire_if_ready(eff, key, Some(&ctx))?;
            }
        }
        Ok(())
    }

    fn roll_off(&mut self) -> Eng<String> {
        let lowest = self.lowest_wins();
        self.promote_pending(); // last turn's `when=NEXT` mods become THIS roll's (#50)
        let (mut sa, mut va) = self.roll_for("A", true);
        let (mut sb, mut vb) = self.roll_for("B", true);
        // Switch-rolled-skill (Scott Prime): "you may switch the rolled skill to
        // Power" — offered before boosts/mods so they land on the switched skill.
        let (nsa, nva, nsb, nvb) = self.offer_switches(sa, va, sb, vb)?;
        sa = nsa;
        va = nva;
        sb = nsb;
        vb = nvb;
        // In-roll boosts (Soborno): after the skill is known, before the winner is
        // decided, a player may pay a cost for +delta to THIS roll.
        va = self.offer_roll_boost("A", sa, va, false)?;
        vb = self.offer_roll_boost("B", sb, vb, false)?;
        let (a, b) = self.apply_in_roll_mods(sa, va, sb, vb); // Tomato: roll-skill debuff
        va = a;
        vb = b;
        let (nsa, nva, nsb, nvb) = self.offer_rerolls(sa, va, sb, vb)?; // Dunn/Jay White
        sa = nsa;
        va = nva;
        sb = nsb;
        vb = nvb;
        self.consume_pending();
        let mut bumps: i64 = 0;
        while bumps < MAX_TIE_REROLLS {
            if let Some((nsa, nva, nsb, nvb, nb)) = self.try_elective_bump(sa, va, sb, vb, bumps)? {
                sa = nsa;
                va = nva;
                sb = nsb;
                vb = nvb;
                bumps = nb;
                continue;
            }
            if va != vb {
                break; // a decided roll: no value tie and no elected bump
            }
            if let Some(forced) = self.tie_winner() {
                return Ok(self.finish_roll_off(sa, va, sb, vb, bumps, forced));
            }
            // Would-bump replacement (Rey Zerblade): pay a cost for +delta *instead*
            // of the bump; if that breaks the tie, the bump is skipped.
            va = self.offer_roll_boost("A", sa, va, true)?;
            vb = self.offer_roll_boost("B", sb, vb, true)?;
            if va != vb {
                break;
            }
            let (nsa, nva, nsb, nvb, nb) = self.do_bump(bumps)?;
            sa = nsa;
            va = nva;
            sb = nsb;
            vb = nvb;
            bumps = nb;
        }
        let winner = roll_winner(va, vb, lowest);
        Ok(self.finish_roll_off(sa, va, sb, vb, bumps, winner))
    }

    /// Record the roll context, latch `turn_bumped`, log the `turn_result`, and
    /// return the decided winner — the shared tail of every roll-off exit.
    fn finish_roll_off(
        &mut self,
        sa: Skill,
        va: i64,
        sb: Skill,
        vb: i64,
        bumps: i64,
        winner: String,
    ) -> String {
        self.record_roll_ctx(sa, va, sb, vb);
        self.turn_bumped = bumps > 0;
        let t = self.state.turn_no;
        self.log(Event::TurnResult {
            t,
            winner: winner.clone(),
            tie_bumps: bumps,
        });
        winner
    }

    /// The elective same-skill bump (Mastermind's "Ringside Ruckus"): both rolled
    /// the same skill but different values, so the owner MAY spend a per-match
    /// charge to bump instead of resolving. `Some(fresh roll)` if a bump was taken.
    #[allow(clippy::type_complexity)]
    fn try_elective_bump(
        &mut self,
        sa: Skill,
        va: i64,
        sb: Skill,
        vb: i64,
        bumps: i64,
    ) -> Eng<Option<(Skill, i64, Skill, i64, i64)>> {
        if va == vb || sa != sb {
            return Ok(None);
        }
        let Some(owner) = self.elective_bump_owner() else {
            return Ok(None);
        };
        if !self.elect_bump(&owner, va, vb)? {
            return Ok(None);
        }
        Ok(Some(self.do_bump(bumps)?))
    }

    /// Perform a bump: both draw 1, fire OnBump punishes, and re-roll (pending mods
    /// are dropped on a bump re-roll). Returns the fresh `(sa, va, sb, vb, bumps+1)`.
    fn do_bump(&mut self, bumps: i64) -> Eng<(Skill, i64, Skill, i64, i64)> {
        self.draw("A", 1, DeckEnd::Top)?;
        self.draw("B", 1, DeckEnd::Top)?;
        let bumps = bumps + 1;
        self.run_on_bump()?; // bump-punish gimmicks (Mastermind: opp next roll -2)
        let (sa, va) = self.roll_for("A", false);
        let (sb, vb) = self.roll_for("B", false);
        let (sa, va, sb, vb) = self.offer_switches(sa, va, sb, vb)?; // a bump re-roll is a turn roll too
        let (va, vb) = self.apply_in_roll_mods(sa, va, sb, vb); // debuff re-rolls too
        let (sa, va, sb, vb) = self.offer_rerolls(sa, va, sb, vb)?; // re-roll offered post-bump too
        Ok((sa, va, sb, vb, bumps))
    }

    /// Offer each side its once-per-turn turn-roll re-roll (Dunn, Jay White). A taken
    /// re-roll REPLACES that side's (skill, value) with a fresh die — kept even if
    /// worse — and spends the `ONCE_PER_TURN` charge; declining leaves it for a later
    /// roll in the same roll-off (initial or any bump). Re-checked each call, so Jay
    /// White keys on the opponent's *current* roll. Boosts/in-roll mods are not
    /// re-applied to a re-rolled die (no re-roll competitor also carries those).
    fn offer_rerolls(
        &mut self,
        mut sa: Skill,
        mut va: i64,
        mut sb: Skill,
        mut vb: i64,
    ) -> Eng<(Skill, i64, Skill, i64)> {
        let ctx_a = RollContext {
            skill: Some(sa),
            gap: Some(vb - va),
            value: Some(va),
            opp_skill: Some(sb),
        };
        let ctx_b = RollContext {
            skill: Some(sb),
            gap: Some(va - vb),
            value: Some(vb),
            opp_skill: Some(sa),
        };
        // Each side may spend a re-roll; the target die (own, the opponent's, or a
        // chosen player's) is re-rolled in place.
        for owner in ["A", "B"] {
            let (own_ctx, opp_ctx) = if owner == "A" {
                (&ctx_a, &ctx_b)
            } else {
                (&ctx_b, &ctx_a)
            };
            if let Some(target) = self.offer_reroll(owner, own_ctx, opp_ctx)? {
                let (ns, nv) = self.roll_for(&target, false);
                self.log_effect(
                    owner,
                    "Reroll",
                    Some(&target),
                    json!({"skill": ns.name(), "value": nv}),
                );
                if target == "A" {
                    sa = ns;
                    va = nv;
                } else {
                    sb = ns;
                    vb = nv;
                }
            }
        }
        Ok((sa, va, sb, vb))
    }

    /// Offer each side its "switch the rolled skill" option (Scott Prime). A taken
    /// switch replaces that side's rolled `(skill, value)` — the die keeps its roll
    /// mods (value is recomputed on the new skill's stat). Offered at every turn-roll
    /// point (initial roll + each bump re-roll), mirroring `offer_rerolls`.
    fn offer_switches(
        &mut self,
        mut sa: Skill,
        mut va: i64,
        mut sb: Skill,
        mut vb: i64,
    ) -> Eng<(Skill, i64, Skill, i64)> {
        for owner in ["A", "B"] {
            let (skill, value) = if owner == "A" { (sa, va) } else { (sb, vb) };
            if let Some((ns, nv)) = self.offer_switch(owner, skill, value)? {
                if owner == "A" {
                    sa = ns;
                    va = nv;
                } else {
                    sb = ns;
                    vb = nv;
                }
            }
        }
        Ok((sa, va, sb, vb))
    }

    /// `owner`'s turn-roll switch: if a standing `SwitchRolledSkill` fires for the
    /// rolled `skill`, recompute the value on the new skill (`value` minus the old
    /// skill's stat plus the new one's, preserving any roll mods) and log it.
    fn offer_switch(&mut self, owner: &str, skill: Skill, value: i64) -> Eng<Option<(Skill, i64)>> {
        let Some(to) = self.find_switch(owner, skill)? else {
            return Ok(None);
        };
        let nv = value - self.stat(owner, skill) + self.stat(owner, to);
        self.log_effect(
            owner,
            "SwitchRolledSkill",
            Some(owner),
            json!({"from": skill.name(), "to": to.name(), "value": nv}),
        );
        Ok(Some((to, nv)))
    }

    /// The first standing `SwitchRolledSkill` effect whose `from` matches the rolled
    /// `skill`, whose gate holds, and whose optional offer is taken; returns its `to`
    /// skill (the switched-to skill), or `None`. Shared by the turn roll-off and the
    /// Finish roll (both trigger "when you roll `from`").
    fn find_switch(&mut self, owner: &str, skill: Skill) -> Eng<Option<Skill>> {
        let effects = self.standing_effects(owner);
        for eff in &effects {
            let Some((from, to)) = eff.actions.iter().find_map(|a| match a {
                Action::SwitchRolledSkill { from_skill, to } => Some((*from_skill, *to)),
                _ => None,
            }) else {
                continue;
            };
            if skill != from {
                continue;
            }
            let ctx = RollContext {
                skill: Some(skill),
                gap: None,
                value: Some(self.stat(owner, skill)),
                opp_skill: None,
            };
            if !(self.may_fire(eff, owner)
                && conditions::holds(&eff.condition, &self.state, owner, Some(&ctx)))
            {
                continue;
            }
            if eff.optional && !self.take_optional(eff, owner)? {
                continue; // declined "you may switch"
            }
            self.mark_fired(eff, owner);
            return Ok(Some(to));
        }
        Ok(None)
    }

    /// `owner`'s re-roll offer: the first standing `Reroll` effect whose gate holds
    /// and whose charge is unspent is offered; returns the KEY of the player whose die
    /// should be re-rolled (own / opponent / a chosen player), or `None` if none fires.
    /// The gate reads the opponent's roll for an `InRoll{who=OPP}` trigger (Jay White
    /// "when your opponent rolls 9/10"), else the owner's (Reverend "when you roll …").
    fn offer_reroll(
        &mut self,
        owner: &str,
        own_ctx: &RollContext,
        opp_ctx: &RollContext,
    ) -> Eng<Option<String>> {
        let effects = self.standing_effects(owner);
        for eff in &effects {
            // Only a THIS re-roll is offered structurally; a NEXT re-roll is a
            // deferred grant (handled by `act_reroll` + `reroll_grants`), not fired here.
            let Some((who, choose)) = eff.actions.iter().find_map(|a| match a {
                Action::Reroll {
                    who,
                    choose,
                    when: RollWhen::This,
                    ..
                } => Some((*who, *choose)),
                _ => None,
            }) else {
                continue;
            };
            let gate_ctx = match eff.trigger {
                Trigger::InRoll { who: Who::Opp, .. } => opp_ctx,
                _ => own_ctx,
            };
            if !(self.may_fire(eff, owner)
                && conditions::holds(&eff.condition, &self.state, owner, Some(gate_ctx)))
            {
                continue;
            }
            if eff.optional && !self.take_optional(eff, owner)? {
                continue; // declined "you may" — charge left for a later roll
            }
            self.mark_fired(eff, owner);
            let target = if choose {
                self.decide_reroll_target(owner)?
            } else if who == Who::Opp {
                self.state.opponent_of(owner)
            } else {
                owner.to_owned()
            };
            return Ok(Some(target));
        }
        // A granted "re-roll your next turn roll" (King Brian Cage): a one-shot
        // optional self-re-roll, usable at any roll point until spent.
        if self.state.players[owner].reroll_grants.this_turn > 0 && self.offer_yes_no(owner)? {
            self.state
                .players
                .get_mut(owner)
                .unwrap()
                .reroll_grants
                .this_turn -= 1;
            return Ok(Some(owner.to_owned()));
        }
        Ok(None)
    }

    /// A bare optional yes/no offer to `key` (no backing effect) — the policy's
    /// `optional` read decides.
    fn offer_yes_no(&mut self, key: &str) -> Eng<bool> {
        let legal = vec![json!({"kind": "yes"}), json!({"kind": "no"})];
        Ok(self.decide("optional", key, legal)?["kind"] == "yes")
    }

    /// "Choose any player to re-roll" (Grim Librarian): the owner picks which side.
    fn decide_reroll_target(&mut self, owner: &str) -> Eng<String> {
        let legal = vec![
            json!({"kind": "reroll_target", "target": "OPP"}),
            json!({"kind": "reroll_target", "target": "SELF"}),
        ];
        let chosen = self.decide("reroll_target", owner, legal)?;
        Ok(if chosen["target"] == "SELF" {
            owner.to_owned()
        } else {
            self.state.opponent_of(owner)
        })
    }

    /// A player holding an `ElectBumpOnSameSkill` grant with a per-match charge
    /// still available (else `None`).
    fn elective_bump_owner(&self) -> Option<String> {
        for key in ["A", "B"] {
            for eff in self.standing_effects(key) {
                for a in &eff.actions {
                    if let Action::ElectBumpOnSameSkill { uses } = a {
                        let used = self.state.players[key]
                            .freq_counters
                            .get("match:elect_bump")
                            .copied()
                            .unwrap_or(0);
                        if used < *uses {
                            return Some(key.to_owned());
                        }
                    }
                }
            }
        }
        None
    }

    /// Offer `owner` the elective same-skill bump and spend a charge if taken. The
    /// options carry a `losing` hint so a policy can bump a loss into a re-roll.
    fn elect_bump(&mut self, owner: &str, va: i64, vb: i64) -> Eng<bool> {
        let (mine, theirs) = if owner == "A" { (va, vb) } else { (vb, va) };
        let losing = mine < theirs;
        let legal = vec![
            json!({"kind": "yes", "point": "elect_bump", "losing": losing}),
            json!({"kind": "no", "point": "elect_bump", "losing": losing}),
        ];
        if self.decide("elect_bump", owner, legal)?["kind"] != "yes" {
            return Ok(false);
        }
        let fc = &mut self.state.players.get_mut(owner).unwrap().freq_counters;
        let cur = fc.get("match:elect_bump").copied().unwrap_or(0);
        fc.insert("match:elect_bump".to_owned(), cur + 1);
        Ok(true)
    }

    /// Offer `key`'s in-roll boosts for a roll of `skill` and return the (maybe
    /// boosted) value. `on_bump` selects the initial-roll boosts (Soborno) vs the
    /// would-bump-tie ones (Rey Zerblade); taking one pays its cost then adds delta.
    fn offer_roll_boost(&mut self, key: &str, skill: Skill, value: i64, on_bump: bool) -> Eng<i64> {
        let effects = self.standing_effects(key);
        let mut value = value;
        for eff in &effects {
            let Trigger::OnRollBoost {
                skill: tskill,
                delta,
                on_bump: t_on_bump,
            } = &eff.trigger
            else {
                continue;
            };
            if *t_on_bump != on_bump || (tskill.is_some() && *tskill != Some(skill)) {
                continue;
            }
            if !(self.may_fire(eff, key)
                && conditions::holds(&eff.condition, &self.state, key, None))
            {
                continue;
            }
            if eff.optional && !self.take_optional(eff, key)? {
                continue;
            }
            self.mark_fired(eff, key);
            self.apply_actions(eff, key)?; // pay the cost (e.g. a type-matched discard)
            value += *delta;
            self.log_effect(
                key,
                "RollBoost",
                Some(key),
                json!({"skill": skill.name(), "delta": *delta}),
            );
        }
        Ok(value)
    }

    /// Apply automatic in-roll modifiers to the current roll (Tomato Tomato Jr.:
    /// "when you or your target roll Power, your target's roll is -1"). Each matching
    /// `InRoll` effect's `ModifyRoll(when=THIS)` deltas land on the named side — one
    /// action, one application, so an `either`-gated debuff is capped, never doubled.
    fn apply_in_roll_mods(&self, sa: Skill, va: i64, sb: Skill, vb: i64) -> (i64, i64) {
        let mut vals: BTreeMap<&str, i64> = BTreeMap::from([("A", va), ("B", vb)]);
        // Roll context for the in-progress roll-off, so a value-gated in-roll modifier
        // (Numer01: "when your opponent's turn roll is 10, your roll is +2") can read
        // the current roll — the recorded `roll_ctx` is not written until the roll-off
        // resolves. Which side's roll the condition reads follows the trigger's `who`,
        // exactly as the OnRoll path does (see `RollValue`).
        let ctx_a = RollContext {
            skill: Some(sa),
            gap: Some(vb - va),
            value: Some(va),
            opp_skill: Some(sb),
        };
        let ctx_b = RollContext {
            skill: Some(sb),
            gap: Some(va - vb),
            value: Some(vb),
            opp_skill: Some(sa),
        };
        for owner in ["A", "B"] {
            let opp = self.state.opponent_of(owner);
            for eff in self.standing_effects(owner) {
                if !matches!(eff.trigger, Trigger::InRoll { .. })
                    || !self.in_roll_matches(&eff.trigger, owner, sa, sb)
                {
                    continue;
                }
                let Trigger::InRoll { who, .. } = &eff.trigger else {
                    continue;
                };
                let reads_self = *who == Who::SelfSide;
                let cond_ctx = match (owner, reads_self) {
                    ("A", true) | ("B", false) => &ctx_a,
                    _ => &ctx_b,
                };
                if !conditions::holds(&eff.condition, &self.state, owner, Some(cond_ctx)) {
                    continue;
                }
                for a in &eff.actions {
                    if let Action::ModifyRoll {
                        who, delta, when, ..
                    } = a
                    {
                        if *when == RollWhen::This {
                            let target = if *who == Who::SelfSide {
                                owner
                            } else {
                                opp.as_str()
                            };
                            *vals.get_mut(target).unwrap() += *delta;
                        }
                    }
                }
            }
        }
        (vals["A"], vals["B"])
    }

    /// Whether an `InRoll` trigger fires for this roll (skill gate; `either` fires
    /// once if either side rolled the skill — a capped modifier).
    fn in_roll_matches(&self, trig: &Trigger, owner: &str, sa: Skill, sb: Skill) -> bool {
        let Trigger::InRoll { skill, who, either } = trig else {
            return false;
        };
        let Some(want) = skill else {
            return true;
        };
        if *either {
            return sa == *want || sb == *want;
        }
        let opp = self.state.opponent_of(owner);
        let roller = if *who == Who::SelfSide {
            owner
        } else {
            opp.as_str()
        };
        let rolled = if roller == "A" { sa } else { sb };
        rolled == *want
    }

    /// True iff either side's active gimmick declares the roll-off lowest-wins (a
    /// Static `LowestRollWins`; blanking Fae restores highest-wins).
    fn lowest_wins(&self) -> bool {
        for key in ["A", "B"] {
            for eff in self.standing_effects(key) {
                if matches!(eff.trigger, Trigger::Static)
                    && eff
                        .actions
                        .iter()
                        .any(|a| matches!(a, Action::LowestRollWins))
                {
                    return true;
                }
            }
        }
        false
    }

    /// Stash each side's rolled skill + signed gap (opponent minus self, so a
    /// positive gap means that side rolled lower) for roll-scoped conditions.
    fn record_roll_ctx(&mut self, sa: Skill, va: i64, sb: Skill, vb: i64) {
        self.roll_ctx = BTreeMap::from([
            (
                "A".to_owned(),
                RollContext {
                    skill: Some(sa),
                    gap: Some(vb - va),
                    value: Some(va),
                    opp_skill: Some(sb),
                },
            ),
            (
                "B".to_owned(),
                RollContext {
                    skill: Some(sb),
                    gap: Some(va - vb),
                    value: Some(vb),
                    opp_skill: Some(sa),
                },
            ),
        ]);
    }

    fn roll_for(&mut self, key: &str, use_pending: bool) -> (Skill, i64) {
        let skill = self.state.rng.roll();
        let base = self.stat(key, skill);
        let delta = if use_pending {
            self.state.players[key].pending_roll_mods.this_turn
        } else {
            0
        };
        let mut mods = Vec::new();
        if delta != 0 {
            mods.push(RollMod {
                src: "pending".to_owned(),
                delta,
            });
        }
        let value = base + delta;
        let t = self.state.turn_no;
        self.log(Event::Roll {
            t,
            player: key.to_owned(),
            skill: skill.name().to_owned(),
            base,
            value,
            mods,
        });
        (skill, value)
    }

    /// Fold a queued `when=NEXT` roll mod into the imminent roll (#50): promoting
    /// `next -> this` at the START of the following roll-off makes such a mod land
    /// on the immediately-following roll, not the turn after.
    fn promote_pending(&mut self) {
        for player in self.state.players.values_mut() {
            player.pending_roll_mods.this_turn += player.pending_roll_mods.next_turn;
            player.pending_roll_mods.next_turn = 0;
        }
    }

    /// The initial roll spent `this`; clear it so a pending mod applies once (bump
    /// re-rolls run with `use_pending=false`, so they never re-read it).
    fn consume_pending(&mut self) {
        for player in self.state.players.values_mut() {
            player.pending_roll_mods.this_turn = 0;
        }
    }

    /// The forced tie winner: the sole holder of a `win_tie` flag (consumed here),
    /// or `None` if zero or both sides hold it (then the tie bumps).
    fn tie_winner(&mut self) -> Option<String> {
        let mut holders = Vec::new();
        for (k, p) in self.state.players.iter_mut() {
            if p.flags
                .remove("win_tie")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                holders.push(k.clone());
            }
        }
        if holders.len() == 1 {
            holders.into_iter().next()
        } else {
            None
        }
    }

    /// Draw for the won turn; `Ok(false)` if the game ended by count-out (exhausting
    /// deck+hand on a won turn is a win).
    fn draw_for_turn(&mut self, key: &str) -> Eng<bool> {
        let player = &self.state.players[key];
        if player.deck.is_empty() && player.hand.is_empty() {
            self.win(key, "count_out");
            return Ok(false);
        }
        self.draw(key, 1, DeckEnd::Top)?;
        Ok(true)
    }
}

// ---------------------------------------------------------------------------
// Free helpers
// ---------------------------------------------------------------------------

/// The roll-off winner. Highest roll wins, unless a lowest-wins gimmick (Fae)
/// flips it to the lowest; A holds the edge on a residual tie.
fn roll_winner(va: i64, vb: i64, lowest: bool) -> String {
    let a_wins = if lowest { va <= vb } else { va >= vb };
    if a_wins { "A" } else { "B" }.to_owned()
}

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
        Trigger::OnBury { .. } => "OnBury",
        Trigger::StartOfTurn => "StartOfTurn",
        Trigger::StartOfMatch => "StartOfMatch",
        Trigger::OnBreakout { .. } => "OnBreakout",
        Trigger::OnShuffle { .. } => "OnShuffle",
        Trigger::OnDiscardMove { .. } => "OnDiscardMove",
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

/// Whether a `Stop` action's order/type filter covers this attack (`None` = any).
/// Whether `attack` satisfies every `StopRequiresTag` gate in a stop `eff` — a
/// passive marker paired with a sibling `Stop`, requiring the attacked card carry
/// the named tag ("Stop any Grapple **with a Spotlight**"). No gate ⇒ always true.
fn attacker_meets_tag_gates(eff: &Effect, attack: &Card) -> bool {
    eff.actions.iter().all(|a| match a {
        Action::StopRequiresTag { tag } => attack.tags.contains(tag),
        _ => true,
    })
}

fn stop_matches(stop: &Action, attack: &Card) -> bool {
    let Action::Stop {
        order, atk_type, ..
    } = stop
    else {
        return false;
    };
    if let Some(o) = order {
        if *o != attack.play_order {
            return false;
        }
    }
    atk_type.is_none() || *atk_type == Some(attack.atk_type)
}

/// Whether `attack` declares itself `Unstoppable` against `stopper` — an
/// `Unstoppable` whose `by_order` is the stopper's play order (or `None` = by
/// anything). "Cannot be stopped by Follow Ups".
fn is_unstoppable_by(attack: &Card, stopper: &Card) -> bool {
    attack.effects.iter().any(|eff| {
        eff.actions.iter().any(|a| {
            matches!(a, Action::Unstoppable { by_order }
                if by_order.is_none() || *by_order == Some(stopper.play_order))
        })
    })
}

/// Whether `card` is a legal play given the player's own persistent board (the
/// order-only chain, DESIGN.md §6): a Lead always; a Follow Up needs a Lead; a
/// Finish needs a Follow Up. Type is irrelevant to the chain.
fn playable(board: &[Card], card: &Card) -> bool {
    match card.play_order {
        PlayOrder::Lead => true,
        PlayOrder::Followup => board.iter().any(|c| c.play_order == PlayOrder::Lead),
        PlayOrder::Finish => board.iter().any(|c| c.play_order == PlayOrder::Followup),
        PlayOrder::None => false,
    }
}

/// Whether `card` can act as a Stop — carries at least one `Stop` action (its
/// online condition is not checked; a revealed Stop is discarded regardless).
fn is_stop_card(card: &Card) -> bool {
    card.effects
        .iter()
        .any(|eff| eff.actions.iter().any(|a| matches!(a, Action::Stop { .. })))
}

/// Whether a card revealed by [`Engine::act_reveal_for_draw`] counts toward the
/// draw: a Stop card (`Stop`), or one whose move type equals the actor's rolled
/// skill (`RolledSkill`; no match when the actor did not roll a move skill).
fn reveal_matches(card: &Card, match_on: RevealMatch, rolled: Option<Skill>) -> bool {
    match match_on {
        RevealMatch::Stop => is_stop_card(card),
        RevealMatch::RolledSkill => {
            rolled.is_some_and(|sk| atk_type_matches_skill(card.atk_type, sk))
        }
    }
}

/// True iff a card's attack (move) type is the same move as `skill` — i.e. one of
/// the three move skills Strike/Grapple/Submission and matching. `AtkType::None`
/// and the non-move skills (Power/Agility/Technique) never match.
fn atk_type_matches_skill(atk: AtkType, skill: Skill) -> bool {
    matches!(
        (atk, skill),
        (AtkType::Strike, Skill::Strike)
            | (AtkType::Grapple, Skill::Grapple)
            | (AtkType::Submission, Skill::Submission)
    )
}

/// Value a scried card by how much the actor wants it kept/drawn: a Finish (a
/// win condition) over a stop (defense) over a plain card. Mirrors the
/// discard-recycle read so scry keeps the deck's best on top / in hand.
fn scry_value(card: &Card) -> i64 {
    if card.play_order == PlayOrder::Finish {
        3
    } else if is_stop_card(card) {
        2
    } else {
        1
    }
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
        Action::SwapHandDiscard => "SwapHandDiscard",
        Action::RecurToDeckTop { .. } => "RecurToDeckTop",
        Action::CountsAsInPlay { .. } => "CountsAsInPlay",
        Action::RemoveFromPlay { .. } => "RemoveFromPlay",
        Action::ReturnToHand { .. } => "ReturnToHand",
        Action::RevealAndDiscard { .. } => "RevealAndDiscard",
        Action::RevealForDraw { .. } => "RevealForDraw",
        Action::Peek { .. } => "Peek",
        Action::Scry { .. } => "Scry",
        Action::RevealRoute { .. } => "RevealRoute",
        Action::ShuffleHandDraw { .. } => "ShuffleHandDraw",
        Action::ModifyRoll { .. } => "ModifyRoll",
        Action::BuffSkill { .. } => "BuffSkill",
        Action::MaxHandSize { .. } => "MaxHandSize",
        Action::AddText { .. } => "AddText",
        Action::StopRequiresTag { .. } => "StopRequiresTag",
        Action::Reroll { .. } => "Reroll",
        Action::SwitchRolledSkill { .. } => "SwitchRolledSkill",
        Action::WinTie { .. } => "WinTie",
        Action::Bump { .. } => "Bump",
        Action::ElectBumpOnSameSkill { .. } => "ElectBumpOnSameSkill",
        Action::Stop { .. } => "Stop",
        Action::BlankGimmick { .. } => "BlankGimmick",
        Action::FlipGimmick { .. } => "FlipGimmick",
        Action::BlankText { .. } => "BlankText",
        Action::BlankStoppedText => "BlankStoppedText",
        Action::ChooseName { .. } => "ChooseName",
        Action::LoseBy { .. } => "LoseBy",
        Action::DisqualificationRule { .. } => "DisqualificationRule",
        Action::ConsideredCompare { .. } => "ConsideredCompare",
        Action::SuppressOpponentDraw => "SuppressOpponentDraw",
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

#[cfg(test)]
mod breakout_modifier_tests {
    use super::*;

    fn deck(uuid: &str) -> Deck {
        serde_json::from_value(json!({
            "competitor": {
                "db_uuid": uuid, "name": uuid, "division": "World Championship",
                "stats": {"Power": 5, "Agility": 5, "Technique": 5,
                          "Submission": 5, "Grapple": 5, "Strike": 5},
            },
            "entrance": {"db_uuid": format!("{uuid}-ent"), "name": "ent"},
            "cards": [],
        }))
        .expect("deck")
    }

    /// A `Static` gimmick effect wrapping a single `BreakoutModifier`, gated by
    /// `condition` ("Always" by default).
    fn breakout_mod(delta: i64, attempts: Value, condition: Value) -> Value {
        json!({
            "@type": "Effect",
            "trigger": {"@type": "Static"},
            "condition": condition,
            "actions": [{"@type": "BreakoutModifier", "delta": delta, "attempts": attempts}],
            "duration": "WHILE_IN_PLAY",
            "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
            "raw_clause": "test", "source": "gimmick", "optional": false
        })
    }

    fn engine() -> Engine {
        let decider = Box::new(ReplayDecider::new(BTreeMap::new(), BTreeMap::new()));
        Engine::new(
            deck("A"),
            deck("B"),
            decider,
            1,
            String::new(),
            "sim".into(),
        )
    }

    fn push_gimmick(engine: &mut Engine, key: &str, eff: Value) {
        engine
            .state
            .players
            .get_mut(key)
            .unwrap()
            .competitor
            .effects
            .push(serde_json::from_value(eff).expect("effect"));
    }

    #[test]
    fn attempts_gate_selects_the_nth_roll() {
        // El Super Hombre V1: "Your 3rd breakout roll each turn is +2." Applies only
        // to the 3rd attempt; the 1st and 2nd see nothing.
        let mut engine = engine();
        push_gimmick(
            &mut engine,
            "A",
            breakout_mod(2, json!(3), json!({"@type": "Always"})),
        );
        assert_eq!(engine.breakout_bonus("A", 1), 0);
        assert_eq!(engine.breakout_bonus("A", 2), 0);
        assert_eq!(engine.breakout_bonus("A", 3), 2);
    }

    #[test]
    fn unattempted_modifier_applies_to_every_roll_and_stacks() {
        // A flat "your breakout rolls are +1" (attempts null) applies to all three,
        // and stacks additively with an attempt-gated modifier.
        let mut engine = engine();
        push_gimmick(
            &mut engine,
            "A",
            breakout_mod(1, Value::Null, json!({"@type": "Always"})),
        );
        push_gimmick(
            &mut engine,
            "A",
            breakout_mod(2, json!(3), json!({"@type": "Always"})),
        );
        assert_eq!(engine.breakout_bonus("A", 1), 1);
        assert_eq!(engine.breakout_bonus("A", 3), 3);
    }

    #[test]
    fn false_condition_and_wrong_side_do_not_count() {
        // A gated modifier whose condition is false contributes nothing, and a
        // modifier on B never leaks into A's breakout (each reads its own standing set).
        let mut engine = engine();
        push_gimmick(
            &mut engine,
            "A",
            breakout_mod(
                2,
                Value::Null,
                json!({"@type": "CrowdMeterCompare", "cmp": ">=", "value": 5}),
            ),
        );
        push_gimmick(
            &mut engine,
            "B",
            breakout_mod(4, Value::Null, json!({"@type": "Always"})),
        );
        assert_eq!(engine.breakout_bonus("A", 1), 0);
        assert_eq!(engine.breakout_bonus("B", 1), 4);
    }

    #[test]
    fn blanked_gimmick_suppresses_the_modifier() {
        // A blanked gimmick contributes no breakout modifier (standing_effects skips it).
        let mut engine = engine();
        push_gimmick(
            &mut engine,
            "A",
            breakout_mod(2, json!(3), json!({"@type": "Always"})),
        );
        engine.state.players.get_mut("A").unwrap().gimmick_blanked = true;
        assert_eq!(engine.breakout_bonus("A", 3), 0);
    }

    #[test]
    fn breakout_roll_honors_the_modifier() {
        // The defender's stats are all 5, so a finish of 8 is unbreakable (5 < 8) with
        // no modifier — but a flat +5 breakout modifier lifts every roll to 10 and
        // breaks out on the first attempt. Drives the real `breakout()` roll, proving
        // the bonus reaches `stat_breaks_out` as a negative penalty.
        let mut engine = engine();
        assert!(!engine.breakout("A", 8), "5 < 8 cannot break out unaided");
        push_gimmick(
            &mut engine,
            "A",
            breakout_mod(5, Value::Null, json!({"@type": "Always"})),
        );
        assert!(
            engine.breakout("A", 8),
            "+5 lifts the roll to 10 and breaks out"
        );
        // The applied modifier is recorded as a negative penalty on the roll.
        let Some(Event::Breakout { rolls, .. }) = engine.log.events.last() else {
            panic!("last event is a Breakout");
        };
        assert_eq!(rolls[0].penalty, -5);
    }
}

#[cfg(test)]
mod on_stop_order_tests {
    use super::*;

    fn card(uuid: &str, order: &str) -> Value {
        json!({
            "atk_type": "Strike", "db_uuid": uuid, "effects": [], "finish_bonuses": {},
            "name": uuid, "number": 1, "play_order": order, "raw_text": "", "tags": []
        })
    }

    /// La Fenix (Super Lucha): A's gimmick tutors a Finish to hand when A's *Finish*
    /// is stopped (`OnStop{dir: YOURS, order: Finish}`). A's deck holds one Finish
    /// (the tutor target) and one Lead.
    fn la_fenix_engine() -> Engine {
        let gimmick = json!({
            "@type": "Effect",
            "trigger": {"@type": "OnStop", "dir": "YOURS", "order": "Finish"},
            "condition": {"@type": "Always"},
            "actions": [{"@type": "Search",
                "filter": {"@type": "CardFilter", "number": null, "atk_type": null,
                           "play_order": "Finish", "tag": null, "name": null, "raw": null},
                "dest": "HAND", "count": 1}],
            "duration": "INSTANT",
            "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
            "raw_clause": "test", "source": "gimmick", "optional": false
        });
        let deck_a: Deck = serde_json::from_value(json!({
            "competitor": {"db_uuid": "LF", "name": "La Fenix", "division": "World Championship",
                "stats": {"Power":5,"Agility":5,"Technique":5,"Submission":5,"Grapple":5,"Strike":5},
                "effects": [gimmick]},
            "entrance": {"db_uuid": "LF-ent", "name": "ent"},
            "cards": [card("tutor-finish", "Finish"), card("some-lead", "Lead")],
        }))
        .expect("deck A");
        let deck_b: Deck = serde_json::from_value(json!({
            "competitor": {"db_uuid": "B", "name": "B", "division": "World Championship",
                "stats": {"Power":5,"Agility":5,"Technique":5,"Submission":5,"Grapple":5,"Strike":5}},
            "entrance": {"db_uuid": "B-ent", "name": "ent"}, "cards": [],
        }))
        .expect("deck B");
        let decider = Box::new(ReplayDecider::new(BTreeMap::new(), BTreeMap::new()));
        Engine::new(deck_a, deck_b, decider, 1, String::new(), "sim".into())
    }

    fn tutored(engine: &Engine) -> bool {
        engine.state.players["A"]
            .hand
            .iter()
            .any(|c| c.db_uuid == "tutor-finish")
    }

    #[test]
    fn stopping_a_finish_fires_the_order_gated_tutor() {
        let mut engine = la_fenix_engine();
        let attack: Card = serde_json::from_value(card("my-finish", "Finish")).unwrap();
        let stop: Card = serde_json::from_value(card("their-stop", "Lead")).unwrap();
        engine.apply_stop("A", "B", attack, stop).unwrap();
        assert!(
            tutored(&engine),
            "a stopped Finish tutors the deck Finish to hand"
        );
    }

    #[test]
    fn stopping_a_lead_does_not_fire_the_finish_gated_tutor() {
        let mut engine = la_fenix_engine();
        let attack: Card = serde_json::from_value(card("my-lead", "Lead")).unwrap();
        let stop: Card = serde_json::from_value(card("their-stop", "Lead")).unwrap();
        engine.apply_stop("A", "B", attack, stop).unwrap();
        assert!(
            !tutored(&engine),
            "the order=Finish gate stays inert when a Lead is stopped"
        );
    }
}

#[cfg(test)]
mod on_shuffle_tests {
    use super::*;

    fn card(uuid: &str) -> Value {
        json!({
            "atk_type": "Strike", "db_uuid": uuid, "effects": [], "finish_bonuses": {},
            "name": uuid, "number": 1, "play_order": "Lead", "raw_text": "", "tags": []
        })
    }

    /// Memes Dealer V2 on A: `OnShuffle{who=OPP}` → Draw 2, so A draws whenever B's
    /// deck is shuffled by an effect. Both decks hold cards so the draw is observable.
    fn memes_engine() -> Engine {
        let gimmick = json!({
            "@type": "Effect",
            "trigger": {"@type": "OnShuffle", "who": "OPP"},
            "condition": {"@type": "Always"},
            "actions": [{"@type": "Draw", "n": 2, "source": "TOP", "who": "SELF",
                         "per": null, "per_who": "SELF"}],
            "duration": "INSTANT",
            "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
            "raw_clause": "test", "source": "gimmick", "optional": false
        });
        let stats =
            json!({"Power":5,"Agility":5,"Technique":5,"Submission":5,"Grapple":5,"Strike":5});
        let cards: Vec<Value> = (0..10).map(|i| card(&format!("c{i}"))).collect();
        let deck_a: Deck = serde_json::from_value(json!({
            "competitor": {"db_uuid": "MD", "name": "Memes", "division": "Underworld",
                "stats": stats, "effects": [gimmick]},
            "entrance": {"db_uuid": "MD-ent", "name": "ent"}, "cards": cards.clone(),
        }))
        .expect("deck A");
        let deck_b: Deck = serde_json::from_value(json!({
            "competitor": {"db_uuid": "B", "name": "B", "division": "Underworld", "stats": stats},
            "entrance": {"db_uuid": "B-ent", "name": "ent"}, "cards": cards,
        }))
        .expect("deck B");
        let decider = Box::new(ReplayDecider::new(BTreeMap::new(), BTreeMap::new()));
        Engine::new(deck_a, deck_b, decider, 1, String::new(), "sim".into())
    }

    fn hand(engine: &Engine, key: &str) -> usize {
        engine.state.players[key].hand.len()
    }

    #[test]
    fn opponents_effect_shuffle_fires_the_draw() {
        // B shuffles their own deck via an effect -> A (the opponent) draws 2.
        let mut engine = memes_engine();
        engine.act_shuffle_deck(Who::SelfSide, "B").unwrap();
        assert_eq!(hand(&engine, "A"), 2, "A draws 2 when B's deck is shuffled");
    }

    #[test]
    fn own_shuffle_does_not_fire_the_opp_gated_draw() {
        // A shuffling their OWN deck must not fire A's who=OPP OnShuffle.
        let mut engine = memes_engine();
        engine.act_shuffle_deck(Who::SelfSide, "A").unwrap();
        assert_eq!(
            hand(&engine, "A"),
            0,
            "who=OPP does not fire on your own shuffle"
        );
    }

    #[test]
    fn setup_shuffle_does_not_fire_on_shuffle() {
        // The match-start setup shuffle bypasses OnShuffle: A gets only its opening hand.
        let mut engine = memes_engine();
        engine.setup().unwrap();
        assert_eq!(
            hand(&engine, "A"),
            OPENING_HAND,
            "setup shuffle draws no OnShuffle bonus"
        );
    }
}

#[cfg(test)]
mod on_discard_move_tests {
    use super::*;

    /// Always takes the first legal option — these tests exercise the trigger's
    /// firing, not the choice, and every decision point here is a card pick.
    struct FirstLegal;

    impl Decider for FirstLegal {
        fn decide(
            &mut self,
            _point: &str,
            _viewer: &str,
            legal: &[Value],
            _state: &mut GameState,
        ) -> Option<Value> {
            legal.first().cloned()
        }

        fn policy_name(&self, _viewer: &str) -> String {
            "first-legal".to_owned()
        }
    }

    fn card(uuid: &str) -> Value {
        json!({
            "atk_type": "Strike", "db_uuid": uuid, "effects": [], "finish_bonuses": {},
            "name": uuid, "number": 1, "play_order": "Lead", "raw_text": "", "tags": []
        })
    }

    /// Brumeister V2 on A: `OnDiscardMove{who=OPP}` → `RemoveFromPlay{OPP, 1}`, so A
    /// discards one of B's in-play cards whenever an effect pulls cards out of B's
    /// discard pile. B starts with a stocked discard pile and two cards in play.
    fn brumeister_engine() -> Engine {
        let gimmick = json!({
            "@type": "Effect",
            "trigger": {"@type": "OnDiscardMove", "who": "OPP"},
            "condition": {"@type": "Always"},
            "actions": [{"@type": "RemoveFromPlay", "who": "OPP", "count": 1,
                         "selector": {"@type": "CardFilter", "number": null, "atk_type": null,
                                      "play_order": null, "tag": null, "name": null, "raw": null,
                                      "name_contains": [], "text_contains": []}}],
            "duration": "INSTANT",
            "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
            "raw_clause": "test", "source": "gimmick", "optional": false
        });
        let stats =
            json!({"Power":5,"Agility":5,"Technique":5,"Submission":5,"Grapple":5,"Strike":5});
        let cards: Vec<Value> = (0..10).map(|i| card(&format!("c{i}"))).collect();
        let deck_a: Deck = serde_json::from_value(json!({
            "competitor": {"db_uuid": "BR", "name": "Brumeister", "division": "Underworld",
                "stats": stats, "effects": [gimmick]},
            "entrance": {"db_uuid": "BR-ent", "name": "ent"}, "cards": cards.clone(),
        }))
        .expect("deck A");
        let deck_b: Deck = serde_json::from_value(json!({
            "competitor": {"db_uuid": "B", "name": "B", "division": "Underworld", "stats": stats},
            "entrance": {"db_uuid": "B-ent", "name": "ent"}, "cards": cards,
        }))
        .expect("deck B");
        let mut engine = Engine::new(
            deck_a,
            deck_b,
            Box::new(FirstLegal),
            1,
            String::new(),
            "sim".into(),
        );
        // Stock every zone the discard-exit paths read: a pile to pull from, a board
        // to be punished, and a hand so the hand/discard swap is not a no-op.
        for side in ["A", "B"] {
            let p = engine.state.players.get_mut(side).unwrap();
            for i in 0..3 {
                p.discard
                    .push(serde_json::from_value(card(&format!("{side}d{i}"))).unwrap());
                p.in_play
                    .push(serde_json::from_value(card(&format!("{side}p{i}"))).unwrap());
                p.hand
                    .push(serde_json::from_value(card(&format!("{side}h{i}"))).unwrap());
            }
        }
        engine
    }

    fn board(engine: &Engine, key: &str) -> usize {
        engine.state.players[key].in_play.len()
    }

    fn any_card() -> CardFilter {
        CardFilter::default()
    }

    #[test]
    fn opponents_recur_to_hand_fires_the_board_wipe() {
        // B pulls a card out of their own discard -> A discards one of B's in-play.
        let mut engine = brumeister_engine();
        engine.act_add_from_discard(&any_card(), "B").unwrap();
        assert_eq!(board(&engine, "B"), 2, "B loses one in-play card");
        assert_eq!(board(&engine, "A"), 3, "A's own board is untouched");
    }

    #[test]
    fn own_discard_move_does_not_fire_the_opp_gated_effect() {
        // A pulling from their OWN pile must not fire A's who=OPP OnDiscardMove.
        let mut engine = brumeister_engine();
        engine.act_add_from_discard(&any_card(), "A").unwrap();
        assert_eq!(board(&engine, "B"), 3, "who=OPP ignores your own pile");
    }

    #[test]
    fn every_effect_driven_exit_fires_it() {
        // Each of the other discard-exit paths on B's pile also counts as a "move".
        for exit in [
            "shuffle_into_deck",
            "recur_to_deck_top",
            "swap_hand_discard",
        ] {
            let mut engine = brumeister_engine();
            match exit {
                "shuffle_into_deck" => engine.act_shuffle_into_deck(&any_card(), "B").unwrap(),
                "recur_to_deck_top" => engine.act_recur_to_deck_top(&any_card(), 2, "B").unwrap(),
                _ => engine.act_swap_hand_discard("B").unwrap(),
            }
            assert_eq!(board(&engine, "B"), 2, "{exit} fires OnDiscardMove");
        }
    }

    #[test]
    fn fires_once_per_action_not_per_card() {
        // "moves ANY NUMBER of cards": a 2-card recur is still a single trigger.
        let mut engine = brumeister_engine();
        engine.act_recur_to_deck_top(&any_card(), 2, "B").unwrap();
        assert_eq!(
            board(&engine, "B"),
            2,
            "two cards recurred still discards only one"
        );
    }

    #[test]
    fn passing_does_not_fire_it() {
        // The mechanical pass-and-recycle is not a card effect.
        let mut engine = brumeister_engine();
        engine.do_pass("B").unwrap();
        assert_eq!(board(&engine, "B"), 3, "pass-and-recycle is not an effect");
    }
}

#[cfg(test)]
mod timed_buff_tests {
    use super::*;

    fn card(uuid: &str) -> Value {
        json!({
            "atk_type": "Strike", "db_uuid": uuid, "effects": [], "finish_bonuses": {},
            "name": uuid, "number": 1, "play_order": "Lead", "raw_text": "", "tags": []
        })
    }

    /// A bare two-sided engine; the timed-buff paths are driven directly.
    fn engine() -> Engine {
        let stats =
            json!({"Power":5,"Agility":5,"Technique":5,"Submission":5,"Grapple":5,"Strike":5});
        let cards: Vec<Value> = (0..6).map(|i| card(&format!("c{i}"))).collect();
        let deck = |u: &str| -> Deck {
            serde_json::from_value(json!({
                "competitor": {"db_uuid": u, "name": u, "division": "World Championship",
                    "stats": stats},
                "entrance": {"db_uuid": format!("{u}-ent"), "name": "ent"}, "cards": cards.clone(),
            }))
            .expect("deck")
        };
        let decider = Box::new(ReplayDecider::new(BTreeMap::new(), BTreeMap::new()));
        Engine::new(
            deck("A"),
            deck("B"),
            decider,
            1,
            String::new(),
            "sim".into(),
        )
    }

    const CLAUSE: &str = "+1 to Strike and +5 to Submission (Max +5 to each)";

    fn grant(engine: &mut Engine, skill: Skill, delta: i64) {
        engine.grant_timed_buff(
            TimedBuff {
                skill,
                delta,
                until: Duration::UntilStartOfYourNextTurn,
                source: CLAUSE.to_owned(),
                cap: Some(5),
                granted_turn: 0,
            },
            Who::SelfSide,
            "A",
        );
    }

    fn buff_total(engine: &Engine, skill: Skill) -> i64 {
        engine.state.players["A"]
            .timed_buffs
            .iter()
            .filter(|b| b.skill == skill)
            .map(|b| b.delta)
            .sum()
    }

    #[test]
    fn repeat_firings_of_one_clause_accumulate_and_cap() {
        // Snake Pitt Super Lucha, hand-adjudicated: each qualifying Power roll adds
        // another +1 Strike / +5 Submission, and "(Max +5 to each)" is the ceiling on
        // the ACCUMULATED total — so Strike climbs 1..5 and stops, Submission caps at
        // once. One entry per (clause, skill), never a growing list.
        let mut engine = engine();
        for expected in 1..=5 {
            grant(&mut engine, Skill::Strike, 1);
            grant(&mut engine, Skill::Submission, 5);
            assert_eq!(buff_total(&engine, Skill::Strike), expected);
            assert_eq!(buff_total(&engine, Skill::Submission), 5, "capped at once");
        }
        grant(&mut engine, Skill::Strike, 1);
        assert_eq!(
            buff_total(&engine, Skill::Strike),
            5,
            "Strike stops at the cap"
        );
        assert_eq!(
            engine.state.players["A"].timed_buffs.len(),
            2,
            "one entry per (clause, skill) — repeats accumulate, never append"
        );
    }

    #[test]
    fn the_buff_feeds_the_derived_stats() {
        let mut engine = engine();
        grant(&mut engine, Skill::Submission, 5);
        assert_eq!(
            engine.stat("A", Skill::Submission),
            10,
            "base 5 + a capped +5 reaches the derived stat"
        );
        assert_eq!(engine.stat("B", Skill::Submission), 5, "B is untouched");
    }

    #[test]
    fn until_start_of_your_next_turn_survives_the_granting_turns_roll() {
        // Granted on turn 3's roll: the sweep for turn 3 must NOT take it, or the buff
        // would never survive the turn that created it.
        let mut engine = engine();
        engine.state.turn_no = 3;
        grant(&mut engine, Skill::Submission, 5);
        engine.sweep_next_turn_buffs("A");
        assert_eq!(buff_total(&engine, Skill::Submission), 5);
    }

    #[test]
    fn it_survives_every_turn_its_owner_is_not_active() {
        // Granted turn 3; B wins turns 4 and 5 -> A's buff is untouched throughout.
        let mut engine = engine();
        engine.state.turn_no = 3;
        grant(&mut engine, Skill::Submission, 5);
        for turn in 4..=5 {
            engine.state.turn_no = turn;
            engine.sweep_next_turn_buffs("B");
            assert_eq!(buff_total(&engine, Skill::Submission), 5, "turn {turn}");
        }
        // Turn 6: A wins the roll. The buff fed that roll and is swept right after.
        engine.state.turn_no = 6;
        engine.sweep_next_turn_buffs("A");
        assert_eq!(
            buff_total(&engine, Skill::Submission),
            0,
            "swept after the roll"
        );
    }

    #[test]
    fn until_end_of_turn_is_not_touched_by_the_next_turn_sweep() {
        // The two durations have separate sweeps; the roll-time sweep must ignore
        // UntilEndOfTurn (which is cleared at the top of the following turn instead).
        let mut engine = engine();
        engine.grant_timed_buff(
            TimedBuff {
                skill: Skill::Strike,
                delta: 2,
                until: Duration::UntilEndOfTurn,
                source: "until the end of the turn".to_owned(),
                cap: None,
                granted_turn: 0,
            },
            Who::SelfSide,
            "A",
        );
        engine.state.turn_no = 9;
        engine.sweep_next_turn_buffs("A");
        assert_eq!(
            buff_total(&engine, Skill::Strike),
            2,
            "wrong sweep must not fire"
        );
    }
}

#[cfg(test)]
mod blank_stopped_text_tests {
    use super::*;

    /// A card whose "If Stopped" text draws 2 — the thing the blank must suppress.
    fn attack_with_if_stopped() -> Card {
        serde_json::from_value(json!({
            "atk_type": "Grapple", "db_uuid": "attack", "name": "If Stopped Grapple",
            "number": 5, "play_order": "Lead", "raw_text": "If Stopped, draw 2 cards.",
            "tags": [], "finish_bonuses": {},
            "effects": [{
                "@type": "Effect",
                "trigger": {"@type": "OnStop", "dir": "YOURS", "order": null},
                "condition": {"@type": "Always"},
                "actions": [{"@type": "Draw", "n": 2, "source": "TOP", "who": "SELF",
                             "per": null, "per_who": "SELF"}],
                "duration": "INSTANT",
                "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
                "raw_clause": "If Stopped, draw 2 cards.", "source": "card", "optional": false
            }]
        }))
        .unwrap()
    }

    /// The stop card: "when you stop a card, the stopped card has blank text until the
    /// end of the turn" (`blanks = true`), or an inert stop card (`blanks = false`).
    fn stop_card(blanks: bool) -> Card {
        let effects = if blanks {
            json!([{
                "@type": "Effect",
                "trigger": {"@type": "OnStop", "dir": "THEIRS", "order": null},
                "condition": {"@type": "Always"},
                "actions": [{"@type": "BlankStoppedText"}],
                "duration": "INSTANT",
                "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
                "raw_clause": "the stopped card has blank text until the end of the turn",
                "source": "card", "optional": false
            }])
        } else {
            json!([])
        };
        serde_json::from_value(json!({
            "atk_type": "Grapple", "db_uuid": "stopper", "name": "Blocker", "number": 6,
            "play_order": "Lead", "raw_text": "", "tags": [], "finish_bonuses": {},
            "effects": effects
        }))
        .unwrap()
    }

    fn engine() -> Engine {
        let stats =
            json!({"Power":5,"Agility":5,"Technique":5,"Submission":5,"Grapple":5,"Strike":5});
        let cards: Vec<Value> = (0..8)
            .map(|i| {
                json!({"atk_type": "Strike", "db_uuid": format!("c{i}"), "effects": [],
                       "finish_bonuses": {}, "name": format!("c{i}"), "number": 1,
                       "play_order": "Lead", "raw_text": "", "tags": []})
            })
            .collect();
        let deck = |u: &str| -> Deck {
            serde_json::from_value(json!({
                "competitor": {"db_uuid": u, "name": u, "division": "World Championship",
                    "stats": stats},
                "entrance": {"db_uuid": format!("{u}-ent"), "name": "ent"}, "cards": cards.clone(),
            }))
            .expect("deck")
        };
        let decider = Box::new(ReplayDecider::new(BTreeMap::new(), BTreeMap::new()));
        Engine::new(
            deck("A"),
            deck("B"),
            decider,
            1,
            String::new(),
            "sim".into(),
        )
    }

    /// A's card is stopped by B; returns how many cards A drew from "If Stopped".
    fn run_stop(blanks: bool) -> (Engine, usize) {
        let mut engine = engine();
        let before = engine.state.players["A"].hand.len();
        engine
            .apply_stop("A", "B", attack_with_if_stopped(), stop_card(blanks))
            .unwrap();
        let drew = engine.state.players["A"].hand.len() - before;
        (engine, drew)
    }

    #[test]
    fn an_unblanked_stop_lets_if_stopped_fire() {
        // Baseline: without the blank, "If Stopped, draw 2" resolves normally.
        let (_, drew) = run_stop(false);
        assert_eq!(drew, 2, "If Stopped fires when nothing blanks it");
    }

    #[test]
    fn blanking_the_stopped_card_suppresses_if_stopped() {
        // The point of the family: the blank lands before the stopped card's own
        // OnStop, so its "If Stopped" text never triggers.
        let (engine, drew) = run_stop(true);
        assert_eq!(drew, 0, "a blanked card's If Stopped must not fire");
        assert!(
            engine.state.blanked_text.contains("attack"),
            "the stopped card is recorded as blanked"
        );
    }

    #[test]
    fn the_blank_lasts_the_rest_of_the_turn_and_is_swept() {
        let (mut engine, _) = run_stop(true);
        let attack = attack_with_if_stopped();
        assert!(
            engine.state.is_text_blanked(&attack, "A"),
            "still blanked later in the same turn"
        );
        engine.sweep_end_of_turn(); // the next turn's per-turn resets sweep it
        assert!(
            !engine.state.is_text_blanked(&attack, "A"),
            "the blank does not outlive the turn"
        );
    }
}

#[cfg(test)]
mod choose_name_tests {
    use super::*;

    /// Always picks the option named by `pick` at a `name` decision point.
    struct PickName(&'static str);

    impl Decider for PickName {
        fn decide(
            &mut self,
            _point: &str,
            _viewer: &str,
            legal: &[Value],
            _state: &mut GameState,
        ) -> Option<Value> {
            legal
                .iter()
                .find(|o| o["name"].as_str() == Some(self.0))
                .cloned()
                .or_else(|| legal.first().cloned())
        }

        fn policy_name(&self, _viewer: &str) -> String {
            "pick-name".to_owned()
        }
    }

    const NAMES: [&str; 3] = ["Kendo Stick", "Steel Chair", "Trash Can"];

    /// Raven's gimmick: bind one name at match start, then one OnHit per option gated
    /// on the binding — exactly one should ever be live.
    fn raven_effects() -> Value {
        let mut effects = vec![json!({
            "@type": "Effect",
            "trigger": {"@type": "StartOfMatch"},
            "condition": {"@type": "Always"},
            "actions": [{"@type": "ChooseName", "options": NAMES}],
            "duration": "INSTANT",
            "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
            "raw_clause": "Choose 1", "source": "gimmick", "optional": false
        })];
        for n in NAMES {
            effects.push(json!({
                "@type": "Effect",
                "trigger": {"@type": "OnHit", "atk_type": null, "name_contains": [n],
                            "text_contains": [], "on_any": false},
                "condition": {"@type": "ChosenNameIs", "name": n, "who": "SELF"},
                "actions": [{"@type": "Draw", "n": 2, "source": "TOP", "who": "SELF",
                             "per": null, "per_who": "SELF"}],
                "duration": "INSTANT",
                "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
                "raw_clause": "draw 2", "source": "gimmick", "optional": false
            }));
        }
        Value::Array(effects)
    }

    fn engine(pick: &'static str) -> Engine {
        let stats =
            json!({"Power":5,"Agility":5,"Technique":5,"Submission":5,"Grapple":5,"Strike":5});
        let cards: Vec<Value> = (0..10)
            .map(|i| {
                json!({"atk_type": "Strike", "db_uuid": format!("c{i}"), "effects": [],
                       "finish_bonuses": {}, "name": format!("c{i}"), "number": 1,
                       "play_order": "Lead", "raw_text": "", "tags": []})
            })
            .collect();
        let deck_a: Deck = serde_json::from_value(json!({
            "competitor": {"db_uuid": "RV", "name": "Raven", "division": "World Championship",
                "stats": stats, "effects": raven_effects()},
            "entrance": {"db_uuid": "RV-ent", "name": "ent"}, "cards": cards.clone(),
        }))
        .expect("deck A");
        let deck_b: Deck = serde_json::from_value(json!({
            "competitor": {"db_uuid": "B", "name": "B", "division": "World Championship",
                "stats": stats},
            "entrance": {"db_uuid": "B-ent", "name": "ent"}, "cards": cards,
        }))
        .expect("deck B");
        Engine::new(
            deck_a,
            deck_b,
            Box::new(PickName(pick)),
            1,
            String::new(),
            "sim".into(),
        )
    }

    /// Fire A's hit gimmicks against a card named `card_name`; return cards drawn.
    fn hit(engine: &mut Engine, card_name: &str) -> usize {
        let card: Card = serde_json::from_value(json!({
            "atk_type": "Strike", "db_uuid": "hit", "effects": [], "finish_bonuses": {},
            "name": card_name, "number": 1, "play_order": "Lead", "raw_text": "", "tags": []
        }))
        .unwrap();
        let before = engine.state.players["A"].hand.len();
        engine.run_hit_gimmicks(&card, "A").unwrap();
        engine.state.players["A"].hand.len() - before
    }

    #[test]
    fn the_binding_is_recorded_at_match_start() {
        let mut engine = engine("Steel Chair");
        engine.setup().unwrap();
        assert_eq!(
            engine.state.players["A"].chosen_name.as_deref(),
            Some("Steel Chair")
        );
    }

    #[test]
    fn only_the_chosen_name_draws() {
        let mut engine = engine("Steel Chair");
        engine.setup().unwrap();
        assert_eq!(
            hit(&mut engine, "Folding Steel Chair"),
            2,
            "chosen name hits"
        );
        assert_eq!(hit(&mut engine, "Kendo Stick Shot"), 0, "unchosen is inert");
        assert_eq!(hit(&mut engine, "Trash Can Lid"), 0, "unchosen is inert");
        assert_eq!(hit(&mut engine, "Dropkick"), 0, "unrelated card is inert");
    }

    #[test]
    fn a_different_choice_moves_the_live_effect() {
        let mut engine = engine("Trash Can");
        engine.setup().unwrap();
        assert_eq!(hit(&mut engine, "Trash Can Lid"), 2);
        assert_eq!(hit(&mut engine, "Folding Steel Chair"), 0);
    }

    #[test]
    fn nothing_fires_before_a_choice_is_bound() {
        // ChosenNameIs is false while the binding is None, so no OnHit is live.
        let mut engine = engine("Steel Chair");
        assert_eq!(hit(&mut engine, "Folding Steel Chair"), 0);
    }
}

#[cfg(test)]
mod hit_order_and_per_cap_tests {
    use super::*;

    fn lead(uuid: &str) -> Value {
        json!({"atk_type": "Strike", "db_uuid": uuid, "effects": [], "finish_bonuses": {},
               "name": uuid, "number": 1, "play_order": "Lead", "raw_text": "", "tags": []})
    }

    /// Sticky Sailboat: OnHit{order=Lead} -> draw 1 per OTHER Lead in play, max 3.
    fn gimmick() -> Value {
        json!({
            "@type": "Effect",
            "trigger": {"@type": "OnHit", "atk_type": null, "name_contains": [],
                        "text_contains": [], "on_any": false, "order": "Lead"},
            "condition": {"@type": "Always"},
            "actions": [{"@type": "Draw", "n": 1, "source": "TOP", "who": "SELF",
                         "per": {"@type": "CardFilter", "number": null, "atk_type": null,
                                 "play_order": "Lead", "tag": null, "name": null, "raw": null,
                                 "name_contains": [], "text_contains": []},
                         "per_who": "SELF", "cap": 3, "per_excludes_trigger": true}],
            "duration": "INSTANT",
            "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
            "raw_clause": "test", "source": "gimmick", "optional": false
        })
    }

    fn engine() -> Engine {
        let stats =
            json!({"Power":5,"Agility":5,"Technique":5,"Submission":5,"Grapple":5,"Strike":5});
        let cards: Vec<Value> = (0..20).map(|i| lead(&format!("c{i}"))).collect();
        let deck_a: Deck = serde_json::from_value(json!({
            "competitor": {"db_uuid": "SS", "name": "Sticky", "division": "World Championship",
                "stats": stats, "effects": [gimmick()]},
            "entrance": {"db_uuid": "SS-ent", "name": "ent"}, "cards": cards.clone(),
        }))
        .expect("deck A");
        let deck_b: Deck = serde_json::from_value(json!({
            "competitor": {"db_uuid": "B", "name": "B", "division": "World Championship",
                "stats": stats},
            "entrance": {"db_uuid": "B-ent", "name": "ent"}, "cards": cards,
        }))
        .expect("deck B");
        let decider = Box::new(ReplayDecider::new(BTreeMap::new(), BTreeMap::new()));
        Engine::new(deck_a, deck_b, decider, 1, String::new(), "sim".into())
    }

    /// Put `leads` Leads on A's board, then resolve a hit of `hit` (already in play,
    /// as `run_hit_gimmicks` sees it); return cards drawn.
    fn hit_with(board_leads: usize, hit: Value) -> usize {
        let mut engine = engine();
        {
            let p = engine.state.players.get_mut("A").unwrap();
            for i in 0..board_leads {
                p.in_play
                    .push(serde_json::from_value(lead(&format!("b{i}"))).unwrap());
            }
            p.in_play.push(serde_json::from_value(hit.clone()).unwrap());
        }
        let card: Card = serde_json::from_value(hit).unwrap();
        let before = engine.state.players["A"].hand.len();
        engine.run_hit_gimmicks(&card, "A").unwrap();
        engine.state.players["A"].hand.len() - before
    }

    #[test]
    fn the_triggering_lead_is_excluded_from_its_own_count() {
        // Board = 1 other Lead + the hit Lead. "each OTHER Lead" => 1, not 2.
        assert_eq!(hit_with(1, lead("hit")), 1);
        // No other Leads: the hit card alone must not draw for itself.
        assert_eq!(hit_with(0, lead("hit")), 0);
    }

    #[test]
    fn the_max_clamps_the_per_count() {
        // 5 other Leads would be 5; "(Max 3)" clamps it.
        assert_eq!(hit_with(5, lead("hit")), 3);
        assert_eq!(hit_with(3, lead("hit")), 3, "exactly at the cap");
        assert_eq!(hit_with(2, lead("hit")), 2, "under the cap is untouched");
    }

    #[test]
    fn the_order_gate_ignores_non_leads() {
        // Hitting a Follow Up must not fire an order=Lead gimmick, however many
        // Leads are on the board.
        let mut followup = lead("hit");
        followup["play_order"] = json!("Followup");
        assert_eq!(hit_with(3, followup), 0);
    }
}
