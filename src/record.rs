//! Portable **match records** — the public interchange format for replay
//! (DESIGN.md §8.1, pinned in `schemas/v1/match_record.schema.json`).
//!
//! The game log ([`crate::gamelog`]) is the engine's own stream: loss-less, seeded,
//! and byte-for-byte replayable — but engine-shaped and *not* safe to publish (its
//! `decision` events carry the deciding player's whole hand as `legal` options).
//! A **record** is the shape a consumer stores, shares, and replays:
//!
//! - **frames** — an ordered sequence of [`Frame`]s, each an *observable* (spectator)
//!   public state plus the [`Action`] that produced it. This is the playback layer,
//!   and it is the only layer an imported real-life game can supply.
//! - **replay** — for engine-run games only, the compact re-simulation seed
//!   (`seed + decks + seats + decisions`, a
//!   [`SessionSnapshot`](crate::session::SessionSnapshot)). Absent for imports.
//!
//! Two record [`kinds`](RecordKind) share one schema:
//!
//! | | `full` | `observer` |
//! |---|---|---|
//! | produced by | this engine | a human/importer transcribing a real match |
//! | hidden zones | never in the frames (counts only) | never |
//! | `replay` seed | present → re-simulatable | absent → playback only |
//! | `engine` stamp | present | absent |
//!
//! Both replay identically in a viewer, because a viewer only ever walks `frames`.
//! Nothing in a record — either kind — reveals a hidden zone: hands and decks are
//! counts, and a movement the [game log marks `hidden`](crate::gamelog::CardMovement)
//! projects to a count with no card ids. That is what makes a record publishable.

use crate::cards::Card;
use crate::gamelog::{BreakoutRoll, CardMovement, Event, RollMod};
use crate::session::SessionSnapshot;
use crate::state::{GameState, PlayerState};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// The record schema version this module reads and writes. Mirrors the `version`
/// field of `schemas/v1/match_record.schema.json`; bump both on any shape change.
pub const RECORD_SCHEMA_VERSION: i64 = 1;

/// The seat keys a match always has.
const SEATS: [&str; 2] = ["A", "B"];

/// Result reasons the engine itself produces; anything else in an imported record
/// is legal but flagged as a warning.
const KNOWN_REASONS: [&str; 5] = [
    "finish",
    "count_out",
    "disqualification",
    "pinfall",
    "turn_cap",
];

// ---------------------------------------------------------------------------
// Card references
// ---------------------------------------------------------------------------

/// A card as named in a record: its database identity plus optional display hints.
///
/// `card` is the canonical `db_uuid` (join key against the card DB). An importer
/// that cannot identify a card may leave it empty — the record stays valid and the
/// validator reports it as a warning. `name`/`number` are convenience copies so a
/// viewer can render a frame with no card-DB round trip; the engine always fills them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardRef {
    /// `db_uuid`, or `""` for a card the importer could not identify.
    pub card: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub number: Option<i64>,
}

impl CardRef {
    /// A bare reference carrying only the database uuid.
    pub fn uuid(uuid: &str) -> Self {
        Self {
            card: uuid.to_owned(),
            name: None,
            number: None,
        }
    }

    /// A full reference to a known main-deck card.
    pub fn from_card(card: &Card) -> Self {
        Self {
            card: card.db_uuid.clone(),
            name: Some(card.name.clone()),
            number: Some(card.number),
        }
    }
}

// ---------------------------------------------------------------------------
// Actions — the spectator-visible projection of a game-log event
// ---------------------------------------------------------------------------

