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

    // "Each player discards N cards from their hand" (non-random): two Discard actions
    // (SELF then OPP), each the hand owner's own choice (random=false, choose=false).
    let acts = serde_json::to_value(
        &parse_text(
            "Each player discards 2 cards from their hand.",
            EffectSource::Card,
            None,
            None,
        )[0],
    )
    .unwrap()["actions"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(acts.len(), 2);
    assert_eq!(acts[0]["@type"], "Discard");
    assert_eq!(acts[0]["who"], "SELF");
    assert_eq!(acts[0]["count"], 2);
    assert_eq!(acts[0]["random"], false);
    assert_eq!(acts[0]["choose"], false);
    assert_eq!(acts[1]["who"], "OPP");

    // "Each player discards their hand": two discard-all Discards (whole hand).
    let acts = serde_json::to_value(
        &parse_text(
            "Each player discards their hand.",
            EffectSource::Card,
            None,
            None,
        )[0],
    )
    .unwrap()["actions"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(acts.len(), 2);
    assert_eq!(acts[0]["all"], true);
    assert_eq!(acts[0]["who"], "SELF");
    assert_eq!(acts[1]["all"], true);
    assert_eq!(acts[1]["who"], "OPP");

    // "Each player reveals N card(s) in their hand" -> two Reveal actions (fog-of-war).
    let acts = serde_json::to_value(
        &parse_text(
            "Each player reveals 1 card in their hand.",
            EffectSource::Card,
            None,
            None,
        )[0],
    )
    .unwrap()["actions"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(acts.len(), 2);
    assert_eq!(acts[0]["@type"], "Reveal");
    assert_eq!(acts[0]["who"], "SELF");
    assert_eq!(acts[0]["count"], 1);
    assert_eq!(acts[1]["who"], "OPP");

    // "Each player discards the bottom card of their deck" -> two MillDeck{BOTTOM}.
    let acts = serde_json::to_value(
        &parse_text(
            "Each player discards the bottom card of their deck.",
            EffectSource::Card,
            None,
            None,
        )[0],
    )
    .unwrap()["actions"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(acts.len(), 2);
    assert_eq!(acts[0]["@type"], "MillDeck");
    assert_eq!(acts[0]["who"], "SELF");
    assert_eq!(acts[0]["count"], 1);
    assert_eq!(acts[0]["from"], "BOTTOM");
    assert_eq!(acts[1]["who"], "OPP");

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

    // "Bury N [<selector>] cards in ANY/EITHER player's discard pile" -> Bury.choose
    // (the actor picks from both piles). Bare "cards" = any; a typed middle sets the
    // selector; "either" is a synonym for "any"; the "up to"/apostrophe variants parse.
    let a = only_action("Bury 2 cards in any player's discard pile.");
    assert_eq!(a["@type"], "Bury");
    assert_eq!(a["choose"], true);
    assert_eq!(a["count"], 2);
    assert_eq!(a["selector"]["atk_type"], Value::Null);
    let a = only_action("Bury 1 Grapple in either player's discard pile.");
    assert_eq!(a["choose"], true);
    assert_eq!(a["selector"]["atk_type"], "Grapple");
    // Apostrophe-less "players" and "up to" both parse.
    let a = only_action("Bury up to 2 cards in any players discard pile.");
    assert_eq!(a["choose"], true);
    assert_eq!(a["count"], 2);
}

/// Schema-v83 grammar families (Cardona): the match-no-DQ condition gate, per-count
/// bury "for each … in play", and shuffle-a-card-you-have-in-play. Asserted against
/// the whole-DB grammar directly (absent from the frozen sample decks).
#[test]
fn v83_grammar_families() {
    fn only(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        serde_json::to_value(&effs[0]).unwrap()
    }

    // --- match-no-DQ condition gate ---
    let e = only("If this match has no disqualifications, your next turn roll is +1.");
    assert_eq!(e["condition"]["@type"], "MatchHasNoDisqualifications");
    assert_eq!(e["actions"][0]["@type"], "ModifyRoll");
    assert_eq!(e["actions"][0]["when"], "NEXT");
    // Case-insensitive "No Disqualifications" + a Finish-roll bonus.
    let e = only("If this match has No Disqualifications, your Finish roll is +1.");
    assert_eq!(e["condition"]["@type"], "MatchHasNoDisqualifications");
    assert_eq!(e["actions"][0]["@type"], "FinishRollBonus");
    // "+N to <Skill>" gated → FinishRollBonus{when_skill} (a conditional FinishBonus
    // would be summed unconditionally into the finish-bonus map).
    let e = only("If this match has no disqualifications, +5 to Power.");
    assert_eq!(e["condition"]["@type"], "MatchHasNoDisqualifications");
    assert_eq!(e["actions"][0]["@type"], "FinishRollBonus");
    assert_eq!(e["actions"][0]["when_skill"], "Power");
    assert_eq!(e["actions"][0]["delta"], 5);

    // --- bury per-count "for each … in play" ---
    let a = only("Bury 1 card in your opponent's discard pile for each Strike you have in play.")
        ["actions"][0]
        .clone();
    assert_eq!(a["@type"], "Bury");
    assert_eq!(a["who"], "OPP");
    assert_eq!(a["source"], "DISCARD");
    assert_eq!(a["per"]["atk_type"], "Strike");
    assert_eq!(a["per_who"], "SELF");
    // Opponent hand-bury per Lead; "randomly" sets the random flag.
    let a = only("Your opponent buries 1 card in their hand for each Lead you have in play.")
        ["actions"][0]
        .clone();
    assert_eq!(a["source"], "HAND");
    assert_eq!(a["who"], "OPP");
    assert_eq!(a["per"]["play_order"], "Lead");
    // Name-filter variant.
    let a = only(
        "Your opponent randomly buries 1 card in their hand for each card you have in play with \"Hammer\" in the name.",
    )["actions"][0]
        .clone();
    assert_eq!(a["random"], true);
    assert_eq!(a["per"]["name_contains"][0], "Hammer");

    // --- shuffle a card you have in play into your deck ---
    let a = only("Shuffle 1 Follow Up you have in play into your deck.")["actions"][0].clone();
    assert_eq!(a["@type"], "ShuffleIntoDeck");
    assert_eq!(a["source"], "IN_PLAY");
    assert_eq!(a["selector"]["play_order"], "Followup");
    // "other card" → any card, still IN_PLAY.
    let a = only("Shuffle 1 other card you have in play into your deck.")["actions"][0].clone();
    assert_eq!(a["source"], "IN_PLAY");
    assert_eq!(a["selector"]["play_order"], Value::Null);
}

/// DQ-CAUSE family grammar (task #94): the "if stopped, you lose via
/// disqualification" self-loss and its casing / conditional-escape / pay-or-lose
/// variants. Asserted against the whole-DB grammar directly.
#[test]
fn dq_cause_grammar() {
    fn one(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        serde_json::to_value(&effs[0]).unwrap()
    }
    fn is_dq_loss(a: &Value) -> bool {
        a["@type"] == "LoseBy" && a["kind"] == "DISQUALIFICATION" && a["who"] == "SELF"
    }

    // Plain self-loss, tolerant of casing / punctuation / "this card is" / plural.
    for text in [
        "If stopped, you lose the match via disqualification.",
        "If stopped, you lose the match via Disqualification.",
        "If stopped you lose the match via disqualification.",
        "If this card is stopped, you lose the match via disqualification.",
        "If stopped, you lose the match via Disqualifications.",
    ] {
        let e = one(text);
        assert_eq!(e["trigger"]["@type"], "OnStop", "{text:?}");
        assert_eq!(e["trigger"]["dir"], "YOURS", "{text:?}");
        assert_eq!(e["condition"]["@type"], "Always", "{text:?}");
        assert!(is_dq_loss(&e["actions"][0]), "{text:?}");
    }

    // "unless <cond>" — the loss is gated by Not(cond); the escape delegates to the
    // shared condition parser (hand size, crowd meter).
    let e = one("If stopped, unless you have 10 or more cards in hand, you lose the match via disqualification.");
    assert_eq!(e["condition"]["@type"], "Not");
    assert_eq!(e["condition"]["item"]["@type"], "HandSizeCompare");
    assert!(is_dq_loss(&e["actions"][0]));

    let e =
        one("If stopped, unless the Crowd Meter is 2 or greater, you lose the match via disqualification.");
    assert_eq!(e["condition"]["item"]["@type"], "CrowdMeterCompare");

    // An escape the condition parser cannot map stays Unsupported (not silently dropped).
    let effs = parse_text(
        "If stopped, unless the moon is full, you lose the match via disqualification.",
        EffectSource::Card,
        None,
        None,
    );
    let e = serde_json::to_value(&effs[0]).unwrap();
    assert_eq!(e["actions"][0]["@type"], "Unsupported");

    // Pay-or-lose: a Choice between discarding and taking the loss.
    let e = one(
        "If stopped, discard 1 card from your hand or you lose the match via disqualification.",
    );
    assert_eq!(e["actions"][0]["@type"], "Choice");
    let opts = e["actions"][0]["options"].as_array().unwrap();
    assert_eq!(opts.len(), 2);
    assert_eq!(opts[0]["actions"][0]["@type"], "Discard");
    assert!(is_dq_loss(&opts[1]["actions"][0]));

    // Pay-AND-lose: both the discard and the loss happen (an AND rider).
    let e = one(
        "If stopped, discard 1 card from your hand and you lose the match via disqualification.",
    );
    let acts = e["actions"].as_array().unwrap();
    assert_eq!(acts[0]["@type"], "Discard");
    assert!(is_dq_loss(&acts[1]));

    // Typed-discard cost: discard a Strike or take the loss.
    let e = one(
        "If stopped, unless you discard 1 Strike from your hand, you lose the match via disqualification.",
    );
    assert_eq!(e["actions"][0]["@type"], "Choice");
    let opts = e["actions"][0]["options"].as_array().unwrap();
    assert_eq!(opts[0]["actions"][0]["@type"], "Discard");
    assert_eq!(opts[0]["actions"][0]["selector"]["atk_type"], "Strike");
    assert!(is_dq_loss(&opts[1]["actions"][0]));

    // OR-name-list escape ("unless you have a card in play with X/Y/Z in the name").
    let e = one(
        "If stopped, you lose the match via disqualification unless you have a card in play with \"Block\", \"Clothesline\", or \"Tackle\" in the name.",
    );
    assert_eq!(e["condition"]["@type"], "Not");
    assert_eq!(e["condition"]["item"]["@type"], "HasInPlay");
    assert_eq!(
        e["condition"]["item"]["filter"]["name_contains"]
            .as_array()
            .unwrap()
            .len(),
        3
    );

    // "and you do not have X in play" — a negated in-play gate on the loss.
    let e = one(
        "If stopped, and you do not have \"Call to the Crowd\" in play, you lose the match via disqualification.",
    );
    assert_eq!(e["condition"]["@type"], "Not");
    assert_eq!(e["condition"]["item"]["@type"], "HasInPlay");
    assert!(is_dq_loss(&e["actions"][0]));

    // Discard-plus-bury-this-card cost, or lose.
    let e = one(
        "If stopped, discard 1 card from your hand and bury this card or lose the match via disqualification.",
    );
    let opts = e["actions"][0]["options"].as_array().unwrap();
    assert_eq!(opts[0]["actions"][0]["@type"], "Discard");
    assert_eq!(opts[0]["actions"][1]["@type"], "BuryThisCard");
    assert!(is_dq_loss(&opts[1]["actions"][0]));

    // Randomly-bury-your-hand cost, or lose.
    let e = one("If stopped, randomly bury your hand or you lose the match via disqualification.");
    let opts = e["actions"][0]["options"].as_array().unwrap();
    assert_eq!(opts[0]["actions"][0]["@type"], "Bury");
    assert_eq!(opts[0]["actions"][0]["source"], "HAND");
    assert_eq!(opts[0]["actions"][0]["random"], true);

    // Breakout-roll loss: OnBreakoutRoll(Opp) gated on the rolled value.
    let e = one(
        "If your opponent rolls 10 for their Breakout roll, you lose the match via disqualification.",
    );
    assert_eq!(e["trigger"]["@type"], "OnBreakoutRoll");
    assert_eq!(e["trigger"]["who"], "OPP");
    assert_eq!(e["condition"]["@type"], "RollValue");
    assert_eq!(e["condition"]["value"], 10);
    assert!(is_dq_loss(&e["actions"][0]));

    // Competitor-identity escape ("unless you are <name>").
    let e = one(
        "If stopped and you are not Paul Walter Hauser, you lose the match via disqualification.",
    );
    assert_eq!(e["condition"]["item"]["@type"], "CompetitorIs");
    assert_eq!(
        e["condition"]["item"]["name_contains"][0],
        "Paul Walter Hauser"
    );

    // Hit-this-turn escape.
    let e = one(
        "If stopped, unless you hit another card this turn, you lose the match via disqualification.",
    );
    assert_eq!(e["condition"]["item"]["@type"], "HitThisTurn");

    // Spotlight-count escape (tag filter).
    let e = one(
        "If stopped, unless you have at least 6 Spotlight cards in play you lose the match via disqualification.",
    );
    assert_eq!(e["condition"]["item"]["@type"], "HasInPlay");
    assert_eq!(e["condition"]["item"]["filter"]["tag"], "Spotlight");
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

    // "Add the bottom N cards of your deck to your hand" is the same bottom-draw,
    // phrased as an add (a "Choose one:" option on Booty Drop Chop and kin).
    let e = parse1("Add the bottom 2 cards of your deck to your hand.");
    assert_eq!(e["actions"][0]["@type"], "Draw");
    assert_eq!(e["actions"][0]["source"], "BOTTOM");
    assert_eq!(e["actions"][0]["n"], 2);

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

    // "higher than" is a synonym for "greater than" (cmp still Gt). Unlocks the
    // stop-body cards via the generic gate rule -> stop_condition.
    let e = parse1("If your Agility skill is higher than your opponent's Agility skill, stop any Follow Up Grapple.");
    assert_eq!(e["condition"]["@type"], "SkillCompare");
    assert_eq!(e["condition"]["skill"], "Agility");
    assert_eq!(e["condition"]["cmp"], ">");
    assert_eq!(e["actions"][0]["@type"], "Stop");

    // "greater than or equal to" promotes the comparator Gt -> Ge (value null =
    // self >= opp, no delta).
    let e = parse1("If your Power skill is greater than or equal to your opponent's Power skill, stop any Submission.");
    assert_eq!(e["condition"]["cmp"], ">=");
    assert_eq!(e["condition"]["vs"], "OPP_SAME");
    assert_eq!(e["condition"]["value"], Value::Null);
    assert_eq!(e["actions"][0]["@type"], "Stop");

    // Self-vs-self: two of YOUR OWN skills (the #13/#14/#15 "equal-8" stops). vs =
    // SELF_OTHER (no "opponent's"), vs_skill = the right operand's skill.
    let e = parse1("If your Agility skill is greater than your Strike skill, stop any Grapple.");
    assert_eq!(e["condition"]["@type"], "SkillCompare");
    assert_eq!(e["condition"]["skill"], "Agility");
    assert_eq!(e["condition"]["vs"], "SELF_OTHER");
    assert_eq!(e["condition"]["vs_skill"], "Strike");
    assert_eq!(e["condition"]["cmp"], ">");
    assert_eq!(e["actions"][0]["@type"], "Stop");
    assert_eq!(e["actions"][0]["atk_type"], "Grapple");
    // The bare first operand ("your Strike", no "skill") parses the same way.
    let e = parse1("If your Strike is greater than your Agility skill, stop any Grapple.");
    assert_eq!(e["condition"]["vs"], "SELF_OTHER");
    assert_eq!(e["condition"]["skill"], "Strike");
    assert_eq!(e["condition"]["vs_skill"], "Agility");

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

    // Name-descriptor per-count draw: the "with 'X' in the name" qualifier trails
    // "you have in play", so it routes through in_play_filter (name-substring filter).
    let e = parse1("Draw 1 card for each card you have in play with \"Table\" in the name.");
    assert_eq!(e["actions"][0]["@type"], "Draw");
    assert_eq!(e["actions"][0]["per"]["name_contains"][0], "Table");
    assert_eq!(e["actions"][0]["per_who"], "SELF");

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

/// Multiline "Choose one:" header (Booty Drop Chop): the header sits on its own line
/// and its options are the FOLLOWING clauses, composed into a single `Choice` (not
/// each option fired independently, which would draw both piles).
#[test]
fn multiline_choose_one_composes_a_choice() {
    let effs = parse_text(
        "Your maximum handsize is +3.\nChoose one:\nDraw 3 cards.\nAdd the bottom 3 cards of your deck to your hand.",
        EffectSource::Card,
        None,
        None,
    );
    // Two effects only: the max-handsize buff, then ONE Choice (the header clause is
    // consumed, not left as a separate Unsupported, and the options are not independent).
    assert_eq!(effs.len(), 2, "max-handsize + one composed Choice");
    let e = serde_json::to_value(&effs[1]).unwrap();
    assert_eq!(e["actions"][0]["@type"], "Choice");
    let opts = e["actions"][0]["options"].as_array().unwrap();
    assert_eq!(opts.len(), 2);
    assert_eq!(opts[0]["actions"][0]["@type"], "Draw");
    assert_eq!(opts[0]["actions"][0]["source"], "TOP");
    assert_eq!(opts[0]["actions"][0]["n"], 3);
    assert_eq!(opts[1]["actions"][0]["@type"], "Draw");
    assert_eq!(opts[1]["actions"][0]["source"], "BOTTOM");
    assert_eq!(opts[1]["actions"][0]["n"], 3);

    // A "Choose one:" whose options don't both parse falls through — the header stays
    // Unsupported rather than silently dropping the choice.
    let effs = parse_text(
        "Choose one:\nDraw 3 cards.\nDo something the parser cannot model at all.",
        EffectSource::Card,
        None,
        None,
    );
    let types: Vec<String> = effs
        .iter()
        .map(|e| {
            serde_json::to_value(e).unwrap()["actions"][0]["@type"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert!(
        types.contains(&"Unsupported".to_owned()),
        "unparseable options leave the header Unsupported, got {types:?}"
    );
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

/// Flip-cards grammar (task #119): "up to", opponent, and each-player variants
/// all reuse the existing `Flip { n, who }` node.
#[test]
fn flip_grammar() {
    fn acts(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        serde_json::to_value(&effs[0]).unwrap()["actions"].clone()
    }

    // Bare and "up to" both flip N from your own deck.
    for text in ["Flip 2 cards.", "Flip up to 2 cards."] {
        let a = acts(text);
        assert_eq!(a.as_array().unwrap().len(), 1, "one action for {text:?}");
        assert_eq!(a[0]["@type"], "Flip");
        assert_eq!(a[0]["n"], 2);
        assert_eq!(a[0]["who"], "SELF");
    }

    // Opponent-targeted flip.
    let a = acts("Your opponent flips 1 card.");
    assert_eq!(a[0]["@type"], "Flip");
    assert_eq!(a[0]["n"], 1);
    assert_eq!(a[0]["who"], "OPP");

    // "Each player" fans out to two Flips (self then opp), like each-player draw.
    let a = acts("Each player flips 3 cards.");
    assert_eq!(a.as_array().unwrap().len(), 2);
    assert_eq!(a[0]["who"], "SELF");
    assert_eq!(a[1]["who"], "OPP");
    assert_eq!(a[0]["n"], 3);
    assert_eq!(a[1]["n"], 3);

    // Per-count: "for each <order> you have in play" -> Flip.per / per_who=SELF.
    let a = acts("Flip 1 card for each Follow Up you have in play.");
    assert_eq!(a[0]["@type"], "Flip");
    assert_eq!(a[0]["who"], "SELF");
    assert_eq!(a[0]["per"]["play_order"], "Followup");
    assert_eq!(a[0]["per_who"], "SELF");

    // "for each other <S>" strips "other"; opponent flips, still counted vs SELF.
    let a = acts("Your opponent flips 2 cards for each other Strike you have in play.");
    assert_eq!(a[0]["who"], "OPP");
    assert_eq!(a[0]["per"]["atk_type"], "Strike");
    assert_eq!(a[0]["per_who"], "SELF");
    assert_eq!(a[0]["n"], 2);
}

/// Flip-until grammar (task #119): "Flip cards until you flip a <X>[, add it to
/// your hand]" reuses `Flip` with the `until` filter + `until_to_hand`.
#[test]
fn flip_until_grammar() {
    fn acts(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        serde_json::to_value(&effs[0]).unwrap()["actions"].clone()
    }

    // "add that <X> to your hand" -> until_to_hand.
    let a = acts("Flip cards until you flip a Submission, add that Submission to your hand.");
    assert_eq!(a[0]["@type"], "Flip");
    assert_eq!(a[0]["until"]["atk_type"], "Submission");
    assert_eq!(a[0]["until_to_hand"], true);
    assert_eq!(a[0]["who"], "SELF");

    // A play-order filter, "add that card" phrasing, and the "your flip" typo.
    let a = acts("Flip cards until your flip a Follow Up, add that card to your hand.");
    assert_eq!(a[0]["until"]["play_order"], "Followup");
    assert_eq!(a[0]["until_to_hand"], true);

    // Bare "until you flip a <X>" (no add) -> until_to_hand=false.
    let a = acts("Flip cards until you flip a Follow Up.");
    assert_eq!(a[0]["until"]["play_order"], "Followup");
    assert_eq!(a[0]["until_to_hand"], false);

    // Stop-card filter flows through the flip-until path too.
    let a = acts("Flip cards until you flip a Stop, add it to your hand.");
    assert_eq!(a[0]["until"]["is_stop"], true);
    assert_eq!(a[0]["until_to_hand"], true);
}

/// Scry-flip grammar (task #119): "Look at/Reveal the top N cards of your deck,
/// add M to your hand and flip the others" -> Scry with rest=FLIP.
#[test]
fn scry_flip_grammar() {
    fn acts(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        serde_json::to_value(&effs[0]).unwrap()["actions"].clone()
    }

    // "Look at" keeps the window private (reveal=false); "and flip the others".
    let a =
        acts("Look at the top 4 cards of your deck, add 2 cards to your hand and flip the others.");
    assert_eq!(a[0]["@type"], "Scry");
    assert_eq!(a[0]["deck"], "SELF");
    assert_eq!(a[0]["top"], 4);
    assert_eq!(a[0]["to_hand"], 2);
    assert_eq!(a[0]["reveal"], false);
    assert_eq!(a[0]["rest"], "FLIP");

    // "Reveal" makes the ids public; "put M in your hand, flip the other".
    let a = acts("Reveal the top 2 cards of your deck, put 1 in your hand, flip the other.");
    assert_eq!(a[0]["@type"], "Scry");
    assert_eq!(a[0]["top"], 2);
    assert_eq!(a[0]["to_hand"], 1);
    assert_eq!(a[0]["reveal"], true);
    assert_eq!(a[0]["rest"], "FLIP");
}

/// Single-card peek with an optional flip (task #119): "Look at the top card of
/// your opponent's deck, you may flip it" -> Scry{top:1, deck:OPP, rest:MayFlip}.
#[test]
fn scry_may_flip_grammar() {
    fn acts(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        serde_json::to_value(&effs[0]).unwrap()["actions"].clone()
    }

    // Opponent's deck, private "look at".
    let a = acts("Look at the top card of your opponent's deck, you may flip it.");
    assert_eq!(a[0]["@type"], "Scry");
    assert_eq!(a[0]["deck"], "OPP");
    assert_eq!(a[0]["top"], 1);
    assert_eq!(a[0]["to_hand"], 0);
    assert_eq!(a[0]["reveal"], false);
    assert_eq!(a[0]["rest"], "MAY_FLIP");

    // Own deck + public "Reveal" variant folds into the same node.
    let a = acts("Reveal the top card of your deck, you may flip it.");
    assert_eq!(a[0]["deck"], "SELF");
    assert_eq!(a[0]["reveal"], true);
    assert_eq!(a[0]["rest"], "MAY_FLIP");
}

/// Compound flip + recur-to-hand (task #119): "Flip N cards, then take/add M
/// <filter> from your discard pile [and add it] to your hand" -> Flip then
/// AddFromDiscard, reusing both existing nodes.
#[test]
fn flip_then_recur_grammar() {
    fn acts(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        serde_json::to_value(&effs[0]).unwrap()["actions"].clone()
    }

    // "take M card ... and add it to your hand" (any-card recur).
    let a = acts("Flip 4 cards, then take 1 card from your discard pile and add it to your hand.");
    assert_eq!(a.as_array().unwrap().len(), 2);
    assert_eq!(a[0]["@type"], "Flip");
    assert_eq!(a[0]["n"], 4);
    assert_eq!(a[1]["@type"], "AddFromDiscard");
    assert_eq!(a[1]["filter"]["atk_type"], Value::Null);
    assert_eq!(a[1]["filter"]["play_order"], Value::Null);

    // "add M <atk> ... to your hand" (typed recur).
    let a = acts("Flip 2 cards, add 1 Grapple from your discard pile to your hand.");
    assert_eq!(a[0]["n"], 2);
    assert_eq!(a[1]["@type"], "AddFromDiscard");
    assert_eq!(a[1]["filter"]["atk_type"], "Grapple");

    // Name-quoted recur filter survives the compound.
    let a = acts("Flip 4 cards, then add 2 cards with \"Lariat\" or \"Clothesline\" in the name from your discard pile to your hand.");
    assert_eq!(a[0]["n"], 4);
    assert_eq!(a[1]["filter"]["name_contains"][0], "Lariat");
}

/// Per-card flip self-trigger (task #119): "If this card is flipped, [you may] add
/// it to your hand" -> OnFlip{SELF} + AddSelfToHand, "you may" on Effect::optional.
#[test]
fn flip_self_to_hand_grammar() {
    fn one(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        serde_json::to_value(&effs[0]).unwrap()
    }

    // Mandatory: comma, not optional.
    let e = one("If this card is flipped, add it to your hand.");
    assert_eq!(e["trigger"]["@type"], "OnFlip");
    assert_eq!(e["trigger"]["who"], "SELF");
    assert_eq!(e["trigger"]["count"], Value::Null);
    assert_eq!(e["actions"][0]["@type"], "AddSelfToHand");
    assert_eq!(e["optional"], false);

    // "you may" (with comma) -> optional.
    let e = one("If this card is flipped, you may add it to your hand.");
    assert_eq!(e["actions"][0]["@type"], "AddSelfToHand");
    assert_eq!(e["optional"], true);

    // "you may" without the comma (the DB has both).
    let e = one("If this card is flipped you may add it to your hand.");
    assert_eq!(e["optional"], true);

    // shuffle-self: "back into your deck" (optional) and the mandatory "from your
    // discard pile back into your deck".
    let e = one("If this card is flipped, you may shuffle it back into your deck.");
    assert_eq!(e["trigger"]["@type"], "OnFlip");
    assert_eq!(e["actions"][0]["@type"], "ShuffleSelfIntoDeck");
    assert_eq!(e["optional"], true);
    let e = one("If this card is flipped, shuffle it from your discard pile back into your deck.");
    assert_eq!(e["actions"][0]["@type"], "ShuffleSelfIntoDeck");
    assert_eq!(e["optional"], false);
    // the "shuffleit" typo (real DB text).
    let e = one("If this card is flipped, you may shuffleit into your deck.");
    assert_eq!(e["actions"][0]["@type"], "ShuffleSelfIntoDeck");

    // play-self: plain, "as an additional card", and the "during your turn" gate.
    let e = one("If this card is flipped, you may play it.");
    assert_eq!(e["actions"][0]["@type"], "PlaySelf");
    assert_eq!(e["condition"]["@type"], "Always");
    assert_eq!(e["optional"], true);
    let e = one("If this card is flipped, you may play it as an additional card this turn.");
    assert_eq!(e["actions"][0]["@type"], "PlaySelf");
    let e = one("If this card is flipped during your turn, you may play it.");
    assert_eq!(e["actions"][0]["@type"], "PlaySelf");
    assert_eq!(e["condition"]["@type"], "DuringTurn");
    assert_eq!(e["condition"]["who"], "SELF");
    // The "During your turn, if this card is flipped …" prefix form.
    let e = one("During your turn, if this card is flipped you may play it.");
    assert_eq!(e["actions"][0]["@type"], "PlaySelf");
    assert_eq!(e["condition"]["@type"], "DuringTurn");
}

/// Provenance-gated flip self-trigger grammar (task #119, schema v87): "flipped by
/// \"<X>\"" -> FlippedByName; "flipped for your Gimmick, <action>" -> FlippedForGimmick.
#[test]
fn flip_provenance_grammar() {
    fn one(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        serde_json::to_value(&effs[0]).unwrap()
    }

    // "flipped by \"<name>\"" -> FlippedByName gate + AddSelfToHand (comma optional,
    // case variance in the DB handled by the CI substring match at eval time).
    let e = one("If this card is flipped by \"Set Up the Steel Chain\", add it to your hand.");
    assert_eq!(e["trigger"]["@type"], "OnFlip");
    assert_eq!(e["condition"]["@type"], "FlippedByName");
    assert_eq!(e["condition"]["names"][0], "Set Up the Steel Chain");
    assert_eq!(e["actions"][0]["@type"], "AddSelfToHand");
    let e = one("If this card is flipped by \"Set up the Table\" add it to your hand.");
    assert_eq!(e["condition"]["names"][0], "Set up the Table");

    // "flipped for your Gimmick, <action>" -> FlippedForGimmick gate; each maps to a
    // distinct existing action.
    let e = one("If this card is flipped for your Gimmick, you may play it.");
    assert_eq!(e["condition"]["@type"], "FlippedForGimmick");
    assert_eq!(e["actions"][0]["@type"], "PlaySelf");
    assert_eq!(e["optional"], true);
    let e = one("If flipped for your Gimmick, you may shuffle your deck.");
    assert_eq!(e["condition"]["@type"], "FlippedForGimmick");
    assert_eq!(e["actions"][0]["@type"], "ShuffleDeck");
    let e = one("If this card is flipped for your Gimmick, your opponent randomly discards 1 card in their hand.");
    assert_eq!(e["condition"]["@type"], "FlippedForGimmick");
    assert_eq!(e["actions"][0]["@type"], "Discard");
    assert_eq!(e["actions"][0]["who"], "OPP");
    assert_eq!(e["actions"][0]["random"], true);
    let e = one("If this card is flipped for your Gimmick your turn roll is +1.");
    assert_eq!(e["condition"]["@type"], "FlippedForGimmick");
    assert_eq!(e["actions"][0]["@type"], "ModifyRoll");
    assert_eq!(e["actions"][0]["delta"], 1);
    assert_eq!(e["actions"][0]["when"], "NEXT");
}

/// Gated reveal+flip grammar (task #119): "If you have another <order> in play, look
/// at the top N cards of your deck; put M in your hand, and flip the others" -> the
/// scry_flip Scry gated on HasInPlay{<order>}.
#[test]
fn gated_reveal_flip_grammar() {
    fn one(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        serde_json::to_value(&effs[0]).unwrap()
    }

    // "another Lead" gate + top-3, put-1, flip others.
    let e = one("If you have another Lead in play, look at the top 3 cards of your deck; put 1 in your hand, and flip the others.");
    assert_eq!(e["condition"]["@type"], "HasInPlay");
    assert_eq!(e["condition"]["filter"]["play_order"], "Lead");
    assert_eq!(e["actions"][0]["@type"], "Scry");
    assert_eq!(e["actions"][0]["top"], 3);
    assert_eq!(e["actions"][0]["to_hand"], 1);
    assert_eq!(e["actions"][0]["rest"], "FLIP");
    assert_eq!(e["actions"][0]["reveal"], false);

    // "another Follow Up" gate + top-4; "put 1 in your hand and flip the others" (no
    // comma before "and").
    let e = one("If you have another Follow Up in play, look at the top 4 cards of your deck; put 1 in your hand and flip the others.");
    assert_eq!(e["condition"]["filter"]["play_order"], "Followup");
    assert_eq!(e["actions"][0]["top"], 4);
    assert_eq!(e["actions"][0]["to_hand"], 1);
    assert_eq!(e["actions"][0]["rest"], "FLIP");
}

/// Flip-pool select grammar (task #119, schema v88): "Flip N cards, [randomly] add M
/// [of the] flipped [<type>] to your hand" -> [Flip, AddFlippedToHand].
#[test]
fn add_flipped_to_hand_grammar() {
    fn acts(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        serde_json::to_value(&effs[0]).unwrap()["actions"].clone()
    }

    // "add M of the flipped cards" -> count=M, any filter, not random.
    let a = acts("Flip 2 cards, add 1 of the flipped cards to your hand.");
    assert_eq!(a[0]["@type"], "Flip");
    assert_eq!(a[0]["n"], 2);
    assert_eq!(a[1]["@type"], "AddFlippedToHand");
    assert_eq!(a[1]["count"], 1);
    assert_eq!(a[1]["filter"]["atk_type"], Value::Null);
    assert_eq!(a[1]["random"], false);

    // "randomly add 1" -> random.
    let a = acts("Flip 2 cards, randomly add 1 of the flipped cards to your hand.");
    assert_eq!(a[1]["random"], true);

    // Typed filter, no "cards" noun, no "of the".
    let a = acts("Flip 3 cards, add 1 flipped Strike to your hand.");
    assert_eq!(a[1]["count"], 1);
    assert_eq!(a[1]["filter"]["atk_type"], "Strike");

    // "all" -> count null (all matching); "Flip 6" without the "cards" noun.
    let a = acts("Flip 6, add all flipped Strikes to your hand.");
    assert_eq!(a[0]["n"], 6);
    assert_eq!(a[1]["count"], Value::Null);
    assert_eq!(a[1]["filter"]["atk_type"], "Strike");
}

/// Standing flip-trigger grammar (task #119, schema v89 on_self split): "When/After you
/// flip [any number of | N or more] cards, [you may] add M of the flipped cards to your
/// hand" -> standing OnFlip (on_self=false) firing AddFlippedToHand.
#[test]
fn standing_flip_add_grammar() {
    fn one(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        serde_json::to_value(&effs[0]).unwrap()
    }

    // "any number" -> count None, on_self false (standing), not random.
    let e = one("When you flip any number of cards, add 1 of the flipped cards to your hand.");
    assert_eq!(e["trigger"]["@type"], "OnFlip");
    assert_eq!(e["trigger"]["count"], Value::Null);
    assert_eq!(e["trigger"]["on_self"], false);
    assert_eq!(e["actions"][0]["@type"], "AddFlippedToHand");
    assert_eq!(e["actions"][0]["random"], false);

    // "randomly" rider.
    let e =
        one("When you flip any number of cards, randomly add 1 of the flipped cards to your hand.");
    assert_eq!(e["actions"][0]["random"], true);

    // "N or more" -> at_least threshold; "you may" -> optional.
    let e = one("After you flip 3 or more cards, you may add 1 of the flipped cards to your hand.");
    assert_eq!(e["trigger"]["count"], 3);
    assert_eq!(e["trigger"]["at_least"], true);
    assert_eq!(e["trigger"]["on_self"], false);
    assert_eq!(e["optional"], true);
    assert_eq!(e["actions"][0]["@type"], "AddFlippedToHand");
}

/// Generic flip trigger-body split (task #119, schema v89): "When/After you flip
/// <count>, <body>" re-parses <body> through the grammar and attaches a standing
/// OnFlip, reusing every body rule.
#[test]
fn flip_trigger_body_split() {
    fn one(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        serde_json::to_value(&effs[0]).unwrap()
    }

    // Body "draw 1 card" (lowercase, mid-sentence) reuses the Draw rule.
    let e = one("After you flip any number of cards, draw 1 card.");
    assert_eq!(e["trigger"]["@type"], "OnFlip");
    assert_eq!(e["trigger"]["count"], Value::Null);
    assert_eq!(e["trigger"]["on_self"], false);
    assert_eq!(e["actions"][0]["@type"], "Draw");

    // Body "your next turn roll is +1" reuses the ModifyRoll rule.
    let e = one("After you flip any number of cards, your next turn roll is +1.");
    assert_eq!(e["actions"][0]["@type"], "ModifyRoll");
    assert_eq!(e["actions"][0]["delta"], 1);

    // "N or more" prefix + opponent-bury body -> at_least threshold trigger + Bury.
    let e = one("When you flip 2 or more cards your opponent buries 1 card in their hand.");
    assert_eq!(e["trigger"]["count"], 2);
    assert_eq!(e["trigger"]["at_least"], true);
    assert_eq!(e["actions"][0]["@type"], "Bury");
    assert_eq!(e["actions"][0]["who"], "OPP");

    // A body with no grammar leaves the whole clause Unsupported.
    let e = one("After you flip any number of cards, contemplate the void.");
    assert_eq!(e["actions"][0]["@type"], "Unsupported");
}

/// Generic roll trigger-body split (task #119): "When you roll <Skill>[ for your turn
/// roll][:,] <body>" re-parses <body> through the grammar with OnRoll{skill} attached.
#[test]
fn roll_trigger_body_split() {
    fn one(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        serde_json::to_value(&effs[0]).unwrap()
    }

    // Comma separator + "your next turn roll is +4" body -> OnRoll{Submission} + ModifyRoll.
    let e = one("When you roll Submission, your next turn roll is +4.");
    assert_eq!(e["trigger"]["@type"], "OnRoll");
    assert_eq!(e["trigger"]["skill"], "Submission");
    assert_eq!(e["trigger"]["who"], "SELF");
    assert_eq!(e["actions"][0]["@type"], "ModifyRoll");
    assert_eq!(e["actions"][0]["delta"], 4);

    // Colon separator + scry body.
    let e = one("When you roll Technique: Look at the top 2 cards of your deck, add 1 to your hand and flip the others.");
    assert_eq!(e["trigger"]["skill"], "Technique");
    assert_eq!(e["actions"][0]["@type"], "Scry");

    // "for your turn roll" is absorbed; the body's own who (OPP) survives the split.
    let e = one(
        "When you roll Submission for your turn roll, your opponent buries 1 card in their hand.",
    );
    assert_eq!(e["trigger"]["@type"], "OnRoll");
    assert_eq!(e["trigger"]["skill"], "Submission");
    assert_eq!(e["actions"][0]["@type"], "Bury");
    assert_eq!(e["actions"][0]["who"], "OPP");

    // A body with no grammar -> whole clause Unsupported (not a stray OnRoll).
    let e = one("When you roll Power, ponder your legacy.");
    assert_eq!(e["actions"][0]["@type"], "Unsupported");
}

/// Multi-skill roll OR (task #119): "When you roll <S1>, … or <Sn>, <body>" -> a single
/// OnRoll{None} effect gated by an Or of RollWasSkill on the named skills.
#[test]
fn multi_skill_roll_body_split() {
    fn one(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        serde_json::to_value(&effs[0]).unwrap()
    }

    // Two-skill "X or Y" -> OnRoll{None} + Or of two RollWasSkill; body ModifyRoll.
    let e =
        one("When you roll Technique or Submission for your turn roll, your next turn roll is +1.");
    assert_eq!(e["trigger"]["@type"], "OnRoll");
    assert_eq!(e["trigger"]["skill"], Value::Null);
    assert_eq!(e["condition"]["@type"], "Or");
    let skills: Vec<&str> = e["condition"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            assert_eq!(c["@type"], "RollWasSkill");
            c["skill"].as_str().unwrap()
        })
        .collect();
    assert_eq!(skills, ["Technique", "Submission"]);
    assert_eq!(e["actions"][0]["@type"], "ModifyRoll");

    // Three-skill "X, Y, or Z" -> three RollWasSkill; the body's own who survives.
    let e = one("When you roll Strike, Submission, or Grapple for your turn roll, your opponent buries 1 card in their hand.");
    assert_eq!(e["condition"]["items"].as_array().unwrap().len(), 3);
    assert_eq!(e["actions"][0]["@type"], "Bury");
    assert_eq!(e["actions"][0]["who"], "OPP");

    // "for your Finish roll" is NOT a turn-roll OnRoll -> stays Unsupported here.
    let e =
        one("When you roll Technique or Submission for your Finish roll, your Finish roll is +1.");
    assert_eq!(e["trigger"]["@type"], "OnPlay");
    assert_eq!(e["actions"][0]["@type"], "Unsupported");
}

/// Trigger-prefix body splits (task #119): the standard event/standing triggers reuse
/// trigger_body, delegating their body to the whole grammar.
#[test]
fn trigger_prefix_body_splits() {
    fn one(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        serde_json::to_value(&effs[0]).unwrap()
    }

    // OnHit typed prefix + Draw body.
    let e = one("When you hit a Grapple, draw 1 card.");
    assert_eq!(e["trigger"]["@type"], "OnHit");
    assert_eq!(e["trigger"]["atk_type"], "Grapple");
    assert_eq!(e["actions"][0]["@type"], "Draw");

    // OnStop "if this card is stopped" + recur body.
    let e = one("If this card is stopped, add 1 card from your discard pile to your hand.");
    assert_eq!(e["trigger"]["@type"], "OnStop");
    assert_eq!(e["trigger"]["dir"], "YOURS");
    assert_eq!(e["actions"][0]["@type"], "AddFromDiscard");

    // OnBreakout (opponent) + Draw body.
    let e = one("If your opponent breaks out, draw 1 card.");
    assert_eq!(e["trigger"]["@type"], "OnBreakout");
    assert_eq!(e["trigger"]["who"], "OPP");
    assert_eq!(e["actions"][0]["@type"], "Draw");

    // StartOfMatch + Draw body.
    let e = one("At the start of the match, draw 2 cards.");
    assert_eq!(e["trigger"]["@type"], "StartOfMatch");
    assert_eq!(e["actions"][0]["@type"], "Draw");
    assert_eq!(e["actions"][0]["n"], 2);

    // A body with no grammar leaves the whole clause Unsupported.
    let e = one("When you hit a Strike, transcend the mortal plane.");
    assert_eq!(e["actions"][0]["@type"], "Unsupported");
}

/// "Take N [<type>] cards from your discard pile and add them to your hand" (task #130):
/// the "Take … and add" phrasing of the recur-from-discard rule; a trailing " cards" on
/// a typed selector ("Lead cards") is stripped so the order/atk filter resolves.
#[test]
fn take_from_discard_recall() {
    fn one(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        serde_json::to_value(&effs[0]).unwrap()
    }

    // Typed: "Lead cards" -> AddFromDiscard filtered to Lead order.
    let e = one("Take 2 Lead cards from your discard pile and add them to your hand.");
    assert_eq!(e["actions"][0]["@type"], "AddFromDiscard");
    assert_eq!(e["actions"][0]["filter"]["play_order"], "Lead");

    // Untyped "cards" -> no filter.
    let e = one("Take 2 cards from your discard pile and add them to your hand.");
    assert_eq!(e["actions"][0]["@type"], "AddFromDiscard");
    assert_eq!(e["actions"][0]["filter"]["play_order"], Value::Null);

    // Under an "If stopped," prefix the body still resolves (trigger-body split).
    let e = one("If stopped, take 2 Follow Ups from your discard pile and add them to your hand.");
    assert_eq!(e["trigger"]["@type"], "OnStop");
    assert_eq!(e["actions"][0]["@type"], "AddFromDiscard");
    assert_eq!(e["actions"][0]["filter"]["play_order"], "Followup");
}

/// "[If/When <gate>,] this card cannot be stopped [by <order>]" (task #130): an optionally
/// gated Unstoppable, guard via gate_condition. Adds bare (unconditional) + opponent-side
/// gates (opp roll / opp-in-play) over the pre-existing stop_condition vocabulary.
#[test]
fn cannot_be_stopped_gates() {
    fn one(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        serde_json::to_value(&effs[0]).unwrap()
    }
    fn unstop(text: &str) -> Value {
        let e = one(text);
        assert_eq!(e["actions"][0]["@type"], "Unstoppable", "{text:?}");
        e.clone()
    }

    // Bare -> unconditional.
    let e = unstop("This card cannot be stopped.");
    assert_eq!(e["condition"]["@type"], "Always");

    // Opponent-roll gate (gate_condition's opp branch).
    let e =
        unstop("If your opponent rolled Power for their turn roll, this card cannot be stopped.");
    assert_eq!(e["condition"]["@type"], "RollWasSkill");
    assert_eq!(e["condition"]["who"], "OPP");

    // Opponent-in-play gate (none / count).
    let e = unstop("When your opponent has 0 cards in play this card cannot be stopped.");
    assert_eq!(e["condition"]["@type"], "HasInPlay");
    assert_eq!(e["condition"]["who"], "OPP");
    assert_eq!(e["condition"]["cmp"], "<");

    // Crowd-Meter gate still works (via stop_condition fallback), with a by-order.
    let e = one("If the Crowd Meter is 5 or greater, this card cannot be stopped by Follow Ups.");
    assert_eq!(e["actions"][0]["@type"], "Unstoppable");
    assert_eq!(e["actions"][0]["by_order"], "Followup");
    assert_eq!(e["condition"]["@type"], "CrowdMeterCompare");
}

/// Match-stipulation gate (task #130): "this is a <X> Match" -> IsMatchType, an OR-set
/// when disjoined ("Steel Cage or Liger's Den"). Cascades through every gated family —
/// generic body, double-bonuses, also-a, cannot-be-stopped. schema v92.
#[test]
fn match_type_gates() {
    fn one(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        serde_json::to_value(&effs[0]).unwrap()
    }

    // Generic body gate: "If this is a Steel Cage Match, draw 2 cards."
    let e = one("If this is a Steel Cage Match, draw 2 cards.");
    assert_eq!(e["actions"][0]["@type"], "Draw");
    assert_eq!(e["condition"]["@type"], "IsMatchType");
    assert_eq!(e["condition"]["types"][0], "STEEL_CAGE");

    // Disjunction -> OR-set of two types, cannot-be-stopped body.
    let e = one("If this is a Steel Cage or Liger's Den Match, this card cannot be stopped.");
    assert_eq!(e["actions"][0]["@type"], "Unstoppable");
    assert_eq!(e["condition"]["@type"], "IsMatchType");
    assert_eq!(e["condition"]["types"][0], "STEEL_CAGE");
    assert_eq!(e["condition"]["types"][1], "LIGERS_DEN");

    // also-a body, Tag Team.
    let e = one("If this is a Tag Team Match, this card is also a Finish.");
    assert_eq!(e["actions"][0]["@type"], "AlsoLead");
    assert_eq!(e["actions"][0]["condition"]["@type"], "IsMatchType");
    assert_eq!(e["actions"][0]["condition"]["types"][0], "TAG_TEAM");

    // An unrecognized stipulation ("Singles") declines cleanly -> Unsupported.
    let e = one("If this is a Singles Match, draw 2 cards.");
    assert_eq!(e["actions"][0]["@type"], "Unsupported");
}

/// Crowd-Meter swing body (task #130): "The Crowd Meter is +N / -N" -> `CrowdMeter{delta}`,
/// the printed sign verbatim. Trigger-prefixed variants reach the body via the trigger
/// split; gate-prefixed ones AND their condition. schema-neutral (CrowdMeter pre-existed).
#[test]
fn crowd_meter_swing() {
    fn one(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        serde_json::to_value(&effs[0]).unwrap()
    }

    // Bare positive swing -> OnPlay, delta verbatim.
    let e = one("The Crowd Meter is +2.");
    assert_eq!(e["actions"][0]["@type"], "CrowdMeter");
    assert_eq!(e["actions"][0]["delta"], 2);
    assert_eq!(e["trigger"]["@type"], "OnPlay");

    // Negative swing keeps the printed sign.
    let e = one("The Crowd Meter is -1.");
    assert_eq!(e["actions"][0]["delta"], -1);

    // Trigger prefix: "If stopped, …" -> OnStop trigger + the swing body.
    let e = one("If stopped, the Crowd Meter is +3.");
    assert_eq!(e["actions"][0]["@type"], "CrowdMeter");
    assert_eq!(e["actions"][0]["delta"], 3);
    assert_eq!(e["trigger"]["@type"], "OnStop");

    // Gate prefix: a Crowd-Meter threshold ANDs onto the condition, staying OnPlay.
    let e = one("If the Crowd Meter is 3 or greater, the Crowd Meter is +1.");
    assert_eq!(e["actions"][0]["@type"], "CrowdMeter");
    assert_eq!(e["condition"]["@type"], "CrowdMeterCompare");

    // Per-count swing ("+2 for each Stop") is a different shape -> declines cleanly.
    let e = one("The Crowd Meter is +2 for each Stop in play.");
    assert_eq!(e["actions"][0]["@type"], "Unsupported");
}

/// Dynamic-target skill buff (task #130): "+N to your lowest/highest skill" and "Your
/// lowest/highest skill is +N" -> BuffSkill with target_lowest/target_highest, resolved
/// to the extreme base skill at derived-stats time. schema v93.
#[test]
fn lowest_highest_skill_buff() {
    fn buff(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        let e = serde_json::to_value(&effs[0]).unwrap();
        assert_eq!(e["actions"][0]["@type"], "BuffSkill", "{text:?}");
        e["actions"][0].clone()
    }

    // "+N to your lowest skill" -> target_lowest, static WhileInPlay.
    let a = buff("+2 to your lowest skill.");
    assert_eq!(a["target_lowest"], true);
    assert_eq!(a["target_highest"], false);
    assert_eq!(a["delta"], 2);

    // "Your lowest skill is +N" -> same.
    let a = buff("Your lowest skill is +1.");
    assert_eq!(a["target_lowest"], true);
    assert_eq!(a["delta"], 1);

    // Highest mirror.
    let a = buff("+3 to your highest skill.");
    assert_eq!(a["target_highest"], true);
    assert_eq!(a["target_lowest"], false);
    let a = buff("Your highest skill is +2.");
    assert_eq!(a["target_highest"], true);
}

/// Flat self breakout-roll bonus (task #130): "Your breakout rolls are +N" / "+N to your
/// breakout rolls" / "Your Nrd breakout roll is +N" -> BreakoutModifier (self-directed,
/// no skill gate). Opponent-directed forms have no `who` field yet -> stay Unsupported.
#[test]
fn breakout_roll_bonus() {
    fn one(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        serde_json::to_value(&effs[0]).unwrap()
    }

    // "Your breakout rolls are +N" -> every attempt (attempts=null), no skill gate.
    let e = one("Your breakout rolls are +1.");
    assert_eq!(e["actions"][0]["@type"], "BreakoutModifier");
    assert_eq!(e["actions"][0]["delta"], 1);
    assert_eq!(e["actions"][0]["attempts"], Value::Null);
    assert_eq!(e["actions"][0]["when_skill"], Value::Null);

    // "+N to your breakout rolls" -> same.
    let e = one("+2 to your breakout rolls.");
    assert_eq!(e["actions"][0]["@type"], "BreakoutModifier");
    assert_eq!(e["actions"][0]["delta"], 2);

    // Attempt-indexed: "Your 3rd breakout roll is +N" -> attempts=3.
    let e = one("Your 3rd breakout roll is +2.");
    assert_eq!(e["actions"][0]["attempts"], 3);
    assert_eq!(e["actions"][0]["delta"], 2);

    // Gate cascade: both "If <gate>," and "When <gate>," Crowd-Meter thresholds AND onto
    // the condition (the generic gate rule accepts either prefix for state gates).
    for prefix in ["If", "When"] {
        let e = one(&format!(
            "{prefix} the Crowd Meter is 5 or greater, your breakout rolls are +1."
        ));
        assert_eq!(e["actions"][0]["@type"], "BreakoutModifier", "{prefix}");
        assert_eq!(e["condition"]["@type"], "CrowdMeterCompare", "{prefix}");
    }

    // Opponent-directed: who=OPP (schema v94).
    let e = one("Your opponent's breakout rolls are -1.");
    assert_eq!(e["actions"][0]["@type"], "BreakoutModifier");
    assert_eq!(e["actions"][0]["who"], "OPP");
    assert_eq!(e["actions"][0]["delta"], -1);

    // Opponent, attempt-indexed.
    let e = one("Your opponent's 2nd breakout roll is -2.");
    assert_eq!(e["actions"][0]["who"], "OPP");
    assert_eq!(e["actions"][0]["attempts"], 2);
    assert_eq!(e["actions"][0]["delta"], -2);

    // The self forms stay who=SELF.
    let e = one("Your breakout rolls are +1.");
    assert_eq!(e["actions"][0]["who"], "SELF");

    // Per-count opp form ("for each …") has no per-count on this action -> declines.
    let e = one("Your opponent's breakout rolls are -1 for each Stop they have in play.");
    assert_eq!(e["actions"][0]["@type"], "Unsupported");
}

/// Opponent per-count next-turn-roll penalty (task #130): "Your opponent's next turn roll
/// is -N for each [other] <X> you have in play" -> ModifyRoll{who:OPP, per, per_who:SELF} —
/// the opp-directed mirror of the existing self per-count rule.
#[test]
fn opp_next_roll_per_count() {
    fn one(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        serde_json::to_value(&effs[0]).unwrap()
    }

    let e = one("Your opponent's next turn roll is -1 for each Lead you have in play.");
    assert_eq!(e["actions"][0]["@type"], "ModifyRoll");
    assert_eq!(e["actions"][0]["who"], "OPP");
    assert_eq!(e["actions"][0]["delta"], -1);
    assert_eq!(e["actions"][0]["when"], "NEXT");
    assert_eq!(e["actions"][0]["per_who"], "SELF");
    assert_eq!(e["actions"][0]["per"]["play_order"], "Lead");

    // "other" is tolerated; a Stop per-count filters on is_stop.
    let e = one("Your opponent's next turn roll is -2 for each other Grapple you have in play.");
    assert_eq!(e["actions"][0]["per"]["atk_type"], "Grapple");
    let e = one("Your opponent's next turn roll is -1 for each Stop you have in play.");
    assert_eq!(e["actions"][0]["per"]["is_stop"], true);

    // The self per-count rule now accepts a negative delta too ("-N for each other Lead").
    let e = one("Your next turn roll is -1 for each other Lead you have in play.");
    assert_eq!(e["actions"][0]["@type"], "ModifyRoll");
    assert_eq!(e["actions"][0]["who"], "SELF");
    assert_eq!(e["actions"][0]["delta"], -1);
    assert_eq!(e["actions"][0]["per"]["play_order"], "Lead");
}

/// Gimmick-blank grammar (task #130): "[Your opponent's] Gimmick is blank" -> a Static
/// BlankGimmick marker (the action pre-existed but was override-only). WhileInPlay.
#[test]
fn gimmick_blank_grammar() {
    fn one(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        serde_json::to_value(&effs[0]).unwrap()
    }

    // Opponent's gimmick blanked.
    let e = one("Your opponent's Gimmick is blank.");
    assert_eq!(e["actions"][0]["@type"], "BlankGimmick");
    assert_eq!(e["actions"][0]["who"], "OPP");
    assert_eq!(e["actions"][0]["duration"], "WHILE_IN_PLAY");
    assert_eq!(e["trigger"]["@type"], "Static");

    // Self.
    let e = one("Your Gimmick is blank.");
    assert_eq!(e["actions"][0]["who"], "SELF");

    // Gated cascade via the generic gate rule.
    let e = one("If the Crowd Meter is 5 or greater, your opponent's Gimmick is blank.");
    assert_eq!(e["actions"][0]["@type"], "BlankGimmick");
    assert_eq!(e["condition"]["@type"], "CrowdMeterCompare");
}

/// Re-roll grammar (task #130): the `Reroll` action pre-existed but was override-only.
/// "[You may] re-roll your [next] turn roll" / "… your Finish roll" / "[You may] force
/// your opponent to re-roll …". "You may" -> optional; "next" -> NEXT, else THIS.
#[test]
fn reroll_grammar() {
    fn one(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        serde_json::to_value(&effs[0]).unwrap()
    }

    // Self, next turn roll, optional.
    let e = one("You may re-roll your next turn roll.");
    assert_eq!(e["actions"][0]["@type"], "Reroll");
    assert_eq!(e["actions"][0]["who"], "SELF");
    assert_eq!(e["actions"][0]["when"], "NEXT");
    assert_eq!(e["actions"][0]["finish"], false);
    assert_eq!(e["optional"], true);

    // Bare "your turn roll" -> THIS (current roll).
    let e = one("Re-roll your turn roll.");
    assert_eq!(e["actions"][0]["when"], "THIS");
    assert_eq!(e["optional"], false);

    // Opponent, forced.
    let e = one("You may force your opponent to re-roll their next turn roll.");
    assert_eq!(e["actions"][0]["who"], "OPP");
    assert_eq!(e["actions"][0]["when"], "NEXT");

    // Finish roll.
    let e = one("You may re-roll your Finish roll.");
    assert_eq!(e["actions"][0]["finish"], true);
    assert_eq!(e["actions"][0]["who"], "SELF");

    // Cascade: "If stopped, you may re-roll …" -> OnStop trigger + optional body.
    let e = one("If stopped, you may re-roll your next turn roll.");
    assert_eq!(e["actions"][0]["@type"], "Reroll");
    assert_eq!(e["trigger"]["@type"], "OnStop");
    assert_eq!(e["optional"], true);
}

/// "[If/When <gate>,] this card is also a <order>" (task #130): the card gains an extra
/// play-order slot via AlsoLead, whose OWN condition carries the gate. gate_condition now
/// falls back to the rich stop_condition parser (Crowd Meter, skill/hand compare, …).
#[test]
fn also_a_order_gates() {
    fn one(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        serde_json::to_value(&effs[0]).unwrap()
    }
    fn also(text: &str) -> Value {
        let e = one(text);
        assert_eq!(e["actions"][0]["@type"], "AlsoLead", "{text:?}");
        e["actions"][0].clone()
    }

    // Bare (no gate) -> Always; order picked from the phrase.
    let a = also("This card is also a Finish.");
    assert_eq!(a["order"], "Finish");
    assert_eq!(a["condition"]["@type"], "Always");

    // Crowd-Meter gate (via stop_condition fallback), "If" and "When" prefixes.
    let a = also("If the Crowd Meter is 5 or greater, this card is also a Finish.");
    assert_eq!(a["order"], "Finish");
    assert_eq!(a["condition"]["@type"], "CrowdMeterCompare");
    assert_eq!(a["condition"]["value"], 5);
    let a = also("When the Crowd Meter is 2 or greater this card is also a Lead.");
    assert_eq!(a["order"], "Lead");
    assert_eq!(a["condition"]["@type"], "CrowdMeterCompare");

    // In-play gate.
    let a = also("If you have another Follow Up in play, this card is also a Finish.");
    assert_eq!(a["condition"]["@type"], "HasInPlay");
    assert_eq!(a["condition"]["filter"]["play_order"], "Followup");

    // A gate gate_condition/stop_condition can't parse -> whole clause Unsupported.
    let e = one("If played as a Stop, this card is also a Finish.");
    assert_eq!(e["actions"][0]["@type"], "Unsupported");
}

/// Hit-history gate (task #130): "If you hit <filter> (this|last) turn, <body>" and the
/// generic "If <gate>, <body>" rule that keeps the body's trigger and AND-s the gate.
#[test]
fn hit_history_and_generic_gate() {
    fn one(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        serde_json::to_value(&effs[0]).unwrap()
    }

    // Hit-history condition feeds the double-bonuses rule (via gate_condition).
    let e = one("If you hit a Grapple last turn, double these bonuses.");
    assert_eq!(e["actions"][0]["@type"], "DoubleFinishIf");
    let c = &e["actions"][0]["condition"];
    assert_eq!(c["@type"], "HitCard");
    assert_eq!(c["last_turn"], true);
    assert_eq!(c["filter"]["atk_type"], "Grapple");

    // Hit-history gate over a non-double body, via the generic gate rule; the body keeps
    // its natural trigger and the HitCard gate is AND-ed on.
    let e = one("If you hit a card with \"Dragon\" in the name last turn, draw 4 cards.");
    assert_eq!(e["actions"][0]["@type"], "Draw");
    assert_eq!(e["actions"][0]["n"], 4);
    assert_eq!(e["condition"]["@type"], "HitCard");
    assert_eq!(e["condition"]["filter"]["name_contains"][0], "Dragon");
    assert_eq!(e["condition"]["last_turn"], true);

    // "this turn" hit + any-card ("another card").
    let e = one("If you hit another card this turn, draw 1 card.");
    assert_eq!(e["condition"]["@type"], "HitCard");
    assert_eq!(e["condition"]["last_turn"], false);
    assert_eq!(e["actions"][0]["@type"], "Draw");

    // Generic gate with a roll gate over a compound body.
    let e =
        one("If you rolled Power for your turn roll, draw 1 card and bury 1 card in your hand.");
    assert_eq!(e["condition"]["@type"], "RollWasSkill");
    let kinds: Vec<&str> = e["actions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["@type"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["Draw", "Bury"]);

    // A gate with an internal comma (multi-name list) fails gate_condition -> Unsupported.
    let e = one(
        "If you hit a card with \"Barricade\", \"Beatdown\", or \"Blindside\" in the name last turn, draw 1 card.",
    );
    assert_eq!(e["actions"][0]["@type"], "Unsupported");
}

/// "If <gate>, double these bonuses" (task #130): a 137-clause family mapping to
/// DoubleFinishIf{condition}, where gate_condition parses the common turn-roll / flag /
/// in-play gates. Only the ×2 "double" form maps; unmodeled gates stay Unsupported.
#[test]
fn double_these_bonuses_gates() {
    fn one(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        serde_json::to_value(&effs[0]).unwrap()
    }
    fn cond(text: &str) -> Value {
        let e = one(text);
        assert_eq!(e["actions"][0]["@type"], "DoubleFinishIf", "{text:?}");
        e["actions"][0]["condition"].clone()
    }

    // Turn-roll skill gate (self / opponent).
    let c = cond("If you rolled Power for your turn roll, double these bonuses.");
    assert_eq!(c["@type"], "RollWasSkill");
    assert_eq!(c["skill"], "Power");
    assert_eq!(c["who"], "SELF");
    let c = cond("If your opponent rolled Grapple for their turn roll, double these bonuses.");
    assert_eq!(c["who"], "OPP");

    // Flag gates.
    assert_eq!(
        cond("If you re-rolled your turn roll, double these bonuses.")["@type"],
        "RerolledTurnRoll"
    );
    assert_eq!(
        cond("If you ended the last turn without playing a card, double these bonuses.")["@type"],
        "EndedTurnNoPlay"
    );
    // The bumped case is caught first by the dedicated DoubleFinishIfBumped rule.
    let e = one("If you bumped on the last turn roll, double these bonuses.");
    assert_eq!(e["actions"][0]["@type"], "DoubleFinishIfBumped");

    // In-play gates: name-substring and typed count.
    let c = cond("If you have a card with \"Wrench\" in the name in play, double these bonuses.");
    assert_eq!(c["@type"], "HasInPlay");
    assert_eq!(c["filter"]["name_contains"][0], "Wrench");
    let c = cond("If you have 3 Submissions in play, double these bonuses.");
    assert_eq!(c["@type"], "HasInPlay");
    assert_eq!(c["count"], 3);
    assert_eq!(c["filter"]["atk_type"], "Submission");

    // Roll value.
    let c = cond("If you rolled 10 for your turn roll, double these bonuses.");
    assert_eq!(c["@type"], "RollValue");
    assert_eq!(c["value"], 10);

    // A Tag Team match now parses as a match-type gate (schema v92).
    let c = cond("If this is a Tag Team match, double these bonuses.");
    assert_eq!(c["@type"], "IsMatchType");
    assert_eq!(c["types"][0], "TAG_TEAM");

    // An unmodeled gate leaves the whole clause Unsupported (no partial DoubleFinishIf).
    let e = one("If it is the first turn of the game, double these bonuses.");
    assert_eq!(e["actions"][0]["@type"], "Unsupported");
    // "triple" is not the ×2 form -> stays Unsupported.
    let e = one("If you rolled Submission for your turn roll, triple these bonuses.");
    assert_eq!(e["actions"][0]["@type"], "Unsupported");
}

/// Condition-gate prefixes (task #130): "If you rolled <skill> for your turn roll, <body>"
/// and "If you have a card with 'X' in the name in play, <body>" keep the body's natural
/// trigger and AND a RollWasSkill / HasInPlay gate onto its condition.
#[test]
fn gate_prefix_body_splits() {
    fn one(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        serde_json::to_value(&effs[0]).unwrap()
    }

    // Roll-gate over a bury-all body: the body keeps its OnHit trigger, gated on the roll.
    let e = one("If you rolled Agility for your turn roll; look at your opponent's hand, they bury all Leads.");
    assert_eq!(e["trigger"]["@type"], "OnHit");
    assert_eq!(e["actions"][0]["@type"], "Bury");
    assert_eq!(e["actions"][0]["all"], true);
    assert_eq!(e["condition"]["@type"], "RollWasSkill");
    assert_eq!(e["condition"]["skill"], "Agility");
    assert_eq!(e["condition"]["who"], "SELF");

    // Name-in-play gate over a bury-all body.
    let e = one("If you have a card with \"Spear\" in the name in play, look at your opponent's hand, they bury all Leads.");
    assert_eq!(e["actions"][0]["@type"], "Bury");
    assert_eq!(e["condition"]["@type"], "HasInPlay");
    assert_eq!(e["condition"]["who"], "SELF");
    assert_eq!(e["condition"]["filter"]["name_contains"][0], "Spear");

    // A gate whose body has no grammar leaves the whole clause Unsupported.
    let e = one("If you rolled Power for your turn roll, ascend to a higher plane.");
    assert_eq!(e["actions"][0]["@type"], "Unsupported");
}

/// "[Look at your opponent's hand,] they bury/discard all <type>" (task #130, schema
/// v90): the opponent sheds EVERY hand card of a type — Bury/Discard `all`, who=Opp.
#[test]
fn bury_discard_all_of_type() {
    fn one(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        serde_json::to_value(&effs[0]).unwrap()
    }

    let e = one("Look at your opponent's hand, they bury all Strike cards.");
    assert_eq!(e["actions"][0]["@type"], "Bury");
    assert_eq!(e["actions"][0]["all"], true);
    assert_eq!(e["actions"][0]["who"], "OPP");
    assert_eq!(e["actions"][0]["selector"]["atk_type"], "Strike");

    // Plural order type, no "cards" noun; discard variant.
    let e = one("Look at your opponent's hand, they discard all Strikes.");
    assert_eq!(e["actions"][0]["@type"], "Discard");
    assert_eq!(e["actions"][0]["all"], true);
    assert_eq!(e["actions"][0]["selector"]["atk_type"], "Strike");

    // "discard all their <type>" variant.
    let e = one("Look at your opponent's hand, discard all their Finishes.");
    assert_eq!(e["actions"][0]["@type"], "Discard");
    assert_eq!(e["actions"][0]["all"], true);
    assert_eq!(e["actions"][0]["selector"]["play_order"], "Finish");

    // "cards of the chosen type" has no card filter -> stays Unsupported.
    let e = one("Look at your opponent's hand, they bury all cards of the chosen type.");
    assert_eq!(e["actions"][0]["@type"], "Unsupported");
}

/// OnBreakoutRoll(Opp) trigger-body split (task #130): "Each time your opponent rolls for
/// a Breakout roll, <body>". The body dispatches through the whole grammar; a leading
/// third-person "they <verb>" resolves to the opponent (the roller) via the opp-subject
/// grammar aliases.
#[test]
fn breakout_roll_body_split() {
    fn one(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        serde_json::to_value(&effs[0]).unwrap()
    }

    // Self-side draw body.
    let e = one("Each time your opponent rolls for a Breakout roll, draw 1 card.");
    assert_eq!(e["trigger"]["@type"], "OnBreakoutRoll");
    assert_eq!(e["trigger"]["who"], "OPP");
    assert_eq!(e["actions"][0]["@type"], "Draw");
    assert_eq!(e["actions"][0]["who"], "SELF");

    // "they randomly bury N" -> opponent buries from hand at random.
    let e =
        one("Each time your opponent rolls for a Breakout roll, they randomly bury 1 card in their hand.");
    assert_eq!(e["trigger"]["@type"], "OnBreakoutRoll");
    assert_eq!(e["actions"][0]["@type"], "Bury");
    assert_eq!(e["actions"][0]["who"], "OPP");
    assert_eq!(e["actions"][0]["random"], true);

    // "they flip N cards" -> opponent flips.
    let e = one("Each time your opponent rolls for a breakout roll, they flip 3 cards.");
    assert_eq!(e["actions"][0]["@type"], "Flip");
    assert_eq!(e["actions"][0]["who"], "OPP");

    // Compound body under the trigger.
    let e = one(
        "Each time your opponent rolls for a Breakout roll, draw 2 cards and your opponent discards 2 cards.",
    );
    assert_eq!(e["trigger"]["@type"], "OnBreakoutRoll");
    let kinds: Vec<&str> = e["actions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["@type"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["Draw", "Discard"]);
}

/// Compound trigger bodies (task #119): "<action A> and/then <action B>" under a trigger
/// prefix folds into one effect with a concatenated action list.
#[test]
fn compound_trigger_body() {
    fn one(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        serde_json::to_value(&effs[0]).unwrap()
    }
    fn acts(e: &Value) -> Vec<String> {
        e["actions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["@type"].as_str().unwrap().to_owned())
            .collect()
    }

    // " and " connector: OnRoll{Power} carries both actions.
    let e =
        one("When you roll Power for your turn roll, draw 1 card and your next turn roll is +1.");
    assert_eq!(e["trigger"]["skill"], "Power");
    assert_eq!(acts(&e), ["Draw", "ModifyRoll"]);

    // " then " connector under OnHit.
    let e = one("When you hit a Grapple, draw 1 card then bury 1 card in your hand.");
    assert_eq!(e["trigger"]["@type"], "OnHit");
    assert_eq!(acts(&e), ["Draw", "Bury"]);

    // A spurious "and" inside a single action does NOT over-split: "bury 1 card in your
    // hand and draw 1 card" splits cleanly, but a part that can't parse declines the
    // whole split. Here both parts parse, so it folds.
    let e = one("At the start of the match, draw 2 cards and draw 1 card.");
    assert_eq!(e["trigger"]["@type"], "StartOfMatch");
    assert_eq!(acts(&e), ["Draw", "Draw"]);

    // If any part has no grammar, no compound -> whole clause Unsupported.
    let e = one("When you roll Power, draw 1 card and ascend to godhood.");
    assert_eq!(e["actions"][0]["@type"], "Unsupported");
}

/// Top-level compound clauses (task #119): a standalone "<A> and/then <B>" (no trigger
/// prefix) folds into one effect via the compile-level compound_body fallback.
#[test]
fn top_level_compound_clause() {
    fn one(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        serde_json::to_value(&effs[0]).unwrap()
    }
    fn acts(e: &Value) -> Vec<String> {
        e["actions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["@type"].as_str().unwrap().to_owned())
            .collect()
    }

    // "Draw 1 card, then bury 1 card" -> one effect, two actions.
    let e = one("Draw 1 card, then bury 1 card in your hand.");
    assert_eq!(acts(&e), ["Draw", "Bury"]);

    // " and " connector, both Instant/Always -> folds.
    let e = one("Shuffle your deck, and draw 1 card.");
    assert_eq!(acts(&e), ["ShuffleDeck", "Draw"]);

    // A part with no grammar declines the whole split -> Unsupported.
    let e = one("Draw 1 card and rewrite the laws of physics.");
    assert_eq!(e["actions"][0]["@type"], "Unsupported");
}

/// Name/text-gated OnHit (task #119): "When you hit a [<type>] card with 'X' [or 'Y']
/// in the name/text, <body>" -> OnHit gated on the hit card + body via trigger_body.
#[test]
fn name_gated_on_hit() {
    fn one(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        serde_json::to_value(&effs[0]).unwrap()
    }

    // Single name, "in the name" explicit.
    let e = one("When you hit a card with \"Lightning\" in the name, draw 1 card.");
    assert_eq!(e["trigger"]["@type"], "OnHit");
    assert_eq!(e["trigger"]["name_contains"][0], "Lightning");
    assert_eq!(e["actions"][0]["@type"], "Draw");

    // OR-list of names, comma form (no "in the name").
    let e = one("When you hit a card with \"Bird\" or \"Press\", draw 1 card.");
    let names: Vec<&str> = e["trigger"]["name_contains"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n.as_str().unwrap())
        .collect();
    assert_eq!(names, ["Bird", "Press"]);

    // "in the text" -> text_contains, not name_contains.
    let e = one("When you hit a card with \"Disqualification\" in the text, draw 2 cards.");
    assert_eq!(e["trigger"]["name_contains"].as_array().unwrap().len(), 0);
    assert_eq!(e["trigger"]["text_contains"][0], "Disqualification");

    // Type + name gate.
    let e = one("When you hit a Grapple with \"Guitar\" in the name, add 1 card from your discard pile to your hand.");
    assert_eq!(e["trigger"]["atk_type"], "Grapple");
    assert_eq!(e["trigger"]["name_contains"][0], "Guitar");
    assert_eq!(e["actions"][0]["@type"], "AddFromDiscard");
}

/// "or"-choice bodies (task #119): "<A> or <B>" folds into one Action::Choice, at the
/// top level and under a trigger prefix.
#[test]
fn choice_body_split() {
    fn one(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        serde_json::to_value(&effs[0]).unwrap()
    }
    fn branch_types(e: &Value) -> Vec<String> {
        e["actions"][0]["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["actions"][0]["@type"].as_str().unwrap().to_owned())
            .collect()
    }

    // Top-level "X or Y" -> a single Choice with two options.
    let e = one("Draw 1 card or shuffle 3 cards from your discard pile into your deck.");
    assert_eq!(e["actions"][0]["@type"], "Choice");
    assert_eq!(branch_types(&e), ["Draw", "ShuffleIntoDeck"]);

    // Under a trigger prefix: OnRoll{Agility} carrying the Choice.
    let e = one(
        "When you roll Agility, flip 3 cards or add 1 Strike from your discard pile to your hand.",
    );
    assert_eq!(e["trigger"]["@type"], "OnRoll");
    assert_eq!(e["trigger"]["skill"], "Agility");
    assert_eq!(e["actions"][0]["@type"], "Choice");
    assert_eq!(branch_types(&e), ["Flip", "AddFromDiscard"]);

    // A branch with no grammar declines the whole split -> Unsupported.
    let e = one("Draw 1 card or summon a thunderstorm.");
    assert_eq!(e["actions"][0]["@type"], "Unsupported");
}

/// Per-count next-turn-roll grammar (task #124): "+N for each <X> you have in play"
/// (per_zone=IN_PLAY) and "… in your discard pile" (per_zone=DISCARD), plus the
/// Olympics-pod fidelity grammar: Thud! (BuffSkill per OR-name-list), Rejected!
/// (whole-discard random bury ×2), Impact is Family V2 (blank opp Spotlight Finishes).
#[test]
fn pod_fidelity_grammar() {
    fn a1(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        serde_json::to_value(&effs[0]).unwrap()
    }

    // D2's Thud!: per-count Agility buff over a 4-name OR-list.
    let e = a1("Your Agility skill is +1 for each card you have in play with \"Hammer\", \"Smash\", \"High\", or \"Strike\" in the name.");
    let a = &e["actions"][0];
    assert_eq!(a["@type"], "BuffSkill");
    assert_eq!(a["skill"], "Agility");
    assert_eq!(a["delta"], 1);
    assert_eq!(a["per_zone"], "IN_PLAY");
    assert_eq!(
        a["per"]["name_contains"],
        serde_json::json!(["Hammer", "Smash", "High", "Strike"])
    );
    assert_eq!(a["cap"], Value::Null);
    // With a "(Max +N)" cap.
    let a = a1("Your Technique skill is +1 for each card you have in play with \"Chin\" in the name (Max +3).")["actions"][0].clone();
    assert_eq!(a["cap"], 3);
    assert_eq!(a["per"]["name_contains"], serde_json::json!(["Chin"]));

    // Rejected!: nuke both discard piles at random.
    let e = a1("Each player randomly buries their discard pile.");
    let acts = e["actions"].as_array().unwrap();
    assert_eq!(acts.len(), 2, "buries both players");
    for a in acts {
        assert_eq!(a["@type"], "Bury");
        assert_eq!(a["source"], "DISCARD");
        assert_eq!(a["random"], true);
    }
    let whos: Vec<&str> = acts.iter().map(|a| a["who"].as_str().unwrap()).collect();
    assert!(whos.contains(&"SELF") && whos.contains(&"OPP"));

    // Impact is Family V2: blank opponent's Spotlight Finishes.
    let a = a1("Your opponent's Spotlight Finishes have blank text.")["actions"][0].clone();
    assert_eq!(a["@type"], "BlankText");
    assert_eq!(a["who"], "OPP");
    assert_eq!(a["selector"]["play_order"], "Finish");
    assert_eq!(a["selector"]["tag"], "Spotlight");
}

/// Skill-buff family (task #119/#130): a standing skill buff gated on "another
/// Follow Up or Finish <ATK>" — an OR-of-orders `HasInPlay` at count>=2 (the source
/// card counts, so one OTHER qualifier arms it). Plus the bare "Your <S> skill is +N".
#[test]
fn gated_order_or_skill_buff_grammar() {
    fn one(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        serde_json::to_value(&effs[0]).unwrap()
    }

    let e =
        one("If you have another Follow Up or Finish Strike in play, your Technique skill is +1.");
    assert_eq!(e["trigger"]["@type"], "Static");
    assert_eq!(e["duration"], "WHILE_IN_PLAY");
    let a = &e["actions"][0];
    assert_eq!(a["@type"], "BuffSkill");
    assert_eq!(a["skill"], "Technique");
    assert_eq!(a["delta"], 1);
    let cond = &e["condition"];
    assert_eq!(cond["@type"], "HasInPlay");
    assert_eq!(cond["who"], "SELF");
    assert_eq!(cond["cmp"], ">=");
    assert_eq!(
        cond["count"], 2,
        "'another' on a self-matching card => count>=2"
    );
    assert_eq!(cond["filter"]["atk_type"], "Strike");
    assert_eq!(
        cond["filter"]["play_orders"],
        serde_json::json!(["Followup", "Finish"])
    );
    assert_eq!(cond["filter"]["play_order"], Value::Null);

    // The bare "Your <S> skill is +N" (widened to accept the optional "skill" word)
    // is an unconditional Static buff.
    let a = one("Your Power skill is +2.")["actions"][0].clone();
    assert_eq!(a["@type"], "BuffSkill");
    assert_eq!(a["skill"], "Power");
    assert_eq!(a["delta"], 2);
}

/// Phase-scoped turn-roll bonus (task #131): "during turn rolls" -> a standing
/// TurnRollBonus per skill (single, multi-skill, and the "+N to <S>" phrasing).
#[test]
fn turn_roll_bonus_grammar() {
    fn e(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        serde_json::to_value(&effs[0]).unwrap()
    }

    // Single skill, "Your <S> is +N during turn rolls".
    let v = e("Your Power is +2 during turn rolls.");
    assert_eq!(v["trigger"]["@type"], "Static");
    assert_eq!(v["actions"][0]["@type"], "TurnRollBonus");
    assert_eq!(v["actions"][0]["skill"], "Power");
    assert_eq!(v["actions"][0]["delta"], 2);

    // The "skill" word variant folds in.
    let v = e("Your Grapple skill is +1 during turn rolls.");
    assert_eq!(v["actions"][0]["skill"], "Grapple");

    // Multi-skill fans out to one TurnRollBonus per skill.
    let acts = e("Your Power and Strike are +1 during turn rolls.")["actions"].clone();
    let skills: Vec<&str> = acts
        .as_array()
        .unwrap()
        .iter()
        .map(|a| {
            assert_eq!(a["@type"], "TurnRollBonus");
            a["skill"].as_str().unwrap()
        })
        .collect();
    assert_eq!(skills, vec!["Power", "Strike"]);

    // The "+N to <S> during turn rolls" phrasing maps to TurnRollBonus (not FinishBonus).
    let a = e("+1 to Submission during turn rolls.")["actions"][0].clone();
    assert_eq!(a["@type"], "TurnRollBonus");
    assert_eq!(a["skill"], "Submission");
    assert_eq!(a["delta"], 1);
}

/// Gated flat next-turn-roll bonus (task #131): "If you have another <order|atk> in
/// play[,] your next turn roll is +N" -> OnHit ModifyRoll{NEXT} on HasInPlay count>=2.
/// Order and attack-type gates parse; a name gate declines to Unsupported.
#[test]
fn gated_next_turn_roll_grammar() {
    fn one(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        serde_json::to_value(&effs[0]).unwrap()
    }

    // Attack-type gate, no comma.
    let e = one("If you have another Strike in play your next turn roll is +2.");
    assert_eq!(e["trigger"]["@type"], "OnHit");
    let m = &e["actions"][0];
    assert_eq!(m["@type"], "ModifyRoll");
    assert_eq!(m["who"], "SELF");
    assert_eq!(m["when"], "NEXT");
    assert_eq!(m["delta"], 2);
    let c = &e["condition"];
    assert_eq!(c["@type"], "HasInPlay");
    assert_eq!(c["count"], 2);
    assert_eq!(c["filter"]["atk_type"], "Strike");

    // Play-order gate, with comma.
    let c = one("If you have another Follow Up in play, your next turn roll is +2.")["condition"]
        .clone();
    assert_eq!(c["count"], 2);
    assert_eq!(c["filter"]["play_order"], "Followup");

    // A name gate has no count_filter -> the clause stays Unsupported.
    let a = one("If you have another Saber of Light card in play, your next turn roll is +3.")
        ["actions"][0]
        .clone();
    assert_eq!(a["@type"], "Unsupported");
}

/// "also a Follow Up" conditional (AlsoLead order=Followup gated on RollWasSkill).
#[test]
fn next_roll_percount_and_also_followup_grammar() {
    fn a1(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        serde_json::to_value(&effs[0]).unwrap()
    }

    // In-play per-count next roll.
    let e = a1("Your next turn roll is +1 for each other Lead you have in play.");
    let m = &e["actions"][0];
    assert_eq!(m["@type"], "ModifyRoll");
    assert_eq!(m["when"], "NEXT");
    assert_eq!(m["delta"], 1);
    assert_eq!(m["per"]["play_order"], "Lead");
    assert_eq!(m["per_who"], "SELF");
    assert_eq!(m["per_zone"], "IN_PLAY");

    // Discard-zone per-count next roll.
    let m =
        a1("Your next turn roll is +2 for each Finish in your discard pile.")["actions"][0].clone();
    assert_eq!(m["per"]["play_order"], "Finish");
    assert_eq!(m["per_zone"], "DISCARD");
    assert_eq!(m["delta"], 2);

    // Name-descriptor per-count: the "with 'X' in the name" qualifier trails "you have
    // in play", so it routes through in_play_filter (name-substring filter).
    let m = a1("Your next turn roll is +1 for each card you have in play with \"Steel Chain\" in the name.")
        ["actions"][0]
        .clone();
    assert_eq!(m["@type"], "ModifyRoll");
    assert_eq!(m["delta"], 1);
    assert_eq!(m["per_who"], "SELF");
    assert_eq!(m["per"]["name_contains"][0], "Steel Chain");

    // Opponent-roll penalty, name-descriptor variant (per_who = SELF, the cards YOU
    // have in play; who = OPP, their roll).
    let m = a1("Your opponent's next turn roll is -1 for each card you have in play with \"Kendo Stick\" in the name.")
        ["actions"][0]
        .clone();
    assert_eq!(m["who"], "OPP");
    assert_eq!(m["delta"], -1);
    assert_eq!(m["per_who"], "SELF");
    assert_eq!(m["per"]["name_contains"][0], "Kendo Stick");

    // The plain "+N" rule still yields no per (regression guard for rule ordering).
    let m = a1("Your next turn roll is +3.")["actions"][0].clone();
    assert_eq!(m["per"], Value::Null);
    assert_eq!(m["per_zone"], "IN_PLAY");

    // Skill-keyed pending mod: "The next time you roll <S> for your turn roll, it is
    // +N" -> ModifyRoll{when:NEXT, on_skill:S}. Waits for that skill (engine consumes).
    let m =
        a1("The next time you roll Technique for your turn roll, it is +2.")["actions"][0].clone();
    assert_eq!(m["@type"], "ModifyRoll");
    assert_eq!(m["when"], "NEXT");
    assert_eq!(m["delta"], 2);
    assert_eq!(m["on_skill"], "Technique");
    assert_eq!(m["per"], Value::Null);
    // The "for your turn roll" phrase and the comma are both optional ("… Grapple, it
    // is +5"), and a plain next-roll mod carries no on_skill (serde-skipped when None).
    let m = a1("The next time you roll Grapple, it is +5.")["actions"][0].clone();
    assert_eq!(m["on_skill"], "Grapple");
    assert_eq!(m["delta"], 5);
    let m = a1("Your next turn roll is +3.")["actions"][0].clone();
    assert_eq!(
        m.get("on_skill"),
        None,
        "plain next-roll mod has no on_skill"
    );

    // "If you rolled <skill> … also a Follow Up" -> AlsoLead{order:Followup, RollWasSkill}.
    let e = a1("If you rolled Agility for your turn roll this card is also a Follow Up.");
    let al = &e["actions"][0];
    assert_eq!(al["@type"], "AlsoLead");
    assert_eq!(al["order"], "Followup");
    assert_eq!(al["condition"]["@type"], "RollWasSkill");
    assert_eq!(al["condition"]["skill"], "Agility");
    // With the optional comma too.
    let al = a1("If you rolled Power for your turn roll, this card is also a Follow Up.")
        ["actions"][0]
        .clone();
    assert_eq!(al["condition"]["skill"], "Power");

    // Bump-conditional Follow Up (Kill Shot): "If you bumped on the previous/last
    // turn roll, this card is also a Follow Up." -> AlsoLead{Followup, BumpedLastTurnRoll}.
    for text in [
        "If you bumped on the previous turn roll, this card is also a Follow Up.",
        "If you bumped on the last turn roll, this card is also a Follow Up.",
    ] {
        let al = a1(text)["actions"][0].clone();
        assert_eq!(al["@type"], "AlsoLead", "{text:?}");
        assert_eq!(al["order"], "Followup", "{text:?}");
        assert_eq!(al["condition"]["@type"], "BumpedLastTurnRoll", "{text:?}");
    }
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

/// Stop-eligibility grammar (task #120): "stop any" target robustness + gates.
#[test]
fn stop_eligibility_grammar() {
    fn a1(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        serde_json::to_value(&effs[0]).unwrap()
    }

    // Trailing "card" and a repeated leading "any" both normalize away.
    let e = a1("Stop any Grapple card.");
    assert_eq!(e["actions"][0]["@type"], "Stop");
    assert_eq!(e["actions"][0]["atk_type"], "Grapple");
    assert_eq!(e["actions"][0]["order"], Value::Null);
    let e = a1("Stop any Lead Submission or any Finish Submission.");
    assert_eq!(e["actions"].as_array().unwrap().len(), 2);
    assert_eq!(e["actions"][0]["order"], "Lead");
    assert_eq!(e["actions"][1]["order"], "Finish");

    // "does not have … in play" -> opponent count < 1.
    let e = a1("If your opponent does not have a Lead Grapple in play, stop any Lead Grapple.");
    assert_eq!(e["condition"]["@type"], "HasInPlay");
    assert_eq!(e["condition"]["who"], "OPP");
    assert_eq!(e["condition"]["cmp"], "<");
    assert_eq!(e["condition"]["count"], 1);
    assert_eq!(e["condition"]["filter"]["play_order"], "Lead");
    assert_eq!(e["actions"][0]["@type"], "Stop");

    // Crowd-Meter "N or less" gate.
    let e = a1("If the Crowd Meter is 2 or less, stop any Lead Submission or Finish Submission.");
    assert_eq!(e["condition"]["@type"], "CrowdMeterCompare");
    assert_eq!(e["condition"]["cmp"], "<=");
    assert_eq!(e["condition"]["value"], 2);
    assert_eq!(e["actions"].as_array().unwrap().len(), 2);

    // Compound crowd-Ge AND opponent-has-another.
    let e = a1("If the Crowd Meter is 1 or greater and your opponent has another Submission in play, stop any Submission.");
    assert_eq!(e["condition"]["@type"], "And");
    assert_eq!(e["condition"]["items"][0]["@type"], "CrowdMeterCompare");
    assert_eq!(e["condition"]["items"][1]["@type"], "HasInPlay");
    assert_eq!(e["condition"]["items"][1]["who"], "OPP");

    // "that / even if it cannot be stopped" flags every Stop to bypass Unstoppable.
    let e = a1("Stop any Finish Strike that cannot be stopped.");
    assert_eq!(e["actions"][0]["@type"], "Stop");
    assert_eq!(e["actions"][0]["even_unstoppable"], true);
    let e = a1("Stop any Finish Submission, even if it cannot be stopped.");
    assert_eq!(e["actions"][0]["even_unstoppable"], true);
    // Applies across an OR target and composes with a skill gate.
    let e = a1("If your Power skill is greater than your opponent's Power skill, stop any Follow Up Submission or Finish Submission even if it cannot be stopped.");
    assert_eq!(e["condition"]["@type"], "SkillCompare");
    assert_eq!(e["actions"].as_array().unwrap().len(), 2);
    assert_eq!(e["actions"][0]["even_unstoppable"], true);
    assert_eq!(e["actions"][1]["even_unstoppable"], true);
    // A plain stop leaves the flag false.
    let e = a1("Stop any Grapple.");
    assert_eq!(e["actions"][0]["even_unstoppable"], false);

    // Conditional "this card cannot be stopped" -> Unstoppable{by_order:null} gated
    // by the parsed condition (engine evaluates it from the card owner's side).
    let e = a1("If the Crowd Meter is 5 or greater, this card cannot be stopped.");
    assert_eq!(e["actions"][0]["@type"], "Unstoppable");
    assert_eq!(e["actions"][0]["by_order"], Value::Null);
    assert_eq!(e["condition"]["@type"], "CrowdMeterCompare");
    assert_eq!(e["condition"]["cmp"], ">=");
    assert_eq!(e["condition"]["value"], 5);
    let e = a1("When you have 12 or more cards in your hand, this card cannot be stopped.");
    assert_eq!(e["condition"]["@type"], "HandSizeCompare");
    assert_eq!(e["condition"]["who"], "SELF");
    assert_eq!(e["condition"]["cmp"], ">=");
    let e = a1("If your Submission skill is greater than your opponent's Submission skill, this card cannot be stopped.");
    assert_eq!(e["condition"]["@type"], "SkillCompare");
    assert_eq!(e["condition"]["vs_skill"], Value::Null);
    let e = a1("When you have no Leads in play, this card cannot be stopped.");
    assert_eq!(e["condition"]["@type"], "HasInPlay");
    assert_eq!(e["condition"]["cmp"], "<");
    assert_eq!(e["condition"]["filter"]["play_order"], "Lead");
    let e = a1("If you rolled 7 for your turn roll, this card cannot be stopped.");
    assert_eq!(e["condition"]["@type"], "RollValue");
    assert_eq!(e["condition"]["value"], 7);
    let e = a1("When you and your opponent rolled the same skill for your turn roll, this card cannot be stopped.");
    assert_eq!(e["condition"]["@type"], "SameRolledSkill");
    // An uncovered condition shape declines -> stays Unsupported (honest).
    let e = a1("If this is the first turn of the game, this card cannot be stopped.");
    assert_eq!(e["actions"][0]["@type"], "Unsupported");

    // "Cannot be stopped by \"X\"" -> Unstoppable keyed on the stopper's name.
    let e = a1("Cannot be stopped by \"Beg for Mercy\".");
    assert_eq!(e["actions"][0]["@type"], "Unstoppable");
    assert_eq!(e["actions"][0]["by_name"], "Beg for Mercy");
    assert_eq!(e["actions"][0]["by_order"], Value::Null);
    // "(This card) cannot be stopped by <order>" for Lead/Follow Up/Finish.
    let e = a1("This card cannot be stopped by Follow Ups.");
    assert_eq!(e["actions"][0]["by_order"], "Followup");
    assert_eq!(e["actions"][0]["by_name"], Value::Null);
    // Conditional "… cannot be stopped by <order>".
    let e = a1("When the Crowd Meter is 3 or greater, this card cannot be stopped by Leads.");
    assert_eq!(e["condition"]["@type"], "CrowdMeterCompare");
    assert_eq!(e["actions"][0]["@type"], "Unstoppable");
    assert_eq!(e["actions"][0]["by_order"], "Lead");

    // "at least N greater than your opponent's <S>" -> SkillCompare Ge + value delta.
    let e = a1("If your Submission skill is at least 3 greater than your opponent's Submission skill, stop any Strike.");
    assert_eq!(e["condition"]["@type"], "SkillCompare");
    assert_eq!(e["condition"]["cmp"], ">=");
    assert_eq!(e["condition"]["value"], 3);
    assert_eq!(e["condition"]["vs"], "OPP_SAME");
    assert_eq!(e["actions"][0]["@type"], "Stop");
    assert_eq!(e["actions"][0]["atk_type"], "Strike");

    // "cannot be stopped by Skill Requirement cards" (bare / This card / Your cards)
    // all parse to Unstoppable{by_skillreq}; the engine scopes by where it's authored.
    for text in [
        "Cannot be stopped by Skill Requirement cards.",
        "Cannot be stopped by cards with Skill Requirements.",
        "Your cards cannot be stopped by cards with Skill Requirements.",
    ] {
        let e = a1(text);
        assert_eq!(e["actions"][0]["@type"], "Unstoppable", "{text:?}");
        assert_eq!(e["actions"][0]["by_skillreq"], true, "{text:?}");
        assert_eq!(e["actions"][0]["by_order"], Value::Null, "{text:?}");
    }

    // "Stop any <T> with \"X\" in the name/text" -> Stop{target: name/text filter}.
    let e = a1("Stop any Submission with \"Over the Top\" in the name.");
    assert_eq!(e["actions"][0]["@type"], "Stop");
    assert_eq!(e["actions"][0]["atk_type"], "Submission");
    assert_eq!(
        e["actions"][0]["target"]["name_contains"][0],
        "Over the Top"
    );
    let e = a1("Stop any Grapple with \"Disqualification\" in the text.");
    assert_eq!(
        e["actions"][0]["target"]["text_contains"][0],
        "Disqualification"
    );
    // A plain stop leaves target null.
    let e = a1("Stop any Strike.");
    assert_eq!(e["actions"][0]["target"], Value::Null);

    // Name-only stop: "Stop \"X\"" -> one Stop keyed on the card NAME, no order/type
    // constraint (order/atk_type null; the engine matches any attack with that name).
    let e = a1("Stop \"Full Nelson\".");
    assert_eq!(e["actions"].as_array().unwrap().len(), 1);
    assert_eq!(e["actions"][0]["@type"], "Stop");
    assert_eq!(e["actions"][0]["order"], Value::Null);
    assert_eq!(e["actions"][0]["atk_type"], Value::Null);
    assert_eq!(e["actions"][0]["target"]["name_contains"][0], "Full Nelson");
    // An OR-list of names -> one Stop whose target matches any of them.
    let e = a1("Stop \"School Boy\" or \"Backslide\".");
    let names = &e["actions"][0]["target"]["name_contains"];
    assert_eq!(names[0], "School Boy");
    assert_eq!(names[1], "Backslide");
    // Oxford-comma three-name list.
    let e = a1("Stop \"Double Death Drop\", \"School Boy\", or \"Backslide\".");
    assert_eq!(
        e["actions"][0]["target"]["name_contains"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
}

/// A "During your turn:" / "During your opponent's turn:" window HEADER scopes every
/// clause that follows to a [`Condition::DuringTurn`] turn phase (mirroring a frequency
/// header), rather than becoming a dangling Unsupported clause of its own.
#[test]
fn during_turn_window_header() {
    // Header + body across a newline: the header is consumed (no Unsupported clause for
    // it), and the body keeps its own OnHit trigger. Its gate rides on the trigger, so
    // the (Always) condition takes the window verbatim.
    let effs = parse_text(
        "During your turn:\nWhen you hit a Strike, draw 1 card.",
        EffectSource::Card,
        None,
        None,
    );
    assert_eq!(effs.len(), 1, "header consumed, only the body remains");
    let e = serde_json::to_value(&effs[0]).unwrap();
    assert_eq!(e["trigger"]["@type"], "OnHit");
    assert_eq!(e["condition"]["@type"], "DuringTurn");
    assert_eq!(e["condition"]["who"], "SELF");

    // A body that carries its OWN condition gets the window AND-ed on top of it.
    let effs = parse_text(
        "During your turn:\nIf the Crowd Meter is 3 or greater, draw 1 card.",
        EffectSource::Card,
        None,
        None,
    );
    let e = serde_json::to_value(&effs[0]).unwrap();
    assert_eq!(e["condition"]["@type"], "And");
    assert_eq!(e["condition"]["items"][0]["@type"], "DuringTurn");
    assert_eq!(e["condition"]["items"][0]["who"], "SELF");
    assert_eq!(e["condition"]["items"][1]["@type"], "CrowdMeterCompare");

    // "your opponent's / target's turn" -> DuringTurn{OPP}.
    let effs = parse_text(
        "During your opponent's turn:\nDraw 1 card.",
        EffectSource::Card,
        None,
        None,
    );
    let e = serde_json::to_value(&effs[0]).unwrap();
    assert_eq!(e["condition"]["@type"], "DuringTurn");
    assert_eq!(e["condition"]["who"], "OPP");

    // A standalone header with nothing after it yields zero effects (fully consumed).
    let effs = parse_text("During your turn:", EffectSource::Card, None, None);
    assert!(
        effs.is_empty(),
        "lone header produces no Unsupported clause"
    );
}

/// Deck-tutor grammar (Search, previously override-only): "Search your deck for <SEL>
/// and <route>" over the three destinations, with typed / named / bare selectors.
#[test]
fn search_deck_tutor() {
    fn a1(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        serde_json::to_value(&effs[0]).unwrap()
    }
    // Bare "N cards" -> empty filter; count carried through; HAND destination.
    let e = a1("Search your deck for 2 cards and add them to your hand.");
    assert_eq!(e["actions"][0]["@type"], "Search");
    assert_eq!(e["actions"][0]["dest"], "HAND");
    assert_eq!(e["actions"][0]["count"], 2);
    assert_eq!(e["actions"][0]["filter"]["play_order"], Value::Null);

    // Typed selector "a Finish" -> play_order filter, count 1.
    let e = a1("Search your deck for a Finish and add it to your hand.");
    assert_eq!(e["actions"][0]["filter"]["play_order"], "Finish");
    assert_eq!(e["actions"][0]["count"], 1);

    // "up to N cards ... discard pile" -> DISCARD.
    let e = a1("Search your deck for up to 2 cards and put them into your discard pile.");
    assert_eq!(e["actions"][0]["dest"], "DISCARD");
    assert_eq!(e["actions"][0]["count"], 2);

    // "... on top of your shuffled deck" -> DECK_TOP, atk_type filter.
    let e = a1("Search your deck for 1 Strike and put it on top of your shuffled deck.");
    assert_eq!(e["actions"][0]["dest"], "DECK_TOP");
    assert_eq!(e["actions"][0]["filter"]["atk_type"], "Strike");

    // Named selector -> name_contains filter.
    let e = a1("Search your deck for 1 card with \"Ladder\" in the name and add it to your hand.");
    assert_eq!(e["actions"][0]["filter"]["name_contains"][0], "Ladder");

    // A selector with no CardFilter (Spotlight) declines cleanly -> Unsupported.
    let e = a1("Search your deck for a Spotlight card and add it to your hand.");
    assert_eq!(e["actions"][0]["@type"], "Unsupported");
}

/// No-DQ match rule (DisqualificationRule, previously override-only): "the match has no
/// disqualifications" (Match scope) and "you cannot be disqualified" (SELF scope), each
/// a Static WhileInPlay toggle, with the redundant static-window prefixes accepted.
#[test]
fn no_disqualifications_rule() {
    fn dq(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        let v = serde_json::to_value(&effs[0]).unwrap();
        assert_eq!(v["trigger"]["@type"], "Static", "{text:?}");
        assert_eq!(v["duration"], "WHILE_IN_PLAY", "{text:?}");
        let a = v["actions"][0].clone();
        assert_eq!(a["@type"], "DisqualificationRule", "{text:?}");
        assert_eq!(a["enabled"], false, "{text:?}");
        a
    }
    for text in [
        "This match has no disqualifications.",
        "When this card is in play the match has no disqualifications.",
        "When this card is in play, the match has no disqualifications.",
        "For the rest of the match, this match now has no disqualifications.",
    ] {
        assert_eq!(dq(text)["scope"], "MATCH", "{text:?}");
    }
    assert_eq!(dq("You cannot be disqualified.")["scope"], "SELF");
}

/// Extra-card grant (PlayExtraCard, previously override-only): "You may play an
/// additional card this turn", the N-copy count, and the gated "If <gate>, …" cascade.
#[test]
fn play_extra_card_grant() {
    fn one(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        serde_json::to_value(&effs[0]).unwrap()
    }
    let e = one("You may play an additional card this turn.");
    assert_eq!(e["actions"][0]["@type"], "PlayExtraCard");
    assert_eq!(e["actions"][0]["order"], Value::Null);
    assert_eq!(e["optional"], true);
    assert_eq!(e["condition"]["@type"], "Always");

    // "2 additional cards" -> two grants (each bumps the extra-plays counter).
    let e = one("You may play 2 additional cards this turn.");
    assert_eq!(e["actions"].as_array().unwrap().len(), 2);

    // "extra" is a synonym for "additional".
    assert_eq!(
        one("Play 1 extra card this turn.")["actions"][0]["@type"],
        "PlayExtraCard"
    );

    // Cascades through the generic gate rule: the gate rides on the condition.
    let e = one("If the Crowd Meter is 3 or greater, you may play an additional card this turn.");
    assert_eq!(e["condition"]["@type"], "CrowdMeterCompare");
    assert_eq!(e["actions"][0]["@type"], "PlayExtraCard");
    assert_eq!(e["optional"], true);
}

/// MinHandSize grammar (mirror of the maximum, previously override-only): the three
/// scope shapes, Static WhileInPlay, signed delta.
#[test]
fn minimum_handsize_mods() {
    fn a1(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        serde_json::to_value(&effs[0]).unwrap()
    }
    let e = a1("Your minimum handsize is +2.");
    assert_eq!(e["trigger"]["@type"], "Static");
    assert_eq!(e["duration"], "WHILE_IN_PLAY");
    assert_eq!(e["actions"][0]["@type"], "MinHandSize");
    assert_eq!(e["actions"][0]["delta"], 2);
    assert_eq!(e["actions"][0]["who"], "SELF");

    assert_eq!(
        a1("Your opponent's minimum hand size is -1.")["actions"][0]["who"],
        "OPP"
    );

    // "Each player's" fans out to two actions (SELF + OPP).
    let e = a1("Each player's minimum handsize is +1.");
    let acts = e["actions"].as_array().unwrap();
    assert_eq!(acts.len(), 2);
    assert_eq!(acts[0]["@type"], "MinHandSize");
}

/// Reveal-then family (RevealThen, schema v95): deck top/bottom and random-hand reveals
/// with a name/atk filter, the "add that card to your hand" take, and a parsed +
/// optionally-gated consequence tail.
#[test]
fn reveal_then_family() {
    fn a1(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        serde_json::to_value(&effs[0]).unwrap()
    }
    // Deck top, name match, take-to-hand + optional re-roll rider.
    let e = a1("Reveal the top card of your deck: if it has \"Baseball Bat\" in the name, add that card to your hand, and you may re-roll your next turn roll.");
    let a = &e["actions"][0];
    assert_eq!(a["@type"], "RevealThen");
    assert_eq!(a["reveal_from"], "DECK_TOP");
    assert_eq!(a["filter"]["name_contains"][0], "Baseball Bat");
    assert_eq!(a["take_matched"], true);
    assert_eq!(a["then_optional"], true);
    assert_eq!(a["then"][0]["@type"], "Reroll");

    // Random hand, name match, mandatory draw (no take).
    let a =
        a1("Randomly reveal 1 card in your hand: if it has \"Guitar\" in the name, draw 1 card.")
            ["actions"][0]
            .clone();
    assert_eq!(a["reveal_from"], "HAND_RANDOM");
    assert_eq!(a["take_matched"], false);
    assert_eq!(a["then_optional"], false);
    assert_eq!(a["then"][0]["@type"], "Draw");

    // Attack-type filter (deck bottom).
    let a = a1("Reveal the bottom card of your deck, if it is a Strike, draw 1 card.")["actions"]
        [0]
    .clone();
    assert_eq!(a["reveal_from"], "DECK_BOTTOM");
    assert_eq!(a["filter"]["atk_type"], "Strike");
    assert_eq!(a["then"][0]["@type"], "Draw");

    // "…bury 1 card in any player's discard pile" now parses (Bury.choose), so the
    // reveal-then composes with a Bury consequence.
    let a = a1("Randomly reveal 1 card in your hand: if it has \"Drumstick\" in the name, bury 1 card in any player's discard pile.")["actions"][0].clone();
    assert_eq!(a["@type"], "RevealThen");
    assert_eq!(a["filter"]["name_contains"][0], "Drumstick");
    assert_eq!(a["then"][0]["@type"], "Bury");
    assert_eq!(a["then"][0]["choose"], true);

    // A consequence with no grammar still declines (reveal-then stays Unsupported).
    let a = a1("Randomly reveal 1 card in your hand: if it has \"Widget\" in the name, do something unmodelable.")["actions"][0].clone();
    assert_eq!(a["@type"], "Unsupported");

    // The bare header with no inline "if" stays Unsupported (its consequence is a
    // separate follow-up clause).
    assert_eq!(
        a1("Reveal the top card of your deck:")["actions"][0]["@type"],
        "Unsupported"
    );
}

/// Split-clause reveal: a bare "Reveal the top/bottom card of your deck:" header whose
/// "If <filter>, <consequence>" lands on the NEXT clause is combined into one RevealThen;
/// a header with no valid follow-up stays Unsupported (never silently dropped).
#[test]
fn reveal_then_split_clause() {
    // Header + follow-up across a newline -> one combined RevealThen (deck bottom, take).
    let effs = parse_text(
        "Reveal the bottom card of your deck:\nIf it has \"Wrapping Paper\" in the name, add that card to your hand.",
        EffectSource::Card,
        None,
        None,
    );
    assert_eq!(effs.len(), 1, "the two clauses fold into one effect");
    let a = serde_json::to_value(&effs[0]).unwrap()["actions"][0].clone();
    assert_eq!(a["@type"], "RevealThen");
    assert_eq!(a["reveal_from"], "DECK_BOTTOM");
    assert_eq!(a["filter"]["name_contains"][0], "Wrapping Paper");
    assert_eq!(a["take_matched"], true);

    // Play-order filter + "add it to your hand" (the "it" refers to the revealed card),
    // with a mandatory roll rider.
    let effs = parse_text(
        "Reveal the top card of your deck:\nIf it is a Strike, add it to your hand and your next turn roll is +1.",
        EffectSource::Card,
        None,
        None,
    );
    let a = serde_json::to_value(&effs[0]).unwrap()["actions"][0].clone();
    assert_eq!(a["@type"], "RevealThen");
    assert_eq!(a["filter"]["atk_type"], "Strike");
    assert_eq!(a["take_matched"], true);
    assert_eq!(a["then"][0]["@type"], "ModifyRoll");

    // A header whose next clause ISN'T a filtered consequence: the header stays
    // Unsupported and the next clause is compiled on its own.
    let effs = parse_text(
        "Reveal the top card of your deck:\nDraw 1 card.",
        EffectSource::Card,
        None,
        None,
    );
    assert_eq!(effs.len(), 2);
    let v0 = serde_json::to_value(&effs[0]).unwrap();
    assert_eq!(v0["actions"][0]["@type"], "Unsupported");
    assert_eq!(
        serde_json::to_value(&effs[1]).unwrap()["actions"][0]["@type"],
        "Draw"
    );
}

/// Reveal-and-discard, single/conditional phrasing: "<opponent> randomly reveals N
/// card(s) in their hand; if it is a Stop, they discard it" folds into
/// `RevealAndDiscard{count:N, who:OPP}` (discarding a revealed stop out of N == discard
/// all revealed stops). The enclosing trigger prefix supplies the real trigger.
#[test]
fn reveal_and_discard_if_stop() {
    fn a0(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        serde_json::to_value(&effs[0]).unwrap()
    }

    // OnHit name-gated prefix; "a card" -> count 1.
    let e = a0("When you hit a card with \"Bomb\" or \"America\" in the name, your opponent randomly reveals a card in their hand; If it is a Stop, they discard it.");
    assert_eq!(e["trigger"]["@type"], "OnHit");
    assert_eq!(e["trigger"]["name_contains"][0], "Bomb");
    let a = &e["actions"][0];
    assert_eq!(a["@type"], "RevealAndDiscard");
    assert_eq!(a["count"], 1);
    assert_eq!(a["who"], "OPP");

    // OnRoll prefix; "1 card"; subject "Your opponent"; "if it is a stop" (lowercase).
    let e = a0("When you roll Power for your turn roll: Your opponent randomly reveals 1 card in their hand; if it is a stop, they discard it.");
    assert_eq!(e["trigger"]["@type"], "OnRoll");
    assert_eq!(e["trigger"]["skill"], "Power");
    assert_eq!(e["actions"][0]["@type"], "RevealAndDiscard");

    // "When your opponent stops a card" -> OnStop{YOURS}; subject "they randomly reveal".
    let e = a0("When your opponent stops a card, they randomly reveal 1 card in their hand; if it is a Stop, they discard it.");
    assert_eq!(e["trigger"]["@type"], "OnStop");
    assert_eq!(e["trigger"]["dir"], "YOURS");
    assert_eq!(e["actions"][0]["@type"], "RevealAndDiscard");

    // Plural-typo standalone: "reveals 3 cards in their hands and discards all Stops".
    let a = &a0("Your opponent randomly reveals 3 cards in their hands and discards all Stops.")
        ["actions"][0];
    assert_eq!(a["@type"], "RevealAndDiscard");
    assert_eq!(a["count"], 3);

    // Compound draw-OR-discard else-branch stays Unsupported (RevealAndDiscard can't
    // express the "if not, they discard it" alternative).
    assert_eq!(
        a0("When you hit a card with \"Kick\" or \"Dragon\" in the name, your opponent randomly reveals one card in their hand: if it's a stop, draw 1 card, if not, they discard it.")
            ["actions"][0]["@type"],
        "Unsupported"
    );
}

/// WHILE_IN_DISCARD self-trigger (task #115 slice 1): "When this card is in your discard
/// pile and you roll <S> for your turn roll, [you may] <self-body>" — the discard prefix
/// is a `Duration::WhileInDiscard` marker; the remainder re-parses as a normal OnRoll
/// trigger clause with the self-action body. Only OnRoll fires from discard today, so
/// non-OnRoll (OnHit) and passive bodies decline to Unsupported.
#[test]
fn while_in_discard_onroll_self_recursion() {
    fn eff0(text: &str) -> Value {
        let effs = parse_text(text, EffectSource::Card, None, None);
        assert_eq!(effs.len(), 1, "one effect for {text:?}");
        serde_json::to_value(&effs[0]).unwrap()
    }

    // "add it to your hand" -> AddSelfToHand, OnRoll{skill}, WHILE_IN_DISCARD, optional.
    let e = eff0("When this card is in your discard pile and you roll Power for your turn roll, you may add it to your hand.");
    assert_eq!(e["trigger"]["@type"], "OnRoll");
    assert_eq!(e["trigger"]["skill"], "Power");
    assert_eq!(e["duration"], "WHILE_IN_DISCARD");
    assert_eq!(e["optional"], true);
    assert_eq!(e["actions"][0]["@type"], "AddSelfToHand");

    // "shuffle it into your deck" -> ShuffleSelfIntoDeck (mandatory, no "you may").
    let e = eff0("When this card is in your discard pile and you roll Agility for your turn roll, shuffle it into your deck.");
    assert_eq!(e["duration"], "WHILE_IN_DISCARD");
    assert_eq!(e["optional"], false);
    assert_eq!(e["actions"][0]["@type"], "ShuffleSelfIntoDeck");

    // A plain (non-self) body still attaches to OnRoll + WHILE_IN_DISCARD.
    let e = eff0("When this card is in your discard pile and you roll Strike for your turn roll, draw 1 card.");
    assert_eq!(e["actions"][0]["@type"], "Draw");
    assert_eq!(e["duration"], "WHILE_IN_DISCARD");

    // OnHit-triggered discard recursion declines (engine doesn't fire it from discard
    // yet) -> stays Unsupported rather than becoming silently-inert IR.
    assert_eq!(
        eff0("When this card is in your discard pile and you hit a card with \"Suplex\" in the name, you may shuffle it into your deck.")
            ["actions"][0]["@type"],
        "Unsupported"
    );
    // A passive body (family A) also declines for now.
    assert_eq!(
        eff0("When this card is in your discard pile, your maximum handsize is +1.")["actions"][0]
            ["@type"],
        "Unsupported"
    );
}

/// "If this is a Steel Cage or Liger's Den match, you may flip both cards instead"
/// (Friends and Rivals family): the preceding "each player reveals the top card of
/// their deck and adds it to their hand" is rewritten so the add applies only OUTSIDE
/// those match types, and INSIDE them the player chooses add-both or flip-both.
#[test]
fn flip_both_instead_replaces_add_to_hand() {
    let effs = parse_text(
        "Stop any Lead Strike.\n\
         Each player reveals the top card of their deck and adds it to their hand.\n\
         If this is a Steel Cage or Liger's Den match, you may flip both cards instead.",
        EffectSource::Card,
        None,
        None,
    );
    assert_eq!(effs.len(), 3, "stop + gated add + gated choice");
    let v: Vec<Value> = effs
        .iter()
        .map(|e| serde_json::to_value(e).unwrap())
        .collect();

    // The stop is untouched.
    assert_eq!(v[0]["actions"][0]["@type"], "Stop");

    // The add-to-hand now fires only when it is NOT one of the flip match types.
    assert_eq!(v[1]["actions"].as_array().unwrap().len(), 2);
    assert_eq!(v[1]["actions"][0]["@type"], "Draw");
    assert_eq!(v[1]["condition"]["@type"], "Not");
    assert_eq!(v[1]["condition"]["item"]["@type"], "IsMatchType");
    assert_eq!(v[1]["condition"]["item"]["types"][0], "STEEL_CAGE");

    // Inside those match types, a Choice between add-both and flip-both.
    assert_eq!(v[2]["condition"]["@type"], "IsMatchType");
    assert_eq!(v[2]["actions"][0]["@type"], "Choice");
    let opts = v[2]["actions"][0]["options"].as_array().unwrap();
    assert_eq!(opts[0]["actions"][0]["@type"], "Draw");
    assert_eq!(opts[1]["actions"][0]["@type"], "Flip");
    assert_eq!(
        opts[1]["actions"].as_array().unwrap().len(),
        2,
        "flip both decks"
    );

    // A stray "flip both cards instead" with no preceding reveal-both stays Unsupported.
    let lone = parse_text(
        "If this is a Steel Cage match, you may flip both cards instead.",
        EffectSource::Card,
        None,
        None,
    );
    assert_eq!(
        serde_json::to_value(&lone[0]).unwrap()["actions"][0]["@type"],
        "Unsupported"
    );
}
