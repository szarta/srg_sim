//! Spotlight text-copy family (task #110): the `CopyText` action + re-home
//! semantics (`GameState::copied_effects`). A `CopyText` clause with duration
//! `WHILE_IN_DISCARD` on a card sitting in its owner's discard pile pulls the
//! effects of every matching source card and re-homes them onto the copier,
//! active for as long as the copy clause is — regardless of the source effect's
//! own `WHILE_IN_PLAY` duration. Two worked cases:
//!
//!   * #9 "The D-Roll": copies each Spotlight you have IN PLAY — the copied
//!     finish bonus stacks on top of the source's own (Example A).
//!   * #2 "A Trip to the Upside Down": copies the OPPONENT's Spotlight in THEIR
//!     discard, then blanks it (Example B) — a paired `BlankText`.
//!
//! Driven off positions.json (bull_fae_fresh) with hand-built cards dealt to the
//! relevant zones, asserting against the public `copied_effects` /
//! `is_text_blanked` surfaces.

use serde_json::{json, Value};
use srg_core::ir::{Action, Skill};
use srg_core::state::GameState;
use std::path::PathBuf;

fn card_filter_spotlight() -> Value {
    json!({
        "@type": "CardFilter", "number": null, "atk_type": null, "play_order": null,
        "tag": "Spotlight", "name": null, "raw": null,
        "name_contains": [], "text_contains": []
    })
}

/// A Spotlight card whose only text is a `WHILE_IN_PLAY` `FinishBonus`.
fn spotlight_card(side: &str, i: usize, skill: &str, delta: i64) -> Value {
    json!({
        "atk_type": "Grapple",
        "db_uuid": format!("spot{side}{i}"),
        "name": format!("Spotlight {side}{i}"),
        "number": 2,
        "play_order": "Lead",
        "raw_text": "",
        "tags": ["Spotlight"],
        "finish_bonuses": {},
        "effects": [{
            "@type": "Effect",
            "trigger": {"@type": "Static"},
            "condition": {"@type": "Always"},
            "actions": [{"@type": "FinishBonus", "skill": skill, "delta": delta}],
            "duration": "WHILE_IN_PLAY",
            "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
            "raw_clause": "test spotlight bonus",
            "source": "card",
            "optional": false
        }]
    })
}

/// A copier card carrying `actions` in a single `WHILE_IN_DISCARD` clause.
fn copier_card(uuid: &str, actions: Value) -> Value {
    json!({
        "atk_type": "Grapple",
        "db_uuid": uuid,
        "name": "Copier",
        "number": 29,
        "play_order": "Finish",
        "raw_text": "",
        "tags": [],
        "finish_bonuses": {},
        "effects": [{
            "@type": "Effect",
            "trigger": {"@type": "Static"},
            "condition": {"@type": "Always"},
            "actions": actions,
            "duration": "WHILE_IN_DISCARD",
            "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
            "raw_clause": "test copy clause",
            "source": "card",
            "optional": false
        }]
    })
}

fn base_state() -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/state/positions.json");
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    doc["positions"][0]["state"].clone()
}

fn push(state: &mut Value, side: &str, zone: &str, card: Value) {
    state["players"][side][zone]
        .as_array_mut()
        .expect("zone array")
        .push(card);
}

/// Total `FinishBonus` delta for `skill` across a slice of effects.
fn finish_delta(effects: &[srg_core::ir::Effect], skill: Skill) -> i64 {
    effects
        .iter()
        .flat_map(|e| &e.actions)
        .filter_map(|a| match a {
            Action::FinishBonus { skill: s, delta } if *s == skill => Some(*delta),
            _ => None,
        })
        .sum()
}

// --- Example A: #9 The D-Roll (SELF / IN_PLAY copy) -------------------------

#[test]
fn copies_own_in_play_spotlight_from_discard() {
    let mut state = base_state();
    // A Spotlight in A's play worth +2 Agility, and the copier in A's discard.
    push(
        &mut state,
        "A",
        "in_play",
        spotlight_card("A", 0, "Agility", 2),
    );
    let copy = json!([{ "@type": "CopyText", "who": "SELF", "zone": "IN_PLAY",
        "copy_tags": false, "selector": card_filter_spotlight() }]);
    push(&mut state, "A", "discard", copier_card("droll", copy));
    let gs = GameState::from_dict(state).expect("from_dict");

    // The copier re-homes the Spotlight's +2 Agility from the discard.
    let copied = gs.copied_effects("A");
    assert_eq!(
        finish_delta(&copied, Skill::Agility),
        2,
        "copied bonus present"
    );
}

