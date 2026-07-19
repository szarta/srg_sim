//! Turn-phase gating (task #105 / La Fenix): the `DuringTurn{who}` condition and its
//! use to gate a continuous per-count buff to "during your opponent's turn". Driven
//! against bull_fae_fresh (no gimmick blank), with Fire cards + La Fenix's buff
//! spliced onto A so the buff's magnitude and phase-gating can be observed.

use serde_json::{json, Value};
use srg_core::conditions::{self};
use srg_core::ir::Condition;
use srg_core::state::GameState;
use std::path::PathBuf;

fn fire_card(i: usize) -> Value {
    json!({
        "atk_type": "Strike",
        "db_uuid": format!("fire{i}"),
        "effects": [],
        "finish_bonuses": {},
        "name": format!("Fireball {i}"),
        "number": 1,
        "play_order": "Lead",
        "raw_text": "",
        "tags": []
    })
}

/// bull_fae_fresh with `active` set, La Fenix's `DuringTurn{OPP}`-gated per-count
/// buff (P/T/A +1 per Ash/Fire/Burn in play, Max +3) on A's gimmick, and `fires`
/// Fire cards in A's in-play.
fn state_with(active: &str, fires: usize) -> GameState {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/state/positions.json");
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let mut state = doc["positions"][0]["state"].clone();
    state["active"] = json!(active);
    let buff = |skill: &str| {
        json!({
            "@type": "BuffSkill", "skill": skill, "delta": 1, "who": "SELF",
            "duration": "WHILE_IN_PLAY", "target_highest": false, "per_crowd": false,
            "cap": 3, "per_zone": "IN_PLAY",
            "per": {"@type": "CardFilter", "number": null, "atk_type": null,
                    "play_order": null, "tag": null, "name": null, "raw": null,
                    "name_contains": ["Ash", "Fire", "Burn"], "text_contains": []}
        })
    };
    let eff = json!({
        "@type": "Effect",
        "trigger": {"@type": "Static"},
        "condition": {"@type": "DuringTurn", "who": "OPP"},
        "actions": [buff("Power"), buff("Technique"), buff("Agility")],
        "duration": "WHILE_IN_PLAY",
        "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
        "raw_clause": "test", "source": "gimmick", "optional": false
    });
    state["players"]["A"]["competitor"]["effects"]
        .as_array_mut()
        .unwrap()
        .push(eff);
    let ip = state["players"]["A"]["in_play"].as_array_mut().unwrap();
    for i in 0..fires {
        ip.push(fire_card(i));
    }
    GameState::from_dict(state).expect("from_dict")
}

fn power(gs: &GameState) -> i64 {
    let holds = |c: &Condition| conditions::holds(c, gs, "A", None);
    gs.effective_stats("A", Some(&holds))
        .get(srg_core::ir::Skill::Power)
}

#[test]
fn during_turn_reads_the_active_player() {
    let opp_turn = state_with("B", 0);
    assert!(
        conditions::holds(&during("OPP"), &opp_turn, "A", None),
        "OPP turn"
    );
    assert!(
        !conditions::holds(&during("SELF"), &opp_turn, "A", None),
        "not SELF turn"
    );
    let own_turn = state_with("A", 0);
    assert!(
        !conditions::holds(&during("OPP"), &own_turn, "A", None),
        "not OPP turn"
    );
    assert!(
        conditions::holds(&during("SELF"), &own_turn, "A", None),
        "SELF turn"
    );
}

#[test]
fn buff_applies_only_during_opponents_turn() {
    // A's base Power (bull) is 10. Two Fire cards -> +2 during the opponent's turn.
    let base = power(&state_with("A", 2)); // own turn: gate false, no buff
    let boosted = power(&state_with("B", 2)); // opponent's turn: +2
    assert_eq!(
        boosted - base,
        2,
        "two Fire cards give +2 only on the opponent's turn"
    );
}

#[test]
fn per_count_buff_caps_at_three() {
    let base = power(&state_with("A", 5));
    let boosted = power(&state_with("B", 5)); // five Fire cards, capped at +3
    assert_eq!(boosted - base, 3, "capped at Max +3");
}

fn during(who: &str) -> Condition {
    serde_json::from_value(json!({"@type": "DuringTurn", "who": who})).unwrap()
}
