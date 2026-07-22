//! rules_text -> IR parser regression sample (originally task #71 oracle parity).
//!
//! `fixtures/parser/clauses.json` is a curated 113-card sample pairing each case's
//! RAW text with the Effect IR the parser produces (overrides + grammar +
//! Unsupported), plus a `coverage_golden`. It was frozen from the Python
//! `rules_parser.parse_text` during migration; post-oracle-retirement it is a Rust
//! regression golden (like `cards.ir.json`) whose OUTPUTS are refreshed on
//! legitimate coverage gains via `srg parser-fixture` (`invoke parser-fixture`),
//! keeping the curated INPUTS. The parser must reproduce each list value-identically
//! — the grammar rules, their order, the clause splitter, frequency headers,
//! metadata skipping, and the override table all matching.

use serde_json::Value;
use srg_core::ir::EffectSource;
use srg_core::parser::{coverage, load_overrides, parse_text, CoverageRecord, Overrides};
use std::path::PathBuf;

fn manifest(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel)
}

fn overrides() -> Overrides {
    let json = std::fs::read_to_string(manifest("overrides.ir.json")).expect("read overrides");
    load_overrides(&json).expect("parse overrides")
}

fn source_of(tag: &str) -> EffectSource {
    match tag {
        "card" => EffectSource::Card,
        "gimmick" => EffectSource::Gimmick,
        "entrance" => EffectSource::Entrance,
        other => panic!("unknown source {other:?}"),
    }
}

fn cases() -> Vec<Value> {
    let text = std::fs::read_to_string(manifest("fixtures/parser/clauses.json")).expect("read");
    let doc: Value = serde_json::from_str(&text).expect("valid parser fixture");
    doc["cases"].as_array().expect("cases array").clone()
}

#[test]
fn parse_text_matches_oracle() {
    let ov = overrides();
    let cases = cases();
    assert!(!cases.is_empty(), "no parser cases");
    let (mut grammar_cases, mut override_cases, mut unsupported_effects) = (0, 0, 0);

    for case in &cases {
        let db_uuid = case["db_uuid"].as_str();
        let source = source_of(case["source"].as_str().unwrap());
        let text = case["text"].as_str().unwrap();
        let expected = case["expected"].as_array().unwrap();

        let got: Vec<Value> = parse_text(text, source, db_uuid, Some(&ov))
            .iter()
            .map(|e| serde_json::to_value(e).unwrap())
            .collect();

        assert_eq!(
            got.len(),
            expected.len(),
            "effect count for {db_uuid:?}: text={text:?}"
        );
        for (i, (g, e)) in got.iter().zip(expected).enumerate() {
            assert_eq!(g, e, "effect {i} for {db_uuid:?}: text={text:?}");
        }

        // Coverage bookkeeping for the assertions below.
        if db_uuid.is_some_and(|u| ov.contains_key(u)) {
            override_cases += 1;
        } else if !expected.is_empty() {
            grammar_cases += 1;
        }
        unsupported_effects += expected
            .iter()
            .filter(|e| {
                e["actions"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|x| x["@type"] == "Unsupported"))
            })
            .count();
    }

    // The corpus must exercise all three parser layers.
    assert!(override_cases > 0, "no override cases exercised");
    assert!(grammar_cases > 0, "no grammar cases exercised");
    assert!(unsupported_effects > 0, "no Unsupported sentinel exercised");
}

