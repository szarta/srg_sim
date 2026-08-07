//! "The player with the fewest/most cards in hand draws/discards N" (task #131).
//! The actor is decided by hand size at resolution — modeled as TWO per-seat
//! conditional effects with a NON-STRICT compare (`<=` fewest, `>=` most) so a TIE
//! resolves for BOTH players. This asserts the composition end-to-end: parse the
//! clause, then evaluate each effect's condition against controlled hand sizes.

use serde_json::{json, Value};
use srg_core::conditions;
use srg_core::ir::EffectSource;
use srg_core::parser::parse_text;
use srg_core::state::GameState;
use std::path::PathBuf;

fn base_state() -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/state/positions.json");
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    doc["positions"][0]["state"].clone()
}

/// A state where A holds `a_hand` cards and B holds `b_hand` (identical dummy cards).
fn state_with_hands(a_hand: usize, b_hand: usize) -> GameState {
    let mut s = base_state();
    let mk = |n: usize, side: &str| -> Value {
        (0..n)
            .map(|i| {
                json!({"db_uuid": format!("{side}{i}"), "name": "c", "number": 1,
                       "atk_type": "Strike", "play_order": "Lead", "finish_bonuses": {}, "effects": []})
            })
            .collect()
    };
    s["players"]["A"]["hand"] = mk(a_hand, "a");
    s["players"]["B"]["hand"] = mk(b_hand, "b");
    GameState::from_dict(s).expect("from_dict")
}

/// The `(SELF-effect, OPP-effect)` conditions for a parsed hand-extreme clause,
/// as (`who`, `Condition`) pairs — the owner is A.
fn conditions_of(text: &str) -> Vec<srg_core::ir::Condition> {
    let effs = parse_text(text, EffectSource::Card, None, None);
    assert_eq!(effs.len(), 2, "two per-seat effects for {text:?}");
    effs.into_iter().map(|e| e.condition).collect()
}

/// Whether the owner (A) and opponent (B) each act, given the two effect conditions.
fn actors(text: &str, a_hand: usize, b_hand: usize) -> (bool, bool) {
    let conds = conditions_of(text);
    let gs = state_with_hands(a_hand, b_hand);
    // Effect 0 targets SELF (owner A), effect 1 targets OPP (B) — both evaluated from
    // the owner A's perspective.
    (
        conditions::holds(&conds[0], &gs, "A", None),
        conditions::holds(&conds[1], &gs, "A", None),
    )
}

#[test]
fn fewest_draws_picks_the_smaller_hand_and_ties_go_to_both() {
    let text = "The player with the fewest cards in hand draws 1 card.";
    // A has fewer -> only A draws.
    assert_eq!(actors(text, 2, 5), (true, false));
    // B has fewer -> only B draws.
    assert_eq!(actors(text, 5, 2), (false, true));
    // Tie -> BOTH draw (the non-strict <= holds for each).
    assert_eq!(actors(text, 3, 3), (true, true));
}

#[test]
fn most_discards_picks_the_larger_hand_and_ties_go_to_both() {
    let text = "The player with the most cards in their hand discards 1 card from their hand.";
    // A has more -> only A discards.
    assert_eq!(actors(text, 5, 2), (true, false));
    // B has more -> only B discards.
    assert_eq!(actors(text, 2, 5), (false, true));
    // Tie -> BOTH discard.
    assert_eq!(actors(text, 4, 4), (true, true));
}

#[test]
fn the_two_effects_carry_the_right_actions() {
    let draw = parse_text(
        "The player with fewest cards in hand draws 1 card.",
        EffectSource::Card,
        None,
        None,
    );
    let a = serde_json::to_value(&draw[0]).unwrap();
    assert_eq!(a["actions"][0]["@type"], "Draw");
    assert_eq!(a["actions"][0]["who"], "SELF");
    assert_eq!(a["condition"]["cmp"], "<=");
    assert_eq!(
        serde_json::to_value(&draw[1]).unwrap()["actions"][0]["who"],
        "OPP"
    );

    let disc = parse_text(
        "The player with the most cards in their hand discards 1 card from their hand.",
        EffectSource::Card,
        None,
        None,
    );
    let a = serde_json::to_value(&disc[0]).unwrap();
    assert_eq!(a["actions"][0]["@type"], "Discard");
    assert_eq!(a["condition"]["cmp"], ">=");
}
