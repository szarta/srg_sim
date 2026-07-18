//! Cross-language parser parity (task 75, `docs/design/substrate-split.md` §6).
//!
//! The rules parser is the one part of the engine that is **RNG-independent** —
//! pure `rules_text -> [Effect]` — so it is the piece that *can* be checked
//! byte-for-byte against the Python oracle (the seeded engine cannot: the Rust
//! canonical stream is splitmix64 while the `python` branch is Mersenne Twister,
//! an accepted design split, so whole-log parity is owned by the frozen
//! conformance corpus in `engine_conformance.rs`, not by re-running Python).
//!
//! `scripts/gen_cards_ir.py` drives the **Python parity oracle** over the whole
//! card DB and writes `cards.ir.json` — every parseable record's input text with
//! the IR the oracle compiles it to. This test re-parses each record with the Rust
//! parser and asserts value-identical IR. A grammar divergence between the two
//! ports surfaces here as a failing record.
//!
//! The corpus is a *generated* artifact (its true source, `cards.yaml`, is not
//! vendored), so it is not committed; `invoke conformance` regenerates it into
//! `target/conformance/cards.ir.json` first. When it is absent — a bare
//! `cargo test` / `invoke check`, which stay Python-free — this test **skips
//! loudly** rather than failing. Override `$SRG_CARDS_IR` to point elsewhere.

use serde_json::Value;
use srg_core::ir::EffectSource;
use srg_core::parser::{load_overrides, parse_text, Overrides};
use std::path::PathBuf;

/// Where `invoke conformance` writes the generated corpus.
fn corpus_path() -> PathBuf {
    match std::env::var_os("SRG_CARDS_IR") {
        Some(p) => PathBuf::from(p),
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/conformance/cards.ir.json"),
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

/// Every record's Rust-parsed IR must equal the oracle's, value-for-value.
#[test]
fn rust_parser_matches_oracle_ir() {
    let path = corpus_path();
    if !path.exists() {
        eprintln!(
            "SKIP rust_parser_matches_oracle_ir: {} not found.\n\
             Generate it with `invoke conformance` (needs the Python oracle at ~/data/srg_sim_python),\n\
             or point $SRG_CARDS_IR at an existing cards.ir.json.",
            path.display()
        );
        return;
    }
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
                    "MISMATCH {} (source {:?})\n  text: {:?}\n  rust: {}\n  py  : {}",
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
        "{mismatches}/{} records diverged from the Python oracle (first 10 shown above)",
        records.len()
    );
    eprintln!("parser parity: {} records match the oracle", records.len());
}
