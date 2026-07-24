//! Deterministic scripted-match snapshot fixture (frontend brief item #10).
//!
//! A fixed `(seed + sample decks + scripted choice sequence)` pins an exact ordered
//! `Step` sequence — the artifact the Run It Back frontend replays and snapshot-tests
//! against (forward/back through a finished match, not just jump-to-end). Seat A is a
//! scripted "human" that always takes the first legal option (`legal[0]`); seat B is
//! the `heuristic` AI. Because the same `srg-core` drives the WASM build, the
//! frontend's `WasmSession.open(...).submit(choices[k])` lands on these Steps
//! byte-for-byte — so this test is the engine-side guard on that contract.
//!
//! The fixture lives beside the sample decks it uses
//! (`web/src/sample/scripted_match.json`), which the frontend already vendors.
//!
//! Regenerate after an *intended* engine change (Step shape, coverage, policy):
//!   BLESS_SCRIPTED=1 cargo test --test scripted_match

use serde_json::{json, Value};
use srg_core::cards::Deck;
use srg_core::engine::{DecisionResponse, Step};
use srg_core::session::{Seat, Session};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Fixed match parameters — the fixture's identity. Mirrors the frontend's play
/// setup: seat A human (remote), seat B heuristic AI, so `match_kind` is "real".
const SEED: u64 = 7;
const DECK_A: &str = "deckA.json";
const DECK_B: &str = "deckB.json";

fn sample_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("web/src/sample")
        .join(name)
}

fn load_deck(name: &str) -> Deck {
    let text =
        std::fs::read_to_string(sample_path(name)).unwrap_or_else(|e| panic!("read {name}: {e}"));
    serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("{name} must deserialize into the current Deck schema: {e}"))
}

fn seats() -> BTreeMap<String, Seat> {
    BTreeMap::from([
        ("A".to_owned(), Seat::from_spec("remote")), // scripted human
        ("B".to_owned(), Seat::from_spec("heuristic")), // AI opponent
    ])
}

fn open_session() -> (Session, Step) {
    Session::open(
        load_deck(DECK_A),
        load_deck(DECK_B),
        seats(),
        SEED,
        String::new(),
        "real".to_owned(),
    )
    .expect("open scripted session")
}

/// The scripted human's rule: always take the first legal option. Deterministic and
/// trivially reproducible, which is the whole point of a snapshot fixture.
const SCRIPTED_CHOICE: usize = 0;

/// Drive `open -> Done` with the scripted rule, capturing the ordered choice indices
/// and the ordered `Step` JSON. `steps[0]` is `open`'s step; `steps[k+1]` is the step
/// returned by `submit(choices[k])`. The last step is always `Done`.
fn generate() -> (Vec<usize>, Vec<Value>) {
    let (mut session, mut step) = open_session();
    let mut choices = Vec::new();
    let mut steps = vec![step.to_json()];
    while let Step::Decision(req) = &step {
        let chosen = req
            .legal
            .get(SCRIPTED_CHOICE)
            .expect("a decision always has a non-empty legal list")
            .clone();
        let request_id = req.request_id.clone();
        choices.push(SCRIPTED_CHOICE);
        step = session.submit(DecisionResponse { request_id, chosen });
        steps.push(step.to_json());
    }
    (choices, steps)
}

fn fixture() -> Value {
    let text = std::fs::read_to_string(sample_path("scripted_match.json"))
        .expect("read scripted_match.json (regenerate with BLESS_SCRIPTED=1)");
    serde_json::from_str(&text).expect("scripted_match.json is valid JSON")
}

/// The generated match matches the committed fixture — the engine hasn't drifted from
/// the snapshot the frontend tests against. `BLESS_SCRIPTED=1` rewrites the fixture
/// from the live engine instead of asserting.
#[test]
fn scripted_match_matches_fixture() {
    let (choices, steps) = generate();

    if std::env::var("BLESS_SCRIPTED").is_ok() {
        let step_count = steps.len();
        let doc = json!({
            "description": "Deterministic scripted match for the Run It Back replay snapshot test \
                (frontend brief #10). Seat A (human) always takes legal[0]; seat B is the heuristic \
                AI. Replay: open(decks.A, decks.B, seats, seed) yields steps[0]; submit(choices[k]) \
                yields steps[k+1]; the last step is Done. Regenerate: BLESS_SCRIPTED=1 cargo test \
                --test scripted_match.",
            "seed": SEED,
            "kind": "real",
            "seats": { "A": "remote", "B": "heuristic" },
            "decks": { "A": DECK_A, "B": DECK_B },
            "choices": choices,
            "steps": steps,
        });
        let path = sample_path("scripted_match.json");
        std::fs::write(&path, serde_json::to_string_pretty(&doc).unwrap() + "\n")
            .expect("write fixture");
        eprintln!("blessed {} ({step_count} steps)", path.display());
        return;
    }

    let doc = fixture();
    assert_eq!(doc["seed"].as_u64(), Some(SEED), "fixture seed drifted");
    let want_choices: Vec<usize> =
        serde_json::from_value(doc["choices"].clone()).expect("fixture choices");
    let want_steps: Vec<Value> =
        serde_json::from_value(doc["steps"].clone()).expect("fixture steps");

    assert_eq!(choices, want_choices, "scripted choice sequence drifted");
    assert_eq!(
        steps.len(),
        want_steps.len(),
        "step-sequence length drifted (blessed {} vs live {})",
        want_steps.len(),
        steps.len()
    );
    for (i, (got, want)) in steps.iter().zip(&want_steps).enumerate() {
        assert_eq!(got, want, "step {i} differs from the committed fixture");
    }
}

/// The frontend's exact replay path: read `choices` from the fixture and `submit`
/// each by index, asserting every returned step equals the committed one. This is the
/// contract the browser snapshot test relies on — proven here against the same engine.
#[test]
fn fixture_choices_replay_to_committed_steps() {
    let doc = fixture();
    let choices: Vec<usize> =
        serde_json::from_value(doc["choices"].clone()).expect("fixture choices");
    let want_steps: Vec<Value> =
        serde_json::from_value(doc["steps"].clone()).expect("fixture steps");

    let (mut session, mut step) = open_session();
    assert_eq!(
        step.to_json(),
        want_steps[0],
        "opening step differs from the fixture"
    );

    for (k, &ci) in choices.iter().enumerate() {
        let Step::Decision(req) = &step else {
            panic!("expected a decision before choice {k}, got a terminal step");
        };
        let chosen = req
            .legal
            .get(ci)
            .unwrap_or_else(|| panic!("choice {k} index {ci} out of range"))
            .clone();
        let request_id = req.request_id.clone();
        step = session.submit(DecisionResponse { request_id, chosen });
        assert_eq!(
            step.to_json(),
            want_steps[k + 1],
            "replay step {} differs from the fixture",
            k + 1
        );
    }
    assert!(
        matches!(step, Step::Done(_)),
        "replaying the scripted choices must end at Done"
    );
}

/// Guard the engine-independent UI fixtures: `web/src/sample/deck{A,B}.json` must
/// still deserialize into the current `Deck` schema and pass integrity validation, so
/// the frontend's bundled sample decks can't silently rot against a schema change.
#[test]
fn sample_decks_valid_against_current_schema() {
    for name in [DECK_A, DECK_B] {
        let deck = load_deck(name); // deserialization proves the schema shape
        let problems = deck.validate();
        assert!(
            problems.is_empty(),
            "{name} failed validation: {problems:?}"
        );
    }
}