/// What happened to produce a [`Frame`]: the public projection of one game-log
/// [`Event`], tag-dispatched by `type` under the same names the log uses.
///
/// Log events an observer could *not* see are dropped rather than redacted:
/// `decision` (its `legal` list enumerates the deciding player's hand) and
/// `unsupported` (an engine-coverage diagnostic, not a game event). The one
/// exception is a passed turn: a `turn_action` decision whose choice was `pass`
/// projects to [`Action::Pass`], which carries the seat and nothing else.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    /// The opening frame: the position before anything has happened.
    Start,
    /// A turn roll (or re-roll): the die, its base skill value, and the modifiers.
    Roll {
        player: String,
        skill: String,
        base: i64,
        value: i64,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        mods: Vec<RollMod>,
    },
    /// A card was declared. It reaches `in_play` only if it survives the stop
    /// window, so the board in *this* frame does not yet include it.
    Play {
        player: String,
        card: CardRef,
        order: String,
        atk_type: String,
    },
    /// `player` stopped the card `stopped` with `card`.
    Stop {
        player: String,
        card: CardRef,
        stopped: CardRef,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        reason: String,
    },
    /// The active player passed instead of playing a card. Seat only — the pass
    /// itself is public; the bury it recycles (if any) arrives as its own action.
    Pass { player: String },
    /// The roll-off resolved: `winner` takes the turn (after `tie_bumps` bumps).
    TurnResult { winner: String, tie_bumps: i64 },
    /// Cards drawn — deck→hand is private on both ends, so this is a count only.
    Draw { player: String, count: i64 },
    /// Cards sent to a discard pile (public unless the move was hidden end-to-end).
    Discard {
        player: String,
        count: i64,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        cards: Vec<CardRef>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<String>,
    },
    /// Cards placed back into a deck (`from` names the origin zone / end).
    Bury {
        player: String,
        count: i64,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        cards: Vec<CardRef>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<String>,
    },
    /// Cards pulled by a search/tutor effect.
    Search {
        player: String,
        count: i64,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        cards: Vec<CardRef>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<String>,
    },
    /// A finish attempt and the number it produced.
    FinishAttempt {
        player: String,
        finish: CardRef,
        value: i64,
        crowd_meter: i64,
        auto_success: bool,
    },
    /// The defender's breakout attempt.
    Breakout {
        defender: String,
        broke_out: bool,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        rolls: Vec<BreakoutRoll>,
    },
    /// The crowd meter moved.
    CrowdMeter { delta: i64, value: i64 },
    /// A named card effect resolved. The engine's `detail` payload is *not* carried
    /// (it is bookkeeping and can name hidden cards); the frame's zones show the
    /// consequence.
    Effect {
        src: String,
        action: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<String>,
    },
    /// Free-text colour for something this vocabulary does not model. Never emitted
    /// by the engine — it exists so an importer never has to distort a real match.
    Note { text: String },
    /// The match ended.
    Result {
        winner: String,
        reason: String,
        turns: i64,
    },
}

// ---------------------------------------------------------------------------
// Frames
// ---------------------------------------------------------------------------

/// One seat's public position: the two open zones plus the sizes of the closed ones.
///
/// The optional fields are what a real-life spectator may not have written down;
/// the engine always fills them. Hand and deck *contents* are absent by
/// construction — there is no field that could carry them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayerFrame {
    #[serde(default)]
    pub in_play: Vec<CardRef>,
    #[serde(default)]
    pub discard: Vec<CardRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hand_size: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deck_size: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gimmick_blanked: Option<bool>,
}

/// One step of a replay: the public state as of an [`Action`], with `seq` its
/// 0-based ordinal in the record.
///
/// State is captured *at the moment the action was logged*, not after everything it
/// triggers has settled — a played card, for instance, is still resolving through
/// the stop window and lands on the board in a later frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Frame {
    pub seq: i64,
    pub turn_no: i64,
    /// Seat whose turn it is.
    pub active: String,
    pub crowd_meter: i64,
    pub action: Action,
    pub players: BTreeMap<String, PlayerFrame>,
}

// ---------------------------------------------------------------------------
// The record envelope
// ---------------------------------------------------------------------------

/// Whether a record was produced by this engine or transcribed by an observer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecordKind {
    /// Engine-run: carries a `replay` seed and re-simulates exactly.
    Full,
    /// Transcribed from a real-life / other-platform match: frames only, no seed,
    /// not re-simulatable.
    Observer,
}

/// Descriptive metadata — none of it feeds the engine.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordMeta {
    /// ISO-8601 timestamp, or empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub created: String,
    /// Where the match happened ("get-diced.com Run It Back", "locals 2026-07-19").
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source: String,
    /// Match type / stipulation, if any ("standard", "ring_of_fire", …).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub match_type: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub notes: String,
}

