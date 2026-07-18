//! Game-log event schema, JSONL read/write, replay/verify (DESIGN.md §8).
//!
//! A faithful port of `gamelog.py`. One schema serves both simulated and
//! recorded-human games: a header line plus an ordered stream of events, one
//! JSON object per line. The stream deterministically replays a sim, transcribes
//! a real match, and trains a policy (`decision` events are the training signal).
//!
//! **Canonical form.** The conformance corpus compares logs *structurally* —
//! `canonical()` yields `[header, *events]` as JSON objects, and the fixtures are
//! written with sorted keys. serde_json's default `Map` is a sorted `BTreeMap`,
//! so serializing through [`serde_json::Value`] reproduces that canonical byte
//! form. Events are tag-serialized by `type`; card-movement events serialize
//! `source` under the reserved key `from`. Every field is emitted (the schema
//! marks them all required), so `Option` fields without `skip_serializing_if`
//! render an explicit `null`, matching the Python output.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

/// The §8 schema version this module emits.
pub const SCHEMA_VERSION: i64 = 1;

// ---------------------------------------------------------------------------
// Header and sub-structures
// ---------------------------------------------------------------------------

/// One side's identity in the header: competitor, entrance, deck refs, policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayerInfo {
    pub competitor: String,
    pub entrance: String,
    #[serde(default)]
    pub deck: Vec<String>,
    #[serde(default)]
    pub policy: String,
}

/// The log header: seed, kind, timestamp, per-player info, schema version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Header {
    pub seed: u64,
    pub kind: String,
    pub created: String,
    #[serde(default)]
    pub players: BTreeMap<String, PlayerInfo>,
    #[serde(default = "default_schema")]
    pub schema: i64,
}

fn default_schema() -> i64 {
    SCHEMA_VERSION
}

/// A single roll modifier (source + delta), listed on a `roll` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollMod {
    pub src: String,
    pub delta: i64,
}

/// One defender breakout die (skill, value, penalty, success).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BreakoutRoll {
    pub skill: String,
    pub value: i64,
    pub penalty: i64,
    pub success: bool,
}

/// The shared shape for draw / bury / discard / search events.
///
/// `hidden` marks a move the opponent cannot follow card-for-card (DESIGN.md
/// §7/§8): true iff both endpoints are private zones (hand or deck). The
/// ground-truth card ids stay in the log for deterministic replay; `hidden`
/// gates what an observer projection may reveal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardMovement {
    pub t: i64,
    pub player: String,
    #[serde(default)]
    pub cards: Vec<String>,
    /// Serialized as the reserved key `from` (e.g. `TOP` | `BOTTOM` | a zone).
    #[serde(rename = "from", default)]
    pub source: Option<String>,
    #[serde(default)]
    pub hidden: bool,
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// A log event, tag-dispatched by `type`. Every event carries the turn number
/// `t`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Event {
    #[serde(rename = "roll")]
    Roll {
        t: i64,
        player: String,
        skill: String,
        base: i64,
        value: i64,
        #[serde(default)]
        mods: Vec<RollMod>,
    },
    #[serde(rename = "turn_result")]
    TurnResult {
        t: i64,
        winner: String,
        #[serde(default)]
        tie_bumps: i64,
    },
    #[serde(rename = "decision")]
    Decision {
        t: i64,
        player: String,
        point: String,
        #[serde(default)]
        legal: Vec<Value>,
        #[serde(default)]
        chosen: Value,
        #[serde(default)]
        policy: String,
    },
    #[serde(rename = "play")]
    Play {
        t: i64,
        player: String,
        card: String,
        order: String,
        atk_type: String,
    },
    #[serde(rename = "stop")]
    Stop {
        t: i64,
        player: String,
        card: String,
        stopped: String,
        #[serde(default)]
        reason: String,
    },
    #[serde(rename = "draw")]
    Draw(CardMovement),
    #[serde(rename = "bury")]
    Bury(CardMovement),
    #[serde(rename = "discard")]
    Discard(CardMovement),
    #[serde(rename = "search")]
    Search(CardMovement),
    #[serde(rename = "finish_attempt")]
    FinishAttempt {
        t: i64,
        player: String,
        finish: String,
        value: i64,
        crowd_meter: i64,
        auto_success: bool,
        #[serde(default)]
        bonus: BTreeMap<String, i64>,
    },
    #[serde(rename = "breakout")]
    Breakout {
        t: i64,
        defender: String,
        broke_out: bool,
        #[serde(default)]
        rolls: Vec<BreakoutRoll>,
    },
    #[serde(rename = "crowd_meter")]
    CrowdMeter { t: i64, delta: i64, value: i64 },
    #[serde(rename = "unsupported")]
    Unsupported {
        t: i64,
        owner: String,
        raw: String,
        reason: String,
        #[serde(default)]
        card: Option<String>,
        #[serde(default)]
        gimmick: Option<String>,
    },
    #[serde(rename = "effect")]
    EffectApplied {
        t: i64,
        src: String,
        action: String,
        #[serde(default)]
        target: Option<String>,
        #[serde(default)]
        detail: Value,
    },
    #[serde(rename = "result")]
    Result {
        t: i64,
        winner: String,
        reason: String,
        turns: i64,
    },
}

