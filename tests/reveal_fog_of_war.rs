//! Fog-of-war reveal (task #131): `Action::Reveal` exposes specific hand cards to the
//! opponent. `GameState.revealed_hand` holds the db_uuids a player has revealed; the
//! observable projection shows those cards to the opponent (while they remain in hand)
//! even though the rest of the hand is redacted to a count. A card that leaves the hand
//! (played / discarded) is no longer revealed. Driven off positions.json (bull_fae_fresh).

use serde_json::{json, Value};
use srg_core::state::GameState;
use std::path::PathBuf;

fn base_state() -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/state/positions.json");
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    doc["positions"][0]["state"].clone()
}

/// A minimal hand card with the given uuid/number.
fn card(uuid: &str, number: i64) -> Value {
    json!({
        "db_uuid": uuid, "name": uuid, "number": number, "atk_type": "Strike",
        "play_order": "Lead", "finish_bonuses": {}, "tags": [], "raw_text": "", "effects": []
    })
}

fn hand(state: &mut Value, side: &str, cards: Vec<Value>) {
    state["players"][side]["hand"] = json!(cards);
}

#[test]
fn revealed_hand_cards_are_shown_to_the_opponent_only() {
    let mut s = base_state();
    hand(&mut s, "A", vec![card("h1", 1), card("h2", 2)]);
    // A has revealed only h1.
    s["players"]["A"]["revealed_hand"] = json!(["h1"]);
    let gs = GameState::from_dict(s).expect("from_dict");

    // B (the opponent) sees A's hand redacted to a count, PLUS the revealed h1 — but
    // not the hidden h2.
    let bview = gs.observable("B");
    let a = &bview["players"]["A"];
    assert_eq!(a["hand_size"], 2, "hand stays a count for the opponent");
    assert!(a.get("hand").is_none(), "the full hand is not exposed");
    let revealed: Vec<&str> = a["revealed"]
        .as_array()
        .expect("revealed present")
        .iter()
        .map(|c| c["db_uuid"].as_str().unwrap())
        .collect();
    assert_eq!(revealed, vec!["h1"], "only the revealed card shows");

    // A's own view is the full hand, with no separate `revealed` projection.
    let aview = gs.observable("A");
    assert_eq!(aview["players"]["A"]["hand"].as_array().unwrap().len(), 2);
    assert!(aview["players"]["A"].get("revealed").is_none());
}

#[test]
fn a_revealed_card_that_left_the_hand_is_no_longer_revealed() {
    let mut s = base_state();
    // h1 was revealed, but it is no longer in the hand (played/discarded) — only h2 is.
    hand(&mut s, "A", vec![card("h2", 2)]);
    s["players"]["A"]["revealed_hand"] = json!(["h1", "h2"]);
    let gs = GameState::from_dict(s).expect("from_dict");

    let a = &gs.observable("B")["players"]["A"];
    // h1 is gone from the hand, so it is not exposed; h2 is still revealed and in hand.
    let revealed: Vec<&str> = a["revealed"]
        .as_array()
        .expect("revealed present")
        .iter()
        .map(|c| c["db_uuid"].as_str().unwrap())
        .collect();
    assert_eq!(
        revealed,
        vec!["h2"],
        "only the still-in-hand revealed card shows"
    );
}

#[test]
fn no_revealed_field_when_nothing_is_revealed() {
    let mut s = base_state();
    hand(&mut s, "A", vec![card("h1", 1)]);
    let gs = GameState::from_dict(s).expect("from_dict");
    assert!(
        gs.observable("B")["players"]["A"].get("revealed").is_none(),
        "no `revealed` key when the set is empty"
    );
}
