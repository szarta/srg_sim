//! Golden state parity (task #69).
//!
//! `fixtures/state/positions.json` was generated from the Python oracle: three
//! positions covering base stats, static buffs, an active `HasInPlay`-gated
//! gimmick blank (mastermind-v3 "You're Not Ready" + "Ready to Rumble"), a live
//! peek, and populated hand/discard/in-play zones. For each we check the four
//! DESIGN.md §5/§7 surfaces against the oracle: snapshot round-trip, derived
//! stats, derived hand cap, gimmick-blank derivation, and the `observable`
//! projection for both viewers.

use serde_json::Value;
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