// ---------------------------------------------------------------------------
// Log container: JSONL read/write + verification
// ---------------------------------------------------------------------------

/// A header plus an ordered list of events. Mutable so the engine can append
/// events as a game plays out.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GameLog {
    pub header: Header,
    #[serde(default)]
    pub events: Vec<Event>,
}

/// Serialize a record to its canonical (sorted-key, compact) JSON line.
fn canonical_line<T: Serialize>(record: &T) -> String {
    let value = serde_json::to_value(record).expect("record serializes");
    serde_json::to_string(&value).expect("value serializes")
}

impl GameLog {
    /// Start a new log with the given header and no events.
    pub fn new(header: Header) -> Self {
        Self {
            header,
            events: Vec::new(),
        }
    }

    /// Append an event.
    pub fn append(&mut self, event: Event) {
        self.events.push(event);
    }

    /// The canonical structural form: `[header, *events]` as JSON objects
    /// (matches the conformance corpus's `canonical_log`).
    pub fn canonical(&self) -> Vec<Value> {
        let mut out = Vec::with_capacity(self.events.len() + 1);
        out.push(serde_json::to_value(&self.header).expect("header serializes"));
        out.extend(
            self.events
                .iter()
                .map(|e| serde_json::to_value(e).expect("event serializes")),
        );
        out
    }

    /// Serialize to JSONL: the header line followed by one line per event, each
    /// in canonical sorted-key form.
    pub fn to_lines(&self) -> Vec<String> {
        let mut lines = Vec::with_capacity(self.events.len() + 1);
        lines.push(canonical_line(&self.header));
        lines.extend(self.events.iter().map(canonical_line));
        lines
    }

    /// Write the log as JSONL (a trailing newline after the last line).
    pub fn write(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        let body = self.to_lines().join("\n") + "\n";
        std::fs::write(path, body)
    }

    /// Parse a log from JSONL lines (blank lines ignored).
    pub fn parse<I, S>(lines: I) -> crate::Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let rows: Vec<String> = lines
            .into_iter()
            .map(|s| s.as_ref().to_owned())
            .filter(|l| !l.trim().is_empty())
            .collect();
        let (first, rest) = rows
            .split_first()
            .ok_or_else(|| crate::SrgError::Conformance("empty log: no header line".into()))?;
        let header: Header = serde_json::from_str(first)?;
        let events = rest
            .iter()
            .map(|row| serde_json::from_str(row))
            .collect::<serde_json::Result<Vec<Event>>>()?;
        Ok(Self { header, events })
    }

    /// Read a log from a JSONL file.
    pub fn read(path: impl AsRef<Path>) -> crate::Result<Self> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|source| crate::SrgError::Io {
            path: path.display().to_string(),
            source,
        })?;
        Self::parse(text.lines())
    }
}

/// Structural differences between two logs (empty means they match). The core of
/// replay verification: re-run the engine from the header seed, then `diff` the
/// produced log against the recorded one.
pub fn diff(expected: &GameLog, actual: &GameLog) -> Vec<String> {
    let mut problems = Vec::new();
    if expected.header != actual.header {
        problems.push("header mismatch".to_owned());
    }
    if expected.events.len() != actual.events.len() {
        problems.push(format!(
            "event count: expected {}, got {}",
            expected.events.len(),
            actual.events.len()
        ));
    }
    for (i, (exp, act)) in expected.events.iter().zip(&actual.events).enumerate() {
        if exp != act {
            problems.push(format!("event {i} differs"));
        }
    }
    problems
}

/// True iff two logs are structurally identical.
pub fn matches(expected: &GameLog, actual: &GameLog) -> bool {
    diff(expected, actual).is_empty()
}
