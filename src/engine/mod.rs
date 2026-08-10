//! Turn loop, effect executor, stop resolution, finish sequence (DESIGN.md §6),
//! as a **resumable state machine** (`docs/design/substrate-split.rst` §3.3/§4).
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
    Action, AtkType, BuryFrom, CardFilter, ChoiceOption, Condition, CountZone, DeckEnd, Dest,
    Direction, Duration, Effect, EffectSource, LoseKind, PlayOrder, RerollCost, RerollCostKind,
    RevealDest, RevealFrom, RevealMatch, RevealSource, RollWhen, ScryRest, SearchSource,
    ShuffleSource, Skill, Trigger, Who,
};
use crate::rng::SeededRNG;
use crate::skills::Skills;
use crate::state::{
    FlipProvenance, GameState, MultiTurnRollMod, PendingRollDraw, PendingText, PlayerState,
    SkillRollMod, SkillSetRollMod, TimedBuff,
};
use serde_json::{json, Value};
use std::cmp::Reverse;
use std::collections::BTreeMap;

pub const OPENING_HAND: usize = 3;
pub const HAND_CAP: i64 = 10;
pub const BREAKOUT_ATTEMPTS: usize = 3;
pub const TURN_CAP: i64 = 400;
/// Max finish-roll re-rolls honored per finish — a loop guard for "you may re-roll
/// your Finish roll" (schema v76), independent of each effect's own frequency guard.
const FINISH_REROLL_CAP: usize = 3;
/// Max breakout-roll re-rolls honored per breakout attempt — the loop guard for
/// "re-roll your Breakout roll" (schema v102), independent of each effect's frequency.
const BREAKOUT_REROLL_CAP: usize = 3;
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
/// (`docs/design/substrate-split.rst` §4). Its `point`/`legal`/`chosen` fields are
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
            per_zone,
            on_skill,
        } => Action::ModifyRoll {
            who: *who,
            delta: -*delta,
            when: *when,
            per: per.clone(),
            per_who: *per_who,
            per_zone: *per_zone,
            on_skill: *on_skill,
        },
        Action::BuffSkill {
            skill,
            delta,
            who,
            duration,
            target_highest,
            target_lowest,
            per_crowd,
            cap,
            per,
            per_zone,
            per_excludes_self,
        } => Action::BuffSkill {
            skill: *skill,
            delta: -*delta,
            who: *who,
            duration: *duration,
            target_highest: *target_highest,
            target_lowest: *target_lowest,
            per_crowd: *per_crowd,
            cap: *cap,
            per: per.clone(),
            per_zone: *per_zone,
            per_excludes_self: *per_excludes_self,
        },
        Action::CrowdMeter { delta } => Action::CrowdMeter { delta: -*delta },
        Action::MaxHandSize {
            delta,
            who,
            duration,
            set,
        } => Action::MaxHandSize {
            delta: -*delta,
            who: *who,
            duration: *duration,
            set: *set, // an absolute set is not a signed delta — carried through unchanged
        },
        Action::MinHandSize {
            delta,
            who,
            duration,
        } => Action::MinHandSize {
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
            when_base_le,
            when_base_ge,
            per,
            per_who,
            per_zone,
            per_divisor,
            cap,
            per_excludes_self,
            per_crowd,
        } => Action::FinishRollBonus {
            delta: -*delta,
            when_skill: *when_skill,
            either: *either,
            when_base_le: *when_base_le,
            when_base_ge: *when_base_ge,
            per: per.clone(),
            per_who: *per_who,
            per_zone: *per_zone,
            per_divisor: *per_divisor,
            cap: *cap,
            per_excludes_self: *per_excludes_self,
            per_crowd: *per_crowd,
        },
        Action::BreakoutModifier {
            delta,
            attempts,
            when_skill,
            who,
            either,
            per,
            per_who,
            per_zone,
            per_divisor,
            cap,
            per_excludes_self,
        } => Action::BreakoutModifier {
            delta: -*delta,
            attempts: *attempts,
            when_skill: *when_skill,
            who: *who,
            either: *either,
            per: per.clone(),
            per_who: *per_who,
            per_zone: *per_zone,
            per_divisor: *per_divisor,
            cap: *cap,
            per_excludes_self: *per_excludes_self,
        },
        other => other.clone(),
    }
}

/// Multiply every "number" on one action by `factor`, recursing into a `Choice`'s
/// branches — the transform Pedro Valiant's `ScaleEntranceNumbers` applies to a
/// matching Entrance card's effects. Covers the signed deltas plus the count-like
/// fields ("draw 1" → "draw 3"); anything with no number is returned unchanged.
fn scale_action(action: &Action, factor: i64) -> Action {
    let mut a = action.clone();
    match &mut a {
        Action::Choice { options } => {
            for o in options.iter_mut() {
                o.actions = o.actions.iter().map(|x| scale_action(x, factor)).collect();
            }
        }
        Action::ModifyRoll { delta, .. }
        | Action::BuffSkill { delta, .. }
        | Action::CrowdMeter { delta }
        | Action::MaxHandSize { delta, .. }
        | Action::MinHandSize { delta, .. }
        | Action::FinishBonus { delta, .. }
        | Action::FinishRollBonus { delta, .. }
        | Action::TurnRollBonus { delta, .. }
        | Action::BreakoutModifier { delta, .. } => *delta *= factor,
        Action::Draw { n, .. } => *n *= factor,
        Action::Discard { count, .. } => *count *= factor,
        _ => {}
    }
    a
}

