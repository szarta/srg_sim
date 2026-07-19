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
    Action, BuryFrom, CardFilter, ChoiceOption, Condition, DeckEnd, Dest, Direction, DqScope,
    Duration, Effect, LoseKind, PlayOrder, RollWhen, Skill, Trigger, Who,
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
            | Action::Reroll { .. }
            | Action::Unstoppable { .. }
            | Action::AlsoLead { .. }
            | Action::DoubleFinishIfBumped
            | Action::DisqualificationRule { .. }
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
            return self.bury_from_hand(&target, count.max(0) as usize, random, selector);
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
    ) -> Eng<()> {
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
        Ok(())
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
        }
        let winner = self.turn_roll()?;
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
        let effects = card.effects.clone();
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
        let effects = self.standing_effects(key);
        for eff in &effects {
            let Trigger::OnHit {
                atk_type,
                name_contains,
                text_contains,
                ..
            } = &eff.trigger
            else {
                continue;
            };
            // A bare OnHit (no gate) is the card's OWN "when this hits", already
            // fired via `run_effects`; only fire standing gimmicks that gate on
            // the hit card's type and/or name/text.
            let has_name_gate = !name_contains.is_empty() || !text_contains.is_empty();
            if atk_type.is_none() && !has_name_gate {
                continue;
            }
            let type_ok = atk_type.is_none_or(|want| want == card.atk_type);
            let name_gate = CardFilter {
                name_contains: name_contains.clone(),
                text_contains: text_contains.clone(),
                ..Default::default()
            };
            if type_ok && conditions::card_matches(card, &name_gate) {
                self.fire_if_ready(eff, key, None)?;
            }
        }
        Ok(())
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
        stopper.effects.iter().any(|eff| {
            eff.actions.iter().any(|action| {
                matches!(action, Action::Stop { .. })
                    && stop_matches(action, attack)
                    && conditions::holds(&eff.condition, &self.state, defender, None)
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
        self.run_effects(&attack_effects, "OnStop", active, None)?; // attack card: "if this is stopped"
        self.run_effects(&stop_effects, "OnStop", defender, None)?; // stop card: "when this stops"
                                                                    // Standing competitor/entrance OnStop, dir-aware from each owner's POV: the
                                                                    // attacker's card was stopped (YOURS), the defender stopped a card (THEIRS =
                                                                    // "when you Stop a card", e.g. Gia).
        self.run_on_stop_gimmicks(active, Direction::Yours)?;
        self.run_on_stop_gimmicks(defender, Direction::Theirs)?;
        Ok(())
    }

    /// Fire `key`'s standing (gimmick/entrance) `OnStop` effects whose direction
    /// matches `dir` — THEIRS for the stopper, YOURS for the stopped attacker.
    /// Unlike `run_effects` (trigger-name match only), this consults `OnStop.dir`.
    fn run_on_stop_gimmicks(&mut self, key: &str, dir: Direction) -> Eng<()> {
        let effects = self.gimmick_standing_effects(key);
        for eff in &effects {
            if matches!(eff.trigger, Trigger::OnStop { dir: d } if d == dir) {
                self.fire_if_ready(eff, key, None)?;
            }
        }
        Ok(())
    }

    // -- finish sequence + breakout ---------------------------------------

    /// The finish roll: base stat + the whole in-play combo's printed bonuses for the
    /// rolled skill + flat Finish-roll bonuses + crowd meter. Auto-success, else the
    /// defender's breakout attempt decides win vs. resume (DESIGN.md §5/§6).
    fn finish_sequence(&mut self, finisher: &str, defender: &str, card: &Card) -> Eng<()> {
        let skill = self.state.rng.roll();
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
                    delta, when_skill, ..
                } = a
                {
                    if when_skill.is_none() || *when_skill == Some(skill) {
                        total += *delta;
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

    /// Up to `BREAKOUT_ATTEMPTS` defender rolls; the first that beats the finish
    /// value breaks out. Returns whether the defender broke out.
    fn breakout(&mut self, defender: &str, finish_value: i64) -> bool {
        let cm = self.state.crowd_meter;
        let mut rolls: Vec<BreakoutRoll> = Vec::new();
        let mut broke = false;
        for _ in 0..BREAKOUT_ATTEMPTS {
            let skill = self.state.rng.roll();
            let val = self.stat(defender, skill);
            let success = crate::finish::stat_breaks_out(val, finish_value, 0, cm);
            rolls.push(BreakoutRoll {
                skill: skill.name().to_owned(),
                value: val,
                penalty: 0,
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
    fn on_broken_out(&mut self, _finisher: &str) -> Eng<()> {
        for key in ["A", "B"] {
            self.discard_in_play(key);
        }
        self.state.crowd_meter += 1;
        let t = self.state.turn_no;
        let value = self.state.crowd_meter;
        self.log(Event::CrowdMeter { t, delta: 1, value });
        for key in ["A", "B"] {
            let effects = self.standing_effects(key);
            self.run_effects(&effects, "OnBreakout", key, None)?;
        }
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
        };
        let ctx_b = RollContext {
            skill: Some(sb),
            gap: Some(va - vb),
            value: Some(vb),
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
            let Some((who, choose)) = eff.actions.iter().find_map(|a| match a {
                Action::Reroll { who, choose, .. } => Some((*who, *choose)),
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
        Ok(None)
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
        };
        let ctx_b = RollContext {
            skill: Some(sb),
            gap: Some(va - vb),
            value: Some(vb),
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
                },
            ),
            (
                "B".to_owned(),
                RollContext {
                    skill: Some(sb),
                    gap: Some(va - vb),
                    value: Some(vb),
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

/// Whether a `Stop` action's order/type filter covers this attack (`None` = any).
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
        Action::DisqualificationRule { .. } => "DisqualificationRule",
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