/// One side's identity in a record.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Participant {
    /// The human/AI behind the seat ("Brandon", "heuristic"), if known.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub player: String,
    pub competitor: CardRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entrance: Option<CardRef>,
    /// The 30-card decklist, when known. Absent for most imports.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deck: Vec<CardRef>,
}

/// How the match ended (the `result` game-log event / [`GameResult`](crate::engine::GameResult)).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchResult {
    /// `"A"`, `"B"`, or `"draw"`.
    pub winner: String,
    pub reason: String,
    pub turns: i64,
}

/// A complete, portable match record. See the module docs for the two kinds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MatchRecord {
    pub schema_version: i64,
    pub kind: RecordKind,
    /// [`version_info`](crate::version_info) of the engine that produced this record
    /// — the replay-fidelity stamp. Absent on observer records (no engine ran).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<Value>,
    #[serde(default)]
    pub meta: RecordMeta,
    pub players: BTreeMap<String, Participant>,
    pub frames: Vec<Frame>,
    pub result: MatchResult,
    /// The compact re-simulation seed (`full` records only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay: Option<SessionSnapshot>,
}

/// The outcome of [`MatchRecord::validate`]: `errors` reject the record, `warnings`
/// are things a consumer should know (missing counts, unidentified cards).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Validation {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

impl Validation {
    /// True iff there are no errors (warnings are fine).
    pub fn is_valid(&self) -> bool {
        self.errors.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Building frames from engine state
// ---------------------------------------------------------------------------

/// `db_uuid` → display reference for every card in the match, built once when
/// recording starts (with all 60 cards still in their decks).
///
/// Zone scans alone cannot name a card *in transit*: a played card has left its
/// owner's hand and does not reach the board until it survives the stop window, so
/// the `play` action would otherwise carry a bare uuid.
#[derive(Debug, Clone, Default)]
pub struct CardNames(BTreeMap<String, CardRef>);

impl CardNames {
    /// Index every card currently in any zone of any seat.
    pub fn from_state(state: &GameState) -> Self {
        let mut index = BTreeMap::new();
        for player in state.players.values() {
            for card in zones(player) {
                index.insert(card.db_uuid.clone(), CardRef::from_card(card));
            }
        }
        Self(index)
    }

    fn get(&self, uuid: &str) -> Option<CardRef> {
        self.0.get(uuid).cloned()
    }
}

/// The opening frame: the position before the first event.
pub fn opening_frame(state: &GameState) -> Frame {
    build_frame(0, Action::Start, state)
}

/// Project one game-log event into a frame over `state` (the state as of that
/// event), or `None` for an event no observer may see (`decision`, `unsupported`).
pub fn frame_for(seq: i64, event: &Event, state: &GameState, names: &CardNames) -> Option<Frame> {
    let ctx = Ctx { state, names };
    project(event, &ctx).map(|action| build_frame(seq, action, state))
}

/// What the projection needs: the live state (zones, counts) and the card index.
struct Ctx<'a> {
    state: &'a GameState,
    names: &'a CardNames,
}

fn build_frame(seq: i64, action: Action, state: &GameState) -> Frame {
    Frame {
        seq,
        turn_no: state.turn_no,
        active: state.active.clone(),
        crowd_meter: state.crowd_meter,
        action,
        players: state
            .players
            .iter()
            .map(|(key, player)| (key.clone(), observe(key, player, state)))
            .collect(),
    }
}

fn observe(key: &str, player: &PlayerState, state: &GameState) -> PlayerFrame {
    PlayerFrame {
        in_play: refs(&player.in_play),
        discard: refs(&player.discard),
        hand_size: Some(player.hand.len() as i64),
        deck_size: Some(player.deck.len() as i64),
        gimmick_blanked: Some(state.is_gimmick_blanked(key)),
    }
}

fn refs(cards: &[Card]) -> Vec<CardRef> {
    cards.iter().map(CardRef::from_card).collect()
}

/// Attach display hints to a uuid: the match's card index first, then a scan of
/// `player`'s zones. Only ever called for cards the observer may already see.
fn resolve(ctx: &Ctx, player: &str, uuid: &str) -> CardRef {
    ctx.names
        .get(uuid)
        .or_else(|| {
            ctx.state
                .players
                .get(player)
                .and_then(|p| find_card(p, uuid))
                .map(CardRef::from_card)
        })
        .unwrap_or_else(|| CardRef::uuid(uuid))
}

fn zones(player: &PlayerState) -> impl Iterator<Item = &Card> {
    [&player.in_play, &player.discard, &player.hand, &player.deck]
        .into_iter()
        .flatten()
}

fn find_card<'a>(player: &'a PlayerState, uuid: &str) -> Option<&'a Card> {
    zones(player).find(|card| card.db_uuid == uuid)
}