#[test]
fn copy_clause_inert_while_copier_is_in_play() {
    // Same board, but the copier is IN PLAY, not in the discard: a WHILE_IN_DISCARD
    // copy clause is not active from play, so nothing is copied (zone gate).
    let mut state = base_state();
    push(
        &mut state,
        "A",
        "in_play",
        spotlight_card("A", 0, "Agility", 2),
    );
    let copy = json!([{ "@type": "CopyText", "who": "SELF", "zone": "IN_PLAY",
        "copy_tags": false, "selector": card_filter_spotlight() }]);
    push(&mut state, "A", "in_play", copier_card("droll", copy));
    let gs = GameState::from_dict(state).expect("from_dict");

    assert!(
        gs.copied_effects("A").is_empty(),
        "no copy while copier in play"
    );
}

#[test]
fn nothing_to_copy_yields_no_effects() {
    let mut state = base_state();
    let copy = json!([{ "@type": "CopyText", "who": "SELF", "zone": "IN_PLAY",
        "copy_tags": false, "selector": card_filter_spotlight() }]);
    push(&mut state, "A", "discard", copier_card("droll", copy));
    let gs = GameState::from_dict(state).expect("from_dict");

    assert!(
        gs.copied_effects("A").is_empty(),
        "no Spotlight in play to copy"
    );
}

// --- Example B: #2 A Trip to the Upside Down (OPP / DISCARD copy + blank) ----

#[test]
fn copies_and_blanks_opponent_discard_spotlight() {
    let mut state = base_state();
    // Opponent's Spotlight sits in B's discard, worth +3 Grapple.
    push(
        &mut state,
        "B",
        "discard",
        spotlight_card("B", 0, "Grapple", 3),
    );
    let copy = json!([
        { "@type": "CopyText", "who": "OPP", "zone": "DISCARD",
          "copy_tags": false, "selector": card_filter_spotlight() },
        { "@type": "BlankText", "who": "OPP", "selector": card_filter_spotlight() }
    ]);
    push(&mut state, "A", "discard", copier_card("trip", copy));
    let gs = GameState::from_dict(state).expect("from_dict");

    // A steals the +3 Grapple from the opponent's discarded Spotlight...
    let copied = gs.copied_effects("A");
    assert_eq!(
        finish_delta(&copied, Skill::Grapple),
        3,
        "opponent's bonus copied"
    );

    // ...and blanks the opponent's copy (the paired BlankText, WHILE_IN_DISCARD).
    let victim = spotlight_card("B", 0, "Grapple", 3);
    let victim: srg_core::cards::Card = serde_json::from_value(victim).unwrap();
    assert!(
        gs.is_text_blanked(&victim, "B"),
        "opponent Spotlight blanked"
    );
}

#[test]
fn stacks_when_you_also_hold_the_same_spotlight() {
    // Re-home means the copy stacks ON TOP of the source's own bonus (Example A's
    // point): a +2 Agility Spotlight in play, copied from the discard, totals +4.
    let mut state = base_state();
    push(
        &mut state,
        "A",
        "in_play",
        spotlight_card("A", 0, "Agility", 2),
    );
    let copy = json!([{ "@type": "CopyText", "who": "SELF", "zone": "IN_PLAY",
        "copy_tags": false, "selector": card_filter_spotlight() }]);
    push(&mut state, "A", "discard", copier_card("droll", copy));
    let gs = GameState::from_dict(state).expect("from_dict");

    // The in-play card contributes +2 on its own; the copier re-homes another +2.
    // `copied_effects` returns only the copied layer, so it is +2 here; the total
    // +4 is what `standing_effects` (in-play + copied) yields at the finish.
    assert_eq!(finish_delta(&gs.copied_effects("A"), Skill::Agility), 2);
}
