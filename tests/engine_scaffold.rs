//! Engine scaffold (task 72a): the continuation/decision machinery.
//!
//! Full turn-loop parity lands at 72e (byte-identical log replay). This guards
//! the pieces 72a delivers: the log **header** the engine builds from decks +
//! decider (players, competitor/entrance names, deck refs, policy names, seed,
//! kind) must match the oracle's, and the `decide` seam must auto-take a lone
//! option yet suspend (`Yield`) when the replay decider runs dry.

use serde_json::Value;
use srg_core::cards::Deck;
use srg_core::engine::{Engine, ReplayDecider};
use std::collections::BTreeMap;
use std::path::PathBuf;

fn fixtures() -> Vec<(String, Value)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/conformance");
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("read conformance dir") {
        let path = entry.expect("entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let doc: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("json");
        out.push((
            path.file_stem().unwrap().to_string_lossy().into_owned(),
            doc,
        ));
    }
    out
}

fn engine_from(doc: &Value) -> Engine {
    let deck_a: Deck = serde_json::from_value(doc["decks"]["A"].clone()).expect("deck A");
    let deck_b: Deck = serde_json::from_value(doc["decks"]["B"].clone()).expect("deck B");
    let decisions: BTreeMap<String, Vec<Value>> =
        serde_json::from_value(doc["decisions"].clone()).expect("decisions");
    let policies: BTreeMap<String, String> =
        serde_json::from_value(doc["policies"].clone()).expect("policies");
    let seed = doc["seed"].as_u64().expect("seed");
    let kind = doc["kind"].as_str().unwrap_or("sim").to_owned();
    let decider = Box::new(ReplayDecider::new(decisions, policies));
    Engine::new(deck_a, deck_b, decider, seed, String::new(), kind)
}

#[test]
fn header_matches_oracle() {
    let fixtures = fixtures();
    assert!(!fixtures.is_empty(), "no conformance fixtures");
    for (label, doc) in &fixtures {
        let engine = engine_from(doc);
        let header = serde_json::to_value(&engine.log.header).expect("header serializes");
        assert_eq!(
            header, doc["log"][0],
            "{label}: engine header != fixture log header"
        );
    }
}