/// A card movement's public face: card ids when the log says the move was visible,
/// a bare count when it says `hidden` (both endpoints private).
fn moved(mv: &CardMovement, ctx: &Ctx) -> (i64, Vec<CardRef>) {
    let count = mv.cards.len() as i64;
    if mv.hidden {
        return (count, Vec::new());
    }
    let cards = mv
        .cards
        .iter()
        .map(|uuid| resolve(ctx, &mv.player, uuid))
        .collect();
    (count, cards)
}

/// The event → action projection (see [`Action`]).
fn project(event: &Event, ctx: &Ctx) -> Option<Action> {
    match event {
        Event::Decision {
            player,
            point,
            chosen,
            ..
        } if point == "turn_action" && chosen["kind"] == "pass" => Some(Action::Pass {
            player: player.clone(),
        }),
        Event::Decision { .. } | Event::Unsupported { .. } => None,
        Event::Draw(mv) => Some(Action::Draw {
            player: mv.player.clone(),
            count: mv.cards.len() as i64,
        }),
        Event::Discard(mv) => Some(movement_action(mv, ctx, MovementKind::Discard)),
        Event::Bury(mv) => Some(movement_action(mv, ctx, MovementKind::Bury)),
        Event::Search(mv) => Some(movement_action(mv, ctx, MovementKind::Search)),
        _ => project_play(event, ctx),
    }
}

enum MovementKind {
    Discard,
    Bury,
    Search,
}

fn movement_action(mv: &CardMovement, ctx: &Ctx, kind: MovementKind) -> Action {
    let player = mv.player.clone();
    let (count, cards) = moved(mv, ctx);
    let from = mv.source.clone();
    match kind {
        MovementKind::Discard => Action::Discard {
            player,
            count,
            cards,
            from,
        },
        MovementKind::Bury => Action::Bury {
            player,
            count,
            cards,
            from,
        },
        MovementKind::Search => Action::Search {
            player,
            count,
            cards,
            from,
        },
    }
}

fn project_play(event: &Event, ctx: &Ctx) -> Option<Action> {
    match event {
        Event::Play {
            player,
            card,
            order,
            atk_type,
            ..
        } => Some(Action::Play {
            player: player.clone(),
            card: resolve(ctx, player, card),
            order: order.clone(),
            atk_type: atk_type.clone(),
        }),
        Event::Stop {
            player,
            card,
            stopped,
            reason,
            ..
        } => Some(Action::Stop {
            player: player.clone(),
            card: resolve(ctx, player, card),
            stopped: resolve(ctx, other_seat(player), stopped),
            reason: reason.clone(),
        }),
        _ => project_match(event, ctx),
    }
}

fn project_match(event: &Event, ctx: &Ctx) -> Option<Action> {
    match event {
        Event::Roll {
            player,
            skill,
            base,
            value,
            mods,
            ..
        } => Some(Action::Roll {
            player: player.clone(),
            skill: skill.clone(),
            base: *base,
            value: *value,
            mods: mods.clone(),
        }),
        Event::TurnResult {
            winner, tie_bumps, ..
        } => Some(Action::TurnResult {
            winner: winner.clone(),
            tie_bumps: *tie_bumps,
        }),
        Event::FinishAttempt {
            player,
            finish,
            value,
            crowd_meter,
            auto_success,
            ..
        } => Some(Action::FinishAttempt {
            player: player.clone(),
            finish: resolve(ctx, player, finish),
            value: *value,
            crowd_meter: *crowd_meter,
            auto_success: *auto_success,
        }),
        _ => project_tail(event),
    }
}

