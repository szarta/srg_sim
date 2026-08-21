//! `RelatedFinishesInPlay{count}` (schema v157): the owner has ≥ count of their OWN
//! competitor's related Finishes in play — "if you have 2 Syzygy Finishes in play". Counts
//! card IDENTITY (`Competitor.related_finishes` uuids), not Finish play order, so a logoless
//! finish the deck happens to run never counts. Driven against the bull_fae_fresh position
//! with A's competitor `related_finishes` and `in_play` spliced.

use serde_json::{json, Value};
use srg_core::conditions;
use srg_core::ir::Condition;
use srg_core::state::GameState;
use std::path::PathBuf;

/// A bare in-play card carrying `db_uuid` (order/type are irrelevant to the identity count).
fn card(uuid: &str) -> Value {
    json!({
        "atk_type": "Strike", "db_uuid": uuid, "name": uuid, "number": 1,
        "play_order": "Finish", "raw_text": "", "tags": [], "finish_bonuses": {}, "effects": []
    })
}

/// bull_fae_fresh with A's competitor `related_finishes` = `related` and A's `in_play`
/// holding a card for each uuid in `in_play`.
fn state_with(related: &[&str], in_play: &[&str]) -> GameState {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/state/positions.json");
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let mut state = doc["positions"][0]["state"].clone();
    state["players"]["A"]["competitor"]["related_finishes"] = json!(related);
    state["players"]["A"]["in_play"] = Value::Array(in_play.iter().map(|u| card(u)).collect());
    GameState::from_dict(state).expect("from_dict")
}

#[test]
fn counts_only_the_competitors_related_finishes_in_play() {
    let related = ["f1", "f2", "f3"];
    let gate = |n| Condition::RelatedFinishesInPlay { count: n };

    // Two of the three related finishes in play -> a count-2 gate holds, count-3 does not.
    let gs = state_with(&related, &["f1", "f2"]);
    assert!(conditions::holds(&gate(2), &gs, "A", None));
    assert!(!conditions::holds(&gate(3), &gs, "A", None));

    // A logoless (non-related) finish in play does NOT count toward the set.
    let gs = state_with(&related, &["f1", "logoless_finish"]);
    assert!(conditions::holds(&gate(1), &gs, "A", None));
    assert!(!conditions::holds(&gate(2), &gs, "A", None));
}
