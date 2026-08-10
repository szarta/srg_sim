//! Turn-memory flag gates (schema v120/v121): `BrokeOutLastTurn{who}` reads
//! `flags["broke_out_turn"]` (stamped by `breakout`) and `StoppedCard{who,last_turn}`
//! reads `flags["stopped_card_turn"]` (stamped by `apply_stop`). Both hold iff the stamped
//! turn equals `turn_no - 1` (last turn) or `turn_no` (this turn). Driven against the
//! bull_fae_fresh position with the flag spliced onto a seat and `turn_no` set.

use serde_json::{json, Value};
use srg_core::conditions;
use srg_core::ir::{Condition, Who};
use srg_core::state::GameState;
use std::path::PathBuf;

/// bull_fae_fresh at `turn_no`, with a turn-number `flag` set to `stamp` (if Some) on `seat`.
fn state_flag(turn_no: i64, seat: &str, flag: &str, stamp: Option<i64>) -> GameState {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/state/positions.json");
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let mut state = doc["positions"][0]["state"].clone();
    state["turn_no"] = json!(turn_no);
    if let Some(t) = stamp {
        state["players"][seat]["flags"][flag] = json!(t);
    }
    GameState::from_dict(state).expect("from_dict")
}

/// bull_fae_fresh at `turn_no`, with `flags["broke_out_turn"]` set to `broke` (if Some)
/// on player `seat`.
fn state_with(turn_no: i64, seat: &str, broke: Option<i64>) -> GameState {
    state_flag(turn_no, seat, "broke_out_turn", broke)
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

fn stopped(who: Who, last_turn: bool) -> Condition {
    Condition::StoppedCard { who, last_turn }
}

#[test]
fn stopped_card_reads_this_and_last_turn() {
    // A stopped a card on turn 4.
    let gs = state_flag(5, "A", "stopped_card_turn", Some(4));
    // On turn 5, turn 4 is "last turn" but not "this turn".
    assert!(
        conditions::holds(&stopped(Who::SelfSide, true), &gs, "A", None),
        "last turn"
    );
    assert!(
        !conditions::holds(&stopped(Who::SelfSide, false), &gs, "A", None),
        "not this turn"
    );
    // From B's vantage A is the opponent.
    assert!(
        conditions::holds(&stopped(Who::Opp, true), &gs, "B", None),
        "A (B's opp) stopped last turn"
    );

    // A stop stamped on the CURRENT turn reads as "this turn", not "last turn".
    let gs = state_flag(5, "A", "stopped_card_turn", Some(5));
    assert!(
        conditions::holds(&stopped(Who::SelfSide, false), &gs, "A", None),
        "this turn"
    );
    assert!(
        !conditions::holds(&stopped(Who::SelfSide, true), &gs, "A", None),
        "not last turn"
    );

    // No flag -> false either way.
    let gs = state_flag(5, "A", "stopped_card_turn", None);
    assert!(
        !conditions::holds(&stopped(Who::SelfSide, true), &gs, "A", None),
        "no stop -> false"
    );
}