fn project_tail(event: &Event) -> Option<Action> {
    match event {
        Event::Breakout {
            defender,
            broke_out,
            rolls,
            ..
        } => Some(Action::Breakout {
            defender: defender.clone(),
            broke_out: *broke_out,
            rolls: rolls.clone(),
        }),
        Event::CrowdMeter { delta, value, .. } => Some(Action::CrowdMeter {
            delta: *delta,
            value: *value,
        }),
        Event::EffectApplied {
            src,
            action,
            target,
            ..
        } => Some(Action::Effect {
            src: src.clone(),
            action: action.clone(),
            target: target.clone(),
        }),
        Event::Result {
            winner,
            reason,
            turns,
            ..
        } => Some(Action::Result {
            winner: winner.clone(),
            reason: reason.clone(),
            turns: *turns,
        }),
        _ => None,
    }
}

fn other_seat(key: &str) -> &str {
    if key == "A" {
        "B"
    } else {
        "A"
    }
}

// ---------------------------------------------------------------------------
// Reading, writing, validating
// ---------------------------------------------------------------------------

impl MatchRecord {
    /// Parse a record from JSON text.
    pub fn parse(text: &str) -> crate::Result<Self> {
        Ok(serde_json::from_str(text)?)
    }

    /// Read a record from a JSON file.
    pub fn read(path: impl AsRef<std::path::Path>) -> crate::Result<Self> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|source| crate::SrgError::Io {
            path: path.display().to_string(),
            source,
        })?;
        Self::parse(&text)
    }

    /// Write the record as pretty JSON.
    pub fn write(&self, path: impl AsRef<std::path::Path>) -> std::io::Result<()> {
        let body = serde_json::to_string_pretty(self).expect("record serializes") + "\n";
        std::fs::write(path, body)
    }

    /// True iff this record can be re-simulated by the engine (a `full` record with
    /// its replay seed). Observer records are playback-only.
    pub fn is_replayable(&self) -> bool {
        self.kind == RecordKind::Full && self.replay.is_some()
    }

    /// Check the record's internal consistency. Structural only — it does not know
    /// the card DB (the CLI's `--cards` cross-check resolves uuids) and it does not
    /// re-derive the rules, so it cannot tell whether an imported match was *played*
    /// legally, only whether the archive is well-formed.
    pub fn validate(&self) -> Validation {
        let mut v = Validation::default();
        self.check_envelope(&mut v);
        self.check_participants(&mut v);
        self.check_frames(&mut v);
        self.check_result(&mut v);
        v
    }

    fn check_envelope(&self, v: &mut Validation) {
        if self.schema_version != RECORD_SCHEMA_VERSION {
            v.errors.push(format!(
                "schema_version {} != supported {RECORD_SCHEMA_VERSION}",
                self.schema_version
            ));
        }
        match self.kind {
            RecordKind::Observer if self.replay.is_some() => v.errors.push(
                "observer record carries a replay seed: an observed match is not re-simulatable"
                    .to_owned(),
            ),
            RecordKind::Observer if self.engine.is_some() => v
                .warnings
                .push("observer record carries an engine stamp".to_owned()),
            RecordKind::Full if self.replay.is_none() => v
                .warnings
                .push("full record has no replay seed: it cannot be re-simulated".to_owned()),
            RecordKind::Full if self.engine.is_none() => v.warnings.push(
                "full record has no engine stamp: replay fidelity is unverifiable".to_owned(),
            ),
            _ => {}
        }
    }

    fn check_participants(&self, v: &mut Validation) {
        for seat in SEATS {
            let Some(p) = self.players.get(seat) else {
                v.errors.push(format!("no participant for seat {seat}"));
                continue;
            };
            if p.competitor.card.is_empty() && p.competitor.name.is_none() {
                v.errors
                    .push(format!("seat {seat}: competitor has neither uuid nor name"));
            }
            if !p.deck.is_empty() && p.deck.len() != crate::cards::DECK_SIZE {
                v.warnings.push(format!(
                    "seat {seat}: decklist has {} cards, expected {}",
                    p.deck.len(),
                    crate::cards::DECK_SIZE
                ));
            }
        }
        for seat in self.players.keys() {
            if !SEATS.contains(&seat.as_str()) {
                v.errors.push(format!("unknown seat key {seat:?}"));
            }
        }
    }

    fn check_frames(&self, v: &mut Validation) {
        if self.frames.is_empty() {
            v.errors.push("record has no frames".to_owned());
            return;
        }
        for (i, frame) in self.frames.iter().enumerate() {
            check_frame(i, frame, v);
        }
        let turns: Vec<i64> = self.frames.iter().map(|f| f.turn_no).collect();
        if turns.windows(2).any(|w| w[1] < w[0]) {
            v.errors
                .push("frames go backwards in turn_no: they must be chronological".to_owned());
        }
        if !matches!(self.frames[0].action, Action::Start) {
            v.warnings
                .push("first frame is not a 'start' action".to_owned());
        }
        self.check_unidentified(v);
    }

    fn check_unidentified(&self, v: &mut Validation) {
        let blanks = self
            .frames
            .iter()
            .flat_map(|f| f.players.values())
            .flat_map(|p| p.in_play.iter().chain(&p.discard))
            .filter(|r| r.card.is_empty())
            .count();
        if blanks > 0 {
            v.warnings.push(format!(
                "{blanks} card reference(s) have no uuid: those cards cannot be joined to the card DB"
            ));
        }
    }

    fn check_result(&self, v: &mut Validation) {
        if !["A", "B", "draw"].contains(&self.result.winner.as_str()) {
            v.errors.push(format!(
                "result.winner {:?} is not A, B, or draw",
                self.result.winner
            ));
        }
        if !KNOWN_REASONS.contains(&self.result.reason.as_str()) {
            v.warnings.push(format!(
                "result.reason {:?} is not one of {KNOWN_REASONS:?}",
                self.result.reason
            ));
        }
        let Some(last) = self.frames.last() else {
            return;
        };
        match &last.action {
            Action::Result {
                winner,
                reason,
                turns,
            } => self.check_final_frame(winner, reason, *turns, v),
            _ => v
                .errors
                .push("last frame is not a 'result' action: the record is truncated".to_owned()),
        }
    }

    fn check_final_frame(&self, winner: &str, reason: &str, turns: i64, v: &mut Validation) {
        let r = &self.result;
        if (winner, reason, turns) != (r.winner.as_str(), r.reason.as_str(), r.turns) {
            v.errors.push(format!(
                "final frame result ({winner}/{reason}/{turns}) disagrees with record result \
                 ({}/{}/{})",
                r.winner, r.reason, r.turns
            ));
        }
    }
}

