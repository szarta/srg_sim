//! `SkillCompare` vs-opponent delta (task #120): "at least N greater than your
//! opponent's <S>" is `self >= opp + N` (Ge, value=N). The engine's vs-opponent
//! branch adds `value` to the opponent's skill. Driven against bull_fae_fresh,
//! where A.Power == B.Power == 10, so the boundary flips exactly at the delta.

use serde_json::{json, Value};
use srg_core::conditions;
use srg_core::ir::Condition;
use srg_core::state::GameState;
use std::path::PathBuf;

fn base_state() -> GameState {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/state/positions.json");
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    GameState::from_dict(doc["positions"][0]["state"].clone()).expect("from_dict")
}

fn ge_delta(value: i64) -> Condition {
    serde_json::from_value(json!({
        "@type": "SkillCompare", "skill": "Power", "cmp": ">=",
        "who": "SELF", "vs": "OPP_SAME", "value": value, "vs_skill": null
    }))
    .unwrap()
}

#[test]
fn at_least_n_greater_flips_at_the_delta() {
    // A.Power == B.Power == 10.
    let state = base_state();
    // "at least 0 greater" == "greater than or equal": 10 >= 10 -> true.
    assert!(conditions::holds(&ge_delta(0), &state, "A", None));
    // "at least 1 greater": 10 >= 11 -> false (A is not ahead).
    assert!(!conditions::holds(&ge_delta(1), &state, "A", None));
    // A hypothetical 3-point lead requirement is likewise unmet at parity.
    assert!(!conditions::holds(&ge_delta(3), &state, "A", None));
}
