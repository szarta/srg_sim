//! `BrokeOutLastTurn{who}` (schema v120): `who` broke out on the PREVIOUS turn. Read
//! from `flags["broke_out_turn"]` (stamped by `breakout` on success), true iff it equals
//! `turn_no - 1`. Driven against the bull_fae_fresh position with the flag spliced onto a
//! seat and `turn_no` set, so the "last turn" comparison can be observed both ways.

use serde_json::{json, Value};
use srg_core::conditions;
use srg_core::ir::{Condition, Who};
use srg_core::state::GameState;
use std::path::PathBuf;

/// bull_fae_fresh at `turn_no`, with `flags["broke_out_turn"]` set to `broke` (if Some)
/// on player `seat`.
fn state_with(turn_no: i64, seat: &str, broke: Option<i64>) -> GameState {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/state/positions.json");
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let mut state = doc["positions"][0]["state"].clone();
    state["turn_no"] = json!(turn_no);
    if let Some(t) = broke {
        state["players"][seat]["flags"]["broke_out_turn"] = json!(t);
    }
    GameState::from_dict(state).expect("from_dict")
}

fn broke(who: Who) -> Condition {
    Condition::BrokeOutLastTurn { who }
}

#[test]
fn broke_out_last_turn_reads_the_previous_turn_flag() {
    // A broke out on turn 4; on turn 5 that is "last turn" -> true for A, false for B.
    let gs = state_with(5, "A", Some(4));
    assert!(
        conditions::holds(&broke(Who::SelfSide), &gs, "A", None),
        "A broke out last turn"
    );
    assert!(
        !conditions::holds(&broke(Who::Opp), &gs, "A", None),
        "B did not"
    );
    // From B's vantage, the SELF/OPP roles flip.
    assert!(
        conditions::holds(&broke(Who::Opp), &gs, "B", None),
        "A (B's opp) broke out last turn"
    );

    // A stale flag two turns old is NOT "last turn".
    let gs = state_with(6, "A", Some(4));
    assert!(
        !conditions::holds(&broke(Who::SelfSide), &gs, "A", None),
        "turn 4 is not turn 5"
    );

    // No flag at all -> false (never broke out).
    let gs = state_with(5, "A", None);
    assert!(
        !conditions::holds(&broke(Who::SelfSide), &gs, "A", None),
        "no breakout -> false"
    );
}
