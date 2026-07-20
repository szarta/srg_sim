//! OR-play-order card filtering (task #79 / Cherie Von Danish): `CardFilter.
//! play_orders` expresses "1 **Lead or Follow Up** with 'Roll' in the name" — a
//! disjunction the single-valued `play_order` cannot state. Empty stays "no
//! constraint", so every filter authored before schema v41 is unaffected.

use serde_json::json;
use srg_core::cards::Card;
use srg_core::conditions::card_matches;
use srg_core::ir::CardFilter;

fn card(name: &str, order: &str) -> Card {
    serde_json::from_value(json!({
        "atk_type": "Strike",
        "db_uuid": format!("u-{name}-{order}"),
        "effects": [],
        "finish_bonuses": {},
        "name": name,
        "number": 1,
        "play_order": order,
        "raw_text": "",
        "tags": []
    }))
    .expect("card")
}

/// A filter with the given `play_orders` list and no other constraint.
fn orders_filter(orders: &[&str]) -> CardFilter {
    serde_json::from_value(json!({
        "@type": "CardFilter", "number": null, "atk_type": null, "play_order": null,
        "play_orders": orders, "tag": null, "name": null, "raw": null,
        "name_contains": [], "text_contains": []
    }))
    .expect("filter")
}

#[test]
fn matches_any_listed_order_and_rejects_the_rest() {
    let filt = orders_filter(&["Lead", "Followup"]);
    assert!(card_matches(&card("a", "Lead"), &filt));
    assert!(card_matches(&card("b", "Followup"), &filt));
    assert!(
        !card_matches(&card("c", "Finish"), &filt),
        "Finish is not listed"
    );
}

#[test]
fn an_empty_list_constrains_nothing() {
    // The pre-v41 default: every filter already in the corpus carries `[]`, so the
    // new field must be inert for all of them.
    let filt = orders_filter(&[]);
    for order in ["Lead", "Followup", "Finish", "None"] {
        assert!(
            card_matches(&card("x", order), &filt),
            "{order} should match"
        );
    }
}

#[test]
fn it_ands_with_the_other_criteria() {
    // Cherie's real filter: (Lead OR Followup) AND a name substring.
    let filt: CardFilter = serde_json::from_value(json!({
        "@type": "CardFilter", "number": null, "atk_type": null, "play_order": null,
        "play_orders": ["Lead", "Followup"], "tag": null, "name": null, "raw": null,
        "name_contains": ["Roll", "Chop", "Cut"], "text_contains": []
    }))
    .expect("filter");
    assert!(card_matches(&card("Barrel Roll", "Lead"), &filt));
    assert!(card_matches(&card("Chop Block", "Followup"), &filt));
    // Right order, wrong name.
    assert!(!card_matches(&card("Dropkick", "Lead"), &filt));
    // Right name, wrong order.
    assert!(!card_matches(&card("Barrel Roll", "Finish"), &filt));
}

#[test]
fn a_singular_play_order_still_narrows_a_disjunction() {
    // Both fields set is legal and ANDs, even though authors set only one.
    let filt: CardFilter = serde_json::from_value(json!({
        "@type": "CardFilter", "number": null, "atk_type": null, "play_order": "Lead",
        "play_orders": ["Lead", "Followup"], "tag": null, "name": null, "raw": null,
        "name_contains": [], "text_contains": []
    }))
    .expect("filter");
    assert!(card_matches(&card("a", "Lead"), &filt));
    assert!(
        !card_matches(&card("b", "Followup"), &filt),
        "play_order still binds"
    );
}
