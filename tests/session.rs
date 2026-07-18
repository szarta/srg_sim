//! Session parity + recovery (task 74): the resumable [`Session`] driver over the
//! decision protocol. Proven against the same golden corpus as the batch engine —
//! remote seats fed the recorded answers, and local-policy seats run straight to
//! `Done` — plus snapshot/restore round-trips that must land byte-identically.

use serde_json::Value;
use srg_core::cards::Deck;
use srg_core::engine::{DecisionResponse, Step};
use srg_core::session::{Seat, Session};
use std::collections::BTreeMap;
use std::path::PathBuf;

struct Fixture {
    label: String,
    deck_a: Deck,
    deck_b: Deck,
    decisions: BTreeMap<String, Vec<Value>>,
    policies: BTreeMap<String, String>,
    seed: u64,
    kind: String,
    log: Vec<Value>,
}

fn fixtures() -> Vec<Fixture> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/conformance");
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("read conformance dir") {
        let path = entry.expect("entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let doc: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("json");
        out.push(Fixture {
            label: path.file_stem().unwrap().to_string_lossy().into_owned(),
            deck_a: serde_json::from_value(doc["decks"]["A"].clone()).expect("deck A"),
            deck_b: serde_json::from_value(doc["decks"]["B"].clone()).expect("deck B"),
            decisions: serde_json::from_value(doc["decisions"].clone()).expect("decisions"),
            policies: serde_json::from_value(doc["policies"].clone()).expect("policies"),
            seed: doc["seed"].as_u64().expect("seed"),
            kind: doc["kind"].as_str().unwrap_or("sim").to_owned(),
            log: doc["log"].as_array().expect("log array").clone(),
        });
    }
    assert!(!out.is_empty(), "no conformance fixtures");
    out
}

fn assert_log_eq(label: &str, got: &[Value], want: &[Value]) {
    for (i, (g, w)) in got.iter().zip(want.iter()).enumerate() {
        assert_eq!(
            g,
            w,
            "{label}: log record {i} differs\n  got : {}\n  want: {}",
            serde_json::to_string(g).unwrap(),
            serde_json::to_string(w).unwrap(),
        );
    }
    assert_eq!(got.len(), want.len(), "{label}: log length differs");
}

/// One seat per player, all `Remote` (every decision crosses the wire) or all
/// `Local` (resolved by the named policy), keyed by the fixture's policy names.
fn seats(policies: &BTreeMap<String, String>, remote: bool) -> BTreeMap<String, Seat> {
    policies
        .iter()
        .map(|(k, name)| {
            let seat = if remote {
                Seat::Remote {
                    policy: name.clone(),
                }
            } else {
                Seat::Local {
                    policy: name.clone(),
                }
            };
            (k.clone(), seat)
        })
        .collect()
}

/// Drive a parked session to `Done` by feeding each request's recorded answer.
fn drive(session: &mut Session, mut step: Step, recorded: &BTreeMap<String, Vec<Value>>) {
    let mut cursors: BTreeMap<String, usize> = BTreeMap::new();
    while let Step::Decision(req) = step {
        let idx = cursors.entry(req.viewer.clone()).or_default();
        let chosen = recorded[&req.viewer][*idx].clone();
        *idx += 1;
        step = session.submit(DecisionResponse {
            request_id: req.request_id,
            chosen,
        });
    }
}

/// Remote seats fed the recorded answers reproduce the fixture log byte-for-byte —
/// the wire path lands on the same log as the batch driver.
#[test]
fn remote_seats_match_fixture_log() {
    for fx in fixtures() {
        let (mut session, step) = Session::open(
            fx.deck_a,
            fx.deck_b,
            seats(&fx.policies, true),
            fx.seed,
            String::new(),
            fx.kind,
        )
        .expect("open");
        drive(&mut session, step, &fx.decisions);
        let log = session.log().expect("finished session has a log");
        assert_log_eq(&fx.label, &log.canonical(), &fx.log);
    }
}

