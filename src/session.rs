//! The wire-facing match driver (`docs/design/substrate-split.rst` §3.3/§4).
//!
//! [`Session`] externalizes the engine's one synchronous decision call into a
//! resumable protocol: it advances the turn loop until a player must choose, then
//! parks and surfaces a [`DecisionRequest`]; [`submit`](Session::submit) feeds the
//! chosen option back and resumes. Only [`observable`](crate::state::GameState::observable)
//! state crosses the wire — the seed and every hidden zone stay server-side, so a
//! client cannot predict its own draws (the §7 anti-cheat boundary reused as the
//! network trust boundary).
//!
//! Each seat is either [`Seat::Remote`] (decisions cross the wire — a human or a
//! remote client) or [`Seat::Local`] (resolved by a local [`Policy`] — an AI
//! opponent that never suspends). A session with two local seats runs straight to
//! `Done`; two remote seats yield at every decision.
//!
//! **Determinism & recovery.** Under the hood this is a *replay-from-seed*
//! continuation: each step rebuilds the engine over `(seed, decks, seats,
//! decisions[])` and re-runs to the next unanswered decision (WASM-safe, no
//! threads). Only remote answers are recorded — local-policy choices re-derive
//! deterministically. So [`snapshot`](Session::snapshot) is just that tuple, and
//! [`restore`](Session::restore) replays it to a byte-identical [`Step`] and
//! [`GameLog`]. The whole session is a pure function of its snapshot.

use crate::cards::Deck;
use crate::engine::{Decider, DecisionRequest, DecisionResponse, Engine, GameResult, Step, Yield};
use crate::gamelog::GameLog;
use crate::policy::{build_policy, Policy};
use crate::record::{
    CardRef, Frame, MatchRecord, MatchResult, Participant, RecordKind, RecordMeta,
    RECORD_SCHEMA_VERSION,
};
use crate::state::GameState;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, VecDeque};

/// The two seat keys a match always has.
const SEATS: [&str; 2] = ["A", "B"];

// ---------------------------------------------------------------------------
// Seats
// ---------------------------------------------------------------------------

/// How one player's decisions are resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum Seat {
    /// Decisions cross the wire (a human or a remote client). `policy` labels the
    /// seat in the log header and `decision` events (e.g. `"human"`).
    Remote { policy: String },
    /// Decisions are resolved locally by `build_policy(policy)` — an AI opponent
    /// that never suspends.
    Local { policy: String },
}

impl Seat {
    /// The policy label recorded in the log for this seat.
    pub fn label(&self) -> &str {
        match self {
            Seat::Remote { policy } | Seat::Local { policy } => policy,
        }
    }

    /// Parse a seat spec: `"remote"` is a wire seat (a human/agent decides via the
    /// protocol); any other value names a local AI [`policy`](crate::policy). The
    /// console (`srg session`) and the WASM bindings both open seats this way.
    pub fn from_spec(spec: &str) -> Seat {
        if spec == "remote" {
            Seat::Remote {
                policy: "remote".to_owned(),
            }
        } else {
            Seat::Local {
                policy: spec.to_owned(),
            }
        }
    }
}

/// Why [`Session::open`] refused to start a match (fail-closed, DESIGN.md §3.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    /// A deck failed validation; carries `validate()`'s problem list.
    InvalidDeck { seat: String, problems: Vec<String> },
    /// A `Local` seat named a policy `build_policy` does not know.
    UnknownPolicy { seat: String, name: String },
    /// The seat map is missing a required key (`A` or `B`).
    MissingSeat { seat: String },
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::InvalidDeck { seat, problems } => {
                write!(f, "deck {seat} is invalid: {}", problems.join("; "))
            }
            SessionError::UnknownPolicy { seat, name } => {
                write!(f, "seat {seat} names unknown policy {name:?}")
            }
            SessionError::MissingSeat { seat } => write!(f, "no seat for {seat}"),
        }
    }
}

impl std::error::Error for SessionError {}

