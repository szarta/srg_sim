//! Meta-comparison override (task #104): `ConsideredCompare` forces a player's
//! vs-opponent `SkillCompare` / `HandSizeCompare` to resolve a fixed way "for card
//! effects" (RaRa Perre "skills considered higher"; Theo V2 "hand considered
//! fewer"). Driven against a real position: bull_fae_fresh has A.Power == B.Power,
//! so the `=` case is normally TRUE — the strict override must flip it to FALSE.

use serde_json::{json, Value};
use srg_core::conditions;
use srg_core::ir::Condition;
use srg_core::state::GameState;
use std::path::PathBuf;

/// The `bull_fae_fresh` position's state, with `extra_effects` spliced onto A's
/// competitor gimmick (so A carries the meta-comparison declaration).
fn state_with(a_extra: &[Value]) -> GameState {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/state/positions.json");
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let mut state = doc["positions"][0]["state"].clone();
    let effects = state["players"]["A"]["competitor"]["effects"]
        .as_array_mut()
        .unwrap();
    effects.extend(a_extra.iter().cloned());
    GameState::from_dict(state).expect("from_dict")
}

fn declare(domain: &str, order: &str) -> Value {
    json!({
        "@type": "Effect",
        "trigger": {"@type": "Static"},
        "condition": {"@type": "Always"},
        "actions": [{"@type": "ConsideredCompare", "domain": domain, "order": order}],
        "duration": "WHILE_IN_PLAY",
        "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
        "raw_clause": "test",
        "source": "gimmick",
        "optional": false
    })
}

fn skill_cmp(cmp: &str) -> Condition {
    serde_json::from_value(json!({
        "@type": "SkillCompare", "skill": "Power", "cmp": cmp,
        "who": "SELF", "vs": "OPP", "value": null, "vs_skill": null
    }))
    .unwrap()
}

fn hand_cmp(cmp: &str) -> Condition {
    serde_json::from_value(json!({
        "@type": "HandSizeCompare", "cmp": cmp, "who": "SELF", "vs": "OPP", "value": null
    }))
    .unwrap()
}

#[test]
fn skill_override_greater_is_strict() {
    // Baseline: A.Power (10) == B.Power (10) — `>` false, `>=`/`=` true, `<` false.
    let base = state_with(&[]);
    assert!(!conditions::holds(&skill_cmp(">"), &base, "A", None));
    assert!(conditions::holds(&skill_cmp("="), &base, "A", None));

    // RaRa Perre: A's skills considered GREATER — `>`/`>=` hold, `=`/`<`/`<=` fail.
    let g = state_with(&[declare("SKILL", "GREATER")]);
    assert!(
        conditions::holds(&skill_cmp(">"), &g, "A", None),
        "> forced true"
    );
    assert!(
        conditions::holds(&skill_cmp(">="), &g, "A", None),
        ">= forced true"
    );
    assert!(
        !conditions::holds(&skill_cmp("="), &g, "A", None),
        "= strict-false"
    );
    assert!(
        !conditions::holds(&skill_cmp("<"), &g, "A", None),
        "< forced false"
    );
    assert!(
        !conditions::holds(&skill_cmp("<="), &g, "A", None),
        "<= forced false"
    );
}

#[test]
fn override_is_scoped_to_the_declaring_subject() {
    // A declares GREATER; a SkillCompare owned by B (subject = B, who=SELF) sees no
    // declaration and resolves on the real stats (B.Power 10 == A.Power 10).
    let g = state_with(&[declare("SKILL", "GREATER")]);
    assert!(
        !conditions::holds(&skill_cmp(">"), &g, "B", None),
        "B not overridden"
    );
    assert!(
        conditions::holds(&skill_cmp("="), &g, "B", None),
        "B real ="
    );
}

#[test]
fn hand_override_less_is_strict() {
    // Theo V2: A's hand considered fewer (LESS) — `<`/`<=` hold, `>`/`>=`/`=` fail,
    // whatever the real hand sizes are.
    let l = state_with(&[declare("HAND", "LESS")]);
    assert!(
        conditions::holds(&hand_cmp("<"), &l, "A", None),
        "< forced true"
    );
    assert!(
        conditions::holds(&hand_cmp("<="), &l, "A", None),
        "<= forced true"
    );
    assert!(
        !conditions::holds(&hand_cmp(">"), &l, "A", None),
        "> forced false"
    );
    assert!(
        !conditions::holds(&hand_cmp("="), &l, "A", None),
        "= strict-false"
    );
}

#[test]
fn skill_domain_does_not_leak_into_hand() {
    // A SKILL declaration must not affect a HandSizeCompare (and vice-versa).
    let g = state_with(&[declare("SKILL", "GREATER")]);
    // hand `<` resolves on real sizes, not forced by the SKILL override.
    let base_lt = conditions::holds(&hand_cmp("<"), &state_with(&[]), "A", None);
    assert_eq!(
        conditions::holds(&hand_cmp("<"), &g, "A", None),
        base_lt,
        "SKILL override must not touch HandSizeCompare"
    );
}
