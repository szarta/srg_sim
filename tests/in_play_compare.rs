//! Cross-board count compare (task #79 / Snake Pitt V3): the `InPlayCompare`
//! condition and its use to gate a continuous buff on "when your target has more
//! Strikes in play [than you]". Driven against bull_fae_fresh (positions.json)
//! with plain Strike cards dealt to each board and Snake Pitt's Agility buff
//! spliced onto A's gimmick, so the gate can be observed as the counts cross.

use serde_json::{json, Value};
use srg_core::conditions::{self};
use srg_core::ir::{Condition, Skill};
use srg_core::state::GameState;
use std::path::PathBuf;

fn strike_card(side: &str, i: usize) -> Value {
    json!({
        "atk_type": "Strike",
        "db_uuid": format!("strk{side}{i}"),
        "effects": [],
        "finish_bonuses": {},
        "name": format!("Jab {side}{i}"),
        "number": 1,
        "play_order": "Lead",
        "raw_text": "",
        "tags": []
    })
}

/// bull_fae_fresh with Snake Pitt V3's `InPlayCompare{OPP > SELF}`-gated
/// `BuffSkill(Agility,+1)` on A's gimmick, `opp` Strike cards on B's board, and
/// `own` Strike cards on A's board.
fn state_with(own: usize, opp: usize) -> GameState {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/state/positions.json");
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let mut state = doc["positions"][0]["state"].clone();
    let eff = json!({
        "@type": "Effect",
        "trigger": {"@type": "Static"},
        "condition": {
            "@type": "InPlayCompare", "who": "OPP", "vs_who": "SELF", "cmp": ">",
            "filter": {"@type": "CardFilter", "number": null, "atk_type": "Strike",
                       "play_order": null, "tag": null, "name": null, "raw": null,
                       "name_contains": [], "text_contains": []}
        },
        "actions": [{
            "@type": "BuffSkill", "skill": "Agility", "delta": 1, "who": "SELF",
            "duration": "WHILE_IN_PLAY", "target_highest": false, "per_crowd": false,
            "cap": null, "per": null, "per_zone": "IN_PLAY"
        }],
        "duration": "WHILE_IN_PLAY",
        "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
        "raw_clause": "test", "source": "gimmick", "optional": false
    });
    state["players"]["A"]["competitor"]["effects"]
        .as_array_mut()
        .unwrap()
        .push(eff);
    for (side, n) in [("A", own), ("B", opp)] {
        let ip = state["players"][side]["in_play"].as_array_mut().unwrap();
        for i in 0..n {
            ip.push(strike_card(side, i));
        }
    }
    GameState::from_dict(state).expect("from_dict")
}

fn agility(gs: &GameState) -> i64 {
    let holds = |c: &Condition| conditions::holds(c, gs, "A", None);
    gs.effective_stats("A", Some(&holds)).get(Skill::Agility)
}

fn cond(who: &str, vs_who: &str, cmp: &str) -> Condition {
    serde_json::from_value(json!({
        "@type": "InPlayCompare", "who": who, "vs_who": vs_who, "cmp": cmp,
        "filter": {"@type": "CardFilter", "number": null, "atk_type": "Strike",
                   "play_order": null, "tag": null, "name": null, "raw": null,
                   "name_contains": [], "text_contains": []}
    }))
    .unwrap()
}

#[test]
fn holds_only_when_target_has_strictly_more() {
    // OPP(B) strikes vs SELF(A) strikes, read from A's POV.
    let more = state_with(1, 3); // opp 3 > self 1
    let equal = state_with(2, 2); // tie: "more" is false
    let fewer = state_with(3, 1); // opp fewer
    assert!(conditions::holds(
        &cond("OPP", "SELF", ">"),
        &more,
        "A",
        None
    ));
    assert!(!conditions::holds(
        &cond("OPP", "SELF", ">"),
        &equal,
        "A",
        None
    ));
    assert!(!conditions::holds(
        &cond("OPP", "SELF", ">"),
        &fewer,
        "A",
        None
    ));
}

#[test]
fn buff_applies_only_when_opponent_has_more_strikes() {
    // Same board size on both sides isolates the gate from any strike-count side
    // effects: the only difference is which side holds more.
    let no_buff = agility(&state_with(2, 2)); // tie -> gate false
    let buffed = agility(&state_with(1, 3)); // opp has more -> +1 Agility
    assert_eq!(
        buffed - no_buff,
        1,
        "opponent's strike lead gives +1 Agility"
    );
}

#[test]
fn direction_matters() {
    // The mirror ("you have more") reads who=SELF, vs_who=OPP.
    let self_more = state_with(3, 1);
    assert!(conditions::holds(
        &cond("SELF", "OPP", ">"),
        &self_more,
        "A",
        None
    ));
    assert!(!conditions::holds(
        &cond("OPP", "SELF", ">"),
        &self_more,
        "A",
        None
    ));
}
