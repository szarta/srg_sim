//! Whole-engine byte-parity (task 72e): the parity proof for the entire engine
//! (task 72). For every golden conformance fixture, replay its recorded
//! `decisions[]` through the engine and assert the produced [`GameLog`] is
//! byte-identical to the fixture's `log` — first via the batch [`Engine::play`]
//! driver, then via the resumable [`Session`] driver, confirming both drivers
//! share one protocol and land on the same log.

use serde_json::Value;
use srg_core::cards::Deck;
use srg_core::engine::{DecisionResponse, Engine, ReplayDecider, Session, Step};
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

/// Assert `got` equals the fixture `want` log, pinpointing the first divergent
/// record (huge logs make a raw `assert_eq!` dump useless).
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
    assert_eq!(
        got.len(),
        want.len(),
        "{label}: log length differs (got {}, want {})",
        got.len(),
        want.len(),
    );
}

/// The batch driver: one engine over the full recorded `decisions[]` never
/// suspends, and its log must match the fixture byte-for-byte.
#[test]
fn batch_replay_matches_fixture_log() {
    for fx in fixtures() {
        let decider = Box::new(ReplayDecider::new(fx.decisions, fx.policies));
        let mut engine = Engine::new(
            fx.deck_a,
            fx.deck_b,
            decider,
            fx.seed,
            String::new(),
            fx.kind,
        );
        engine
            .play()
            .unwrap_or_else(|_| panic!("{}: batch replay suspended", fx.label));
        assert_log_eq(&fx.label, &engine.log.canonical(), &fx.log);
    }
}

/// The resumable driver: feed each `DecisionRequest`'s recorded answer via
/// `submit`, and the session's final log must match the fixture — the same log
/// the batch driver produced, proving the two drivers share one protocol.
#[test]
fn session_replay_matches_fixture_log() {
    for fx in fixtures() {
        let recorded = fx.decisions.clone();
        let mut cursors: BTreeMap<String, usize> = BTreeMap::new();
        let (mut session, mut step) = Session::open(
            fx.deck_a,
            fx.deck_b,
            fx.policies,
            fx.seed,
            String::new(),
            fx.kind,
        );
        loop {
            match step {
                Step::Done(_) => break,
                Step::Decision(req) => {
                    let idx = cursors.entry(req.viewer.clone()).or_default();
                    let chosen = recorded[&req.viewer]
                        .get(*idx)
                        .unwrap_or_else(|| {
                            panic!("{}: no recorded answer for {}", fx.label, req.viewer)
                        })
                        .clone();
                    *idx += 1;
                    step = session.submit(DecisionResponse {
                        request_id: req.request_id,
                        chosen,
                    });
                }
            }
        }
        let log = session.log().expect("finished session has a log");
        assert_log_eq(&fx.label, &log.canonical(), &fx.log);
    }
}
