//! Game-log schema parity (task #70).
//!
//! Every conformance fixture embeds a full canonical log (`[header, *events]`)
//! produced by the Python engine — real streams covering every event type. The
//! Rust `gamelog` structs must reproduce that canonical form exactly:
//!
//!   * each fixture log deserializes into a typed [`GameLog`] and its
//!     [`canonical`](GameLog::canonical) re-serialization is value-identical to
//!     the stored form (the `from` alias, `t`/`type` tags, always-present
//!     optional/`null` fields, and defaults all match);
//!   * the JSONL text round-trip (`to_lines` → `parse`) is lossless.

use serde_json::Value;
use srg_core::gamelog::{Event, GameLog, Header};
use std::collections::BTreeSet;
use std::path::PathBuf;

/// Build a typed `GameLog` from a fixture's canonical `log` (list of objects).
fn game_log_from_rows(rows: &[Value]) -> GameLog {
    let (header_row, event_rows) = rows.split_first().expect("log has a header row");
    let header: Header = serde_json::from_value(header_row.clone()).expect("header deserializes");
    let events: Vec<Event> = event_rows
        .iter()
        .map(|r| serde_json::from_value(r.clone()).expect("event deserializes"))
        .collect();
    let mut log = GameLog::new(header);
    for e in events {
        log.append(e);
    }
    log
}

fn fixtures() -> Vec<(String, Vec<Value>)> {
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
        let rows = doc["log"].as_array().expect("log array").clone();
        out.push((stem, rows));
    }
    out
}

#[test]
fn canonical_log_round_trips() {
    let fixtures = fixtures();
    assert!(!fixtures.is_empty(), "no conformance fixtures found");
    let mut event_types = BTreeSet::new();
    for (label, rows) in &fixtures {
        let log = game_log_from_rows(rows);
        // Record which event types this corpus exercises (coverage guard below).
        for row in &rows[1..] {
            event_types.insert(row["type"].as_str().unwrap().to_owned());
        }
        assert_eq!(
            &log.canonical(),
            rows,
            "{label}: canonical log did not round-trip"
        );
    }
    // The corpus should exercise a broad slice of the §8 event union.
    assert!(
        event_types.len() >= 10,
        "expected >=10 distinct event types across the corpus, saw {}: {event_types:?}",
        event_types.len()
    );
}

#[test]
fn jsonl_text_round_trips() {
    for (label, rows) in fixtures() {
        let log = game_log_from_rows(&rows);
        let lines = log.to_lines();
        let reparsed = GameLog::parse(&lines).expect("parse JSONL");
        assert_eq!(reparsed, log, "{label}: JSONL text round-trip diverged");
        // First line is the header; the rest are events.
        assert_eq!(lines.len(), rows.len(), "{label}: line count");
    }
}

#[test]
fn parse_rejects_empty_log() {
    let empty: Vec<String> = vec![" ".to_owned(), "".to_owned()];
    assert!(
        GameLog::parse(&empty).is_err(),
        "blank-only input must error"
    );
}
