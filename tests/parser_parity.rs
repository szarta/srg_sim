//! Parser regression golden (`docs/design/substrate-split.md` §6, Phase 2).
//!
//! The rules parser is the one part of the engine that is **RNG-independent** —
//! pure `rules_text -> [Effect]` — so it is checked against a committed golden
//! corpus, `fixtures/parser/cards.ir.json`: every parseable DB record's input text
//! with the Effect IR it compiles to. This test re-parses each record with the Rust
//! parser and asserts value-identical IR, so any unintended parser change surfaces
//! here as a failing record.
//!
//! **Provenance.** The corpus was validated cross-language against the Python parser
//! oracle at Phase-1 (`tests/parser_parity` over the whole DB, 6386/6386). At Phase 2
//! (task #79) the Python engine was retired; the corpus is now regenerated from the
//! authoritative Rust parser via `srg cards-ir` and committed. It is a **snapshot
//! regression guard**: change the parser (or update the card DB) → regenerate with
//! `srg cards-ir` and review the diff. `$SRG_CARDS_IR` overrides the path.

use serde_json::Value;
use srg_core::ir::EffectSource;
use srg_core::parser::{load_overrides, parse_text, Overrides};
use std::path::PathBuf;

/// The committed parser golden (`srg cards-ir` regenerates it).
fn corpus_path() -> PathBuf {
    match std::env::var_os("SRG_CARDS_IR") {
        Some(p) => PathBuf::from(p),
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/parser/cards.ir.json"),
    }
}

/// The Rust override table, loaded from the committed pre-expanded `overrides.ir.json`.
fn overrides() -> Overrides {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("overrides.ir.json");
    let json = std::fs::read_to_string(&path).expect("read overrides.ir.json");
    load_overrides(&json).expect("parse overrides.ir.json")
}

/// One `cards.ir.json` record: the oracle's input text and the IR it produced.
struct Record {
    db_uuid: String,
    source: EffectSource,
    rules_text: String,
    effects: Value,
}

fn load_corpus(path: &PathBuf) -> Vec<Record> {
    let text = std::fs::read_to_string(path).expect("read cards.ir.json");
    let rows: Vec<Value> = serde_json::from_str(&text).expect("cards.ir.json is a JSON array");
    rows.into_iter()
        .map(|r| Record {
            db_uuid: r["db_uuid"].as_str().expect("db_uuid").to_owned(),
            source: serde_json::from_value(r["source"].clone()).expect("source"),
            rules_text: r["rules_text"].as_str().expect("rules_text").to_owned(),
            effects: r["effects"].clone(),
        })
        .collect()
}

/// Every record's Rust-parsed IR must equal the committed golden, value-for-value.
#[test]
fn rust_parser_matches_golden() {
    let path = corpus_path();
    let overrides = overrides();
    let records = load_corpus(&path);
    assert!(!records.is_empty(), "cards.ir.json is empty");

    let mut mismatches = 0usize;
    for rec in &records {
        let effects = parse_text(
            &rec.rules_text,
            rec.source,
            Some(&rec.db_uuid),
            Some(&overrides),
        );
        let got = serde_json::to_value(&effects).expect("serialize parsed effects");
        if got != rec.effects {
            mismatches += 1;
            if mismatches <= 10 {
                eprintln!(
                    "MISMATCH {} (source {:?})\n  text: {:?}\n  now : {}\n  gold: {}",
                    rec.db_uuid,
                    rec.source,
                    rec.rules_text,
                    serde_json::to_string(&got).unwrap(),
                    serde_json::to_string(&rec.effects).unwrap(),
                );
            }
        }
    }
    assert_eq!(
        mismatches,
        0,
        "{mismatches}/{} records diverged from the parser golden — regenerate with \
         `srg cards-ir` and review the diff (first 10 shown above)",
        records.len()
    );
    eprintln!("parser golden: {} records match", records.len());
}