#[test]
fn coverage_report_matches_oracle() {
    let ov = overrides();
    let text = std::fs::read_to_string(manifest("fixtures/parser/clauses.json")).expect("read");
    let doc: Value = serde_json::from_str(&text).expect("valid fixture");

    let records_json = doc["coverage_records"].as_array().unwrap();
    let records: Vec<CoverageRecord> = records_json
        .iter()
        .map(|r| CoverageRecord {
            text: r["rules_text"].as_str().unwrap(),
            db_uuid: r["db_uuid"].as_str(),
        })
        .collect();
    let report = coverage(&records, Some(&ov));

    let golden = &doc["coverage_golden"];
    assert_eq!(
        report.total as i64,
        golden["total"].as_i64().unwrap(),
        "total"
    );
    assert_eq!(
        report.grammar as i64,
        golden["grammar"].as_i64().unwrap(),
        "grammar"
    );
    assert_eq!(
        report.override_ as i64,
        golden["override"].as_i64().unwrap(),
        "override"
    );
    assert_eq!(
        report.unsupported as i64,
        golden["unsupported"].as_i64().unwrap(),
        "unsupported"
    );
    // top_unparsed: shape + count, count-desc with first-seen tie-break.
    let got_top: Vec<Value> = report
        .top_unparsed
        .iter()
        .map(|(s, c)| serde_json::json!([s, c]))
        .collect();
    assert_eq!(
        &got_top,
        golden["top_unparsed"].as_array().unwrap(),
        "top_unparsed"
    );
}