// ---------------------------------------------------------------------------
// The composite decider: remote queues + local policies
// ---------------------------------------------------------------------------

/// One seat's live decision source for a single replay pass.
enum SeatDriver {
    /// Replays accepted remote answers, then suspends (queue empty → `None`).
    Remote {
        queue: VecDeque<Value>,
        label: String,
    },
    /// A local policy, always answering.
    Local { policy: Box<dyn Policy> },
}

/// Routes each decision to its seat's driver — the engine's [`Decider`] for one
/// replay pass of a [`Session`].
struct SessionDecider {
    seats: BTreeMap<String, SeatDriver>,
}

impl Decider for SessionDecider {
    fn decide(
        &mut self,
        point: &str,
        viewer: &str,
        legal: &[Value],
        state: &mut GameState,
    ) -> Option<Value> {
        match self.seats.get_mut(viewer)? {
            SeatDriver::Remote { queue, .. } => queue.pop_front(),
            SeatDriver::Local { policy } => policy.choose(point, legal, state, viewer),
        }
    }

    fn policy_name(&self, viewer: &str) -> String {
        match self.seats.get(viewer) {
            Some(SeatDriver::Remote { label, .. }) => label.clone(),
            Some(SeatDriver::Local { policy }) => policy.name().to_owned(),
            None => String::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

/// A resumable match over the decision protocol (see the module docs).
pub struct Session {
    deck_a: Deck,
    deck_b: Deck,
    seed: u64,
    created: String,
    kind: String,
    seats: BTreeMap<String, Seat>,
    /// Remote answers accepted so far, per player — the only non-reproducible
    /// state; local-policy choices re-derive from the seed.
    decisions: BTreeMap<String, Vec<Value>>,
    outstanding: Option<DecisionRequest>,
    /// The game log **as of the current step** — refreshed on every [`advance`]
    /// (not just at `Done`), so a consumer can render a live play-by-play between
    /// decisions. The whole match replays each step, so this grows monotonically.
    log: Option<GameLog>,
    /// The full internal [`GameState`] as of the current step (`to_dict`), for
    /// debugging / an attached observer. Unlike the per-viewer `observable_state`
    /// on a [`DecisionRequest`], this is loss-less (all hands, deck order, RNG).
    full_state: Option<Value>,
    /// The observable-frame sequence as of the current step — the replay layer
    /// ([`crate::record`]), refreshed on every [`advance`] like `log`.
    frames: Vec<Frame>,
    result: Option<GameResult>,
}

/// A self-contained, serializable snapshot of a [`Session`] — everything needed to
/// [`restore`](Session::restore) it to a byte-identical state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSnapshot {
    deck_a: Deck,
    deck_b: Deck,
    seed: u64,
    #[serde(default)]
    created: String,
    kind: String,
    seats: BTreeMap<String, Seat>,
    #[serde(default)]
    decisions: BTreeMap<String, Vec<Value>>,
}

impl Session {
    /// Open a match over two decks and a seat per player, running to the first
    /// decision (or straight to `Done` if no seat suspends).
    ///
    /// Fail-closed (DESIGN.md §3.1): both decks must validate, every `Local` seat
    /// must name a known policy, and both `A`/`B` seats must be present.
    pub fn open(
        deck_a: Deck,
        deck_b: Deck,
        seats: BTreeMap<String, Seat>,
        seed: u64,
        created: String,
        kind: String,
    ) -> Result<(Self, Step), SessionError> {
        validate(&deck_a, &deck_b, &seats)?;
        let mut session = Self {
            deck_a,
            deck_b,
            seed,
            created,
            kind,
            seats,
            decisions: BTreeMap::new(),
            outstanding: None,
            log: None,
            full_state: None,
            frames: Vec::new(),
            result: None,
        };
        let step = session.advance();
        Ok((session, step))
    }

    /// Feed the player's choice and resume. A stale/duplicate `request_id` (client
    /// resend, reconnect) is a no-op that re-surfaces the outstanding request —
    /// there is never more than one outstanding request per session.
    pub fn submit(&mut self, response: DecisionResponse) -> Step {
        let Some(req) = self.outstanding.clone() else {
            return Step::Done(self.result.clone().expect("finished session has a result"));
        };
        if req.request_id != response.request_id {
            return Step::Decision(req); // idempotent: ignore a stale/duplicate answer
        }
        self.decisions
            .entry(req.viewer.clone())
            .or_default()
            .push(response.chosen);
        self.advance()
    }

    /// The game log as of the current step — available after every step (not only
    /// at `Done`), so a client can render a live play-by-play. See [`Session::log`]'s
    /// field docs.
    pub fn log(&self) -> Option<&GameLog> {
        self.log.as_ref()
    }

    /// The full internal [`GameState`] (`to_dict`) as of the current step, for
    /// debugging / an attached observer — loss-less, unlike the per-viewer
    /// `observable_state` on a [`DecisionRequest`].
    pub fn debug_state(&self) -> Option<&Value> {
        self.full_state.as_ref()
    }

    /// The ordered observable [`Frame`]s of the match so far — the replay layer
    /// ([`crate::record`]). Complete up to the current step and append-only, so a
    /// viewer can stream them incrementally (see [`frames_from`](Session::frames_from)).
    pub fn frames(&self) -> &[Frame] {
        &self.frames
    }

    /// The frames from ordinal `start` on — the incremental read a live client
    /// wants (the full sequence is re-derived on every step and can get long).
    pub fn frames_from(&self, start: usize) -> &[Frame] {
        self.frames.get(start..).unwrap_or(&[])
    }

    /// The finished match as a portable [`MatchRecord`] (`kind: full`, carrying both
    /// the frame sequence and the compact replay seed). `None` until the match ends —
    /// use [`snapshot`](Session::snapshot) to persist a match in progress.
    pub fn record(&self, meta: RecordMeta) -> Option<MatchRecord> {
        let result = self.result.as_ref()?;
        Some(MatchRecord {
            schema_version: RECORD_SCHEMA_VERSION,
            kind: RecordKind::Full,
            engine: Some(crate::version_info()),
            meta: self.record_meta(meta),
            players: self.participants(),
            frames: self.frames.clone(),
            result: MatchResult {
                winner: result.winner.clone(),
                reason: result.reason.clone(),
                turns: result.turns,
            },
            replay: Some(self.snapshot()),
        })
    }

    /// Fill in what the session knows (creation time, match kind) without
    /// overwriting what the caller supplied.
    fn record_meta(&self, mut meta: RecordMeta) -> RecordMeta {
        if meta.created.is_empty() {
            meta.created = self.created.clone();
        }
        if meta.match_type.is_empty() {
            meta.match_type = self.kind.clone();
        }
        meta
    }

    fn participants(&self) -> BTreeMap<String, Participant> {
        SEATS
            .iter()
            .map(|&key| (key.to_owned(), self.participant(key)))
            .collect()
    }

    fn participant(&self, key: &str) -> Participant {
        let deck = if key == "A" {
            &self.deck_a
        } else {
            &self.deck_b
        };
        Participant {
            player: self
                .seats
                .get(key)
                .map(Seat::label)
                .unwrap_or("")
                .to_owned(),
            competitor: CardRef {
                card: deck.competitor.db_uuid.clone(),
                name: Some(deck.competitor.name.clone()),
                number: None,
            },
            entrance: Some(CardRef {
                card: deck.entrance.db_uuid.clone(),
                name: Some(deck.entrance.name.clone()),
                number: None,
            }),
            deck: deck.cards.iter().map(CardRef::from_card).collect(),
        }
    }

    /// The outstanding request, if the session is parked awaiting a choice.
    pub fn pending(&self) -> Option<&DecisionRequest> {
        self.outstanding.as_ref()
    }

    /// The match result, once the session has reached `Done` (else `None`).
    pub fn result(&self) -> Option<&GameResult> {
        self.result.as_ref()
    }

    /// A self-contained snapshot: `(seed, decks, seats, decisions[])`. Cheap and
    /// serializable; [`restore`](Session::restore) replays it byte-identically.
    pub fn snapshot(&self) -> SessionSnapshot {
        SessionSnapshot {
            deck_a: self.deck_a.clone(),
            deck_b: self.deck_b.clone(),
            seed: self.seed,
            created: self.created.clone(),
            kind: self.kind.clone(),
            seats: self.seats.clone(),
            decisions: self.decisions.clone(),
        }
    }

    /// Rebuild a session from a snapshot, replaying to the same `Step` it was taken
    /// at. Fail-closed like [`open`](Session::open).
    pub fn restore(snap: SessionSnapshot) -> Result<(Self, Step), SessionError> {
        validate(&snap.deck_a, &snap.deck_b, &snap.seats)?;
        let mut session = Self {
            deck_a: snap.deck_a,
            deck_b: snap.deck_b,
            seed: snap.seed,
            created: snap.created,
            kind: snap.kind,
            seats: snap.seats,
            decisions: snap.decisions,
            outstanding: None,
            log: None,
            full_state: None,
            frames: Vec::new(),
            result: None,
        };
        let step = session.advance();
        Ok((session, step))
    }

    /// Rebuild the engine over the accepted decisions and run to the next
    /// suspension (or completion) — one replay-from-seed step.
    fn advance(&mut self) -> Step {
        let decider = self.build_decider();
        let mut engine = Engine::new(
            self.deck_a.clone(),
            self.deck_b.clone(),
            decider,
            self.seed,
            self.created.clone(),
            self.kind.clone(),
        );
        engine.record_frames();
        let step = engine.play();
        // Capture the loss-less state + log-so-far + observable frames as of this
        // step for an observer/debugger, a live play-by-play, and the replay layer
        // (the whole match replays each step, so all three are complete up to here).
        self.full_state = Some(engine.state.to_dict());
        self.frames = engine.take_frames();
        self.log = Some(engine.log);
        match step {
            Ok(result) => {
                self.outstanding = None;
                self.result = Some(result.clone());
                Step::Done(result)
            }
            Err(Yield(req)) => {
                self.outstanding = Some((*req).clone());
                Step::Decision(*req)
            }
        }
    }

    /// A fresh composite decider: a replay queue for each remote seat (its accepted
    /// answers) and a freshly-built policy for each local seat.
    fn build_decider(&self) -> Box<dyn Decider> {
        let seats = self
            .seats
            .iter()
            .map(|(key, seat)| (key.clone(), self.seat_driver(key, seat)))
            .collect();
        Box::new(SessionDecider { seats })
    }

    fn seat_driver(&self, key: &str, seat: &Seat) -> SeatDriver {
        match seat {
            Seat::Remote { policy } => SeatDriver::Remote {
                queue: self.decisions.get(key).cloned().unwrap_or_default().into(),
                label: policy.clone(),
            },
            // `open`/`restore` validated the name, so this cannot fail here.
            Seat::Local { policy } => SeatDriver::Local {
                policy: build_policy(policy).expect("validated local policy name"),
            },
        }
    }
}

/// Fail-closed preflight shared by `open` and `restore`.
fn validate(
    deck_a: &Deck,
    deck_b: &Deck,
    seats: &BTreeMap<String, Seat>,
) -> Result<(), SessionError> {
    for (seat, deck) in [("A", deck_a), ("B", deck_b)] {
        let problems = deck.validate();
        if !problems.is_empty() {
            return Err(SessionError::InvalidDeck {
                seat: seat.to_owned(),
                problems,
            });
        }
    }
    for key in SEATS {
        match seats.get(key) {
            None => {
                return Err(SessionError::MissingSeat {
                    seat: key.to_owned(),
                })
            }
            Some(Seat::Local { policy }) if build_policy(policy).is_none() => {
                return Err(SessionError::UnknownPolicy {
                    seat: key.to_owned(),
                    name: policy.clone(),
                });
            }
            _ => {}
        }
    }
    Ok(())
}
