//! Golden state parity (task #69).
//!
//! `fixtures/state/positions.json` was generated from the Python oracle: three
//! positions covering base stats, static buffs, an active `HasInPlay`-gated
//! gimmick blank (mastermind-v3 "You're Not Ready" + "Ready to Rumble"), a live
//! peek, and populated hand/discard/in-play zones. For each we check the four
//! DESIGN.md §5/§7 surfaces against the oracle: snapshot round-trip, derived
//! stats, derived hand cap, gimmick-blank derivation, and the `observable`
//! projection for both viewers.

use serde_json::{json, Value};
use srg_core::state::GameState;
use std::path::PathBuf;

fn positions() -> Vec<Value> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/state/positions.json");
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let doc: Value = serde_json::from_str(&text).expect("valid state fixture");
    doc["positions"]
        .as_array()
        .expect("positions array")
        .clone()
}

fn name(pos: &Value) -> &str {
    pos["name"].as_str().unwrap_or("<unnamed>")
}

fn state_of(pos: &Value) -> GameState {
    GameState::from_dict(pos["state"].clone())
        .unwrap_or_else(|e| panic!("{}: from_dict failed: {e}", name(pos)))
}

#[test]
fn snapshot_round_trips() {
    for pos in positions() {
        let gs = state_of(&pos);
        assert_eq!(
            gs.to_dict(),
            pos["state"],
            "{}: to_dict != from_dict source",
            name(&pos)
        );
    }
}

#[test]
fn effective_stats_match_oracle() {
    for pos in positions() {
        let gs = state_of(&pos);
        for (key, expected) in pos["effective_stats"].as_object().unwrap() {
            let got = serde_json::to_value(gs.effective_stats(key, None)).unwrap();
            assert_eq!(&got, expected, "{}: effective_stats[{key}]", name(&pos));
        }
    }
}

#[test]
fn effective_hand_cap_matches_oracle() {
    for pos in positions() {
        let gs = state_of(&pos);
        for (key, expected) in pos["effective_hand_cap_base5"].as_object().unwrap() {
            let got = gs.effective_hand_cap(key, 5, None);
            assert_eq!(
                got,
                expected.as_i64().unwrap(),
                "{}: effective_hand_cap[{key}]",
                name(&pos)
            );
        }
    }
}

#[test]
fn gimmick_blank_derivation_matches_oracle() {
    let mut saw_blank = false;
    for pos in positions() {
        let gs = state_of(&pos);
        for (key, expected) in pos["gimmick_blanked"].as_object().unwrap() {
            let got = gs.is_gimmick_blanked(key);
            assert_eq!(
                got,
                expected.as_bool().unwrap(),
                "{}: blanked[{key}]",
                name(&pos)
            );
            saw_blank |= got;
        }
    }
    // Guard: at least one position must exercise the blank-TRUE path.
    assert!(
        saw_blank,
        "no fixture position exercised an active gimmick blank"
    );
}

#[test]
fn observable_projection_matches_oracle() {
    for pos in positions() {
        let gs = state_of(&pos);
        assert_eq!(
            gs.observable("A"),
            pos["observable_A"],
            "{}: observable(A)",
            name(&pos)
        );
        assert_eq!(
            gs.observable("B"),
            pos["observable_B"],
            "{}: observable(B)",
            name(&pos)
        );
    }
}

fn lead_card(name: &str, i: usize) -> Value {
    json!({
        "atk_type": "Strike", "db_uuid": format!("c{i}"), "effects": [],
        "finish_bonuses": {}, "name": name, "number": 1, "play_order": "Lead",
        "raw_text": "", "tags": []
    })
}

fn static_blank_opp(source: &str, condition: Value) -> Value {
    json!({
        "@type": "Effect", "trigger": {"@type": "Static"}, "condition": condition,
        "actions": [{"@type": "BlankGimmick", "who": "OPP", "duration": "WHILE_IN_PLAY"}],
        "duration": "WHILE_IN_PLAY",
        "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
        "raw_clause": "test", "source": source, "optional": false
    })
}

/// A gimmick-sourced Static conditional `BlankGimmick` blanks the opponent (GM
/// Calace V2 / Mr. Snap V1 shape) — but only while its count condition holds AND
/// the owner's own gimmick is still active.
#[test]
fn gimmick_sourced_conditional_blank() {
    let mut base = positions()[0]["state"].clone();
    for k in ["A", "B"] {
        base["players"][k]["competitor"]["effects"] = json!([]);
        base["players"][k]["entrance"]["effects"] = json!([]);
        base["players"][k]["in_play"] = json!([]);
        base["players"][k]["gimmick_blanked"] = json!(false);
    }
    // A's gimmick: while A has >=2 "Bar"-named cards in play, B's gimmick is blank.
    let has_two_bars = json!({
        "@type": "HasInPlay", "who": "SELF", "count": 2, "cmp": ">=",
        "filter": {"@type": "CardFilter", "number": null, "atk_type": null,
                   "play_order": null, "tag": null, "name": null, "raw": null,
                   "name_contains": ["Bar"], "text_contains": []}
    });
    base["players"]["A"]["competitor"]["effects"] =
        json!([static_blank_opp("gimmick", has_two_bars)]);

    // 1 matching card -> below the threshold -> not blanked.
    let mut state = base.clone();
    state["players"]["A"]["in_play"] = json!([lead_card("Crowbar", 1)]);
    assert!(!GameState::from_dict(state).unwrap().is_gimmick_blanked("B"));

    // 2 matching cards -> the count holds -> B is blanked.
    let mut state = base.clone();
    state["players"]["A"]["in_play"] = json!([lead_card("Crowbar", 1), lead_card("Sidebar", 2)]);
    assert!(GameState::from_dict(state).unwrap().is_gimmick_blanked("B"));

    // 2 matching, but B's ENTRANCE unconditionally blanks A: A's gimmick is now
    // inactive, so A's gimmick-sourced blank of B no longer fires (the "only while
    // your own gimmick is active" gate; the blank<->blank loop is guard-bounded).
    let mut state = base.clone();
    state["players"]["A"]["in_play"] = json!([lead_card("Crowbar", 1), lead_card("Sidebar", 2)]);
    state["players"]["B"]["entrance"]["effects"] =
        json!([static_blank_opp("entrance", json!({"@type": "Always"}))]);
    let gs = GameState::from_dict(state).unwrap();
    assert!(gs.is_gimmick_blanked("A"), "B's entrance blanks A");
    assert!(
        !gs.is_gimmick_blanked("B"),
        "A's blanked gimmick cannot blank B"
    );
}
