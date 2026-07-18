//! rules_text -> IR parser parity (task #71).
//!
//! `fixtures/parser/clauses.json` pairs every card / competitor / entrance's RAW
//! text across the six reference decks with the Effect IR the Python
//! `rules_parser.parse_text` produced (overrides + grammar + Unsupported). The
//! Rust parser must reproduce each list value-identically — the grammar rules,
//! their order, the clause splitter, frequency headers, metadata skipping, and
//! the override table all matching.

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
