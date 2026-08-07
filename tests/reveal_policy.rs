//! The reveal-decision heuristic (task #131): at a `reveal` decision the HeuristicPolicy
//! exposes the card that leaks the LEAST information — priority: already-known > dead
//! equal-8 stop (#13-15, own req-skill stat below the opponent's) > non-stop Lead >
//! non-stop Follow Up > Lead Stop > Finish > Follow Up Stop > anything else. Empty deck:
//! the hand is fully inferable from public zones, so anything goes. Driven off
//! positions.json (bull_fae_fresh: A Strike 7 < B Strike 8, so a #13 Strike-req is dead).

use serde_json::{json, Value};
use srg_core::policy::build_policy;
use srg_core::state::GameState;
use std::path::PathBuf;

fn base_state() -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/state/positions.json");
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    doc["positions"][0]["state"].clone()
}

/// A hand card. `stop` adds a Stop action; `req` (skill, min) adds a skill requirement.
fn card(uuid: &str, number: i64, order: &str, stop: bool, req: Option<(&str, i64)>) -> Value {
    let mut effects = vec![];
    if stop {
        effects.push(json!({
            "@type": "Effect", "trigger": {"@type": "Static"}, "condition": {"@type": "Always"},
            "actions": [{"@type": "Stop", "order": "Followup", "atk_type": "Grapple",
                         "source_is_skillreq": false, "even_unstoppable": false}],
            "duration": "INSTANT",
            "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
            "raw_clause": "stop", "source": "card", "optional": false
        }));
    }
    let mut c = json!({
        "db_uuid": uuid, "name": uuid, "number": number, "atk_type": "Strike",
        "play_order": order, "finish_bonuses": {}, "tags": [], "raw_text": "", "effects": effects
    });
    if let Some((skill, min)) = req {
        c["skill_requirements"] = json!([{"skill": skill, "min": min}]);
        c["tags"] = json!(["SkillRequirement"]);
    }
    c
}

fn opt(uuid: &str) -> Value {
    json!({"kind": "reveal", "card": uuid, "number": 0, "order": "Lead"})
}

/// Run the heuristic `reveal` decision over `hand`, returning the chosen db_uuid.
fn choose_reveal(hand: Vec<Value>, revealed: Vec<&str>, empty_deck: bool) -> String {
    let mut s = base_state();
    let uuids: Vec<Value> = hand
        .iter()
        .map(|c| opt(c["db_uuid"].as_str().unwrap()))
        .collect();
    s["players"]["A"]["hand"] = json!(hand);
    s["players"]["A"]["revealed_hand"] = json!(revealed);
    if empty_deck {
        s["players"]["A"]["deck"] = json!([]);
    } else {
        s["players"]["A"]["deck"] = json!([card("d1", 5, "Lead", false, None)]);
    }
    let mut gs = GameState::from_dict(s).expect("from_dict");
    let mut pol = build_policy("heuristic").unwrap();
    let chosen = pol.choose("reveal", &uuids, &mut gs, "A").unwrap();
    chosen["card"].as_str().unwrap().to_owned()
}

#[test]
fn reveals_the_least_informative_card_by_priority() {
    // Non-stop Lead (#7, rank 3) vs Finish (#28, rank 6) -> reveal the Lead.
    let got = choose_reveal(
        vec![
            card("finish", 28, "Finish", false, None),
            card("lead", 7, "Lead", false, None),
        ],
        vec![],
        false,
    );
    assert_eq!(got, "lead");

    // An already-revealed card (rank 1) beats everything — zero new information.
    let got = choose_reveal(
        vec![
            card("lead", 7, "Lead", false, None),
            card("known", 8, "Lead", false, None),
        ],
        vec!["known"],
        false,
    );
    assert_eq!(got, "known");

    // A dead equal-8 stop (#13 Strike-req; A Strike 7 < B Strike 8) (rank 2) beats a
    // plain non-stop Lead (rank 3).
    let got = choose_reveal(
        vec![
            card("lead", 7, "Lead", false, None),
            card("dead13", 13, "Followup", true, Some(("Strike", 8))),
        ],
        vec![],
        false,
    );
    assert_eq!(got, "dead13");
}

#[test]
fn empty_deck_reveals_anything() {
    // With no deck, the whole hand is inferable from public zones, so the heuristic
    // takes the first option regardless of category (here a Finish it would normally
    // avoid revealing).
    let got = choose_reveal(
        vec![
            card("finish", 28, "Finish", false, None),
            card("lead", 7, "Lead", false, None),
        ],
        vec![],
        true,
    );
    assert_eq!(got, "finish");
}