/// Local-policy seats never suspend: `open` runs straight to `Done`, and the log
/// still matches the fixture (Session + local policies == the batch driver).
#[test]
fn local_seats_run_to_done() {
    for fx in fixtures() {
        let (session, step) = Session::open(
            fx.deck_a,
            fx.deck_b,
            seats(&fx.policies, false),
            fx.seed,
            String::new(),
            fx.kind,
        )
        .expect("open");
        assert!(
            matches!(step, Step::Done(_)),
            "{}: local-only session should not suspend",
            fx.label
        );
        let log = session.log().expect("finished session has a log");
        assert_log_eq(&fx.label, &log.canonical(), &fx.log);
    }
}

/// Snapshot/replay state parity *at every decision boundary* (substrate-split.md §6,
/// task 75): at each park, `restore(snapshot)` must reproduce the exact outstanding
/// request the live session holds, and at `Done` the exact log — the session is a pure
/// function of its snapshot, boundary for boundary, not just at the endpoints.
#[test]
fn snapshot_restores_at_every_boundary() {
    for fx in fixtures() {
        let (mut session, mut step) = Session::open(
            fx.deck_a,
            fx.deck_b,
            seats(&fx.policies, true),
            fx.seed,
            String::new(),
            fx.kind,
        )
        .expect("open");
        let mut cursors: BTreeMap<String, usize> = BTreeMap::new();
        let mut boundaries = 0usize;
        while let Step::Decision(req) = step {
            // Restoring here must land on the identical outstanding request.
            let (_restored, restored_step) =
                Session::restore(session.snapshot()).expect("restore mid-run");
            match restored_step {
                Step::Decision(r) => assert_eq!(
                    r.request_id, req.request_id,
                    "{}: boundary {boundaries} restored to a different request",
                    fx.label
                ),
                Step::Done(_) => panic!(
                    "{}: restore finished early at boundary {boundaries}",
                    fx.label
                ),
            }
            boundaries += 1;
            let idx = cursors.entry(req.viewer.clone()).or_default();
            let chosen = fx.decisions[&req.viewer][*idx].clone();
            *idx += 1;
            step = session.submit(DecisionResponse {
                request_id: req.request_id,
                chosen,
            });
        }
        assert!(
            boundaries > 0,
            "{}: expected at least one decision",
            fx.label
        );
        // Terminal boundary: the finished session restores to the same log.
        let (done, done_step) = Session::restore(session.snapshot()).expect("restore done");
        assert!(
            matches!(done_step, Step::Done(_)),
            "{}: restore not done",
            fx.label
        );
        assert_log_eq(&fx.label, &done.log().expect("log").canonical(), &fx.log);
    }
}

/// A snapshot taken after `Done` restores to a byte-identical log; a snapshot taken
/// while parked restores to the same outstanding request.
#[test]
fn snapshot_restore_roundtrip() {
    for fx in fixtures() {
        // Parked at the very first decision: restore reproduces the same request.
        let (fresh, fresh_step) = Session::open(
            fx.deck_a.clone(),
            fx.deck_b.clone(),
            seats(&fx.policies, true),
            fx.seed,
            String::new(),
            fx.kind.clone(),
        )
        .expect("open");
        let (_restored, restored_step) = Session::restore(fresh.snapshot()).expect("restore");
        match (fresh_step, restored_step) {
            (Step::Decision(a), Step::Decision(b)) => assert_eq!(
                a.request_id, b.request_id,
                "{}: restore parked at a different request",
                fx.label
            ),
            _ => panic!("{}: expected both to park at a decision", fx.label),
        }

        // Driven to Done, then snapshot -> restore replays to the same log.
        let (mut session, step) = Session::open(
            fx.deck_a,
            fx.deck_b,
            seats(&fx.policies, true),
            fx.seed,
            String::new(),
            fx.kind,
        )
        .expect("open");
        drive(&mut session, step, &fx.decisions);
        let (done, done_step) = Session::restore(session.snapshot()).expect("restore");
        assert!(
            matches!(done_step, Step::Done(_)),
            "{}: restore not done",
            fx.label
        );
        assert_log_eq(&fx.label, &done.log().expect("log").canonical(), &fx.log);
    }
}