fn check_frame(i: usize, frame: &Frame, v: &mut Validation) {
    if frame.seq != i as i64 {
        v.errors.push(format!(
            "frame {i}: seq {} out of order (frames must be dense and 0-based)",
            frame.seq
        ));
    }
    if !SEATS.contains(&frame.active.as_str()) {
        v.errors
            .push(format!("frame {i}: active seat {:?}", frame.active));
    }
    for seat in SEATS {
        let Some(p) = frame.players.get(seat) else {
            v.errors
                .push(format!("frame {i}: no state for seat {seat}"));
            continue;
        };
        check_counts(i, seat, p, v);
    }
    if let Some(seat) = action_seat(&frame.action) {
        if !SEATS.contains(&seat) {
            v.errors
                .push(format!("frame {i}: action names unknown seat {seat:?}"));
        }
    }
}

fn check_counts(i: usize, seat: &str, p: &PlayerFrame, v: &mut Validation) {
    for (what, n) in [("hand_size", p.hand_size), ("deck_size", p.deck_size)] {
        match n {
            Some(n) if n < 0 => v
                .errors
                .push(format!("frame {i}: seat {seat} {what} is negative")),
            None => v
                .warnings
                .push(format!("frame {i}: seat {seat} has no {what}")),
            _ => {}
        }
    }
}

/// The seat an action is attributed to, if any (for seat-key validation).
fn action_seat(action: &Action) -> Option<&str> {
    match action {
        Action::Roll { player, .. }
        | Action::Play { player, .. }
        | Action::Stop { player, .. }
        | Action::Draw { player, .. }
        | Action::Discard { player, .. }
        | Action::Bury { player, .. }
        | Action::Search { player, .. }
        | Action::FinishAttempt { player, .. } => Some(player),
        Action::Breakout { defender, .. } => Some(defender),
        Action::TurnResult { winner, .. } => Some(winner),
        _ => None,
    }
}
