//! Skill-requirement cards blank live when the owner's effective skill drops below
//! the requirement (task #132; user rule 2026-08-03). A card carrying a
//! `requirements:` block (e.g. the #13/#14/#15 "equal-8" stops, `min_<skill>: 8`)
//! is BLANK — all text inert — whenever the owner's DERIVED skill is below ANY of
//! its thresholds, and un-blanks the moment the skill is restored. Asserted against
//! the public `GameState::is_text_blanked`, driven off positions.json (bull_fae_fresh:
//! A's stats are Agility 5, Technique 6, Strike 7, Submission 8, Grapple 9, Power 10).

use serde_json::{json, Value};
use srg_core::cards::Card;
use srg_core::state::GameState;
use std::path::PathBuf;

fn base_state() -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/state/positions.json");
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    doc["positions"][0]["state"].clone()
}

/// A skill-requirement card (`skill_requirements` = the `reqs` list) whose only text
/// is a `WHILE_IN_PLAY` FinishBonus, so blanking has something to suppress.
fn req_card(reqs: Value) -> Card {
    serde_json::from_value(json!({
        "atk_type": "Strike", "db_uuid": "req", "name": "Req Card", "number": 13,
        "play_order": "Followup", "raw_text": "", "tags": ["SkillRequirement"],
        "finish_bonuses": {}, "skill_requirements": reqs,
        "effects": [{
            "@type": "Effect", "trigger": {"@type": "Static"}, "condition": {"@type": "Always"},
            "actions": [{"@type": "FinishBonus", "skill": "Strike", "delta": 2}],
            "duration": "WHILE_IN_PLAY",
            "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
            "raw_clause": "req card", "source": "card", "optional": false
        }]
    }))
    .expect("req card")
}

/// Splice a Static `+delta`-to-`skill` BuffSkill onto A's competitor gimmick.
fn buff_a(state: &mut Value, skill: &str, delta: i64) {
    let eff = json!({
        "@type": "Effect", "trigger": {"@type": "Static"}, "condition": {"@type": "Always"},
        "actions": [{
            "@type": "BuffSkill", "skill": skill, "delta": delta, "who": "SELF",
            "duration": "WHILE_IN_PLAY", "target_highest": false, "per_crowd": false,
            "cap": null, "per": null, "per_zone": "IN_PLAY"
        }],
        "duration": "WHILE_IN_PLAY",
        "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
        "raw_clause": "buff", "source": "gimmick", "optional": false
    });
    state["players"]["A"]["competitor"]["effects"]
        .as_array_mut()
        .unwrap()
        .push(eff);
}

#[test]
fn blanks_when_effective_skill_below_requirement() {
    let gs = GameState::from_dict(base_state()).expect("from_dict");
    // A's Strike is 7 < 8 — the equal-8 Strike card is blank.
    assert!(gs.is_text_blanked(&req_card(json!([{"skill": "Strike", "min": 8}])), "A"));
    // A's Grapple 9 >= 8 and Power 10 >= 10 (boundary) — those cards are online.
    assert!(!gs.is_text_blanked(&req_card(json!([{"skill": "Grapple", "min": 8}])), "A"));
    assert!(!gs.is_text_blanked(&req_card(json!([{"skill": "Power", "min": 10}])), "A"));
    // A's Power 10 < 16 — a raised requirement blanks it.
    assert!(gs.is_text_blanked(&req_card(json!([{"skill": "Power", "min": 16}])), "A"));
}

#[test]
fn multi_requirement_blanks_if_any_threshold_unmet() {
    let gs = GameState::from_dict(base_state()).expect("from_dict");
    // "Field of Fire"-style AND: Grapple(9)>=8 AND Power(10)>=10 both hold — online.
    let both_met = json!([{"skill": "Grapple", "min": 8}, {"skill": "Power", "min": 10}]);
    assert!(!gs.is_text_blanked(&req_card(both_met), "A"));
    // Grapple(9)>=8 holds but Strike(7)<8 fails — ANY unmet blanks the whole card.
    let one_unmet = json!([{"skill": "Grapple", "min": 8}, {"skill": "Strike", "min": 8}]);
    assert!(gs.is_text_blanked(&req_card(one_unmet), "A"));
}

#[test]
fn un_blanks_live_when_a_buff_restores_the_skill() {
    // The check reads DERIVED (effective) stats, not base: a +2 Strike buff lifts A's
    // Strike 7 -> 9, so the equal-8 Strike card that was blank is now online.
    let card = req_card(json!([{"skill": "Strike", "min": 8}]));

    let plain = GameState::from_dict(base_state()).expect("from_dict");
    assert!(plain.is_text_blanked(&card, "A"), "blank at base Strike 7");

    let mut buffed = base_state();
    buff_a(&mut buffed, "Strike", 2);
    let buffed = GameState::from_dict(buffed).expect("from_dict");
    assert!(
        !buffed.is_text_blanked(&card, "A"),
        "un-blanked once effective Strike reaches 9"
    );
}

#[test]
fn a_plain_card_without_requirements_is_never_requirement_blanked() {
    let gs = GameState::from_dict(base_state()).expect("from_dict");
    // Empty requirements — the requirement path never blanks it (Strike 7 is irrelevant).
    assert!(!gs.is_text_blanked(&req_card(json!([])), "A"));
}
