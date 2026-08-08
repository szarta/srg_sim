//! Self-exclude for a "for each OTHER <X> you have in play" per-count buff (task #131,
//! effect_ir v105). `BuffSkill.per_excludes_self` drops the SOURCE card (the one carrying
//! the buff) from the tally, so a card that itself matches the filter doesn't count
//! itself. Verified through the derived-stats fold on a two-card board.

use serde_json::{json, Value};
use srg_core::ir::Skill;
use srg_core::state::GameState;
use std::path::PathBuf;

fn base_state() -> GameState {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/state/positions.json");
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    GameState::from_dict(doc["positions"][0]["state"].clone()).expect("from_dict")
}

/// A Grapple card; `source` also carries a Static "+1 Power for each [other] Grapple you
/// have in play" buff whose `per_excludes_self` is `excl`.
fn grapple_card(uuid: &str, buff: bool, excl: bool) -> Value {
    let effects = if buff {
        json!([{
            "@type": "Effect",
            "trigger": {"@type": "Static"},
            "condition": {"@type": "Always"},
            "duration": "WHILE_IN_PLAY",
            "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
            "raw_clause": "test",
            "source": "card",
            "optional": false,
            "actions": [{
                "@type": "BuffSkill", "skill": "Power", "delta": 1, "who": "SELF",
                "duration": "WHILE_IN_PLAY", "target_highest": false, "target_lowest": false,
                "per_crowd": false, "cap": null,
                "per": {"@type": "CardFilter", "atk_type": "Grapple"},
                "per_zone": "IN_PLAY", "per_excludes_self": excl
            }]
        }])
    } else {
        json!([])
    };
    json!({
        "atk_type": "Grapple", "db_uuid": uuid, "effects": effects,
        "finish_bonuses": {}, "name": uuid, "number": 1, "play_order": "Lead", "tags": []
    })
}

/// Put two Grapple cards on A's board: a buff source + one other Grapple. With
/// `per_excludes_self` the buff counts only the OTHER card (+1); without it, the source
/// counts itself too (+2). B is untouched.
fn power_with_exclude(excl: bool) -> i64 {
    let mut doc: Value = {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/state/positions.json");
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    };
    let st = &mut doc["positions"][0]["state"];
    st["players"]["A"]["in_play"] = json!([
        grapple_card("source-grapple", true, excl),
        grapple_card("other-grapple", false, false),
    ]);
    let state = GameState::from_dict(st.clone()).expect("from_dict");
    state.effective_stat("A", Skill::Power, None)
}

#[test]
fn per_excludes_self_drops_the_source_card_from_the_count() {
    let base = base_state().players["A"].competitor.stats.get(Skill::Power);

    // Excluded: only the OTHER Grapple counts -> +1.
    assert_eq!(
        power_with_exclude(true),
        base + 1,
        "for each OTHER Grapple: source card is not counted"
    );
    // Not excluded: source + other both count -> +2 (the control).
    assert_eq!(
        power_with_exclude(false),
        base + 2,
        "for each Grapple (no exclude): source counts itself too"
    );
}
