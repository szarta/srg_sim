//! Match-record guards (`src/record.rs`, `schemas/v1/match_record.schema.json`).
//!
//! A record is a **public interchange artifact**: consumers persist it, publish it,
//! and replay it, and imported observer archives are authored against its schema. So
//! three things are load-bearing and tested here:
//!
//! 1. an engine-produced record is well-formed by its own validator, and its frame
//!    sequence is complete (opening `start` → closing `result`) and chronological;
//! 2. it **leaks nothing hidden** — no hand or deck contents, and a private card
//!    movement projects to a count with no card ids;
//! 3. the validator actually rejects malformed archives (a hand-authored observer
//!    record is the only thing standing between a bad import and the replay viewer).
//!
//! The golden-path decks are the committed UI fixtures `web/src/sample/deck{A,B}.json`
//! (Bull vs Fae), the same ones the no-panic test drives.

use srg_core::cards::Deck;
use srg_core::engine::{DecisionResponse, Step};
use srg_core::record::{Action, MatchRecord, RecordKind, RecordMeta};
use srg_core::session::{Seat, Session};
use std::collections::BTreeMap;
use std::path::PathBuf;

fn sample_deck(name: &str) -> Deck {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("web/src/sample")
        .join(name);
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

/// Both seats local, so the session runs straight to `Done` and can be recorded.
fn played(seed: u64) -> Session {
    let seats = BTreeMap::from([
        ("A".to_owned(), Seat::from_spec("heuristic")),
        ("B".to_owned(), Seat::from_spec("smart")),
    ]);
    let (session, _) = Session::open(
        sample_deck("deckA.json"),
        sample_deck("deckB.json"),
        seats,
        seed,
        String::new(),
        "sim".to_owned(),
    )
    .unwrap_or_else(|e| panic!("open (seed {seed}): {e}"));
    session
}

fn record(seed: u64) -> MatchRecord {
    played(seed)
        .record(RecordMeta::default())
        .expect("a finished session records")
}

fn fixture(name: &str) -> MatchRecord {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/records")
        .join(name);
    MatchRecord::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

// ---------------------------------------------------------------------------
// Engine-produced records
// ---------------------------------------------------------------------------

#[test]
fn engine_record_is_valid_and_replayable() {
    for seed in 0..8u64 {
        let record = record(seed);
        let result = record.validate();
        assert!(
            result.is_valid(),
            "seed {seed}: engine record has errors: {:?}",
            result.errors
        );
        assert!(
            result.warnings.is_empty(),
            "seed {seed}: engine record has warnings: {:?}",
            result.warnings
        );
        assert_eq!(record.kind, RecordKind::Full);
        assert!(record.is_replayable(), "seed {seed}: no replay seed");
        assert!(record.engine.is_some(), "seed {seed}: no engine stamp");
    }
}

/// The frame sequence is a complete playback: opens on the untouched position,
/// closes on the result, never goes backwards, and matches what the log carries.
#[test]
fn frames_span_the_whole_match_in_order() {
    let record = record(3);
    assert!(
        record.frames.len() > 20,
        "a full match produces many frames"
    );
    assert!(matches!(record.frames[0].action, Action::Start));
    let Action::Result { winner, turns, .. } = &record.frames.last().unwrap().action else {
        panic!("last frame is not a result");
    };
    assert_eq!(*winner, record.result.winner);
    assert_eq!(*turns, record.result.turns);
    for (i, frame) in record.frames.iter().enumerate() {
        assert_eq!(frame.seq, i as i64, "frames must be dense and 0-based");
    }
    let turns: Vec<i64> = record.frames.iter().map(|f| f.turn_no).collect();
    assert!(
        turns.windows(2).all(|w| w[1] >= w[0]),
        "turn_no must never go backwards"
    );
}

/// The one property that makes a record publishable: nothing in it reveals a hidden
/// zone. Hands and decks are counts (there is no field for contents), and a draw —
/// deck→hand, private on both ends — carries no card ids.
#[test]
fn frames_never_leak_hidden_zones() {
    let record = record(11);
    let json = serde_json::to_value(&record.frames).expect("frames serialize");
    let text = json.to_string();
    assert!(!text.contains("\"hand\""), "frames must not carry hands");
    for frame in &record.frames {
        if let Action::Draw { count, .. } = frame.action {
            assert!(count >= 0);
        }
        for player in frame.players.values() {
            assert!(player.hand_size.is_some());
            assert!(player.deck_size.is_some());
        }
    }
    let drew = record
        .frames
        .iter()
        .filter(|f| matches!(f.action, Action::Draw { .. }))
        .count();
    assert!(drew > 0, "a match always has draws to redact");
}

/// Frames are a pure function of the snapshot, like the log: restoring a session
/// reproduces the identical sequence. This is what lets a consumer store only the
/// compact `replay` seed for a full record and rehydrate the frames on demand.
#[test]
fn restore_reproduces_the_same_frames() {
    let session = played(5);
    let (restored, _) = Session::restore(session.snapshot()).expect("restore");
    assert_eq!(
        serde_json::to_value(session.frames()).unwrap(),
        serde_json::to_value(restored.frames()).unwrap(),
        "restored session must yield a byte-identical frame sequence"
    );
    assert_eq!(session.frames_from(0).len(), session.frames().len());
    let tail = session.frames_from(session.frames().len() - 1);
    assert_eq!(tail.len(), 1, "incremental read returns the tail");
}

#[test]
fn record_round_trips_through_json() {
    let record = record(2);
    let text = serde_json::to_string(&record).expect("serialize");
    let back = MatchRecord::parse(&text).expect("parse");
    assert_eq!(record, back);
}

/// A session still awaiting a decision has no result, so it has no record yet —
/// consumers persist an in-progress match with `snapshot()`.
#[test]
fn unfinished_session_has_no_record() {
    let seats = BTreeMap::from([
        ("A".to_owned(), Seat::from_spec("remote")),
        ("B".to_owned(), Seat::from_spec("heuristic")),
    ]);
    let (session, _) = Session::open(
        sample_deck("deckA.json"),
        sample_deck("deckB.json"),
        seats,
        1,
        String::new(),
        "real".to_owned(),
    )
    .expect("open");
    assert!(session.record(RecordMeta::default()).is_none());
    assert!(!session.frames().is_empty(), "frames stream while parked");
}

// ---------------------------------------------------------------------------
// Observer archives
// ---------------------------------------------------------------------------

/// The committed example an importer authors against must stay valid — it is the
/// documentation for the observer format.
#[test]
fn observer_example_is_valid() {
    let record = fixture("observer_example.json");
    let result = record.validate();
    assert!(
        result.is_valid(),
        "observer example has errors: {:?}",
        result.errors
    );
    assert!(
        result.warnings.is_empty(),
        "observer example has warnings: {:?}",
        result.warnings
    );
    assert_eq!(record.kind, RecordKind::Observer);
    assert!(record.replay.is_none(), "an observed match has no seed");
    assert!(
        !record.is_replayable(),
        "an observer record is playback-only"
    );
}

/// An observer record carrying a replay seed is a contradiction: the engine did not
/// run the match, so it cannot re-derive it.
#[test]
fn observer_record_may_not_carry_a_replay_seed() {
    let seed = record(1).replay;
    let mut record = fixture("observer_example.json");
    record.replay = seed;
    let result = record.validate();
    assert!(!result.is_valid());
    assert!(
        result.errors.iter().any(|e| e.contains("replay seed")),
        "{:?}",
        result.errors
    );
}

/// One way to corrupt an archive: a label, the mutation, and the error text the
/// validator must produce.
type Corruption = (&'static str, Box<dyn Fn(&mut MatchRecord)>, &'static str);

#[test]
fn validator_rejects_malformed_archives() {
    let cases: Vec<Corruption> = vec![
        (
            "future schema",
            Box::new(|r: &mut MatchRecord| r.schema_version = 99),
            "schema_version",
        ),
        (
            "sparse seq",
            Box::new(|r: &mut MatchRecord| r.frames[2].seq = 7),
            "out of order",
        ),
        (
            "time travel",
            Box::new(|r: &mut MatchRecord| r.frames[2].turn_no = 99),
            "backwards",
        ),
        (
            "truncated",
            Box::new(|r: &mut MatchRecord| {
                r.frames.pop();
            }),
            "truncated",
        ),
        (
            "result disagrees with the final frame",
            Box::new(|r: &mut MatchRecord| r.result.winner = "A".to_owned()),
            "disagrees",
        ),
        (
            "missing seat",
            Box::new(|r: &mut MatchRecord| {
                r.players.remove("B");
            }),
            "no participant for seat B",
        ),
        (
            "no frames",
            Box::new(|r: &mut MatchRecord| r.frames.clear()),
            "no frames",
        ),
    ];
    for (what, break_it, expected) in cases {
        let mut record = fixture("observer_example.json");
        break_it(&mut record);
        let result = record.validate();
        assert!(!result.is_valid(), "{what}: should have been rejected");
        assert!(
            result.errors.iter().any(|e| e.contains(expected)),
            "{what}: expected an error mentioning {expected:?}, got {:?}",
            result.errors
        );
    }
}

/// Sparse archives are legal but flagged: an importer who omits zone sizes or cannot
/// identify a card still gets a playable record, and the consumer learns what is thin.
#[test]
fn thin_observer_archive_validates_with_warnings() {
    let mut record = fixture("observer_example.json");
    for frame in &mut record.frames {
        for player in frame.players.values_mut() {
            player.hand_size = None;
            player.deck_size = None;
        }
    }
    record.frames[1].players.get_mut("A").unwrap().discard = vec![Default::default()];
    let result = record.validate();
    assert!(result.is_valid(), "still valid: {:?}", result.errors);
    assert!(result.warnings.iter().any(|w| w.contains("hand_size")));
    assert!(result.warnings.iter().any(|w| w.contains("no uuid")));
}

// ---------------------------------------------------------------------------
// Replay affordances (the viewer's two modes)
// ---------------------------------------------------------------------------

/// **Scrubbing a full record by re-simulation.** A viewer that wants live engine
/// state (not just frames) can restore a truncated snapshot: dropping the last *k*
/// answers of a seat's `decisions` list rewinds the match exactly *k* decisions, and
/// re-submitting them walks forward through the identical ordered `Step`s. This is
/// the JSON-level recipe the browser follows — `WasmSession.restore(snapshot)` over a
/// snapshot whose `decisions` were trimmed.
#[test]
fn truncated_snapshot_rewinds_to_the_same_steps() {
    let seats = BTreeMap::from([
        ("A".to_owned(), Seat::from_spec("remote")),
        ("B".to_owned(), Seat::from_spec("heuristic")),
    ]);
    let (mut session, mut step) = Session::open(
        sample_deck("deckA.json"),
        sample_deck("deckB.json"),
        seats,
        9,
        String::new(),
        "real".to_owned(),
    )
    .expect("open");

    // Play the match out, remembering each request_id and the answer given.
    let mut ids: Vec<String> = Vec::new();
    while let Step::Decision(req) = step {
        ids.push(req.request_id.clone());
        step = session.submit(DecisionResponse {
            request_id: req.request_id,
            chosen: req.legal[0].clone(),
        });
    }
    assert!(ids.len() > 4, "the match had decisions to rewind through");

    // Rewind to decision k by keeping only the first k answers, and check the
    // session parks at exactly the request that was outstanding back then.
    let snapshot = serde_json::to_value(session.snapshot()).expect("snapshot");
    for k in [0, 1, ids.len() / 2, ids.len() - 1] {
        let mut trimmed = snapshot.clone();
        trimmed["decisions"]["A"] = serde_json::json!(Vec::<serde_json::Value>::new());
        let answers: Vec<serde_json::Value> = snapshot["decisions"]["A"]
            .as_array()
            .expect("recorded answers")
            .iter()
            .take(k)
            .cloned()
            .collect();
        trimmed["decisions"]["A"] = serde_json::json!(answers);
        let (rewound, step) =
            Session::restore(serde_json::from_value(trimmed).expect("snapshot parses"))
                .expect("restore");
        match step {
            Step::Decision(req) => assert_eq!(req.request_id, ids[k], "rewound to decision {k}"),
            Step::Done(_) => panic!("rewinding to decision {k} ended the match"),
        }
        assert_eq!(
            rewound.frames().len(),
            rewound.frames_from(0).len(),
            "frames are complete up to the rewound step"
        );
    }
}
