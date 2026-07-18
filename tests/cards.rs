//! Domain-model round-trip (task #69).
//!
//! The conformance fixtures embed two full serialized [`Deck`]s (competitor +
//! entrance + 30 cards each, with finish-bonus maps and compiled effect IR).
//! Deserializing and re-serializing each must be value-identical, validating the
//! `cards.rs` port against real card data.

use serde_json::Value;
use srg_core::cards::Deck;
use std::path::PathBuf;

fn fixture_decks() -> Vec<(String, Value)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/conformance");
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("read conformance dir") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let doc: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read fixture"))
                .expect("valid fixture json");
        let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
        for side in ["A", "B"] {
            out.push((format!("{stem}:{side}"), doc["decks"][side].clone()));
        }
    }
    out
}

#[test]
fn decks_round_trip() {
    let decks = fixture_decks();
    assert!(!decks.is_empty(), "no fixture decks found");
    for (label, deck_json) in decks {
        let deck: Deck = serde_json::from_value(deck_json.clone())
            .unwrap_or_else(|e| panic!("{label}: deserialize Deck failed: {e}"));
        assert!(
            deck.is_valid(),
            "{label}: deck should be valid (30 cards 1..=30)"
        );
        let reser = serde_json::to_value(&deck).expect("serialize Deck");
        assert_eq!(reser, deck_json, "{label}: Deck did not round-trip");
    }
}