/// Hand-disruption grammar (task #39): bury from a player's HAND. These clauses
/// are absent from the six oracle reference decks, so they are asserted directly
/// against the whole-DB grammar rather than the frozen oracle fixture.
#[test]
fn hand_bury_grammar() {
    fn only_action(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        let v = serde_json::to_value(&effs[0]).unwrap();
        v["actions"].as_array().unwrap()[0].clone()
    }
    fn bury(a: &Value) -> (String, i64, bool, bool, String) {
        (
            a["who"].as_str().unwrap().to_owned(),
            a["count"].as_i64().unwrap(),
            a["random"].as_bool().unwrap(),
            a["choose"].as_bool().unwrap(),
            a["source"].as_str().unwrap().to_owned(),
        )
    }

    // Opponent hand-bury: plain / randomly / N-random / look-and-choose.
    let a = only_action("Your opponent buries 2 cards in their hand.");
    assert_eq!(a["@type"], "Bury");
    assert_eq!(bury(&a), ("OPP".into(), 2, false, false, "HAND".into()));
    assert_eq!(
        bury(&only_action(
            "Your opponent randomly buries 1 card in their hand."
        )),
        ("OPP".into(), 1, true, false, "HAND".into())
    );
    assert_eq!(
        bury(&only_action(
            "Your opponent buries 1 random card in their hand."
        )),
        ("OPP".into(), 1, true, false, "HAND".into())
    );
    assert_eq!(
        bury(&only_action(
            "Look at your opponent's hand, choose 1 card and bury it."
        )),
        ("OPP".into(), 1, false, true, "HAND".into())
    );

    // Self hand-bury.
    assert_eq!(
        bury(&only_action("Bury 1 card in your hand.")),
        ("SELF".into(), 1, false, false, "HAND".into())
    );

    // Look-and-choose discard from the opponent's hand (Discard{choose,who:OPP}).
    let d = only_action("Look at your opponent's hand, choose 1 card and discard it.");
    assert_eq!(d["@type"], "Discard");
    assert_eq!(d["who"], "OPP");
    assert_eq!(d["choose"], true);
    assert_eq!(d["count"], 1);
    // Filtered form carries the play-order + attack-type selector.
    let d = only_action("Look at your opponent's hand, choose 1 Follow Up Strike and discard it.");
    assert_eq!(d["@type"], "Discard");
    assert_eq!(d["choose"], true);
    assert_eq!(d["selector"]["play_order"], "Followup");
    assert_eq!(d["selector"]["atk_type"], "Strike");

    // Draw-then-bury-self rider: Draw then Bury{SELF,HAND}, independent counts.
    let effs = parse_text(
        "Draw 2 cards, then bury 1 card in your hand.",
        EffectSource::Card,
        None,
        None,
    );
    let acts = serde_json::to_value(&effs[0]).unwrap()["actions"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(acts.len(), 2);
    assert_eq!(acts[0]["@type"], "Draw");
    assert_eq!(acts[0]["n"], 2);
    assert_eq!(
        bury(&acts[1]),
        ("SELF".into(), 1, false, false, "HAND".into())
    );

    // Each player: two Bury actions (SELF then OPP).
    let effs = parse_text(
        "Each player buries 1 card in their hand.",
        EffectSource::Card,
        None,
        None,
    );
    let acts = serde_json::to_value(&effs[0]).unwrap()["actions"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(acts.len(), 2);
    assert_eq!(bury(&acts[0]).0, "SELF");
    assert_eq!(bury(&acts[1]).0, "OPP");

    // Conditional prefix carries a HasInPlay gate + OnPlay trigger.
    let effs = parse_text(
        "If you have another Follow Up in play, your opponent buries 1 card in their hand.",
        EffectSource::Card,
        None,
        None,
    );
    let e = serde_json::to_value(&effs[0]).unwrap();
    assert_eq!(e["condition"]["@type"], "HasInPlay");
    assert_eq!(e["condition"]["filter"]["play_order"], "Followup");
    assert_eq!(e["trigger"]["@type"], "OnPlay");
    assert_eq!(bury(&e["actions"][0]).0, "OPP");
}

/// Draw-rider grammar (task #49): deck-position, conditional, and compare draws.
/// Absent from the six-deck sample except "Draw the bottom card", so asserted
/// against the whole-DB grammar directly.
#[test]
fn draw_rider_grammar() {
    fn parse1(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        serde_json::to_value(&effs[0]).unwrap()
    }

    // Deck-position: bottom card, and top+bottom (two draws).
    let e = parse1("Draw the bottom card of your deck.");
    assert_eq!(e["actions"][0]["@type"], "Draw");
    assert_eq!(e["actions"][0]["source"], "BOTTOM");
    assert_eq!(e["actions"][0]["n"], 1);
    let e = parse1("Draw the top and bottom card of your deck.");
    assert_eq!(e["actions"].as_array().unwrap().len(), 2);
    assert_eq!(e["actions"][0]["source"], "TOP");
    assert_eq!(e["actions"][1]["source"], "BOTTOM");

    // Conditional (HasInPlay gate, OnPlay): another <atk>/<order> in play.
    let e = parse1("If you have another Strike in play, draw 2 cards.");
    assert_eq!(e["trigger"]["@type"], "OnPlay");
    assert_eq!(e["condition"]["@type"], "HasInPlay");
    assert_eq!(e["condition"]["filter"]["atk_type"], "Strike");
    assert_eq!(e["actions"][0]["n"], 2);
    let e = parse1("If you have another Follow Up in play, draw 1 card.");
    assert_eq!(e["condition"]["filter"]["play_order"], "Followup");

    // Skill compare: same-skill (vs_skill null) and cross-skill (vs_skill set).
    let e =
        parse1("If your Power skill is greater than your opponent's Power skill, draw 2 cards.");
    assert_eq!(e["condition"]["@type"], "SkillCompare");
    assert_eq!(e["condition"]["skill"], "Power");
    assert_eq!(e["condition"]["vs"], "OPP_SAME");
    assert_eq!(e["condition"]["vs_skill"], Value::Null);
    let e =
        parse1("If your Grapple skill is greater than your opponent's Power skill, draw 3 cards.");
    assert_eq!(e["condition"]["skill"], "Grapple");
    assert_eq!(e["condition"]["vs_skill"], "Power");

    // "instead" replacement form must NOT parse (stays Unsupported).
    let e = parse1(
        "If your Power skill is greater than your opponent's Power skill, draw 2 cards instead.",
    );
    assert_eq!(e["actions"][0]["@type"], "Unsupported");

    // Hand-size: fewer in hand than opponent.
    let e = parse1("If you have fewer cards in your hand than your opponent, draw 1 card.");
    assert_eq!(e["condition"]["@type"], "HandSizeCompare");
    assert_eq!(e["condition"]["cmp"], "<");
    assert_eq!(e["condition"]["vs"], "OPP");

    // Per-count draw for each X the OPPONENT has in play.
    let e = parse1("Draw 1 card for each Lead your opponent has in play.");
    assert_eq!(e["actions"][0]["per"]["play_order"], "Lead");
    assert_eq!(e["actions"][0]["per_who"], "OPP");

    // OnRoll draws: standing "when you / your opponent roll <S>, draw N".
    let e = parse1("When you roll Technique for your turn roll, draw 1 card.");
    assert_eq!(e["trigger"]["@type"], "OnRoll");
    assert_eq!(e["trigger"]["skill"], "Technique");
    assert_eq!(e["trigger"]["who"], "SELF");
    assert_eq!(e["actions"][0]["@type"], "Draw");
    let e = parse1("When your opponent rolls Power for their turn roll, draw 2 cards.");
    assert_eq!(e["trigger"]["who"], "OPP");
    assert_eq!(e["trigger"]["skill"], "Power");
    assert_eq!(e["actions"][0]["n"], 2);
    assert_eq!(e["actions"][0]["who"], "SELF");
}

/// Finish-roll rider grammar (task #49): rolled-skill and base-roll-gated bonuses.
#[test]
fn finish_rider_grammar() {
    fn frb(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        serde_json::to_value(&effs[0]).unwrap()["actions"][0].clone()
    }

    // Self rolled-skill bonus (either=false, signed delta).
    let a = frb("If you roll Grapple for your Finish roll, it is +1.");
    assert_eq!(a["@type"], "FinishRollBonus");
    assert_eq!(a["when_skill"], "Grapple");
    assert_eq!(a["either"], false);
    assert_eq!(a["delta"], 1);

    // Base-roll gate: "N or less" -> when_base_le; "N or greater" -> when_base_ge.
    let a = frb("If your Finish roll is 6 or less, it is +2.");
    assert_eq!(a["when_base_le"], 6);
    assert_eq!(a["when_base_ge"], Value::Null);
    assert_eq!(a["delta"], 2);
    let a = frb("If your Finish roll is 8 or greater, it is -3.");
    assert_eq!(a["when_base_ge"], 8);
    assert_eq!(a["when_base_le"], Value::Null);
    assert_eq!(a["delta"], -3);

    // "Your <S> skill is +N during Finish rolls" == rolled-skill FinishRollBonus.
    let a = frb("Your Grapple skill is +2 during Finish rolls.");
    assert_eq!(a["@type"], "FinishRollBonus");
    assert_eq!(a["when_skill"], "Grapple");
    assert_eq!(a["delta"], 2);

    // Per-count in-play Finish bonus (order/atk filter).
    let a = frb("Your Finish rolls are +1 for each Strike you have in play.");
    assert_eq!(a["delta"], 1);
    assert_eq!(a["per"]["atk_type"], "Strike");
    assert_eq!(a["per_zone"], "IN_PLAY");
    // Name-based / capped per-counts are declined (stay Unsupported).
    let a =
        frb("Your Finish roll is +1 for each card you have in play with \"Slammin\" in the name.");
    assert_eq!(a["@type"], "Unsupported");
}

/// In-play-removal grammar (task #121): discard an opponent's in-play card.
#[test]
fn in_play_removal_grammar() {
    fn parse1(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        serde_json::to_value(&effs[0]).unwrap()
    }

    // "Discard N" and "Choose N ... and discard it/them" are the same RemoveFromPlay.
    let e = parse1("Discard 1 card your opponent has in play.");
    assert_eq!(e["actions"][0]["@type"], "RemoveFromPlay");
    assert_eq!(e["actions"][0]["who"], "OPP");
    assert_eq!(e["actions"][0]["count"], 1);
    assert_eq!(e["actions"][0]["choose"], false);
    let e = parse1("Choose 2 cards your opponent has in play and discard them.");
    assert_eq!(e["actions"][0]["@type"], "RemoveFromPlay");
    assert_eq!(e["actions"][0]["count"], 2);

    // Order/atk-filtered form.
    let e = parse1("Discard 1 Lead your opponent has in play.");
    assert_eq!(e["actions"][0]["selector"]["play_order"], "Lead");

    // Conditional (HasInPlay, OnPlay) and OnRoll-gated variants.
    let e = parse1("If you have another Strike in play, choose 1 card your opponent has in play and discard it.");
    assert_eq!(e["trigger"]["@type"], "OnPlay");
    assert_eq!(e["condition"]["@type"], "HasInPlay");
    assert_eq!(e["condition"]["filter"]["atk_type"], "Strike");
    assert_eq!(e["actions"][0]["@type"], "RemoveFromPlay");
    let e = parse1("When you roll Power for your turn roll, choose 1 card your opponent has in play and discard it.");
    assert_eq!(e["trigger"]["@type"], "OnRoll");
    assert_eq!(e["trigger"]["skill"], "Power");
    assert_eq!(e["actions"][0]["@type"], "RemoveFromPlay");
}

/// Recur-from-discard grammar (task #122): selector-filtered add/shuffle/put + gates.
#[test]
fn recur_from_discard_grammar() {
    fn a1(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        serde_json::to_value(&effs[0]).unwrap()
    }

    // AddFromDiscard: any / order (plural) / atk / name-substring.
    assert_eq!(
        a1("Add 1 card from your discard pile to your hand.")["actions"][0]["@type"],
        "AddFromDiscard"
    );
    let e = a1("Add 2 Finishes from your discard pile to your hand.");
    assert_eq!(e["actions"][0]["filter"]["play_order"], "Finish");
    let e = a1("Add 1 card with \"Steel Chain\" in the name from your discard pile to your hand.");
    assert_eq!(e["actions"][0]["filter"]["name_contains"][0], "Steel Chain");
    // "stop" is now a CardFilter constraint (is_stop) via the stop-filter enabler.
    let e = a1("Add 1 stop from your discard pile to your hand.");
    assert_eq!(e["actions"][0]["@type"], "AddFromDiscard");
    assert_eq!(e["actions"][0]["filter"]["is_stop"], true);

    // "Take N ... shuffle them into your deck" == ShuffleIntoDeck.
    assert_eq!(
        a1("Take 2 cards from your discard pile and shuffle them into your deck.")["actions"][0]
            ["@type"],
        "ShuffleIntoDeck"
    );

    // Filtered RecurToDeckTop.
    let e = a1("Put 1 Submission from your discard pile on top of your deck.");
    assert_eq!(e["actions"][0]["@type"], "RecurToDeckTop");
    assert_eq!(e["actions"][0]["selector"]["atk_type"], "Submission");

    // Conditional (HasInPlay gate, OnPlay).
    let e = a1("If you have another Submission in play, shuffle 2 cards from your discard pile into your deck.");
    assert_eq!(e["trigger"]["@type"], "OnPlay");
    assert_eq!(e["condition"]["@type"], "HasInPlay");
    assert_eq!(e["condition"]["filter"]["atk_type"], "Submission");
    assert_eq!(e["actions"][0]["@type"], "ShuffleIntoDeck");
    let e = a1(
        "If you have another Follow Up in play, add 1 Finish from your discard pile to your hand.",
    );
    assert_eq!(e["condition"]["filter"]["play_order"], "Followup");
    assert_eq!(e["actions"][0]["filter"]["play_order"], "Finish");
}

/// Stop-card filter enabler: "stop" as a CardFilter (is_stop) flows through
/// per-count, recur, and HasInPlay-gated grammar.
#[test]
fn stop_filter_grammar() {
    fn a1(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        serde_json::to_value(&effs[0]).unwrap()
    }

    // Per-count draw for each stop you / your opponent have in play.
    let e = a1("Draw 1 card for each stop you have in play.");
    assert_eq!(e["actions"][0]["per"]["is_stop"], true);
    assert_eq!(e["actions"][0]["per_who"], "SELF");
    let e = a1("Draw 1 card for each stop your opponent has in play.");
    assert_eq!(e["actions"][0]["per"]["is_stop"], true);
    assert_eq!(e["actions"][0]["per_who"], "OPP");

    // "If your opponent has a stop in play, draw N" -> HasInPlay(OPP, is_stop).
    let e = a1("If your opponent has a stop in play, draw 2 cards.");
    assert_eq!(e["condition"]["@type"], "HasInPlay");
    assert_eq!(e["condition"]["who"], "OPP");
    assert_eq!(e["condition"]["filter"]["is_stop"], true);
    assert_eq!(e["actions"][0]["@type"], "Draw");
}