/// A copy of `effect` with every number in its actions multiplied by `factor`.
fn scale_effect(effect: &Effect, factor: i64) -> Effect {
    let mut out = effect.clone();
    out.actions = effect
        .actions
        .iter()
        .map(|a| scale_action(a, factor))
        .collect();
    out
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
/// The `Bury` action's payload, grouped to stay under the clippy argument limit.
struct BurySpec {
    selector: CardFilter,
    count: i64,
    who: Who,
    random: bool,
    source: BuryFrom,
    choose: bool,
}

struct DrawSpec {
    n: i64,
    source: DeckEnd,
    who: Who,
    per: Option<CardFilter>,
    per_who: Who,
    cap: Option<i64>,
    per_excludes_trigger: bool,
    from_crowd: bool,
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
    /// `db_uuid` of the card whose per-card self-referential effect is currently being
    /// dispatched — bound while `run_self_flips` fires a just-flipped card's own
    /// `OnFlip` effects, and while `run_discard_self_triggers` fires a card's
    /// `WhileInDiscard` trigger from the discard pile — so `AddSelfToHand` /
    /// `ShuffleSelfIntoDeck` / `PlaySelf` know their referent ("add IT to your hand",
    /// where IT is the flipped/discarded card). Transient, never serialized.
    self_card: Option<String>,
    /// `EffectSource` of the effect whose actions are currently being applied, set
    /// around `apply_actions`. `act_flip` reads it to record whether a flip was caused
    /// by a Gimmick effect ("flipped for your Gimmick"). Transient, never serialized.
    firing_source: EffectSource,
    /// Name of the card whose effect is currently resolving a play, set around
    /// `resolve_play`. `act_flip` reads it as the flip's `source_name` ("flipped by
    /// \"Set Up the Ladder\""). `None` outside a play. Transient, never serialized.
    firing_card_name: Option<String>,
    /// In-roll boost accumulated by a `RollBoost` action during an `offer_roll_boost`
    /// call (El Super Hombre V3's "or your roll is +1" choice branch). Reset before each
    /// effect's actions run and read back into the roll value. Transient, never serialized.
    pending_roll_boost: i64,
    decider: Box<dyn Decider>,
    /// Monotonic counter of decisions offered, for `request_id`/`seq`.
    decision_index: u64,
    /// Ordered observable frames (the replay/interchange projection,
    /// [`crate::record`]), captured alongside the log when
    /// [`record_frames`](Engine::record_frames) is on. Off by default: batch drivers
    /// (`analyze`, `audit`, the conformance corpus) play thousands of games and only
    /// ever read the log.
    frames: Vec<crate::record::Frame>,
    /// uuid → name/number for every card in the match, so a frame can name a card in
    /// transit. Built when recording is switched on.
    card_names: crate::record::CardNames,
    recording: bool,
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
                        pending_skill_roll_mods: Vec::new(),
                        pending_roll_draws: Vec::new(),
                        pending_next_roll_skill_mods: Vec::new(),
                        multi_turn_roll_mods: Vec::new(),
                        revealed_hand: Default::default(),
                        reroll_grants: Default::default(),
                        timed_buffs: Vec::new(),
                        chosen_name: None,
                        pending_text: Vec::new(),
                        blank_until_next_turn: None,
                        text_unblank: Vec::new(),
                        freq_counters: BTreeMap::new(),
                        gimmick_blanked: false,
                        gimmick_flipped: false,
                        hits_this_turn: 0,
                        flipped_this_turn: Vec::new(),
                        hit_this_turn: Vec::new(),
                        hit_last_turn: Vec::new(),
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
            self_card: None,
            firing_source: EffectSource::Card,
            firing_card_name: None,
            pending_roll_boost: 0,
            decider,
            decision_index: 0,
            frames: Vec::new(),
            card_names: Default::default(),
            recording: false,
        }
    }

    /// Also capture the observable [`Frame`](crate::record::Frame) sequence as the
    /// match plays — what a replay viewer walks (see [`crate::record`]). Call before
    /// [`play`](Engine::play); off by default.
    pub fn record_frames(&mut self) {
        self.recording = true;
        self.card_names = crate::record::CardNames::from_state(&self.state);
    }

    /// The frames captured so far (empty unless [`record_frames`](Engine::record_frames)
    /// was called).
    pub fn frames(&self) -> &[crate::record::Frame] {
        &self.frames
    }

    /// Take ownership of the captured frames.
    pub fn take_frames(&mut self) -> Vec<crate::record::Frame> {
        std::mem::take(&mut self.frames)
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
        if self.recording {
            self.capture(&event);
        }
        self.log.append(event);
    }

    /// Project an event into an observable frame over the state as it stands *at*
    /// the event (the engine logs each event after applying it). Events no observer
    /// may see project to `None` and add no frame.
    fn capture(&mut self, event: &Event) {
        let seq = self.frames.len() as i64;
        if let Some(frame) = crate::record::frame_for(seq, event, &self.state, &self.card_names) {
            self.frames.push(frame);
        }
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
        match self.entrance_scale_factor(key) {
            Some(factor) => out.extend(
                player
                    .entrance
                    .effects
                    .iter()
                    .map(|e| scale_effect(e, factor)),
            ),
            None => out.extend(player.entrance.effects.iter().cloned()),
        }
        out
    }

    /// The factor `key`'s active `ScaleEntranceNumbers` declaration applies to their
    /// Entrance card, if its name matches (Pedro Valiant). `None` = no scaling.
    fn entrance_scale_factor(&self, key: &str) -> Option<i64> {
        if self.state.is_gimmick_blanked(key) {
            return None;
        }
        let player = &self.state.players[key];
        let ename = player.entrance.name.to_lowercase();
        for eff in &player.competitor.effects {
            if !matches!(eff.trigger, Trigger::Static)
                || !conditions::holds(&eff.condition, &self.state, key, None)
            {
                continue;
            }
            for a in &eff.actions {
                if let Action::ScaleEntranceNumbers {
                    name_contains,
                    factor,
                } = a
                {
                    if name_contains
                        .iter()
                        .any(|s| ename.contains(&s.to_lowercase()))
                    {
                        return Some(*factor);
                    }
                }
            }
        }
        None
    }

    fn standing_effects(&self, key: &str) -> Vec<Effect> {
        let mut out = self.gimmick_standing_effects(key);
        for card in &self.state.players[key].in_play {
            out.extend(card.effects.iter().cloned());
        }
        // Text-copy family (#2/#9): effects re-homed onto `key` from a card it copies
        // (its `CopyText` clause), so a copied Static finish/roll bonus is read here
        // like any other standing effect (DESIGN.md §3, `Action::CopyText`).
        out.extend(self.state.copied_effects(key));
        out
    }

    /// [`standing_effects`](Self::standing_effects) paired with each effect's SOURCE card
    /// `db_uuid` (`None` for gimmick / entrance / copied effects, which carry no in-play
    /// card). A per-count reader that needs "for each OTHER …" exclusion resolves the
    /// source on the counted board by this uuid — the flattening
    /// [`standing_effects`](Self::standing_effects) can't express.
    fn standing_effects_sourced(&self, key: &str) -> Vec<(Option<String>, Effect)> {
        let mut out: Vec<(Option<String>, Effect)> = self
            .gimmick_standing_effects(key)
            .into_iter()
            .map(|e| (None, e))
            .collect();
        for card in &self.state.players[key].in_play {
            for eff in &card.effects {
                out.push((Some(card.db_uuid.clone()), eff.clone()));
            }
        }
        out.extend(
            self.state
                .copied_effects(key)
                .into_iter()
                .map(|e| (None, e)),
        );
        out
    }

    /// `standing_effects` PLUS the triggered effects a card declares while it sits in
    /// `key`'s discard pile (`Duration::WhileInDiscard`, non-`Static`). Used only at
    /// TRIGGER-dispatch sites (the turn roll-off, breakout) so a "when this card is in
    /// your discard pile, when you roll / your opponent breaks out …" clause fires
    /// from the discard — which `standing_effects` (in-play + gimmick only) never
    /// surfaces. Passive reads (finish bonuses, stops, buffs) keep `standing_effects`
    /// so a discarded card contributes no stats. Static discard toggles (DQ rules,
    /// cannot-be-stopped) stay on the `rule_immune` scan, not here.
    fn triggered_effects(&self, key: &str) -> Vec<Effect> {
        let mut out = self.standing_effects(key);
        for card in &self.state.players[key].discard {
            // A discard-blanked card ("cards in your opponent's discard pile have blank
            // text") contributes none of its WhileInDiscard effects.
            if self.state.is_text_blanked(card, key) {
                continue;
            }
            out.extend(
                card.effects
                    .iter()
                    .filter(|e| e.duration == Duration::WhileInDiscard)
                    .cloned(),
            );
        }
        out
    }

    /// The `(source card `db_uuid`, effect)` pairs for every `WhileInDiscard` effect a
    /// card declares while it sits in `key`'s discard pile. A trigger-dispatch site
    /// fires these with the uuid bound as [`Engine::self_card`], so a self-referential
    /// body ("add it to your hand", "shuffle it into your deck") acts on the discarded
    /// card that fired it. Unlike [`Self::triggered_effects`] (which flattens the source
    /// away), this keeps the card identity the self-actions need.
    fn discard_self_triggers(&self, key: &str) -> Vec<(String, Effect)> {
        let mut out = Vec::new();
        for card in &self.state.players[key].discard {
            if self.state.is_text_blanked(card, key) {
                continue; // a discard-blanked card fires no WhileInDiscard triggers
            }
            for eff in &card.effects {
                if eff.duration == Duration::WhileInDiscard {
                    out.push((card.db_uuid.clone(), eff.clone()));
                }
            }
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
            self.discard_from_hand(key, key, excess as usize, false, None)?;
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
        chooser: &str,
        count: usize,
        random: bool,
        selector: Option<&crate::ir::CardFilter>,
    ) -> Eng<usize> {
        // When the chooser is not the hand owner, the effect owner looks at the
        // opponent's hand and picks the discard (mirrors `bury_from_hand`).
        let point = if chooser == key {
            "discard"
        } else {
            "discard_opp_hand"
        };
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
                self.choose_discard(chooser, point, &pool)?
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

    fn choose_discard(&mut self, chooser: &str, point: &str, pool: &[Card]) -> Eng<Card> {
        let legal = pool.iter().map(discard_option).collect();
        let chosen = self.decide(point, chooser, legal)?;
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
        // Record this effect's source for the duration of its actions so a flip it
        // causes can tell "flipped for your Gimmick" apart (restored for nesting).
        let prev_source = self.firing_source;
        self.firing_source = eff.source;
        let result = self.apply_actions_inner(eff, key);
        self.firing_source = prev_source;
        result
    }

    fn apply_actions_inner(&mut self, eff: &Effect, key: &str) -> Eng<()> {
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
                from_crowd,
            } => self.act_draw(
                DrawSpec {
                    n: *n,
                    source: *source,
                    who: *who,
                    per: per.clone(),
                    per_who: *per_who,
                    cap: *cap,
                    per_excludes_trigger: *per_excludes_trigger,
                    from_crowd: *from_crowd,
                },
                key,
            )?,
            Action::Bury {
                selector,
                count,
                who,
                random,
                source,
                choose,
                per,
                per_who,
                all,
            } => {
                // `all` buries every matching card in the target's hand: set the count to
                // the hand size (an upper bound — the per-card loop stops when no matching
                // card remains) and skip `per`. Otherwise "bury 1 … for each <X> you have
                // in play" scales the count by the per-filter match count (Cardona).
                let count = if *all {
                    let target = self.target(*who, key);
                    self.state.players[&target].hand.len() as i64
                } else {
                    per.as_ref()
                        .map_or(*count, |p| *count * self.per_multiplier(p, *per_who, key, None))
                };
                self.act_bury(
                    BurySpec {
                        selector: selector.clone(),
                        count,
                        who: *who,
                        random: *random,
                        source: *source,
                        choose: *choose,
                    },
                    key,
                )?
            }
            Action::Flip {
                n,
                who,
                per,
                per_who,
                until,
                until_to_hand,
            } => {
                if let Some(until) = until {
                    self.act_flip_until(until, *who, *until_to_hand, key)?;
                } else {
                    let mut count = *n;
                    if let Some(per) = per {
                        count *= self.per_multiplier(per, *per_who, key, None);
                    }
                    self.act_flip(count, *who, key)?;
                }
            }
            Action::MillDeck { who, count, from } => self.act_mill_deck(*who, *count, *from, key),
            Action::RollDraw { who, skill, count } => self.act_roll_draw(*who, *skill, *count, key),
            Action::NextRollSkillBonus { who, skills, delta } => {
                self.act_next_roll_skill_bonus(*who, skills, *delta, key)
            }
            Action::MultiTurnRollBonus { who, rolls, delta } => {
                self.act_multi_turn_roll_bonus(*who, *rolls, *delta, key)
            }
            Action::Discard {
                selector,
                count,
                who,
                random,
                per,
                per_who,
                choose,
                all,
            } => {
                // `all` discards every matching card in the target's hand (see Bury): set
                // count to the hand size and skip `per`.
                let (count, per) = if *all {
                    let target = self.target(*who, key);
                    (self.state.players[&target].hand.len() as i64, None)
                } else {
                    (*count, per.as_ref())
                };
                self.act_discard(selector, count, *who, *random, per, *per_who, *choose, key)?
            }
            Action::Search {
                filter,
                dest,
                count,
                source,
            } => self.act_search(filter, *dest, *count, *source, key)?,
            Action::ShuffleDeck { who } => self.act_shuffle_deck(*who, key)?,
            Action::ShuffleIntoDeck { selector, source } => {
                self.act_shuffle_into_deck(selector, *source, key)?
            }
            Action::AddFromDiscard { filter } => self.act_add_from_discard(filter, key)?,
            Action::AddFlippedToHand {
                count,
                filter,
                random,
            } => self.act_add_flipped_to_hand(*count, filter, *random, key)?,
            Action::SwapHandDiscard => self.act_swap_hand_discard(key)?,
            Action::GrantSwapNextTurn { who } => self.act_grant_swap_next_turn(*who, key),
            Action::RecurToDeckTop { selector, count } => {
                self.act_recur_to_deck_top(selector, *count, key)?
            }
            Action::RemoveFromPlay {
                selector,
                who,
                count,
                choose,
            } => self.act_remove_from_play(selector, *who, *count, *choose, key)?,
            Action::DiscardInPlayMatch => self.act_discard_in_play_match(key)?,
            Action::CoupledDiscard { offset } => self.act_coupled_discard(*offset, key)?,
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
            Action::Reveal { who, count } => self.act_reveal(*who, *count, key)?,
            Action::ForceRevealPlay { who } => self.act_force_reveal_play(*who, key),
            Action::CopyEntrance { who } => self.act_copy_entrance(*who, key),
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
            Action::RevealThen {
                reveal_from,
                count,
                filter,
                take_matched,
                then,
                then_optional,
            } => self.act_reveal_then(
                *reveal_from,
                *count,
                filter,
                *take_matched,
                then,
                *then_optional,
                key,
            )?,
            Action::ShuffleHandDraw {
                who,
                count,
                choose,
                hand_count,
            } => self.act_shuffle_hand_draw(*who, *count, *choose, *hand_count, key)?,
            Action::ModifyRoll {
                who,
                delta,
                when,
                per,
                per_who,
                per_zone,
                on_skill,
            } => self.act_modify_roll(
                *who,
                *delta,
                *when,
                per.as_ref(),
                *per_who,
                *per_zone,
                *on_skill,
                key,
            ),
            Action::CrowdMeter { delta } => self.act_crowd(*delta, key),
            Action::RollBoost { delta } => self.pending_roll_boost += *delta,
            Action::WinTie { who } => self.act_win_tie(*who, key),
            Action::BlankGimmick { who, duration } => self.act_blank_gimmick(*who, *duration, key),
            Action::Unblank { selector, who } => self.act_unblank(selector, *who, key),
            Action::FlipGimmick { who } => self.act_flip_gimmick(*who, key),
            Action::LoseBy { kind, who } => self.act_lose_by(*kind, *who, key),
            Action::PlayExtraCard { .. } => self.act_play_extra_card(key),
            Action::Choice { options } => self.act_choice(options, key, source)?,
            Action::AbsorbGimmick { effects } => self.act_absorb_gimmick(effects, key),
            Action::SwapCrowdMeter { name, effects } => {
                self.act_swap_crowd_meter(name, effects, key)
            }
            // Passive markers, read where they matter (roll-off, finish, hand-cap,
            // count_in_play), never executed as a mutation — a no-op, not Unsupported.
            Action::LowestRollWins
            | Action::FlipGimmickSigns { .. }
            | Action::CountsAsInPlay { .. }
            | Action::ElectBumpOnSameSkill { .. }
            | Action::Unstoppable { .. }
            // A card's Stop declaration is read structurally at stop-time
            // (`card_can_stop`), never executed here — so an OnPlay Stop is a no-op,
            // not an "unsupported" event.
            | Action::Stop { .. }
            | Action::AlsoLead { .. }
            | Action::DoubleFinishIfBumped
            | Action::DoubleFinishIf { .. }
            | Action::DisqualificationRule { .. }
            | Action::CountOutRule { .. }
            | Action::ConsideredCompare { .. }
            | Action::SuppressOpponentDraw
            | Action::SuppressSelfHandLoss
            | Action::SwitchRolledSkill { .. }
            | Action::AddText { .. }
            | Action::StopRequiresTag { .. }
            | Action::BlankText { .. }
            // A `CopyText` clause is read by `copied_effects` (folded into
            // `standing_effects`), not executed as a mutation — a no-op here.
            | Action::CopyText { .. }
            | Action::MaxHandSize { .. }
            | Action::MinHandSize { .. }
            | Action::MirrorOpponentIncrease
            | Action::StopCountsOrderAs { .. }
            | Action::SuppressStop { .. }
            | Action::BumpDrawReplace
            // Pretty Paul's bump-replacement is read structurally in `roll_off`
            // (`try_bump_replacement`), never executed here — a no-op, not Unsupported.
            | Action::BumpReplacement { .. }
            | Action::ScaleEntranceNumbers { .. } => {}
            Action::BlankStoppedText => self.act_blank_stopped_text(key),
            Action::BuryThisCard => self.act_bury_this_card(key),
            Action::AddSelfToHand => self.act_add_self_to_hand(key),
            Action::ShuffleSelfIntoDeck => self.act_shuffle_self_into_deck(key)?,
            Action::PlaySelf => self.act_play_self(key)?,
            Action::ChooseName { options } => self.act_choose_name(options, key)?,
            Action::AddTextToNext {
                who,
                selector,
                effects,
            } => self.act_add_text_to_next(*who, selector, effects, key),
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
            // The parser's sentinel for a clause it could not map: log its actual
            // rules text + reason, so the game log / play-by-play reads the clause
            // ("If you have … in play: …"), not the Debug of the node.
            Action::Unsupported { raw_text, reason } => {
                self.log_unsupported(key, raw_text, reason);
            }
            // Any *other* unmodeled action (a marker missing from the passive no-op
            // list) falls back to its Debug form — a bug signal, not a normal clause.
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
        if spec.from_crowd {
            // Count is the live Crowd Meter plus `n` (the signed offset), then capped —
            // "draw cards equal to the Crowd Meter +1 (Max +5)". Never below 0.
            n = (self.state.crowd_meter + spec.n).max(0);
            if let Some(c) = cap {
                n = n.min(c);
            }
        } else if let Some(per) = spec.per.as_ref() {
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
        self.declares_static(key, |a| matches!(a, Action::SuppressOpponentDraw))
    }

    /// Whether `key`'s OWN effect must not cost `target` cards from hand — Sami
    /// "Death Machine" V2: "you do not bury or discard cards from your hand for your
    /// own card effects". Scoped to self-inflicted loss (`key == target`), so an
    /// opponent's effect still takes the cards. Read at the two hand-loss points.
    fn suppresses_self_hand_loss(&self, key: &str, target: &str) -> bool {
        key == target && self.declares_static(key, |a| matches!(a, Action::SuppressSelfHandLoss))
    }

    /// Whether `key` holds an active Static declaration of an action matching `pred`
    /// — on their own gimmick (unless blanked), entrance, or in-play, with the
    /// declaration's own condition holding. The read side of the passive-flag actions
    /// (`SuppressOpponentDraw`, `SuppressSelfHandLoss`), which are never executed.
    fn declares_static(&self, key: &str, pred: impl Fn(&Action) -> bool) -> bool {
        self.state
            .declaration_sources(key)
            .into_iter()
            .any(|(effects, active)| active && self.declares(effects, key, &pred))
    }

    /// Any Static effect among `effects` declaring a `pred`-matching action whose
    /// own condition holds.
    fn declares(&self, effects: &[Effect], key: &str, pred: &impl Fn(&Action) -> bool) -> bool {
        effects.iter().any(|eff| {
            matches!(eff.trigger, Trigger::Static)
                && eff.actions.iter().any(pred)
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

    fn act_bury(&mut self, spec: BurySpec, key: &str) -> Eng<()> {
        let BurySpec {
            count,
            who,
            random,
            source,
            choose,
            ..
        } = spec;
        let selector = &spec.selector;
        if source == BuryFrom::Discard {
            return self.bury_from_discard(selector, count, who, random, choose, key);
        }
        let target = self.target(who, key);
        if source == BuryFrom::Hand {
            if self.suppresses_self_hand_loss(key, &target) {
                self.log_effect(
                    key,
                    "SuppressSelfHandLoss",
                    Some(&target),
                    json!({"n": count}),
                );
                return Ok(());
            }
            // `choose` makes the EFFECT OWNER pick which of the target's hand cards to
            // bury (The Man from I.T. looks at the opponent's hand and chooses);
            // otherwise the hand owner sheds their least valuable.
            let chooser = if choose { key } else { target.as_str() };
            let n =
                self.bury_from_hand(&target, chooser, count.max(0) as usize, random, selector)?;
            if n > 0 {
                self.run_on_bury(&target, true, false)?; // effect-caused hand bury
            }
            return Ok(());
        }
        Ok(())
    }

    /// "Choose 1 card in play and discard it" with no side restriction (Cherry
    /// Glamazon): the actor picks from EITHER board and the card goes to its OWNER's
    /// discard. Mirrors `act_return_to_hand`'s `choose` branch.
    fn remove_from_either_board(
        &mut self,
        selector: &CardFilter,
        count: i64,
        key: &str,
    ) -> Eng<()> {
        let boards: Vec<String> = vec![key.to_owned(), self.state.opponent_of(key)];
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
            let chosen = self.decide("target", key, legal)?;
            let owner = chosen["owner"].as_str().unwrap().to_owned();
            let uuid = chosen["card"].as_str().unwrap().to_owned();
            let player = self.state.players.get_mut(&owner).unwrap();
            let Some(pos) = player.in_play.iter().position(|c| c.db_uuid == uuid) else {
                break;
            };
            let card = player.in_play.remove(pos);
            player.discard.push(card);
            let t = self.state.turn_no;
            self.log(Event::Discard(CardMovement {
                t,
                player: owner,
                cards: vec![uuid],
                source: Some("in_play".to_owned()),
                hidden: false,
            }));
        }
        Ok(())
    }

    /// Bury `count` card(s) from a discard pile to their owner's deck bottom.
    ///
    /// A discard pile has **no meaningful order**, so the bury is a CHOICE: the actor
    /// picks any card in the pile (`random` picks at random instead). `choose` widens
    /// the pool to BOTH piles — "bury 1 card in any player's discard pile" (Cherry
    /// Glamazon); otherwise it is `who`'s pile. `selector` filters the candidates.
    /// Fires `OnDiscardMove` for the pile that lost a card, like every other
    /// effect-driven exit.
    fn bury_from_discard(
        &mut self,
        selector: &CardFilter,
        count: i64,
        who: Who,
        random: bool,
        choose: bool,
        key: &str,
    ) -> Eng<()> {
        let piles: Vec<String> = if choose {
            vec![key.to_owned(), self.state.opponent_of(key)]
        } else {
            vec![self.target(who, key)]
        };
        for _ in 0..count.max(0) {
            let legal: Vec<Value> = piles
                .iter()
                .flat_map(|p| {
                    self.state.players[p]
                        .discard
                        .iter()
                        .filter(|c| conditions::card_matches(c, selector))
                        .map(move |c| {
                            let mut opt = discard_option(c);
                            opt["owner"] = json!(p);
                            opt
                        })
                })
                .collect();
            if legal.is_empty() {
                break;
            }
            let chosen = if random {
                self.state.rng.reveal(&legal).cloned().unwrap()
            } else {
                self.decide("bury", key, legal)?
            };
            let owner = chosen["owner"].as_str().unwrap().to_owned();
            let uuid = chosen["card"].as_str().unwrap().to_owned();
            // `bury_cards` performs the discard -> deck-bottom move itself.
            let Some(card) = self.state.players[&owner]
                .discard
                .iter()
                .find(|c| c.db_uuid == uuid)
                .cloned()
            else {
                break;
            };
            self.bury_cards(&owner, &[card]);
            self.run_on_bury(&owner, false, false)?;
            self.run_on_discard_move(&owner)?;
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
        chooser: &str,
        count: usize,
        random: bool,
        selector: &CardFilter,
    ) -> Eng<usize> {
        // When the chooser is not the hand owner, the effect owner is picking from the
        // opponent's hand (The Man from I.T.) — a distinct decision point whose value
        // read looks in the OTHER player's hand and disrupts the most valuable card.
        let point = if chooser == key {
            "bury_hand"
        } else {
            "bury_opp_hand"
        };
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
                self.pick_from(chooser, &pool, point)?
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

    fn act_flip(&mut self, n: i64, who: Who, key: &str) -> Eng<()> {
        let target = self.target(who, key);
        let flipped: Vec<Card> = {
            let deck = &mut self.state.players.get_mut(&target).unwrap().deck;
            let take = (n.max(0) as usize).min(deck.len());
            deck.drain(..take).collect()
        };
        if flipped.is_empty() {
            return Ok(());
        }
        let count = flipped.len() as i64;
        let uuids: Vec<String> = flipped.iter().map(|c| c.db_uuid.clone()).collect();
        // Each flipped card's OWN `OnFlip` effects ("If this card is flipped, add it to
        // your hand") fire per-card; capture them before the cards join the discard.
        let self_flips: Vec<Card> = flipped
            .iter()
            .filter(|c| c.effects.iter().any(is_on_flip_self))
            .cloned()
            .collect();
        let player = self.state.players.get_mut(&target).unwrap();
        // Record this turn's flips (read by CountZone::FlippedThisTurn) before they
        // join the discard, so a "+1 for each Strike flipped" rider can count them.
        player.flipped_this_turn.extend(flipped.iter().cloned());
        player.discard.extend(flipped);
        let t = self.state.turn_no;
        self.log(Event::Discard(CardMovement {
            t,
            player: target.clone(),
            cards: uuids,
            source: Some("deck".to_owned()),
            hidden: false,
        }));
        // Record what caused this flip so a flipped card's `FlippedForGimmick` /
        // `FlippedByName` gate resolves; saved/restored for nested flips (a PlaySelf
        // self-trigger can flip again). `firing_source`/`firing_card_name` reflect the
        // effect currently applying its actions — the flip's cause.
        let saved = self.state.flip_provenance.take();
        self.state.flip_provenance = Some(FlipProvenance {
            from_gimmick: self.firing_source == EffectSource::Gimmick,
            source_name: self.firing_card_name.clone(),
        });
        // Fire OnFlip AFTER the cards land in the discard (an "add a flipped card"
        // rider needs them present). `count` is how many actually flipped.
        let result = self
            .run_on_flip(&target, count)
            .and_then(|()| self.run_self_flips(&target, &self_flips));
        self.state.flip_provenance = saved;
        result
    }

    /// "`who` discards `count` card(s) from the `from` end of their DECK" — a plain
    /// deck-to-discard mill ([`Action::MillDeck`]), with none of `act_flip`'s flip
    /// semantics (no `flipped_this_turn`, no `OnFlip`, no provenance). "Each player
    /// discards the bottom card of their deck."
    fn act_mill_deck(&mut self, who: Who, count: i64, from: DeckEnd, key: &str) {
        let target = self.target(who, key);
        let milled: Vec<Card> = {
            let deck = &mut self.state.players.get_mut(&target).unwrap().deck;
            let take = (count.max(0) as usize).min(deck.len());
            match from {
                DeckEnd::Top => deck.drain(..take).collect(),
                DeckEnd::Bottom => deck.split_off(deck.len() - take),
            }
        };
        if milled.is_empty() {
            return;
        }
        let uuids: Vec<String> = milled.iter().map(|c| c.db_uuid.clone()).collect();
        self.state
            .players
            .get_mut(&target)
            .unwrap()
            .discard
            .extend(milled);
        let t = self.state.turn_no;
        self.log(Event::Discard(CardMovement {
            t,
            player: target,
            cards: uuids,
            source: Some("deck".to_owned()),
            hidden: false,
        }));
    }

    /// Arm a one-shot roll-conditional draw ([`Action::RollDraw`]): "if your
    /// [opponent's] next turn roll is `<S>`, draw N". Queued on the effect owner, whose
    /// NEXT-turn-roll resolution ([`Self::resolve_pending_roll_draws`]) checks the
    /// watched side (`who`) and draws if it came up `skill`. `who` records only which
    /// side's roll to WATCH — the owner always does the drawing.
    fn act_roll_draw(&mut self, who: Who, skill: Skill, count: i64, key: &str) {
        self.state
            .players
            .get_mut(key)
            .unwrap()
            .pending_roll_draws
            .push(PendingRollDraw {
                skill,
                count,
                watch: who,
            });
        self.log_effect(
            key,
            "RollDraw",
            None,
            json!({"skill": skill.name(), "count": count, "watch": format!("{who:?}")}),
        );
    }

    /// Resolve every player's pending one-shot roll-conditional draws against the turn
    /// roll that just settled ([`Action::RollDraw`]). For each armed entry, the WATCHED
    /// side's resolved turn-roll skill is read from `roll_ctx`; a match draws `count` for
    /// the owner. The queue is drained wholesale — "your NEXT turn roll" is a one-turn
    /// window, so a non-match fizzles rather than carrying over.
    fn resolve_pending_roll_draws(&mut self) -> Eng<()> {
        for key in ["A", "B"] {
            let armed =
                std::mem::take(&mut self.state.players.get_mut(key).unwrap().pending_roll_draws);
            for entry in armed {
                let watched = self.target(entry.watch, key);
                let rolled = self.roll_ctx.get(&watched).and_then(|c| c.skill);
                if rolled == Some(entry.skill) {
                    self.log_effect(
                        key,
                        "RollDraw",
                        None,
                        json!({"skill": entry.skill.name(), "count": entry.count, "fired": true}),
                    );
                    self.draw(key, entry.count.max(0) as usize, DeckEnd::Top)?;
                    if self.ended() {
                        return Ok(());
                    }
                }
            }
        }
        Ok(())
    }

    /// Arm a one-turn skill-gated turn-roll bonus ([`Action::NextRollSkillBonus`]): "+N
    /// to `<S>`, `<S>` during your next turn roll". Queued on the AFFECTED player (the one
    /// whose roll it modifies — `target(who)`, so `Opp` stores it on the opponent); read
    /// by `next_roll_skill_bonus` on that player's next initial roll and drained by
    /// `consume_pending` (a one-turn window).
    fn act_next_roll_skill_bonus(&mut self, who: Who, skills: &[Skill], delta: i64, key: &str) {
        let target = self.target(who, key);
        self.state
            .players
            .get_mut(&target)
            .unwrap()
            .pending_next_roll_skill_mods
            .push(SkillSetRollMod {
                skills: skills.to_vec(),
                delta,
            });
        let names: Vec<&str> = skills.iter().map(|s| s.name()).collect();
        self.log_effect(
            key,
            "NextRollSkillBonus",
            Some(&target),
            json!({"skills": names, "delta": delta}),
        );
    }

    /// Sum the pending one-turn skill-gated bonuses ([`SkillSetRollMod`]) for `key`'s
    /// next turn roll that gate on `skill`. Read-only — the whole queue is drained by
    /// `consume_pending` after the initial roll-off (so a non-match fizzles too).
    fn next_roll_skill_bonus(&self, key: &str, skill: Skill) -> i64 {
        self.state.players[key]
            .pending_next_roll_skill_mods
            .iter()
            .filter(|m| m.skills.contains(&skill))
            .map(|m| m.delta)
            .sum()
    }

    /// Arm a multi-turn turn-roll bonus ([`Action::MultiTurnRollBonus`]): "your
    /// [opponent's] next N turn rolls are +/-N". Queued on the AFFECTED player
    /// (`target(who)`); applied by `multi_turn_roll_bonus` on each of their next `rolls`
    /// initial rolls and decremented by `consume_pending`. A zero/negative `rolls` arms
    /// nothing.
    fn act_multi_turn_roll_bonus(&mut self, who: Who, rolls: i64, delta: i64, key: &str) {
        if rolls <= 0 {
            return;
        }
        let target = self.target(who, key);
        self.state
            .players
            .get_mut(&target)
            .unwrap()
            .multi_turn_roll_mods
            .push(MultiTurnRollMod {
                delta,
                remaining: rolls,
            });
        self.log_effect(
            key,
            "MultiTurnRollBonus",
            Some(&target),
            json!({"rolls": rolls, "delta": delta}),
        );
    }

    /// Sum the live multi-turn bonuses ([`MultiTurnRollMod`]) for `key`'s turn roll.
    /// Read-only — `consume_pending` decrements each entry's `remaining` once per
    /// roll-off and drops the exhausted ones.
    fn multi_turn_roll_bonus(&self, key: &str) -> i64 {
        self.state.players[key]
            .multi_turn_roll_mods
            .iter()
            .filter(|m| m.remaining > 0)
            .map(|m| m.delta)
            .sum()
    }

    /// Fire STANDING (`on_self == false`) `OnFlip` gimmicks after `flipped_side` flipped
    /// `count` cards. Scans BOTH players so a `who=OPP` variant works; the `count` gate
    /// fires on any flip (`None`), exactly `n` (Evee), or `n`-or-more (`at_least`).
    /// Per-card `on_self` triggers are dispatched by `run_self_flips`, not here.
    fn run_on_flip(&mut self, flipped_side: &str, count: i64) -> Eng<()> {
        let opp = self.state.opponent_of(flipped_side);
        for owner in [flipped_side.to_owned(), opp] {
            let effects = self.standing_effects(&owner);
            for eff in &effects {
                let Trigger::OnFlip {
                    who,
                    count: gate,
                    at_least,
                    on_self,
                } = &eff.trigger
                else {
                    continue;
                };
                if *on_self {
                    continue; // a per-card self-trigger; fires via run_self_flips
                }
                let dir_ok = (*who == Who::SelfSide) == (owner.as_str() == flipped_side);
                let count_ok = gate.is_none_or(|g| if *at_least { count >= g } else { count == g });
                if dir_ok && count_ok {
                    self.fire_if_ready(eff, &owner, None)?;
                }
            }
        }
        Ok(())
    }

    /// Dispatch each just-flipped card's OWN `OnFlip{who:SELF}` effects, one card at a
    /// time — "If this card is flipped, [you may] add it to your hand." `self_card`
    /// binds the referent so `AddSelfToHand` (and any future self-referential
    /// action) knows which card fired, mirroring the `stopped_card` stop context.
    fn run_self_flips(&mut self, side: &str, cards: &[Card]) -> Eng<()> {
        for card in cards {
            self.self_card = Some(card.db_uuid.clone());
            for eff in &card.effects {
                if is_on_flip_self(eff) {
                    self.fire_if_ready(eff, side, None)?;
                }
            }
            self.self_card = None;
        }
        Ok(())
    }

    /// Flip-until: mill the target's deck one card at a time until a flipped card
    /// matches `filter` (or the deck empties). Every non-matching card goes to the
    /// discard; the matching card goes to the hand when `to_hand`, else to the
    /// discard with the rest ("Flip cards until you flip a Submission[, add that
    /// Submission to your hand]").
    fn act_flip_until(
        &mut self,
        filter: &CardFilter,
        who: Who,
        to_hand: bool,
        key: &str,
    ) -> Eng<()> {
        let target = self.target(who, key);
        let mut milled: Vec<Card> = Vec::new();
        let mut matched: Option<Card> = None;
        loop {
            let card = {
                let deck = &mut self.state.players.get_mut(&target).unwrap().deck;
                if deck.is_empty() {
                    break;
                }
                deck.remove(0)
            };
            if conditions::card_matches(&card, filter) {
                matched = Some(card);
                break;
            }
            milled.push(card);
        }
        let t = self.state.turn_no;
        let mut to_discard = milled;
        let mut added_to_hand = false;
        if let Some(card) = matched {
            if to_hand {
                let uuid = card.db_uuid.clone();
                self.state.players.get_mut(&target).unwrap().hand.push(card);
                self.log(Event::Search(CardMovement {
                    t,
                    player: target.clone(),
                    cards: vec![uuid],
                    source: Some("deck".to_owned()),
                    hidden: false, // publicly flipped -> its identity is known in hand
                }));
                added_to_hand = true;
            } else {
                to_discard.push(card);
            }
        }
        if !to_discard.is_empty() {
            let uuids = to_discard.iter().map(|c| c.db_uuid.clone()).collect();
            let player = self.state.players.get_mut(&target).unwrap();
            player.discard.extend(to_discard);
            self.log(Event::Discard(CardMovement {
                t,
                player: target.clone(),
                cards: uuids,
                source: Some("deck".to_owned()),
                hidden: false,
            }));
        }
        if added_to_hand {
            self.hand_cap(&target)?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    fn act_discard(
        &mut self,
        selector: &CardFilter,
        count: i64,
        who: Who,
        random: bool,
        per: Option<&CardFilter>,
        per_who: Who,
        choose: bool,
        key: &str,
    ) -> Eng<()> {
        let target = self.target(who, key);
        let mut count = count;
        if let Some(per) = per {
            count *= self.per_multiplier(per, per_who, key, None);
        }
        if count != 0 {
            if self.suppresses_self_hand_loss(key, &target) {
                self.log_effect(
                    key,
                    "SuppressSelfHandLoss",
                    Some(&target),
                    json!({"n": count}),
                );
                return Ok(());
            }
            // `choose` makes the effect owner pick from the target's hand (Look at
            // your opponent's hand …); otherwise the hand owner sheds their own.
            let chooser = if choose { key } else { target.as_str() };
            let n = self.discard_from_hand(
                &target,
                chooser,
                count.max(0) as usize,
                random,
                Some(selector),
            )?;
            if n > 0 {
                self.run_on_bury(&target, true, true)?; // effect-caused hand discard (Tommy)
            }
        }
        Ok(())
    }

    fn act_search(
        &mut self,
        filter: &CardFilter,
        dest: Dest,
        count: i64,
        source: SearchSource,
        key: &str,
    ) -> Eng<()> {
        if dest == Dest::Discard {
            return self.search_to_discard(filter, count, key);
        }
        // Candidate pool: the deck, plus the discard pile when the clause searches
        // "your deck or discard pile". The found card leaves whichever zone holds it.
        let both = source == SearchSource::DeckOrDiscard;
        let matches: Vec<Card> = self.state.players[key]
            .deck
            .iter()
            .chain(
                both.then(|| self.state.players[key].discard.iter())
                    .into_iter()
                    .flatten(),
            )
            .filter(|c| conditions::card_matches(c, filter))
            .cloned()
            .collect();
        let picked = if matches.is_empty() {
            None
        } else {
            Some(self.pick_from(key, &matches, "target")?)
        };
        let mut from_discard = false;
        if let Some(card) = &picked {
            let player = self.state.players.get_mut(key).unwrap();
            if let Some(pos) = player.deck.iter().position(|c| c.db_uuid == card.db_uuid) {
                player.deck.remove(pos);
            } else if let Some(pos) = player
                .discard
                .iter()
                .position(|c| c.db_uuid == card.db_uuid)
            {
                player.discard.remove(pos);
                from_discard = true;
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
                source: Some(if from_discard { "discard" } else { "deck" }.to_owned()),
                hidden: true, // deck/discard -> hand/deck: private, opponent sees only counts
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
    fn act_shuffle_into_deck(
        &mut self,
        selector: &CardFilter,
        source: ShuffleSource,
        key: &str,
    ) -> Eng<()> {
        let from_discard = source == ShuffleSource::Discard;
        let matches: Vec<Card> = {
            let player = &self.state.players[key];
            let zone = if from_discard {
                &player.discard
            } else {
                &player.in_play
            };
            zone.iter()
                .filter(|c| conditions::card_matches(c, selector))
                .cloned()
                .collect()
        };
        if !matches.is_empty() {
            let card = self.pick_from(key, &matches, "target")?;
            {
                let player = self.state.players.get_mut(key).unwrap();
                let zone = if from_discard {
                    &mut player.discard
                } else {
                    &mut player.in_play
                };
                if let Some(pos) = zone.iter().position(|c| c.db_uuid == card.db_uuid) {
                    zone.remove(pos);
                }
                player.deck.push(card.clone());
            }
            let t = self.state.turn_no;
            self.log(Event::Bury(CardMovement {
                t,
                player: key.to_owned(),
                cards: vec![card.db_uuid],
                source: Some(if from_discard { "discard" } else { "play" }.to_owned()),
                hidden: false,
            }));
            // A discard recur fires the discard-move hook ahead of the shuffle's own
            // OnShuffle; an in-play return has no such hook.
            if from_discard {
                self.run_on_discard_move(key)?;
            }
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

    /// "Add N of the flipped cards to your hand" — move up to `count` cards (all matching
    /// when `count` is `None`) from the just-flipped pool to `key`'s hand. The pool is
    /// this turn's flips (`flipped_this_turn`) that are still in the discard and match
    /// `filter`; the owner picks which when there is a choice. See
    /// [`Action::AddFlippedToHand`].
    fn act_add_flipped_to_hand(
        &mut self,
        count: Option<i64>,
        filter: &CardFilter,
        random: bool,
        key: &str,
    ) -> Eng<()> {
        let flipped: Vec<String> = self.state.players[key]
            .flipped_this_turn
            .iter()
            .filter(|c| conditions::card_matches(c, filter))
            .map(|c| c.db_uuid.clone())
            .collect();
        let mut pool: Vec<Card> = self.state.players[key]
            .discard
            .iter()
            .filter(|c| flipped.contains(&c.db_uuid))
            .cloned()
            .collect();
        let take = count.map_or(pool.len(), |n| (n.max(0) as usize).min(pool.len()));
        if take == 0 {
            return Ok(());
        }
        let t = self.state.turn_no;
        for _ in 0..take {
            if pool.is_empty() {
                break;
            }
            let card = if random {
                self.state.rng.reveal(&pool).cloned().unwrap()
            } else {
                self.pick_from(key, &pool, "target")?
            };
            pool.retain(|c| c.db_uuid != card.db_uuid);
            let player = self.state.players.get_mut(key).unwrap();
            if let Some(pos) = player
                .discard
                .iter()
                .position(|c| c.db_uuid == card.db_uuid)
            {
                player.discard.remove(pos);
            }
            player.hand.push(card.clone());
            self.log(Event::Search(CardMovement {
                t,
                player: key.to_owned(),
                cards: vec![card.db_uuid],
                source: Some("discard".to_owned()),
                hidden: false, // flipped cards are public; which one entered hand is known
            }));
        }
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

    /// Arm a deferred, one-shot optional hand↔discard swap on `who` for their next
    /// turn (Mr. Rey). The grant is promoted to usable at the start of that player's
    /// following turn (`promote_swap_grant`) and offered there (`offer_swap_grant`).
    fn act_grant_swap_next_turn(&mut self, who: Who, key: &str) {
        let target = self.target(who, key);
        self.state
            .players
            .get_mut(&target)
            .unwrap()
            .flags
            .insert("swap_grant_next".to_owned(), json!(true));
        self.log_effect(key, "GrantSwapNextTurn", Some(&target), json!({}));
    }

    /// Promote `key`'s deferred swap grant at turn start: a "next turn" grant becomes
    /// usable "this turn"; an unused "this turn" grant EXPIRES (SET, not accumulate —
    /// "once on the next turn"). Mirrors the `reroll_grants` next→this promotion, but
    /// flag-based so it needs no serialized `PlayerState` field.
    fn promote_swap_grant(&mut self, key: &str) {
        let flags = &mut self.state.players.get_mut(key).unwrap().flags;
        if flags.remove("swap_grant_next").is_some() {
            flags.insert("swap_grant_this".to_owned(), json!(true));
        } else {
            flags.remove("swap_grant_this");
        }
    }

    /// Offer `key` their usable swap grant, if any, before they act on this turn: a
    /// single optional hand↔discard swap (Mr. Rey). The grant is consumed whether
    /// taken, declined, or impossible (empty hand/discard) — "once on the next turn".
    fn offer_swap_grant(&mut self, key: &str) -> Eng<()> {
        let has = self.state.players[key]
            .flags
            .get("swap_grant_this")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !has {
            return Ok(());
        }
        self.state
            .players
            .get_mut(key)
            .unwrap()
            .flags
            .remove("swap_grant_this");
        let p = &self.state.players[key];
        if p.hand.is_empty() || p.discard.is_empty() {
            return Ok(()); // nothing to switch — the window still passes
        }
        let legal = vec![
            json!({"kind": "yes", "clause": "switch a hand card with a discard card"}),
            json!({"kind": "no", "clause": "switch a hand card with a discard card"}),
        ];
        if self.decide("optional_swap", key, legal)?["kind"] == "yes" {
            self.act_swap_hand_discard(key)?;
        }
        Ok(())
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
        choose: bool,
        key: &str,
    ) -> Eng<()> {
        if choose {
            return self.remove_from_either_board(selector, count, key);
        }
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

    /// Candyman Dan: discard 1 of the owner's own in-play cards (they choose), then
    /// discard 1 of the OPPONENT's in-play cards of the SAME play order. No-op if the
    /// owner has nothing in play; the second discard is skipped if the opponent has no
    /// matching card.
    fn act_discard_in_play_match(&mut self, key: &str) -> Eng<()> {
        let Some(order) = self.discard_one_in_play(key, key, &CardFilter::default())? else {
            return Ok(());
        };
        let opp = self.state.opponent_of(key);
        let filter = CardFilter {
            play_order: Some(order),
            ..Default::default()
        };
        self.discard_one_in_play(key, &opp, &filter)?;
        Ok(())
    }

    /// Defector's Dismantler (schema v76). "Discard any number of cards from your
    /// hand, your opponent discards the same number of cards from their hand
    /// `offset`." No policy count-choice hook exists, so N is a heuristic: strip the
    /// opponent's hand when affordable — `N = min(self_hand, opp_hand + 1)`, so the
    /// opponent (who sheds `N + offset`) empties whenever the actor can pay. The
    /// self-discard fires `OnBury` so a discard-recur gimmick (Defector's own) still
    /// triggers; a self-hand-loss suppressor zeroes the whole trade.
    fn act_coupled_discard(&mut self, offset: i64, key: &str) -> Eng<()> {
        let opp = self.state.opponent_of(key);
        if self.suppresses_self_hand_loss(key, key) {
            self.log_effect(key, "SuppressSelfHandLoss", Some(key), json!({"n": 0}));
            return Ok(());
        }
        let self_hand = self.state.players[key].hand.len() as i64;
        let opp_hand = self.state.players[&opp].hand.len() as i64;
        let n = self_hand.min(opp_hand + 1).max(0);
        if n > 0 && self.discard_from_hand(key, key, n as usize, false, None)? > 0 {
            self.run_on_bury(key, true, true)?; // effect-caused hand discard (gimmick recur)
        }
        let opp_n = (n + offset).max(0);
        if opp_n > 0 && self.discard_from_hand(&opp, &opp, opp_n as usize, false, None)? > 0 {
            self.run_on_bury(&opp, true, true)?;
        }
        Ok(())
    }

    /// `actor` picks and discards one of `target`'s in-play cards matching `selector`
    /// (to `target`'s discard). Returns the discarded card's play order, or `None` if
    /// none matched — the shared step of Candyman Dan's two-ended trade.
    fn discard_one_in_play(
        &mut self,
        actor: &str,
        target: &str,
        selector: &CardFilter,
    ) -> Eng<Option<PlayOrder>> {
        let matches: Vec<Card> = self.state.players[target]
            .in_play
            .iter()
            .filter(|c| conditions::card_matches(c, selector))
            .cloned()
            .collect();
        if matches.is_empty() {
            return Ok(None);
        }
        let card = self.pick_from(actor, &matches, "target")?;
        let order = card.play_order;
        let player = self.state.players.get_mut(target).unwrap();
        if let Some(pos) = player
            .in_play
            .iter()
            .position(|c| c.db_uuid == card.db_uuid)
        {
            player.in_play.remove(pos);
        }
        player.discard.push(card.clone());
        let t = self.state.turn_no;
        self.log(Event::Discard(CardMovement {
            t,
            player: target.to_owned(),
            cards: vec![card.db_uuid],
            source: Some("in_play".to_owned()),
            hidden: false,
        }));
        Ok(Some(order))
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

    #[allow(clippy::too_many_arguments)]
    fn act_modify_roll(
        &mut self,
        who: Who,
        delta: i64,
        when: RollWhen,
        per: Option<&CardFilter>,
        per_who: Who,
        per_zone: CountZone,
        on_skill: Option<Skill>,
        key: &str,
    ) {
        let target = self.target(who, key);
        let mut delta = delta;
        if let Some(per) = per {
            let counter = self.target(per_who, key);
            delta *= self.state.count_in_zone(per, per_zone, &counter);
        }
        // Skill-keyed pending mod ("the next time you roll <S>, it is +N"): queue it
        // on the target, to be consumed in `roll_for` when that skill is next rolled.
        if let Some(skill) = on_skill {
            self.state
                .players
                .get_mut(&target)
                .unwrap()
                .pending_skill_roll_mods
                .push(SkillRollMod { skill, delta });
            self.log_effect(
                key,
                "ModifyRoll",
                Some(&target),
                json!({"delta": delta, "when": "next", "on_skill": skill.name()}),
            );
            return;
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
        let turn = self.state.turn_no;
        let player = self.state.players.get_mut(&target).unwrap();
        player.gimmick_blanked = true;
        // "…until their next turn" (Stiff Right Hand): mark it for the turn-boundary
        // sweep. Every other duration leaves the stored flag alone — a WHILE_IN_PLAY
        // blank is re-derived by `blank_scan`, not stored here.
        if duration == Duration::UntilStartOfYourNextTurn {
            player.blank_until_next_turn = Some(turn);
        }
        let detail = json!({"duration": serde_json::to_value(duration).unwrap()});
        self.log_effect(key, "BlankGimmick", Some(&target), detail);
    }

    /// "Un-blank your Finishes." — record `selector` on `who`'s persistent
    /// `text_unblank` list so their matching cards are no longer text-blanked
    /// (`is_text_blanked` consults it first, so the un-blank wins over every blank
    /// source). Also drops any matching cards from `blanked_text` — a stop's
    /// per-identity blank this turn is lifted immediately, not just going forward.
    /// Rest-of-match, so it is never swept; idempotent (a repeat selector is harmless).
    fn act_unblank(&mut self, selector: &CardFilter, who: Who, key: &str) {
        let target = self.target(who, key);
        let p = &self.state.players[&target];
        let matching: Vec<String> = p
            .hand
            .iter()
            .chain(&p.deck)
            .chain(&p.discard)
            .chain(&p.in_play)
            .filter(|c| conditions::card_matches(c, selector))
            .map(|c| c.db_uuid.clone())
            .collect();
        for uuid in &matching {
            self.state.blanked_text.remove(uuid);
        }
        self.state
            .players
            .get_mut(&target)
            .unwrap()
            .text_unblank
            .push(selector.clone());
        let detail = json!({"count": matching.len()});
        self.log_effect(key, "Unblank", Some(&target), detail);
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

    /// "`who` reveals `count` card(s) in their hand" (fog-of-war [`Action::Reveal`]).
    /// The revealing player CHOOSES which cards (a `reveal` decision per card, over the
    /// hand minus any already picked THIS reveal); the chosen cards join their
    /// `revealed_hand` so the opponent sees them (in `observable`) while they stay in
    /// hand. Idempotent per card — re-picking an already-revealed card leaks nothing.
    fn act_reveal(&mut self, who: Who, count: i64, key: &str) -> Eng<()> {
        let target = self.target(who, key);
        let mut chosen: Vec<String> = Vec::new();
        for _ in 0..count {
            let pool: Vec<Card> = self.state.players[&target]
                .hand
                .iter()
                .filter(|c| !chosen.contains(&c.db_uuid))
                .cloned()
                .collect();
            if pool.is_empty() {
                break;
            }
            let card = self.choose_reveal(&target, "reveal", &pool)?;
            chosen.push(card.db_uuid);
        }
        let player = self.state.players.get_mut(&target).unwrap();
        for uuid in &chosen {
            player.revealed_hand.insert(uuid.clone());
        }
        self.log_effect(key, "Reveal", Some(&target), json!({"cards": chosen}));
        Ok(())
    }

    /// One `reveal` decision: the reveal target picks a card from `pool` to expose.
    fn choose_reveal(&mut self, chooser: &str, point: &str, pool: &[Card]) -> Eng<Card> {
        let legal = pool.iter().map(reveal_option).collect();
        let chosen = self.decide(point, chooser, legal)?;
        Ok(find_by_uuid(pool, &chosen))
    }

    /// Arm the deferred "forced reveal-and-play" on `who` for their next won turn
    /// (Father Light). A one-shot flag on the target; the actual reveal+play fires
    /// from `take_turn_action` when that player next takes a turn. Idempotent —
    /// re-arming before the target takes a turn leaves it armed exactly once.
    fn act_force_reveal_play(&mut self, who: Who, key: &str) {
        let target = self.target(who, key);
        self.state
            .players
            .get_mut(&target)
            .unwrap()
            .flags
            .insert("forced_reveal_play".to_owned(), json!(true));
        self.log_effect(key, "ForceRevealPlay", Some(&target), json!({}));
    }

    /// Copy `who`'s Entrance onto `key`'s own (El Ganso Ruso): append the target
    /// entrance's effects to `key`'s entrance, so `key` gains that entrance's
    /// ability. Resolved live from the loaded entrances. A copied `StartOfMatch`
    /// ability has already missed its window (setup is past); copied *ongoing*
    /// abilities (OnRoll / Static / etc.) fire naturally thereafter.
    fn act_copy_entrance(&mut self, who: Who, key: &str) {
        let target = self.target(who, key);
        if target == key {
            return;
        }
        let effects: Vec<Effect> = self.state.players[&target].entrance.effects.clone();
        let copied = effects.len();
        self.state
            .players
            .get_mut(key)
            .unwrap()
            .entrance
            .effects
            .extend(effects);
        self.log_effect(
            key,
            "CopyEntrance",
            Some(&target),
            json!({"effects": copied}),
        );
    }

    /// Consume `key`'s armed "forced reveal-and-play" flag, if set.
    fn consume_forced_reveal_play(&mut self, key: &str) -> bool {
        let flags = &mut self.state.players.get_mut(key).unwrap().flags;
        if flags
            .get("forced_reveal_play")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            flags.remove("forced_reveal_play");
            true
        } else {
            false
        }
    }

    /// Father Light's forced play: reveal `active`'s hand one card at a time in
    /// random order until a playable card turns up, then force-play it (no free
    /// choice). Playability is the ordinary rule — a Lead, a Follow-Up with a Lead
    /// in play, or a Finish with a Follow-Up in play (a stop counts as its play
    /// order); an `AlsoLead` self-declaration also qualifies. Returns `true` iff a
    /// card was forced; `false` (whole hand revealed, nothing playable) lets the
    /// caller fall through to the ordinary — necessarily pass-only — turn action.
    fn forced_reveal_and_play(&mut self, active: &str, defender: &str) -> Eng<bool> {
        let chain = self.state.players[active].in_play.clone();
        let mut remaining: Vec<Card> = self.state.players[active].hand.clone();
        let mut revealed: Vec<String> = Vec::new();
        let mut chosen: Option<Card> = None;
        while !remaining.is_empty() {
            let card = self.state.rng.reveal(&remaining).cloned().unwrap();
            let pos = remaining
                .iter()
                .position(|c| c.number == card.number)
                .unwrap();
            remaining.remove(pos);
            revealed.push(card.db_uuid.clone());
            if playable(&chain, &card) || self.also_lead_now(active, &card) {
                chosen = Some(card);
                break;
            }
        }
        self.log_effect(
            active,
            "ForcedReveal",
            None,
            json!({"revealed": revealed,
                   "played": chosen.as_ref().map(|c| c.db_uuid.clone())}),
        );
        let Some(card) = chosen else {
            return Ok(false);
        };
        let taken = self.take_from_hand(active, card.number);
        let landed = self.resolve_play(active, defender, taken.clone())?;
        if landed && taken.play_order == PlayOrder::Finish {
            self.finish_sequence(active, defender, &taken)?;
        }
        Ok(true)
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
            ScryRest::Flip => {
                self.scry_flip_cards(owner, cards);
            }
            ScryRest::MayFlip => {
                // Optional flip ("you may flip it"): the peek is free, so the flip
                // decision is made after seeing the card. Flip only the cards worth
                // milling — on an opponent's deck, the ones worth denying them
                // (Finish / stops, `scry_value >= 2`); on your own, the junk you'd
                // rather shed. Leave the rest on top (best-on-top on your deck,
                // worst-on-top when sabotaging, mirroring `Return`).
                let (flip, keep): (Vec<Card>, Vec<Card>) = cards
                    .into_iter()
                    .partition(|c| (scry_value(c) >= 2) == sabotage);
                self.scry_flip_cards(owner, flip);
                if !keep.is_empty() {
                    let mut ordered = keep;
                    ordered.sort_by_key(|c| Reverse(scry_value(c))); // best first
                    if sabotage {
                        ordered.reverse();
                    }
                    self.scry_to_top(owner, ordered);
                }
            }
        }
    }

    /// Mill `cards` to `owner`'s discard pile (the `Flip` disposition) and log it.
    fn scry_flip_cards(&mut self, owner: &str, cards: Vec<Card>) {
        if cards.is_empty() {
            return;
        }
        let uuids: Vec<String> = cards.iter().map(|c| c.db_uuid.clone()).collect();
        self.state
            .players
            .get_mut(owner)
            .unwrap()
            .discard
            .extend(cards);
        let t = self.state.turn_no;
        self.log(Event::Discard(CardMovement {
            t,
            player: owner.to_owned(),
            cards: uuids,
            source: Some("deck".to_owned()),
            hidden: false,
        }));
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
            None => card.counts_as_atk_type(match_atk),
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

    /// `RevealThen`: reveal card(s) from the owner's deck/hand, and if one matches
    /// `filter` run the consequence (take the matched card to hand when `take_matched`,
    /// then apply `then`; the whole consequence is a "you may" when `then_optional`).
    /// The reveal is a non-destructive peek — only the matched card moves, only when
    /// taken. The owner is always the effect owner ("your deck / your hand").
    #[allow(clippy::too_many_arguments)]
    fn act_reveal_then(
        &mut self,
        reveal_from: RevealSource,
        count: i64,
        filter: &CardFilter,
        take_matched: bool,
        then: &[Action],
        then_optional: bool,
        key: &str,
    ) -> Eng<()> {
        let n = (count.max(1) as usize).min(1_000);
        let revealed = self.reveal_peek(reveal_from, n, key);
        if revealed.is_empty() {
            return Ok(());
        }
        let matched = revealed
            .iter()
            .find(|c| conditions::card_matches(c, filter))
            .cloned();
        self.log_effect(
            key,
            "RevealThen",
            Some(key),
            json!({
                "revealed": revealed.iter().map(|c| c.db_uuid.clone()).collect::<Vec<_>>(),
                "matched": matched.as_ref().map(|c| c.db_uuid.clone()),
            }),
        );
        let Some(card) = matched else {
            return Ok(()); // no revealed card matched -> nothing further
        };
        // "Add that card to your hand" is mandatory on a match; only the extra `then`
        // actions carry the "you may" (e.g. "…, and you may re-roll your next turn roll").
        if take_matched {
            self.take_revealed_to_hand(reveal_from, &card, key);
        }
        if then.is_empty() {
            return Ok(());
        }
        if then_optional {
            let legal = vec![json!({"kind": "yes"}), json!({"kind": "no"})];
            if self.decide("optional", key, legal)?["kind"] != "yes" {
                return Ok(());
            }
        }
        for action in then {
            self.apply_action(action, key, "")?;
            if self.resolve_pending() {
                break;
            }
        }
        Ok(())
    }

    /// Non-destructively read up to `n` cards for a [`RevealSource`]: the top/bottom of
    /// the owner's deck, or `n` uniformly-random cards from the owner's hand. Cards stay
    /// in place (the caller moves only a matched card, only when it takes it).
    fn reveal_peek(&mut self, from: RevealSource, n: usize, key: &str) -> Vec<Card> {
        match from {
            RevealSource::DeckTop => self.state.players[key]
                .deck
                .iter()
                .take(n)
                .cloned()
                .collect(),
            RevealSource::DeckBottom => self.state.players[key]
                .deck
                .iter()
                .rev()
                .take(n)
                .cloned()
                .collect(),
            RevealSource::HandRandom => {
                let mut pool: Vec<Card> = self.state.players[key].hand.clone();
                let mut out = Vec::new();
                for _ in 0..n.min(pool.len()) {
                    let Some(card) = self.state.rng.reveal(&pool).cloned() else {
                        break;
                    };
                    let pos = pool.iter().position(|c| c.db_uuid == card.db_uuid).unwrap();
                    pool.remove(pos);
                    out.push(card);
                }
                out
            }
        }
    }

    /// Move a matched revealed deck card to the owner's hand ("add that card to your
    /// hand"). A hand reveal is a no-op — the card is already in hand.
    fn take_revealed_to_hand(&mut self, from: RevealSource, card: &Card, key: &str) {
        if matches!(from, RevealSource::HandRandom) {
            return;
        }
        let player = self.state.players.get_mut(key).unwrap();
        let Some(pos) = player.deck.iter().position(|c| c.db_uuid == card.db_uuid) else {
            return;
        };
        let taken = player.deck.remove(pos);
        let uuid = taken.db_uuid.clone();
        player.hand.push(taken);
        let t = self.state.turn_no;
        let owner = key.to_owned();
        self.log(Event::Search(CardMovement {
            t,
            player: owner,
            cards: vec![uuid],
            source: None,
            hidden: false,
        }));
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
    fn act_shuffle_hand_draw(
        &mut self,
        who: Who,
        count: i64,
        choose: bool,
        hand_count: Option<i64>,
        key: &str,
    ) -> Eng<()> {
        let target = if choose {
            self.decide_reshuffle_target(key)?
        } else {
            self.target(who, key)
        };
        // `None` shuffles the WHOLE hand (Cyclone); `Some(n)` reveals and shuffles `n`
        // chosen cards (Memes Dealer). The public Bury (hand→deck) is the "reveal".
        let shed: Vec<Card> = match hand_count {
            None => std::mem::take(&mut self.state.players.get_mut(&target).unwrap().hand),
            Some(n) => self.pick_hand_cards(&target, n.max(0) as usize)?,
        };
        if !shed.is_empty() {
            let uuids: Vec<String> = shed.iter().map(|c| c.db_uuid.clone()).collect();
            let t = self.state.turn_no;
            self.state
                .players
                .get_mut(&target)
                .unwrap()
                .deck
                .extend(shed);
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

    /// The owner of `target` reveals and removes up to `n` chosen cards from their hand
    /// (least valuable first, via the `discard` point) — the pick step of a partial
    /// hand shuffle (Memes Dealer).
    fn pick_hand_cards(&mut self, target: &str, n: usize) -> Eng<Vec<Card>> {
        let mut picked: Vec<Card> = Vec::new();
        for _ in 0..n {
            let hand: Vec<Card> = self.state.players[target].hand.clone();
            if hand.is_empty() {
                break;
            }
            let card = self.pick_from(target, &hand, "discard")?;
            let h = &mut self.state.players.get_mut(target).unwrap().hand;
            if let Some(pos) = h.iter().position(|c| c.db_uuid == card.db_uuid) {
                h.remove(pos);
            }
            picked.push(card);
        }
        Ok(picked)
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

    /// Queue a one-shot "added text" on `who`'s next card matching `selector` (the
    /// Madness trio). Held on the TARGET, so it outlives the source card leaving play.
    fn act_add_text_to_next(
        &mut self,
        who: Who,
        selector: &CardFilter,
        effects: &[Effect],
        key: &str,
    ) {
        let target = self.target(who, key);
        let source = effects
            .first()
            .map(|e| e.raw_clause.clone())
            .unwrap_or_default();
        self.state
            .players
            .get_mut(&target)
            .unwrap()
            .pending_text
            .push(PendingText {
                selector: selector.clone(),
                effects: effects.to_vec(),
                source: source.clone(),
            });
        self.log_effect(key, "AddTextToNext", Some(&target), json!({"text": source}));
    }

    /// Add a chosen competitor's Gimmick to `key`'s own (The SRG Boss — "add their
    /// Gimmick to yours"): append `effects` to `key`'s competitor effects so they
    /// fire as standing effects thereafter (and are blanked together with the rest
    /// of `key`'s gimmick). The candidate gimmicks are baked into the authoring
    /// `Choice`; the engine holds no card index to resolve them at runtime.
    fn act_absorb_gimmick(&mut self, effects: &[Effect], key: &str) {
        self.state
            .players
            .get_mut(key)
            .unwrap()
            .competitor
            .effects
            .extend(effects.iter().cloned());
        let clauses: Vec<String> = effects.iter().map(|e| e.raw_clause.clone()).collect();
        self.log_effect(key, "AbsorbGimmick", Some(key), json!({"gimmick": clauses}));
    }

    /// Consume any queued `PendingText` matching `card` and fold its effects onto the
    /// card instance, so the added text travels with it through the stop exchange and
    /// into play. Consumed on PLAY, whether or not the card is subsequently stopped.
    fn apply_pending_text(&mut self, key: &str, card: &mut Card) {
        let player = self.state.players.get_mut(key).unwrap();
        let Some(idx) = player
            .pending_text
            .iter()
            .position(|p| conditions::card_matches(card, &p.selector))
        else {
            return;
        };
        let pending = player.pending_text.remove(idx);
        card.effects.extend(pending.effects.iter().cloned());
        self.log_effect(
            key,
            "AddTextToNext",
            Some(key),
            json!({"card": card.db_uuid, "text": pending.source, "consumed": true}),
        );
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

    /// Bury the triggering (stopped) card — move it from `key`'s discard pile to the
    /// bottom of their deck. A no-op outside a stop context, or if the card has already
    /// left the discard. See [`Action::BuryThisCard`].
    fn act_bury_this_card(&mut self, key: &str) {
        let Some(uuid) = self.stopped_card.clone() else {
            return;
        };
        let player = self.state.players.get_mut(key).unwrap();
        let Some(pos) = player.discard.iter().position(|c| c.db_uuid == uuid) else {
            return;
        };
        let card = player.discard.remove(pos);
        player.deck.push(card);
        self.log_effect(key, "BuryThisCard", None, json!({"card": uuid}));
    }

    /// Add the triggering (flipped) card to `key`'s hand — move it from their discard
    /// pile (where the flip landed it) to their hand. The referent is
    /// [`Engine::self_card`], set per-card during `run_self_flips`. A no-op outside a
    /// flip context or if the card has already left the discard. See
    /// [`Action::AddSelfToHand`].
    fn act_add_self_to_hand(&mut self, key: &str) {
        let Some(uuid) = self.self_card.clone() else {
            return;
        };
        let player = self.state.players.get_mut(key).unwrap();
        let Some(pos) = player.discard.iter().position(|c| c.db_uuid == uuid) else {
            return;
        };
        let card = player.discard.remove(pos);
        player.hand.push(card);
        self.log_effect(key, "AddSelfToHand", None, json!({"card": uuid}));
    }

    /// Shuffle the triggering (flipped) card back into `key`'s deck — move it from their
    /// discard pile to the deck, then shuffle (firing `OnShuffle`). The referent is
    /// [`Engine::self_card`]. A no-op outside a flip context or if the card has
    /// already left the discard. See [`Action::ShuffleSelfIntoDeck`].
    fn act_shuffle_self_into_deck(&mut self, key: &str) -> Eng<()> {
        let Some(uuid) = self.self_card.clone() else {
            return Ok(());
        };
        let player = self.state.players.get_mut(key).unwrap();
        let Some(pos) = player.discard.iter().position(|c| c.db_uuid == uuid) else {
            return Ok(());
        };
        let card = player.discard.remove(pos);
        player.deck.push(card);
        self.log_effect(key, "ShuffleSelfIntoDeck", None, json!({"card": uuid}));
        self.shuffle_deck(key)
    }

    /// Play the triggering (flipped) card immediately — pull it from `key`'s discard and
    /// resolve it as a normal play by `key` (stop window, OnPlay/OnHit), a bonus action
    /// outside the turn's one-card play. The referent is [`Engine::self_card`]. A
    /// no-op outside a flip context or if the card has already left the discard. See
    /// [`Action::PlaySelf`].
    fn act_play_self(&mut self, key: &str) -> Eng<()> {
        let Some(uuid) = self.self_card.clone() else {
            return Ok(());
        };
        let player = self.state.players.get_mut(key).unwrap();
        let Some(pos) = player.discard.iter().position(|c| c.db_uuid == uuid) else {
            return Ok(());
        };
        let card = player.discard.remove(pos);
        let defender = self.state.opponent_of(key);
        self.log_effect(key, "PlaySelf", None, json!({"card": uuid}));
        self.resolve_play(key, &defender, card)?;
        Ok(())
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
        // Same boundary for a "blank until their next turn" poison (Stiff Right Hand).
        if player.blank_until_next_turn.is_some_and(|t| t < turn) {
            player.blank_until_next_turn = None;
            player.gimmick_blanked = false;
        }
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

    /// True iff `loser` is currently immune to a disqualification loss — delegates to
    /// [`GameState::is_dq_immune`], where the last-played (#93) resolution now lives so
    /// the `MatchHasNoDisqualifications` condition can share it.
    fn is_dq_immune(&self, loser: &str) -> bool {
        self.state.is_dq_immune(loser)
    }

    /// True iff `loser` is currently immune to a count-out loss — delegates to
    /// [`GameState::is_count_out_immune`].
    fn is_count_out_immune(&self, loser: &str) -> bool {
        self.state.is_count_out_immune(loser)
    }

    /// Swap the Crowd Meter to a match type (GM Calace V1): append `effects` to the
    /// owner's Entrance effects, where they become always-active standing rules (a
    /// global match condition, unaffected by the owner's gimmick being blanked). The
    /// `Unsupported` sub-effects among them stay inert but surface the unmodeled
    /// clauses. Mirrors [`act_copy_entrance`](Self::act_copy_entrance)'s entrance-extend.
    fn act_swap_crowd_meter(&mut self, name: &str, effects: &[Effect], key: &str) {
        let installed = effects.len();
        self.state
            .players
            .get_mut(key)
            .unwrap()
            .entrance
            .effects
            .extend(effects.iter().cloned());
        self.log_effect(
            key,
            "SwapCrowdMeter",
            None,
            json!({"name": name, "effects": installed}),
        );
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
            player.hits_this_turn = 0; // reset the per-turn hit count (HitThisTurn)
            player.hit_last_turn = std::mem::take(&mut player.hit_this_turn); // rotate hit history
            player.flipped_this_turn.clear(); // reset per-turn flips (FlippedThisTurn)
                                              // Drop revealed-hand entries for cards no longer in hand (played /
                                              // discarded): once it leaves the hand it is no longer a revealed card.
            player
                .revealed_hand
                .retain(|u| player.hand.iter().any(|c| &c.db_uuid == u));
            // Promote a "re-roll your next turn roll" grant to this turn (SET, not
            // accumulate); an unused grant expires.
            player.reroll_grants.this_turn = player.reroll_grants.next_turn;
            player.reroll_grants.next_turn = 0;
        }
        for key in ["A", "B"] {
            self.promote_swap_grant(key); // a "swap next turn" grant becomes usable (Mr. Rey)
        }
        self.sweep_end_of_turn();
        let winner = self.turn_roll()?;
        self.sweep_next_turn_buffs(&winner);
        if self.ended() || !self.draw_for_turn(&winner)? {
            return Ok(());
        }
        self.first_turn_option(&winner)?; // the once-per-player first-turn redraw (§6)
        self.run_start_of_turn(&winner)?; // "once during your turn" gimmicks (Candyman Dan)
        let defender = self.state.opponent_of(&winner);
        self.run_opponent_turn(&defender)?; // "once during your opponent's turn" (Memes Dealer V1)
        self.offer_swap_grant(&winner)?; // a granted "swap on the next turn" (Mr. Rey)
        self.offer_swap_grant(&defender)?; // the grantee need not be the turn winner
        if self.ended() {
            return Ok(());
        }
        self.take_turn_action(&winner)?; // play ONE card (or pass+bury)
        while !self.ended() && self.consume_extra_play(&winner) {
            self.take_turn_action(&winner)?; // a PlayExtraCard granted another action
        }
        Ok(())
    }

    /// Fire the active player's `StartOfTurn` gimmicks — "once during your turn, you
    /// may …" (Candyman Dan). Offered right after the turn draw, before the play action.
    /// Previously a dead trigger; no parsed card or other override emits it.
    fn run_start_of_turn(&mut self, key: &str) -> Eng<()> {
        let effects = self.standing_effects(key);
        self.run_effects(&effects, "StartOfTurn", key, None)
    }

    /// Fire the NON-active player's `DuringOpponentTurn` gimmicks — "once during your
    /// opponent's turn, you may …" (Memes Dealer V1), the mirror of `run_start_of_turn`.
    /// A previously-unused trigger; no parsed card or other override emits it.
    fn run_opponent_turn(&mut self, key: &str) -> Eng<()> {
        let effects = self.standing_effects(key);
        self.run_effects(&effects, "DuringOpponentTurn", key, None)
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
        if self.recording && self.frames.is_empty() {
            self.frames.push(crate::record::opening_frame(&self.state));
        }
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
        // Father Light: a deferred forced reveal-and-play consumes this turn's play
        // when armed and a card is playable; if nothing is playable the whole hand
        // is revealed and play falls through to the ordinary (pass-only) action.
        if self.consume_forced_reveal_play(active)
            && self.forced_reveal_and_play(active, &defender)?
        {
            return Ok(());
        }
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

    /// Passing recycles one card from discard to the bottom of the deck (§6). Stamps
    /// `last_pass_turn` so `Condition::EndedTurnNoPlay` ("if you ended the last turn
    /// without playing a card", The SRG Boss) reads it on the following turn.
    fn do_pass(&mut self, active: &str) -> Eng<()> {
        let turn = self.state.turn_no;
        self.state
            .players
            .get_mut(active)
            .unwrap()
            .flags
            .insert("last_pass_turn".to_owned(), json!(turn));
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

    /// Whether `card` may be played this instant via an `AlsoLead` self-declaration
    /// whose condition currently holds — granting it an extra play-order slot
    /// (`order`: also a Lead / Follow Up / Finish). The condition sees `key`'s
    /// current turn roll (so "if you rolled Agility, this card is also a Follow Up"
    /// resolves), and the target slot must itself be legal against the board.
    fn also_lead_now(&self, key: &str, card: &Card) -> bool {
        let board = &self.state.players[key].in_play;
        let roll = self.roll_ctx.get(key);
        card.effects.iter().any(|eff| {
            eff.actions.iter().any(|a| {
                matches!(a, Action::AlsoLead { condition, order }
                    if playable_as(*order, board)
                        && conditions::holds(condition, &self.state, key, roll))
            })
        })
    }

    // -- play resolution + stops ------------------------------------------

    /// Resolve a played card: log it, offer the stop window FIRST (a stopped card
    /// fires none of its text), then OnPlay, land it, OnHit + type-gated hit
    /// gimmicks, and re-check hand caps. `Ok(true)` iff the card landed and the
    /// match is still live.
    fn resolve_play(&mut self, active: &str, defender: &str, card: Card) -> Eng<bool> {
        // Record the card's name for the duration of its resolution so a flip its own
        // effect causes can gate "flipped by \"<name>\"" (restored for nesting — a
        // PlaySelf during a flip resolves another card).
        let prev_name = self.firing_card_name.replace(card.name.clone());
        let result = self.resolve_play_inner(active, defender, card);
        self.firing_card_name = prev_name;
        result
    }

    fn resolve_play_inner(&mut self, active: &str, defender: &str, card: Card) -> Eng<bool> {
        // Poison: fold any queued "added text" onto the card BEFORE the stop window,
        // so an "If stopped, …" injection reaches `apply_stop`.
        let mut card = card;
        self.apply_pending_text(active, &mut card);
        let t = self.state.turn_no;
        self.log(Event::Play {
            t,
            player: active.to_owned(),
            card: card.db_uuid.clone(),
            order: card.play_order.name().to_owned(),
            atk_type: card.atk_type.name().to_owned(),
        });
        if let Some((stop, extra)) = self.offer_stop(defender, &card)? {
            self.apply_stop(active, defender, card, stop, extra)?;
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
        // This turn's roll context (both sides' rolled skills via `opp_skill`) so a
        // card's own OnPlay/OnHit rider can gate on it — "if either/both players
        // rolled Power for their turn roll, …" (Tomato Tomato Jr.). No existing card
        // carries a roll condition on OnPlay/OnHit, so this only enables new riders.
        let roll = self.roll_ctx.get(active).cloned();
        self.run_effects(&effects, "OnPlay", active, roll.as_ref())?;
        if self.ended() {
            return Ok(false);
        }
        // Stamp the global play sequence (task #93) as the card reaches the board; it
        // rides along into the discard pile, where a "this match has Disqualifications"
        // re-enable is resolved against a standing no-DQ by last-played order.
        card.played_seq = Some(self.state.bump_play_seq());
        {
            let p = self.state.players.get_mut(active).unwrap();
            p.in_play.push(card.clone());
            p.hits_this_turn += 1; // a landed card is a hit this turn (Condition::HitThisTurn)
            p.hit_this_turn.push(card.clone()); // by-card, for a filtered HitCard query
        }
        self.run_effects(&effects, "OnHit", active, roll.as_ref())?; // the card's own "when this hits"
        self.run_hit_gimmicks(&card, active)?; // owner gimmick "when you hit a <type>" (D1)
        self.enforce_hand_caps()?; // a new Static max-handsize mod may force a discard
        Ok(!self.ended())
    }

    /// Fire the standing type-gated `OnHit` gimmicks for a card of `card`'s attack
    /// type that `hitter` just hit (D1). A card's own untyped OnHit already resolved
    /// via `run_effects`, so it is not re-fired. BOTH players are scanned: an
    /// `OnHit{who=Opp}` gimmick fires for the NON-hitter ("after your opponent hits a
    /// Follow Up" — El Super Hombre V2), matching how OnBreakout/OnBury scope.
    fn run_hit_gimmicks(&mut self, card: &Card, hitter: &str) -> Eng<()> {
        // The hit card is already on the board here, so a "for each OTHER … in play"
        // count must drop it (`Draw.per_excludes_trigger`).
        self.hit_card = Some(card.db_uuid.clone());
        let mut result = Ok(());
        for owner in ["A", "B"] {
            result = self.run_hit_gimmicks_inner(card, owner, hitter);
            if result.is_err() {
                break;
            }
        }
        self.hit_card = None;
        result
    }

    fn run_hit_gimmicks_inner(&mut self, card: &Card, key: &str, hitter: &str) -> Eng<()> {
        let effects = self.standing_effects(key);
        for eff in &effects {
            if self.on_hit_trigger_fires(eff, card, key, hitter) {
                self.fire_if_ready(eff, key, None)?;
            }
        }
        // WHILE_IN_DISCARD OnHit (task #115 slice 2): a card in `key`'s discard pile
        // watching a hit — "when this card is in your discard pile and you hit <X>, add it
        // to your hand" / "when your opponent hits a Follow Up, …". `self_card` binds the
        // source so a self-referential body (`AddSelfToHand` / `ShuffleSelfIntoDeck`)
        // resurrects the card that fired it, exactly as `run_on_roll` does at the roll-off.
        for (uuid, eff) in self.discard_self_triggers(key) {
            if self.on_hit_trigger_fires(&eff, card, key, hitter) {
                self.self_card = Some(uuid);
                let r = self.fire_if_ready(&eff, key, None);
                self.self_card = None;
                r?;
            }
        }
        Ok(())
    }

    /// Whether `eff`'s `OnHit` trigger fires for `hitter` hitting `card`, from `key`'s
    /// vantage (`who` selects whose hit — SELF = `key`'s own, OPP = its opponent's). A
    /// bare untyped OnHit is the card's OWN "when this hits" (already fired via
    /// `run_effects`) and is skipped UNLESS `on_any` is set; a gated OnHit fires when the
    /// hit card's attack-type / play-order / name gates all match. Shared by the in-play
    /// standing scan and the discard-pile self-trigger scan.
    fn on_hit_trigger_fires(&self, eff: &Effect, card: &Card, key: &str, hitter: &str) -> bool {
        let Trigger::OnHit {
            atk_type,
            name_contains,
            text_contains,
            on_any,
            order,
            who,
        } = &eff.trigger
        else {
            return false;
        };
        if self.target(*who, key) != hitter {
            return false;
        }
        let has_name_gate = !name_contains.is_empty() || !text_contains.is_empty();
        if atk_type.is_none() && !has_name_gate && order.is_none() && !on_any {
            return false;
        }
        let type_ok = atk_type.is_none_or(|want| card.counts_as_atk_type(want));
        // "When you hit a Lead" — the play-order gate on the HIT card (ANDed).
        let order_ok = order.is_none_or(|want| want == card.play_order);
        let name_gate = CardFilter {
            name_contains: name_contains.clone(),
            text_contains: text_contains.clone(),
            ..Default::default()
        };
        type_ok && order_ok && conditions::card_matches(card, &name_gate)
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
    fn offer_stop(&mut self, defender: &str, card: &Card) -> Eng<Option<(Card, Vec<Card>)>> {
        let need = Self::stops_required(card);
        let stops = self.legal_stops(defender, card);
        // A card that "can only be stopped by N Stops" (King Brian Cage) is
        // unstoppable unless the defender holds N legal stops to commit at once.
        if (stops.len() as i64) < need {
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
        let primary = self.take_from_hand(defender, number);
        // Commit the N-1 additional stops the requirement demands, best (lowest
        // number) remaining first; the pre-check guarantees enough are on hand.
        let mut extra = Vec::new();
        for _ in 1..need {
            let others = self.legal_stops(defender, card);
            let Some(next) = others.first() else { break };
            let n = next.number;
            extra.push(self.take_from_hand(defender, n));
        }
        Ok(Some((primary, extra)))
    }

    /// How many Stops must be committed at once to stop `card` — the max `RequireStops`
    /// count printed on the card (default 1). Read from the attack's own effects.
    fn stops_required(card: &Card) -> i64 {
        card.effects
            .iter()
            .flat_map(|e| &e.actions)
            .filter_map(|a| match a {
                Action::RequireStops { count } => Some(*count),
                _ => None,
            })
            .max()
            .unwrap_or(1)
            .max(1)
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
        if self.state.is_text_blanked(stopper, defender) {
            return false; // a text-blanked stop card cannot stop
        }
        if self.stop_suppressed(defender, stopper) {
            return false; // Jokerfish "your cards #N-N cannot stop cards"
        }
        // An `Unstoppable` attack is stopped only by a Stop that declares
        // `even_unstoppable` ("stop any Finish Strike that cannot be stopped").
        let attacker = self.state.opponent_of(defender);
        let unstoppable = self.attack_is_unstoppable_by(&attacker, attack, stopper);
        stopper.effects.iter().any(|eff| {
            conditions::holds(&eff.condition, &self.state, defender, None)
                && attacker_meets_tag_gates(eff, attack)
                && eff.actions.iter().any(|action| match action {
                    Action::Stop {
                        even_unstoppable, ..
                    } => {
                        (!unstoppable || *even_unstoppable)
                            && self.stop_matches_for(defender, action, attack)
                    }
                    _ => false,
                })
        })
    }

    /// Whether `attack` is `Unstoppable` against `stopper` from `attacker`'s view: a
    /// matching `Unstoppable` action (see [`unstoppable_gate`]) inside an effect whose
    /// condition holds for the attacker. Two scopes: the attack's OWN effects ("this
    /// card cannot be stopped", read with the attacker's turn roll context so "if you
    /// rolled 7 / the same skill" resolve) OR the attacker's gimmick/entrance
    /// declarations ("Your cards cannot be stopped by …" — every one of their cards).
    /// A main-deck card's self-scoped `Unstoppable` never leaks to its owner's other
    /// attacks because the gimmick scan excludes in-play cards.
    fn attack_is_unstoppable_by(&self, attacker: &str, attack: &Card, stopper: &Card) -> bool {
        let roll = self.roll_ctx.get(attacker);
        let holds_for = |eff: &Effect, roll: Option<&RollContext>| {
            eff.actions.iter().any(|a| unstoppable_gate(a, stopper))
                && conditions::holds(&eff.condition, &self.state, attacker, roll)
        };
        attack.effects.iter().any(|eff| holds_for(eff, roll))
            || self
                .gimmick_standing_effects(attacker)
                .iter()
                .any(|eff| matches!(eff.trigger, Trigger::Static) && holds_for(eff, None))
    }

    /// Whether `defender` declares that `stopper` (by its deck number) cannot act as a
    /// Stop — Jokerfish V2's `SuppressStop` number range.
    fn stop_suppressed(&self, defender: &str, stopper: &Card) -> bool {
        let n = stopper.number;
        self.declares_static(defender, |a| {
            matches!(a, Action::SuppressStop { number_min, number_max }
                if n >= *number_min && n <= *number_max)
        })
    }

    /// Whether a `Stop` action's order/type filter covers `attack`, honoring
    /// `defender`'s active `StopCountsOrderAs` reframes: an attack whose order is
    /// reframed also satisfies a `Stop` of the reframed order ("your opponent's
    /// Finishes are also Follow Ups for your Stop cards"). `None` order = any.
    fn stop_matches_for(&self, defender: &str, stop: &Action, attack: &Card) -> bool {
        let Action::Stop {
            order,
            atk_type,
            target,
            ..
        } = stop
        else {
            return false;
        };
        let order_ok = match order {
            None => true,
            Some(o) => {
                *o == attack.play_order
                    || self.declares_static(defender, |a| {
                        matches!(a, Action::StopCountsOrderAs { attack_order, as_order }
                            if *attack_order == attack.play_order && *as_order == *o)
                    })
            }
        };
        let target_ok = target
            .as_ref()
            .is_none_or(|f| conditions::card_matches(attack, f));
        order_ok && atk_type.is_none_or(|want| attack.counts_as_atk_type(want)) && target_ok
    }

    /// Apply a stop: the stopped ATTACK goes to the attacker's discard; the stopping
    /// card enters the defender's board and persists (bypassing the play-sequence
    /// gate). Fires the stop's OnHit + hit gimmicks, then both sides' OnStop.
    /// Land one committed stop card: it enters the defender's play area (a stop is
    /// itself a hit), logs the Stop event, and fires its OnHit + hit gimmicks. Shared
    /// by the primary stop and every extra a `RequireStops` attack forces.
    fn land_stop_card(&mut self, defender: &str, stop: &Card, attack: &Card) -> Eng<()> {
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
        self.run_effects(&stop_effects, "OnHit", defender, None)?;
        self.run_hit_gimmicks(stop, defender)?; // a stop entering play is itself a hit
        Ok(())
    }

    fn apply_stop(
        &mut self,
        active: &str,
        defender: &str,
        attack: Card,
        stop: Card,
        extra: Vec<Card>,
    ) -> Eng<()> {
        self.state
            .players
            .get_mut(active)
            .unwrap()
            .discard
            .push(attack.clone());
        // Stamp the turn on the STOPPING side so `Condition::StoppedCard` reads it this
        // and next turn ("if you stopped a card last turn, …"), like `broke_out_turn`.
        let turn = self.state.turn_no;
        self.state
            .players
            .get_mut(defender)
            .unwrap()
            .flags
            .insert("stopped_card_turn".to_owned(), json!(turn));
        self.land_stop_card(defender, &stop, &attack)?;
        // Extra stops a `RequireStops` attack forced: each lands as a committed stop.
        // The heavy attack-side resolution below (blank-text, OnStop) runs once, keyed
        // off the primary stop — MM's extra stops are vanilla, with no OnStop text.
        for s in &extra {
            self.land_stop_card(defender, s, &attack)?;
        }
        let stop_effects = stop.effects.clone();
        let attack_effects = attack.effects.clone();
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
        // WHILE_IN_DISCARD OnStop with self_card binding (task #115 slice 2b): "when this
        // card is in your discard pile and you stop / your opponent stops <X>, add it to
        // your hand". The re-parsed remainder carries the same `dir` an in-play OnStop
        // would, and this site is already called for both players with both dirs, so the
        // who-scoping falls out of the identical `dir`/`order` match.
        for (uuid, eff) in self.discard_self_triggers(key) {
            if matches!(eff.trigger, Trigger::OnStop { dir: d, order }
                if d == dir && order.is_none_or(|o| o == stopped))
            {
                self.self_card = Some(uuid);
                let r = self.fire_if_ready(&eff, key, None);
                self.self_card = None;
                r?;
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
    /// Roll the finish die and apply any switch-rolled-skill (Scott Prime) so base +
    /// combo recompute from the switched skill. Shared by the initial finish roll and
    /// each finish-roll re-roll.
    fn roll_finish_skill(&mut self, finisher: &str) -> Eng<Skill> {
        let mut skill = self.state.rng.roll();
        if let Some(to) = self.find_switch(finisher, skill)? {
            self.log_effect(
                finisher,
                "SwitchRolledSkill",
                Some(finisher),
                json!({"from": skill.name(), "to": to.name(), "roll": "finish"}),
            );
            skill = to;
        }
        Ok(skill)
    }

    /// Offer `finisher`'s optional finish-roll re-roll ("you may re-roll your Finish
    /// roll", schema v76). Scans standing effects for a `Reroll{when:This, finish:true}`
    /// whose gate holds against the finish roll `skill`, honoring frequency, the
    /// optional "you may", and any in-play cost; on election it runs the effect's
    /// OTHER actions (e.g. "draw 1 card to re-roll") and returns `true` to signal a
    /// re-roll. `who`/`choose` are ignored — a finish re-roll is always the finisher's.
    fn offer_finish_reroll(&mut self, finisher: &str, skill: Skill) -> Eng<bool> {
        let ctx = RollContext {
            skill: Some(skill),
            gap: None,
            value: None,
            opp_skill: None,
        };
        let effects = self.standing_effects(finisher);
        for eff in &effects {
            let Some(cost) = eff.actions.iter().find_map(|a| match a {
                Action::Reroll {
                    when: RollWhen::This,
                    finish: true,
                    cost,
                    ..
                } => Some(cost.clone()),
                _ => None,
            }) else {
                continue;
            };
            if !(self.may_fire(eff, finisher)
                && conditions::holds(&eff.condition, &self.state, finisher, Some(&ctx)))
            {
                continue;
            }
            if let Some(c) = &cost {
                if !self.can_pay_reroll(finisher, c) {
                    continue;
                }
            }
            if eff.optional && !self.take_optional(eff, finisher)? {
                continue;
            }
            self.mark_fired(eff, finisher);
            if let Some(c) = &cost {
                self.pay_reroll(finisher, c)?;
            }
            // The paired non-Reroll actions ("draw 1 card to re-roll") resolve here.
            for a in &eff.actions {
                if !matches!(a, Action::Reroll { .. }) {
                    self.apply_action(a, finisher, &eff.raw_clause)?;
                }
            }
            return Ok(true);
        }
        Ok(false)
    }

    /// Offer a re-roll of `defender`'s just-made breakout die (schema v102). Scans the
    /// defender's own `Reroll{breakout, who:SelfSide}` ("re-roll your Breakout roll")
    /// and the finisher's `Reroll{breakout, who:Opp}` ("force your opponent to re-roll
    /// their Breakout roll") — both re-roll the defender's die, differing only in which
    /// side owns the "you may". Honors frequency, the optional flag, and any in-play
    /// cost; on election runs the effect's paired non-`Reroll` actions and returns
    /// `true`. A no-op (no RNG, no decision) when neither side has one in play, so the
    /// frozen corpus stays byte-identical.
    fn offer_breakout_reroll(&mut self, defender: &str) -> Eng<bool> {
        let finisher = self.state.opponent_of(defender);
        for (owner, want) in [(defender.to_owned(), Who::SelfSide), (finisher, Who::Opp)] {
            let effects = self.standing_effects(&owner);
            for eff in &effects {
                let Some(cost) = eff.actions.iter().find_map(|a| match a {
                    Action::Reroll {
                        breakout: true,
                        who,
                        cost,
                        ..
                    } if *who == want => Some(cost.clone()),
                    _ => None,
                }) else {
                    continue;
                };
                if !(self.may_fire(eff, &owner)
                    && conditions::holds(&eff.condition, &self.state, &owner, None))
                {
                    continue;
                }
                if let Some(c) = &cost {
                    if !self.can_pay_reroll(&owner, c) {
                        continue;
                    }
                }
                if eff.optional && !self.take_optional(eff, &owner)? {
                    continue;
                }
                self.mark_fired(eff, &owner);
                if let Some(c) = &cost {
                    self.pay_reroll(&owner, c)?;
                }
                for a in &eff.actions {
                    if !matches!(a, Action::Reroll { .. }) {
                        self.apply_action(a, &owner, &eff.raw_clause)?;
                    }
                }
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn finish_sequence(&mut self, finisher: &str, defender: &str, card: &Card) -> Eng<()> {
        let mut skill = self.roll_finish_skill(finisher)?;
        // Optional finish-roll re-roll ("you may re-roll your Finish roll", v76):
        // bounded to avoid loops, each taken re-roll re-applies switch-rolled-skill.
        let mut rerolls = 0;
        while rerolls < FINISH_REROLL_CAP && self.offer_finish_reroll(finisher, skill)? {
            rerolls += 1;
            skill = self.roll_finish_skill(finisher)?;
        }
        let base = self.stat(finisher, skill);
        let combo: i64 = {
            let in_play = &self.state.players[finisher].in_play;
            in_play
                .iter()
                .map(|c| self.card_finish_bonus(c, skill, finisher))
                .sum()
        };
        let bonus = combo + self.finish_roll_bonus(finisher, skill, base);
        let cm = self.state.crowd_meter;
        let value = base + bonus + cm;
        let auto = crate::finish::is_auto_success(value, cm);
        self.log_finish_attempt(finisher, card, skill, bonus, value, cm, auto);
        // "When you roll <skill> for your Finish roll" gimmicks fire here, after the
        // roll is determined but before it resolves (The Man from I.T.). No deck card
        // carries `OnFinishRoll`, so the frozen finish games are untouched.
        self.run_on_finish_roll(finisher, skill, value)?;
        if self.ended() {
            return Ok(());
        }
        if !auto {
            let broke = self.breakout(defender, value)?;
            if self.ended() {
                return Ok(()); // an OnBreakoutRoll clause ended the match on the roll
            }
            if broke {
                self.on_broken_out(finisher)?; // defender broke out; the match resumes
                return Ok(());
            }
        }
        self.win(finisher, "finish");
        Ok(())
    }

    /// A single in-play card's Finish-roll combo bonus for `skill`, doubled when the
    /// card declares `DoubleFinishIfBumped` and this turn's roll-off bumped. A
    /// text-blanked card contributes nothing — its printed "+N to <skill>" is blank
    /// (e.g. an opposing Impact is Family V2 blanking a Spotlight finish collapses it
    /// to raw stats). `owner` is the card's controller (the finisher).
    fn card_finish_bonus(&self, card: &Card, skill: Skill, owner: &str) -> i64 {
        if self.state.is_text_blanked(card, owner) {
            return 0;
        }
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
        // Conditional double (schema v77): "double these bonuses if <cond>". The
        // condition sees the owner's turn-roll context so a `RollWasSkill{Power}`
        // gate ("if you rolled Power for your turn roll") resolves at finish time.
        let roll = self.roll_ctx.get(owner);
        if card.effects.iter().any(|eff| {
            eff.actions.iter().any(|a| {
                matches!(a, Action::DoubleFinishIf { condition }
                    if conditions::holds(condition, &self.state, owner, roll))
            })
        }) {
            bonus *= 2;
        }
        bonus
    }

    /// "+N to your Finish rolls" from the finisher's live effects (in-play combo,
    /// gimmick, entrance), each gated by its condition and by its `when_skill`.
    fn finish_roll_bonus(&self, key: &str, skill: Skill, base: i64) -> i64 {
        // The finisher's own bonuses, plus the opponent's `either` bonuses — "if either
        // player rolls <S> for their Finish roll, their roll is -1" applies to whoever
        // is finishing, so it counts from the other board too.
        let opp = self.state.opponent_of(key);
        self.finish_bonus_from(key, skill, base, false)
            + self.finish_bonus_from(&opp, skill, base, true)
    }

    /// Sum `owner`'s active `FinishRollBonus` for a Finish roll of `skill` at `base`.
    /// `either_only` restricts to symmetric (`either`) bonuses — the mode used when
    /// scanning the *opponent's* board, where only "if either player rolls …" reaches the
    /// finisher's roll.
    fn finish_bonus_from(&self, owner: &str, skill: Skill, base: i64, either_only: bool) -> i64 {
        let mut total = 0;
        for (src, eff) in self.standing_effects_sourced(owner) {
            if !conditions::holds(&eff.condition, &self.state, owner, None) {
                continue;
            }
            for a in &eff.actions {
                if let Action::FinishRollBonus {
                    delta,
                    when_skill,
                    when_base_le,
                    when_base_ge,
                    per,
                    per_who,
                    per_zone,
                    per_divisor,
                    cap,
                    per_excludes_self,
                    either,
                    per_crowd,
                } = a
                {
                    if either_only && !either {
                        continue;
                    }
                    // Base-roll gate: "If your Finish roll is N or less/greater" reads
                    // the base (the skill's stat, before any bonuses) — DESIGN §3.
                    if when_base_le.is_some_and(|t| base > t)
                        || when_base_ge.is_some_and(|t| base < t)
                    {
                        continue;
                    }
                    if when_skill.is_none() || *when_skill == Some(skill) {
                        // `per_crowd`: a SECOND live-Crowd-Meter addend (clamped to `cap`),
                        // "Your Finish roll is + the Crowd Meter (Max +N)".
                        if *per_crowd {
                            total += cap
                                .map_or(self.state.crowd_meter, |c| self.state.crowd_meter.min(c));
                            continue;
                        }
                        // Flat `delta`, or `delta * (count of `per_who`'s cards in
                        // `per_zone` matching the filter)` — "+1 per Spotlight in play".
                        let bonus = match per {
                            Some(f) => {
                                let who = self.target(*per_who, owner);
                                // "for each OTHER …": drop the source card (found on the
                                // counted board by uuid) — only bites a SELF-board count.
                                let board = &self.state.players[&who].in_play;
                                let exclude = per_excludes_self
                                    .then(|| src.as_deref())
                                    .flatten()
                                    .and_then(|u| board.iter().find(|c| c.db_uuid == u));
                                let count =
                                    self.state.count_in_zone_excl(f, *per_zone, &who, exclude);
                                // "+1 for every N X" divides the match count first.
                                let mult = count / per_divisor.unwrap_or(1).max(1);
                                let raw = *delta * mult;
                                // "(Max +M)" clamps the per-count product, not the flat.
                                cap.map_or(raw, |c| raw.min(c))
                            }
                            None => *delta,
                        };
                        total += bonus;
                    }
                }
            }
        }
        total
    }

    /// The standing turn-roll bonus for `key` when the roll came up `skill`: the sum of
    /// every active `TurnRollBonus{skill}` on their board (gimmick / entrance / in-play),
    /// the turn-roll parallel of [`finish_roll_bonus`](Self::finish_roll_bonus). Applied
    /// only in the roll-off, so a "during turn rolls" buff never leaks into finish rolls,
    /// stops, or skill comparisons.
    fn turn_roll_bonus(&self, key: &str, skill: Skill) -> i64 {
        // A roller sums the `SelfSide` mods on their OWN board ("your Power is +N during
        // turn rolls") with the `Opp` mods on their OPPONENT's board ("your opponent's
        // Power is -N during their turn rolls"). An `either` mod ("if either player
        // rolls …") applies to whoever rolls, so it counts from whichever board it sits
        // on. Mirrors `breakout_bonus`.
        let opp = self.state.opponent_of(key);
        self.turn_roll_bonus_from(key, skill, Who::SelfSide)
            + self.turn_roll_bonus_from(&opp, skill, Who::Opp)
    }

    /// Sum the `TurnRollBonus{skill}` deltas on `owner`'s board that reach the roller.
    /// `applies_who` is the `who` value that targets the roller from THIS board —
    /// `SelfSide` when `owner` is the roller (their own-roll mods), `Opp` when `owner` is
    /// the roller's opponent (mods that debuff/buff the roller). An `either` mod counts
    /// regardless of `who`.
    fn turn_roll_bonus_from(&self, owner: &str, skill: Skill, applies_who: Who) -> i64 {
        let mut total = 0;
        for eff in self.standing_effects(owner) {
            if !conditions::holds(&eff.condition, &self.state, owner, None) {
                continue;
            }
            for a in &eff.actions {
                if let Action::TurnRollBonus {
                    skill: s,
                    delta,
                    who,
                    either,
                    per_crowd,
                    cap,
                } = a
                {
                    if *s == skill && (*either || *who == applies_who) {
                        // `per_crowd` uses the live Crowd Meter (clamped to `cap`) as the
                        // delta — "your Technique is + the Crowd Meter (Max +3) during
                        // your turn roll"; the flat `delta` otherwise.
                        total += if *per_crowd {
                            cap.map_or(self.state.crowd_meter, |c| self.state.crowd_meter.min(c))
                        } else {
                            *delta
                        };
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
    fn breakout_bonus(&self, defender: &str, attempt_no: i64, rolled: Skill) -> i64 {
        // The defender's own `SelfSide` mods plus their opponent's `Opp` mods both land on
        // the defender's breakout roll ("your breakout rolls are +1" from the defender;
        // "your opponent's breakout rolls are -1" from the other side).
        let opp = self.state.opponent_of(defender);
        self.breakout_mods_from(defender, attempt_no, rolled, Who::SelfSide)
            + self.breakout_mods_from(&opp, attempt_no, rolled, Who::Opp)
    }

    /// Sum the `BreakoutModifier` deltas in `owner`'s standing effects whose `who` equals
    /// `want` and whose attempt/skill gates admit `(attempt_no, rolled)`. Each effect's
    /// condition is evaluated from `owner`'s point of view (they declared it).
    fn breakout_mods_from(&self, owner: &str, attempt_no: i64, rolled: Skill, want: Who) -> i64 {
        let mut total = 0;
        for (src, eff) in self.standing_effects_sourced(owner) {
            if !conditions::holds(&eff.condition, &self.state, owner, None) {
                continue;
            }
            for a in &eff.actions {
                if let Action::BreakoutModifier {
                    delta,
                    attempts,
                    when_skill,
                    who,
                    either,
                    per,
                    per_who,
                    per_zone,
                    per_divisor,
                    cap,
                    per_excludes_self,
                } = a
                {
                    let attempt_ok = attempts.is_none() || *attempts == Some(attempt_no);
                    let skill_ok = when_skill.is_none() || *when_skill == Some(rolled);
                    // An `either` mod applies to whoever is rolling the breakout,
                    // regardless of `who`; otherwise the `who` side must match `want`.
                    if !((*either || *who == want) && attempt_ok && skill_ok) {
                        continue;
                    }
                    // Flat `delta`, or `delta * (count of `per_who`'s cards in `per_zone`
                    // matching the filter)` — "+1 for each Stop they have in play"; the
                    // per-count parallel of `finish_bonus_from`. Counted from `owner`'s
                    // POV (they declared it), so `per_who=Opp` counts the OTHER board.
                    total += match per {
                        Some(f) => self.per_count_product(
                            *delta,
                            f,
                            *per_who,
                            *per_zone,
                            *per_divisor,
                            *cap,
                            *per_excludes_self,
                            src.as_deref(),
                            owner,
                        ),
                        None => *delta,
                    };
                }
            }
        }
        total
    }

    /// The per-count product for a `per`-scaled modifier: `delta * floor(count / divisor)`
    /// clamped to `cap`, where the count is `per_who`'s cards (from `owner`'s POV) in
    /// `per_zone` matching `f`, optionally excluding the `src` card ("for each OTHER …").
    /// Shared by the breakout roll/attempt per-count paths.
    #[allow(clippy::too_many_arguments)]
    fn per_count_product(
        &self,
        delta: i64,
        f: &CardFilter,
        per_who: Who,
        per_zone: CountZone,
        per_divisor: Option<i64>,
        cap: Option<i64>,
        excludes_self: bool,
        src: Option<&str>,
        owner: &str,
    ) -> i64 {
        let counted = self.target(per_who, owner);
        let board = &self.state.players[&counted].in_play;
        let exclude = excludes_self
            .then_some(src)
            .flatten()
            .and_then(|u| board.iter().find(|c| c.db_uuid == u));
        let n = self
            .state
            .count_in_zone_excl(f, per_zone, &counted, exclude);
        let mult = n / per_divisor.unwrap_or(1).max(1);
        cap.map_or(delta * mult, |c| (delta * mult).min(c))
    }

    /// Number of breakout attempts `defender` gets this turn — the "reduced / extra
    /// breakout rolls" family. Base is `BREAKOUT_ATTEMPTS`, overridden by any
    /// `BreakoutAttempts{set}` ("your opponent gets 2 Breakout rolls this turn") and
    /// shifted by `BreakoutAttempts{delta}` ("gets 1 additional / 1 fewer Breakout
    /// roll"). Both boards contribute — a `SelfSide` effect on the defender and an `Opp`
    /// effect on the finisher — the count-family parallel of [`breakout_bonus`](Self::breakout_bonus).
    /// Multiple `set`s take the smallest (most restrictive). The result is clamped to
    /// `[1, BREAKOUT_ATTEMPTS + 7]`: a defender always gets at least one roll, and the
    /// ceiling caps the loop. Returns `BREAKOUT_ATTEMPTS` unchanged when no such card is
    /// in play, keeping the frozen corpus byte-identical.
    fn breakout_attempts_for(&self, defender: &str) -> usize {
        let base = BREAKOUT_ATTEMPTS as i64;
        let opp = self.state.opponent_of(defender);
        let mut set_val: Option<i64> = None;
        let mut delta = 0;
        self.collect_breakout_attempts(defender, Who::SelfSide, &mut set_val, &mut delta);
        self.collect_breakout_attempts(&opp, Who::Opp, &mut set_val, &mut delta);
        if set_val.is_none() && delta == 0 {
            return BREAKOUT_ATTEMPTS; // no count-modifier in play — byte-identical path
        }
        (set_val.unwrap_or(base) + delta).clamp(1, base + 7) as usize
    }

    /// Fold `owner`'s standing `BreakoutAttempts` effects whose `who == want` (and whose
    /// condition holds from `owner`'s POV) into a running `set`/`delta`: `set` keeps the
    /// smallest value seen, `delta` sums (with any `per`-count scaling applied).
    fn collect_breakout_attempts(
        &self,
        owner: &str,
        want: Who,
        set_val: &mut Option<i64>,
        delta: &mut i64,
    ) {
        for (src, eff) in self.standing_effects_sourced(owner) {
            if !conditions::holds(&eff.condition, &self.state, owner, None) {
                continue;
            }
            for a in &eff.actions {
                let Action::BreakoutAttempts {
                    delta: d,
                    set,
                    who,
                    per,
                    per_who,
                    per_zone,
                    per_divisor,
                    cap,
                    per_excludes_self,
                } = a
                else {
                    continue;
                };
                if *who != want {
                    continue;
                }
                if let Some(s) = set {
                    *set_val = Some(set_val.map_or(*s, |cur| cur.min(*s)));
                }
                *delta += match per {
                    Some(f) => self.per_count_product(
                        *d,
                        f,
                        *per_who,
                        *per_zone,
                        *per_divisor,
                        *cap,
                        *per_excludes_self,
                        src.as_deref(),
                        owner,
                    ),
                    None => *d,
                };
            }
        }
    }

    /// Up to `BREAKOUT_ATTEMPTS` defender rolls; the first that beats the finish
    /// value breaks out. Returns whether the defender broke out.
    fn breakout(&mut self, defender: &str, finish_value: i64) -> Eng<bool> {
        let cm = self.state.crowd_meter;
        let mut rolls: Vec<BreakoutRoll> = Vec::new();
        let mut broke = false;
        // `BREAKOUT_ATTEMPTS` by default; raised/lowered by any `BreakoutAttempts`
        // ("your opponent gets 2 Breakout rolls this turn" / "1 additional/fewer").
        let attempts = self.breakout_attempts_for(defender);
        for i in 0..attempts {
            let mut skill = self.state.rng.roll();
            // Optional breakout-roll re-roll ("re-roll your Breakout roll" / "force your
            // opponent to re-roll their Breakout roll", v102), bounded to avoid loops.
            // A no-op — no RNG, no decision — when neither side has one in play, so the
            // frozen corpus (no such card) stays byte-identical.
            let mut rr = 0;
            while rr < BREAKOUT_REROLL_CAP && self.offer_breakout_reroll(defender)? {
                rr += 1;
                skill = self.state.rng.roll();
            }
            let val = self.stat(defender, skill);
            // A `BreakoutModifier{delta}` raises the roll by `delta`; passing it as a
            // NEGATIVE `penalty` keeps the raw-10-always-breaks rule on the unboosted
            // value (a boosted 8->10 is not a "raw 10"). No modifier -> penalty 0 ->
            // byte-identical to before (the frozen corpus has none).
            let penalty = -self.breakout_bonus(defender, i as i64 + 1, skill);
            let success = crate::finish::stat_breaks_out(val, finish_value, penalty, cm);
            rolls.push(BreakoutRoll {
                skill: skill.name().to_owned(),
                value: val,
                penalty,
                success,
            });
            // "If your opponent rolls X for their Breakout roll, you lose" — an
            // OnBreakoutRoll effect on the finisher's side keys off this roll's value.
            // A no-op for decks without one (the frozen corpus has none), so byte-
            // identical there. A caused loss ends the match immediately.
            self.run_on_breakout_roll(defender, skill, val)?;
            if self.ended() {
                break;
            }
            if success {
                broke = true;
                break;
            }
        }
        let t = self.state.turn_no;
        // Stamp this turn number so `Condition::BrokeOutLastTurn` reads it next turn ("if
        // you broke out last turn, …"), mirroring `do_pass`'s `last_pass_turn`.
        if broke {
            self.state
                .players
                .get_mut(defender)
                .unwrap()
                .flags
                .insert("broke_out_turn".to_owned(), json!(t));
        }
        self.log(Event::Breakout {
            t,
            defender: defender.to_owned(),
            broke_out: broke,
            rolls,
        });
        Ok(broke)
    }

    /// Fire `OnBreakoutRoll` effects for `roller`'s just-made breakout roll (skill
    /// `skill`, value `value`). The clause lives on the finisher's side (`who = Opp`,
    /// the defender rolling against their finish); a `RollValue` / `RollWasSkill`
    /// condition reads the roll from the `RollContext`. Resolves any loss it causes
    /// straight away so `breakout` can bail. No-op when no such effect is in play.
    fn run_on_breakout_roll(&mut self, roller: &str, skill: Skill, value: i64) -> Eng<()> {
        let ctx = RollContext {
            skill: Some(skill),
            value: Some(value),
            gap: None,
            opp_skill: None,
        };
        for owner in ["A", "B"] {
            let effects = self.triggered_effects(owner);
            for eff in &effects {
                let Trigger::OnBreakoutRoll { who } = &eff.trigger else {
                    continue;
                };
                if self.target(*who, owner) != roller {
                    continue;
                }
                self.fire_if_ready(eff, owner, Some(&ctx))?;
                self.resolve_pending();
                if self.ended() {
                    return Ok(());
                }
            }
        }
        Ok(())
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
            // WHILE_IN_DISCARD OnBreakout with self_card binding (task #115 slice 2b):
            // "when this card is in your discard pile and either player / your opponent
            // breaks out, you may shuffle it into your deck / add it to your hand". (Was
            // firing flattened via `triggered_effects` but with no `self_card` — the
            // dedicated loop binds the source so the self-action resurrects the right card;
            // splitting the standing scan off keeps it from double-firing.)
            for (uuid, eff) in self.discard_self_triggers(key) {
                let Trigger::OnBreakout { who } = &eff.trigger else {
                    continue;
                };
                if who.is_none_or(|w| self.target(w, key) == breaker) {
                    self.self_card = Some(uuid);
                    let r = self.fire_if_ready(&eff, key, None);
                    self.self_card = None;
                    r?;
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
        // The roll-off is nobody's turn yet: gate `DuringTurn` off for its duration so a
        // "during your turn" buff can't leak into the roll (the flag survives a mid-roll
        // suspend via the snapshot). Cleared once the winner is known and it IS their turn.
        self.state.in_turn_roll = true;
        let winner = self.roll_off()?;
        self.state.active = winner.clone();
        self.state.in_turn_roll = false;
        let loser = self.state.opponent_of(&winner);
        let ctx_w = self.roll_ctx.get(&winner).cloned().unwrap_or_default();
        let ctx_l = self.roll_ctx.get(&loser).cloned().unwrap_or_default();
        let eff_w = self.standing_effects(&winner);
        self.run_effects(&eff_w, "OnWinTurn", &winner, Some(&ctx_w))?;
        let eff_l = self.standing_effects(&loser);
        self.run_effects(&eff_l, "OnLoseTurn", &loser, Some(&ctx_l))?;
        // Same srgpc ordering rule for the post-roll OnRoll gimmicks.
        for key in Self::roll_order(
            self.roll_ctx.get("A").and_then(|c| c.value).unwrap_or(0),
            self.roll_ctx.get("B").and_then(|c| c.value).unwrap_or(0),
        ) {
            self.run_on_roll(key)?;
            self.run_on_rolled_all(key)?;
        }
        // One-shot roll-conditional draws ("if your [opponent's] next turn roll is <S>,
        // draw N") armed on a prior turn resolve against this just-settled turn roll.
        self.resolve_pending_roll_draws()?;
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
        // Standing OnRoll (in-play + gimmick + copied): no self-referent.
        for eff in &self.standing_effects(key) {
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
        // WHILE_IN_DISCARD OnRoll: a card in the discard pile watching the turn roll
        // ("when this card is in your discard pile and you roll <S>, add it to your
        // hand"). `self_card` binds the source card so its self-referential body
        // resurrects the right one.
        for (uuid, eff) in self.discard_self_triggers(key) {
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
                self.self_card = Some(uuid);
                let r = self.fire_if_ready(&eff, key, Some(&ctx));
                self.self_card = None;
                r?;
            }
        }
        Ok(())
    }

    /// Accumulate `key`'s turn-roll skills for its `OnRolledAll` gimmicks. Each records
    /// the rolled skill in a per-effect bitmask (in `freq_counters`, so it persists
    /// across turns — not a `"turn:"` guard); when a gimmick has seen EVERY required
    /// skill, it fires and its accumulator resets ("each time you roll Power, Agility,
    /// and Technique for your turn rolls" — General Lee Wong V2).
    fn run_on_rolled_all(&mut self, key: &str) -> Eng<()> {
        let opp = self.state.opponent_of(key);
        let effects = self.standing_effects(key);
        for eff in &effects {
            let Trigger::OnRolledAll { skills, who } = &eff.trigger else {
                continue;
            };
            let ctx_key = if *who == Who::SelfSide {
                key
            } else {
                opp.as_str()
            };
            let ctx = self.roll_ctx.get(ctx_key).cloned().unwrap_or_default();
            let Some(rolled) = ctx.skill else {
                continue;
            };
            if !skills.contains(&rolled) {
                continue;
            }
            let fc = &mut self.state.players.get_mut(key).unwrap().freq_counters;
            *fc.entry(rolled_set_key(eff)).or_insert(0) |= skill_bit(rolled);
            let want: i64 = skills.iter().map(|&s| skill_bit(s)).fold(0, |a, b| a | b);
            let have = *self.state.players[key]
                .freq_counters
                .get(&rolled_set_key(eff))
                .unwrap_or(&0);
            if (have & want) == want {
                self.state
                    .players
                    .get_mut(key)
                    .unwrap()
                    .freq_counters
                    .remove(&rolled_set_key(eff)); // reset the set — "each time"
                self.fire_if_ready(eff, key, Some(&ctx))?;
            }
            if self.ended() {
                return Ok(());
            }
        }
        Ok(())
    }

    /// Fire `OnFinishRoll` gimmicks for `finisher`'s Finish roll (`skill`/`value`).
    /// A separate trigger from the turn-roll `OnRoll`, so no existing gimmick fires on
    /// a Finish roll. BOTH players are scanned: `who=SELF` fires for the finisher, and
    /// an `OnFinishRoll{who=OPP}` fires for the NON-finisher ("when your opponent rolls
    /// … for their Finish"), matching `OnRoll`'s `who` convention. The Finish roll does
    /// not populate `self.roll_ctx`, so a local context carries the skill/value.
    fn run_on_finish_roll(&mut self, finisher: &str, skill: Skill, value: i64) -> Eng<()> {
        let ctx = RollContext {
            skill: Some(skill),
            value: Some(value),
            gap: None,
            opp_skill: None,
        };
        for owner in ["A", "B"] {
            let effects = self.standing_effects(owner);
            for eff in &effects {
                let Trigger::OnFinishRoll { skill: want, who } = &eff.trigger else {
                    continue;
                };
                if self.target(*who, owner) != finisher {
                    continue;
                }
                if want.is_none() || *want == Some(skill) {
                    self.fire_if_ready(eff, owner, Some(&ctx))?;
                }
                if self.ended() {
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// Resolution order for gimmicks that trigger DURING a turn roll.
    ///
    /// srgpc.net: "If two gimmicks would both trigger during a turn roll, the player
    /// with the higher turn roll must resolve their effect first." Evaluated against
    /// the roll values as they stand entering each stage. On an exact tie the order is
    /// undefined by the rules, so the stable A-then-B order is kept (a tie bumps, and
    /// no gimmick ordering is decided by it).
    fn roll_order(va: i64, vb: i64) -> [&'static str; 2] {
        if vb > va {
            ["B", "A"]
        } else {
            ["A", "B"]
        }
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
        for owner in Self::roll_order(va, vb) {
            if owner == "A" {
                va = self.offer_roll_boost("A", sa, va, false)?;
            } else {
                vb = self.offer_roll_boost("B", sb, vb, false)?;
            }
        }
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
            // Tie-only path (va == vb), so `roll_order`'s documented tie fallback
            // (A then B) already applies; kept explicit for clarity.
            va = self.offer_roll_boost("A", sa, va, true)?;
            vb = self.offer_roll_boost("B", sb, vb, true)?;
            if va != vb {
                break;
            }
            // Pretty Paul "Let It Rip!": its owner MAY replace this bump with drawing
            // + re-rolling both (no bump, so no OnBump gimmick and `bumps` unchanged).
            if let Some((nsa, nva, nsb, nvb, nb)) = self.try_bump_replacement(bumps)? {
                sa = nsa;
                va = nva;
                sb = nsb;
                vb = nvb;
                bumps = nb;
                continue;
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
        self.state.last_turn_bumped = bumps > 0; // read by the NEXT turn's tie loop (Mack-a-Tack)
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
    /// A player's bump card: normally draw 1, but if their OPPONENT declares
    /// `BumpDrawReplace` (Mack-a-Tack), discard 1 from hand INSTEAD ("when you bump,
    /// your opponent discards 1 card instead of drawing").
    fn bump_draw(&mut self, key: &str) -> Eng<()> {
        let opp = self.state.opponent_of(key);
        if self.declares_static(&opp, |a| matches!(a, Action::BumpDrawReplace)) {
            self.discard_from_hand(key, key, 1, false, None)?;
            Ok(())
        } else {
            self.draw(key, 1, DeckEnd::Top)
        }
    }

    fn do_bump(&mut self, bumps: i64) -> Eng<(Skill, i64, Skill, i64, i64)> {
        self.bump_draw("A")?;
        self.bump_draw("B")?;
        let bumps = bumps + 1;
        self.run_on_bump()?; // bump-punish gimmicks (Mastermind: opp next roll -2)
        let (sa, va, sb, vb) = self.reroll_both()?;
        Ok((sa, va, sb, vb, bumps))
    }

    /// "Each player re-rolls their turn roll": fresh dice for both sides, then the
    /// same switch / in-roll-mod / re-roll offers a first roll gets. The re-roll tail
    /// shared by a bump (`do_bump`) and Pretty Paul's bump replacement
    /// (`try_bump_replacement`) so both stay byte-identical to a first roll-off.
    fn reroll_both(&mut self) -> Eng<(Skill, i64, Skill, i64)> {
        let (sa, va) = self.roll_for("A", false);
        let (sb, vb) = self.roll_for("B", false);
        let (sa, va, sb, vb) = self.offer_switches(sa, va, sb, vb)?; // a bump re-roll is a turn roll too
        let (va, vb) = self.apply_in_roll_mods(sa, va, sb, vb); // debuff re-rolls too
        self.offer_rerolls(sa, va, sb, vb) // re-roll offered post-bump too
    }

    /// A player holding an unspent `BumpReplacement` charge (Pretty Paul Says "Let It
    /// Rip!") — `Some((key, draw))` with the number of cards its owner draws instead
    /// of taking the bump, else `None`.
    fn bump_replacement_owner(&self) -> Option<(String, i64)> {
        for key in ["A", "B"] {
            for eff in self.standing_effects(key) {
                for a in &eff.actions {
                    if let Action::BumpReplacement { uses, draw } = a {
                        let used = self.state.players[key]
                            .freq_counters
                            .get("match:bump_replace")
                            .copied()
                            .unwrap_or(0);
                        if used < *uses {
                            return Some((key.to_owned(), *draw));
                        }
                    }
                }
            }
        }
        None
    }

    /// Pretty Paul Says "Let It Rip!": at a tie that would force a bump, its owner MAY
    /// spend a per-match charge to replace the bump — draw `draw` cards and re-roll
    /// both turn rolls INSTEAD. Because the bump never happens, no `OnBump` gimmick
    /// fires (the point vs a sign-flipper) and `bumps` is left unchanged (the turn is
    /// not counted as bumped). `Some(fresh roll)` if the replacement was taken.
    #[allow(clippy::type_complexity)]
    fn try_bump_replacement(&mut self, bumps: i64) -> Eng<Option<(Skill, i64, Skill, i64, i64)>> {
        let Some((owner, draw)) = self.bump_replacement_owner() else {
            return Ok(None);
        };
        if !self.elect_bump_replacement(&owner)? {
            return Ok(None);
        }
        self.draw(&owner, draw.max(0) as usize, DeckEnd::Top)?;
        let (sa, va, sb, vb) = self.reroll_both()?;
        Ok(Some((sa, va, sb, vb, bumps)))
    }

    /// Offer `owner` its once-per-match bump replacement and spend a charge if taken.
    fn elect_bump_replacement(&mut self, owner: &str) -> Eng<bool> {
        let legal = vec![
            json!({"kind": "yes", "point": "bump_replace"}),
            json!({"kind": "no", "point": "bump_replace"}),
        ];
        if self.decide("bump_replace", owner, legal)?["kind"] != "yes" {
            return Ok(false);
        }
        let fc = &mut self.state.players.get_mut(owner).unwrap().freq_counters;
        let cur = fc.get("match:bump_replace").copied().unwrap_or(0);
        fc.insert("match:bump_replace".to_owned(), cur + 1);
        Ok(true)
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
        // chosen player's) is re-rolled in place. Higher roll resolves first (srgpc).
        for owner in Self::roll_order(va, vb) {
            let (own_ctx, opp_ctx) = if owner == "A" {
                (&ctx_a, &ctx_b)
            } else {
                (&ctx_b, &ctx_a)
            };
            if let Some(target) = self.offer_reroll(owner, own_ctx, opp_ctx)? {
                let (ns, mut nv) = self.roll_for(&target, false);
                // OnReroll effects fire on the just-re-rolled die: a roll-modifier body
                // ("their roll is -1", "your roll is +2") adjusts the value here — the
                // re-roll's `roll_for` skips pending mods, so a `ModifyRoll{This}` would
                // otherwise be lost — while draw / shuffle-self bodies resolve in place.
                nv += self.run_on_reroll(&target)?;
                // Stamp the re-rolled side's turn so a `RerolledTurnRoll` finish rider
                // ("… or you re-rolled your turn roll" — King Brian Cage) resolves.
                let turn = self.state.turn_no;
                self.state
                    .players
                    .get_mut(&target)
                    .unwrap()
                    .flags
                    .insert("rerolled_turn".to_owned(), json!(turn));
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

    /// Fire OnReroll effects after `target`'s turn die was re-rolled (schema v104).
    /// Scans both sides — `target`'s own `OnReroll{SelfSide}` and the opponent's
    /// `OnReroll{Opp}` ("when your opponent re-rolls") — plus their WHILE_IN_DISCARD
    /// self-triggers (with `self_card` bound so a "shuffle this card into your deck"
    /// body resurrects the right card). Returns the summed roll-modifier delta, applied
    /// to the re-rolled value by the caller; all other actions resolve in place. A no-op
    /// (delta 0) when no side has an OnReroll effect — the corpus stays byte-identical.
    fn run_on_reroll(&mut self, target: &str) -> Eng<i64> {
        let opp = self.state.opponent_of(target);
        let mut delta = 0;
        for (owner, want) in [(target.to_owned(), Who::SelfSide), (opp, Who::Opp)] {
            for eff in self.standing_effects(&owner) {
                if matches!(eff.trigger, Trigger::OnReroll { who } if who == want) {
                    delta += self.fire_on_reroll(&eff, &owner)?;
                }
            }
            for (uuid, eff) in self.discard_self_triggers(&owner) {
                if matches!(eff.trigger, Trigger::OnReroll { who } if who == want) {
                    self.self_card = Some(uuid);
                    let r = self.fire_on_reroll(&eff, &owner);
                    self.self_card = None;
                    delta += r?;
                }
            }
        }
        Ok(delta)
    }

    /// Fire one OnReroll effect (frequency + condition + optional gated like
    /// [`fire_if_ready`](Self::fire_if_ready)), returning the summed `ModifyRoll` delta
    /// — applied to the re-rolled value by the caller, since a re-roll's `roll_for`
    /// skips the pending-mod path a normal `ModifyRoll{This}` uses. Every non-`ModifyRoll`
    /// action (draw, shuffle-self) resolves normally.
    fn fire_on_reroll(&mut self, eff: &Effect, owner: &str) -> Eng<i64> {
        if !(self.may_fire(eff, owner)
            && conditions::holds(&eff.condition, &self.state, owner, None))
        {
            return Ok(0);
        }
        if eff.optional && !self.take_optional(eff, owner)? {
            return Ok(0);
        }
        self.mark_fired(eff, owner);
        let mut delta = 0;
        for a in &eff.actions {
            if let Action::ModifyRoll {
                delta: d,
                per,
                per_who,
                per_zone,
                ..
            } = a
            {
                let mut dd = *d;
                if let Some(per) = per {
                    let counter = self.target(*per_who, owner);
                    dd *= self.state.count_in_zone(per, *per_zone, &counter);
                }
                delta += dd;
            } else {
                self.apply_action(a, owner, &eff.raw_clause)?;
            }
        }
        Ok(delta)
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
        for owner in Self::roll_order(va, vb) {
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
            let Some((who, choose, cost)) = eff.actions.iter().find_map(|a| match a {
                Action::Reroll {
                    who,
                    choose,
                    when: RollWhen::This,
                    cost,
                    finish: false, // finish-scoped re-rolls are offered in the finish sequence
                    breakout: false, // breakout-scoped re-rolls are offered in the breakout loop
                    ..
                } => Some((*who, *choose, cost.clone())),
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
            // A costed re-roll is offered only while the owner can pay it (an in-play
            // card to shuffle, or enough hand cards to bury/discard). Unaffordable ⇒ not
            // offered, and the frequency charge is left unspent.
            if let Some(c) = &cost {
                if !self.can_pay_reroll(owner, c) {
                    continue;
                }
            }
            if eff.optional && !self.take_optional(eff, owner)? {
                continue; // declined "you may" — charge left for a later roll
            }
            self.mark_fired(eff, owner);
            if let Some(c) = &cost {
                self.pay_reroll(owner, c)?;
            }
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

    /// Whether `owner` has any in-play card matching `filter` (the re-roll cost check).
    fn has_in_play(&self, owner: &str, filter: &CardFilter) -> bool {
        self.state.players[owner]
            .in_play
            .iter()
            .any(|c| conditions::card_matches(c, filter))
    }

    /// Whether `owner` can afford re-roll cost `c` — checked BEFORE the re-roll is
    /// offered, so an unaffordable cost leaves the frequency charge unspent (schema
    /// v103). ShuffleInPlay needs a matching in-play card; the hand costs need `count`
    /// (matching) cards in hand.
    fn can_pay_reroll(&self, owner: &str, c: &RerollCost) -> bool {
        match c.kind {
            RerollCostKind::ShuffleInPlay => c
                .filter
                .as_ref()
                .is_some_and(|f| self.has_in_play(owner, f)),
            RerollCostKind::BuryFromHand | RerollCostKind::DiscardFromHand => {
                let need = c.count.unwrap_or(0).max(0) as usize;
                let hand = &self.state.players[owner].hand;
                let have = match &c.filter {
                    Some(f) => hand
                        .iter()
                        .filter(|c| conditions::card_matches(c, f))
                        .count(),
                    None => hand.len(),
                };
                have >= need
            }
        }
    }

    /// Pay re-roll cost `c` (affordability already confirmed by `can_pay_reroll`).
    /// ShuffleInPlay shuffles one in-play card away; the hand costs shed `count` cards
    /// (the owner picks — never random — mirroring the "you may bury/discard" choice).
    fn pay_reroll(&mut self, owner: &str, c: &RerollCost) -> Eng<()> {
        match c.kind {
            RerollCostKind::ShuffleInPlay => {
                if let Some(f) = &c.filter {
                    self.pay_reroll_cost(owner, f)?;
                }
            }
            RerollCostKind::BuryFromHand => {
                let n = c.count.unwrap_or(0).max(0) as usize;
                let filter = c.filter.clone().unwrap_or_default();
                self.bury_from_hand(owner, owner, n, false, &filter)?;
            }
            RerollCostKind::DiscardFromHand => {
                let n = c.count.unwrap_or(0).max(0) as usize;
                self.discard_from_hand(owner, owner, n, false, c.filter.as_ref())?;
            }
        }
        Ok(())
    }

    /// Pay a costed re-roll: shuffle the first in-play card matching `filter` into
    /// `owner`'s deck (Mr. Hyde's "Potion"). Fires `OnShuffle` like any deck shuffle.
    fn pay_reroll_cost(&mut self, owner: &str, filter: &CardFilter) -> Eng<()> {
        let player = self.state.players.get_mut(owner).unwrap();
        let Some(pos) = player
            .in_play
            .iter()
            .position(|c| conditions::card_matches(c, filter))
        else {
            return Ok(()); // affordability was checked before offering
        };
        let card = player.in_play.remove(pos);
        let uuid = card.db_uuid.clone();
        player.deck.push(card);
        let t = self.state.turn_no;
        self.log(Event::Bury(CardMovement {
            t,
            player: owner.to_owned(),
            cards: vec![uuid],
            source: Some("in_play".to_owned()),
            hidden: false,
        }));
        self.shuffle_deck(owner)
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
            // A `RollBoost` action inside the effect (e.g. a Choice branch, El Super
            // Hombre V3) reports its in-roll delta through `pending_roll_boost`; the
            // trigger's own fixed `delta` (Rey Zerblade) is added on top.
            self.pending_roll_boost = 0;
            self.apply_actions(eff, key)?; // pay the cost / run the chosen branch
            let applied = *delta + self.pending_roll_boost;
            value += applied;
            if applied != 0 {
                self.log_effect(
                    key,
                    "RollBoost",
                    Some(key),
                    json!({"skill": skill.name(), "delta": applied}),
                );
            }
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
        // The base turn roll folds the skill's stat plus any standing "during turn
        // rolls" bonus (TurnRollBonus) — phase-scoped, unlike the general `stat()`.
        let base = self.stat(key, skill) + self.turn_roll_bonus(key, skill);
        let flat = if use_pending {
            self.state.players[key].pending_roll_mods.this_turn
        } else {
            0
        };
        // Skill-keyed pending mod: "the next time you roll <S>, it is +N" fires on the
        // FIRST roll (initial or bump) that comes up its skill, then is consumed. Read
        // on every roll (independent of `use_pending`, which only gates the flat mod).
        let keyed = self.consume_skill_roll_mod(key, skill);
        // One-turn skill-gated pending bonus ("+N to <S>, <S> during your next turn
        // roll"): applies to the INITIAL roll if it comes up a listed skill (read-only
        // here; `consume_pending` drains the whole queue after the roll-off, so a
        // non-match fizzles). Gated by `use_pending` like the flat mod, so bump re-rolls
        // don't re-read it.
        let set = if use_pending {
            self.next_roll_skill_bonus(key, skill)
        } else {
            0
        };
        // Multi-turn bonus ("your next N turn rolls are +N"): skill-agnostic, applied on
        // the initial roll of each of the next N roll-offs (`consume_pending` decrements
        // the counter). Gated by `use_pending` like the other pending mods.
        let multi = if use_pending {
            self.multi_turn_roll_bonus(key)
        } else {
            0
        };
        let delta = flat + keyed + set + multi;
        let mut mods = Vec::new();
        if flat != 0 {
            mods.push(RollMod {
                src: "pending".to_owned(),
                delta: flat,
            });
        }
        if keyed != 0 {
            mods.push(RollMod {
                src: "pending_skill".to_owned(),
                delta: keyed,
            });
        }
        if set != 0 {
            mods.push(RollMod {
                src: "pending_skill_set".to_owned(),
                delta: set,
            });
        }
        if multi != 0 {
            mods.push(RollMod {
                src: "multi_turn".to_owned(),
                delta: multi,
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
    /// re-rolls run with `use_pending=false`, so they never re-read it). Also drains the
    /// one-turn skill-gated queue ([`SkillSetRollMod`]) — it applied (or fizzled) on the
    /// initial roll-off, and "your next turn roll" closes after that one roll.
    fn consume_pending(&mut self) {
        for player in self.state.players.values_mut() {
            player.pending_roll_mods.this_turn = 0;
            player.pending_next_roll_skill_mods.clear();
            // A multi-turn bonus spent one of its N rolls on this roll-off; drop it when
            // exhausted.
            for m in &mut player.multi_turn_roll_mods {
                m.remaining -= 1;
            }
            player.multi_turn_roll_mods.retain(|m| m.remaining > 0);
        }
    }

    /// Remove and sum every pending skill-keyed roll mod for `key` matching `skill`
    /// ("the next time you roll <S>, it is +N"). All entries for that skill refer to
    /// the same next occurrence, so they fire together and are consumed at once; 0 if
    /// none match (the queue is untouched for other skills).
    fn consume_skill_roll_mod(&mut self, key: &str, skill: Skill) -> i64 {
        let mods = &mut self
            .state
            .players
            .get_mut(key)
            .unwrap()
            .pending_skill_roll_mods;
        let mut delta = 0;
        mods.retain(|m| {
            if m.skill == skill {
                delta += m.delta;
                false
            } else {
                true
            }
        });
        delta
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
            if self.is_count_out_immune(key) {
                // "No Count Outs" (a Crowd Meter match type): emptying deck+hand no
                // longer ends the match — there is simply nothing to draw. Play
                // continues (the win must come from a Finish instead).
                self.log_effect(key, "CountOutVoided", None, Value::Null);
                return Ok(true);
            }
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
/// The `freq_counters` key holding an `OnRolledAll` effect's rolled-skill bitmask.
/// A distinct `"rollset:"` namespace, so the per-turn / per-match frequency sweeps
/// leave it alone — the set accumulates across turns until the gimmick fires.
fn rolled_set_key(eff: &Effect) -> String {
    format!("rollset:{}", eff.raw_clause)
}

/// Whether `eff` is a card's own per-flip self-trigger — `OnFlip{on_self: true}` ("If
/// this card is flipped, …"), dispatched per just-flipped card by `run_self_flips`.
/// Standing `OnFlip` triggers (`on_self: false`, e.g. Evee's "flip exactly 3", or "when
/// you flip any number of cards, …") fire via `run_on_flip` instead.
fn is_on_flip_self(eff: &Effect) -> bool {
    matches!(
        eff.trigger,
        Trigger::OnFlip {
            who: Who::SelfSide,
            on_self: true,
            ..
        }
    )
}

/// A single-bit mask for `skill` (its index in [`Skill::ALL`]).
fn skill_bit(skill: Skill) -> i64 {
    1 << Skill::ALL.iter().position(|&s| s == skill).unwrap()
}

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
        Trigger::OnFinishRoll { .. } => "OnFinishRoll",
        Trigger::OnRolledAll { .. } => "OnRolledAll",
        Trigger::InRoll { .. } => "InRoll",
        Trigger::OnRollBoost { .. } => "OnRollBoost",
        Trigger::OnWinTurn => "OnWinTurn",
        Trigger::OnLoseTurn { .. } => "OnLoseTurn",
        Trigger::OnStop { .. } => "OnStop",
        Trigger::OnHit { .. } => "OnHit",
        Trigger::OnBump => "OnBump",
        Trigger::OnBury { .. } => "OnBury",
        Trigger::StartOfTurn => "StartOfTurn",
        Trigger::DuringOpponentTurn => "DuringOpponentTurn",
        Trigger::StartOfMatch => "StartOfMatch",
        Trigger::OnBreakout { .. } => "OnBreakout",
        Trigger::OnBreakoutRoll { .. } => "OnBreakoutRoll",
        Trigger::OnReroll { .. } => "OnReroll",
        Trigger::OnShuffle { .. } => "OnShuffle",
        Trigger::OnFlip { .. } => "OnFlip",
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

/// A `reveal` decision option — one hand card the revealing player could expose.
fn reveal_option(card: &Card) -> Value {
    json!({
        "kind": "reveal",
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

/// Whether an `Unstoppable` action shields the attack against `stopper`: the
/// stopper must satisfy every set gate (play order, name, skill-requirement).
/// Non-`Unstoppable` actions never match.
fn unstoppable_gate(a: &Action, stopper: &Card) -> bool {
    let Action::Unstoppable {
        by_order,
        by_name,
        by_skillreq,
    } = a
    else {
        return false;
    };
    let order_ok = by_order.is_none() || *by_order == Some(stopper.play_order);
    let name_ok = by_name.as_ref().is_none_or(|n| *n == stopper.name);
    let skillreq_ok = !*by_skillreq
        || stopper
            .tags
            .iter()
            .any(|t| t == crate::cards::SKILL_REQUIREMENT_TAG);
    order_ok && name_ok && skillreq_ok
}

/// Whether `card` is a legal play given the player's own persistent board (the
/// order-only chain, DESIGN.md §6): a Lead always; a Follow Up needs a Lead; a
/// Finish needs a Follow Up. Type is irrelevant to the chain.
fn playable(board: &[Card], card: &Card) -> bool {
    playable_as(card.play_order, board)
}

/// Whether a card in play-order slot `order` is legal against `board`: a Lead is
/// always playable, a Follow Up needs a Lead in play, a Finish needs a Follow Up.
/// Shared by `playable` (a card's printed order) and `also_lead_now` (an `AlsoLead`
/// grant's alternate order).
fn playable_as(order: PlayOrder, board: &[Card]) -> bool {
    match order {
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
        Action::MillDeck { .. } => "MillDeck",
        Action::RollDraw { .. } => "RollDraw",
        Action::NextRollSkillBonus { .. } => "NextRollSkillBonus",
        Action::MultiTurnRollBonus { .. } => "MultiTurnRollBonus",
        Action::Discard { .. } => "Discard",
        Action::Search { .. } => "Search",
        Action::ShuffleDeck { .. } => "ShuffleDeck",
        Action::ShuffleIntoDeck { .. } => "ShuffleIntoDeck",
        Action::AddFromDiscard { .. } => "AddFromDiscard",
        Action::AddFlippedToHand { .. } => "AddFlippedToHand",
        Action::SwapHandDiscard => "SwapHandDiscard",
        Action::GrantSwapNextTurn { .. } => "GrantSwapNextTurn",
        Action::RecurToDeckTop { .. } => "RecurToDeckTop",
        Action::CountsAsInPlay { .. } => "CountsAsInPlay",
        Action::RemoveFromPlay { .. } => "RemoveFromPlay",
        Action::DiscardInPlayMatch => "DiscardInPlayMatch",
        Action::CoupledDiscard { .. } => "CoupledDiscard",
        Action::ReturnToHand { .. } => "ReturnToHand",
        Action::RevealAndDiscard { .. } => "RevealAndDiscard",
        Action::RevealForDraw { .. } => "RevealForDraw",
        Action::Peek { .. } => "Peek",
        Action::Reveal { .. } => "Reveal",
        Action::ForceRevealPlay { .. } => "ForceRevealPlay",
        Action::CopyEntrance { .. } => "CopyEntrance",
        Action::Scry { .. } => "Scry",
        Action::RevealRoute { .. } => "RevealRoute",
        Action::RevealThen { .. } => "RevealThen",
        Action::ShuffleHandDraw { .. } => "ShuffleHandDraw",
        Action::ModifyRoll { .. } => "ModifyRoll",
        Action::BuffSkill { .. } => "BuffSkill",
        Action::MaxHandSize { .. } => "MaxHandSize",
        Action::MinHandSize { .. } => "MinHandSize",
        Action::RollBoost { .. } => "RollBoost",
        Action::MirrorOpponentIncrease => "MirrorOpponentIncrease",
        Action::StopCountsOrderAs { .. } => "StopCountsOrderAs",
        Action::SuppressStop { .. } => "SuppressStop",
        Action::BumpDrawReplace => "BumpDrawReplace",
        Action::BumpReplacement { .. } => "BumpReplacement",
        Action::ScaleEntranceNumbers { .. } => "ScaleEntranceNumbers",
        Action::AddText { .. } => "AddText",
        Action::AbsorbGimmick { .. } => "AbsorbGimmick",
        Action::AddTextToNext { .. } => "AddTextToNext",
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
        Action::Unblank { .. } => "Unblank",
        Action::CopyText { .. } => "CopyText",
        Action::BlankStoppedText => "BlankStoppedText",
        Action::BuryThisCard => "BuryThisCard",
        Action::AddSelfToHand => "AddSelfToHand",
        Action::ShuffleSelfIntoDeck => "ShuffleSelfIntoDeck",
        Action::PlaySelf => "PlaySelf",
        Action::ChooseName { .. } => "ChooseName",
        Action::LoseBy { .. } => "LoseBy",
        Action::DisqualificationRule { .. } => "DisqualificationRule",
        Action::CountOutRule { .. } => "CountOutRule",
        Action::SwapCrowdMeter { .. } => "SwapCrowdMeter",
        Action::ConsideredCompare { .. } => "ConsideredCompare",
        Action::SuppressOpponentDraw => "SuppressOpponentDraw",
        Action::SuppressSelfHandLoss => "SuppressSelfHandLoss",
        Action::CrowdMeter { .. } => "CrowdMeter",
        Action::PlayExtraCard { .. } => "PlayExtraCard",
        Action::SetFinishRoll { .. } => "SetFinishRoll",
        Action::FinishBonus { .. } => "FinishBonus",
        Action::FinishRollBonus { .. } => "FinishRollBonus",
        Action::TurnRollBonus { .. } => "TurnRollBonus",
        Action::BreakoutModifier { .. } => "BreakoutModifier",
        Action::BreakoutAttempts { .. } => "BreakoutAttempts",
        Action::LowestRollWins => "LowestRollWins",
        Action::FlipGimmickSigns { .. } => "FlipGimmickSigns",
        Action::Unstoppable { .. } => "Unstoppable",
        Action::AlsoLead { .. } => "AlsoLead",
        Action::DoubleFinishIfBumped => "DoubleFinishIfBumped",
        Action::DoubleFinishIf { .. } => "DoubleFinishIf",
        Action::RequireStops { .. } => "RequireStops",
        Action::AlsoAtkType { .. } => "AlsoAtkType",
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
mod tests;
