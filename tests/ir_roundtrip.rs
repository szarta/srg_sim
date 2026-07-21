//! Effect IR round-trip conformance (task #66).
//!
//! The Rust serde types in `srg_core::ir` must round-trip the *same* JSON the
//! Python dataclasses emit. Two committed corpora guard this:
//!
//!   * `fixtures/ir/deck_effects.json` — every `Effect` node compiled from the
//!     six reference decks (real `cards.ir.json` data, 42 distinct node types).
//!   * `fixtures/ir/all_nodes.json` — one schema-minimal instance of *all* 106
//!     node types, so every `IrNode` variant is exercised even if no deck uses
//!     it yet.
//!
//! For each node we assert `parse -> serialize` is value-identical to the
//! source JSON. Comparison is on `serde_json::Value` (order-independent): the
//! IR is static input, so semantic equality — not byte layout — is the contract.

use serde_json::Value;
use srg_core::ir::IrNode;
use std::path::PathBuf;

fn fixture(name: &str) -> Vec<Value> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/ir")
        .join(name);
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    match serde_json::from_str(&text).expect("fixture is a JSON array") {
        Value::Array(items) => items,
        other => panic!("expected a JSON array, got {other:?}"),
    }
}

/// Deserialize into the typed `IrNode`, re-serialize, and compare values.
fn assert_roundtrips(nodes: &[Value]) {
    for (i, original) in nodes.iter().enumerate() {
        let tag = original
            .get("@type")
            .and_then(Value::as_str)
            .unwrap_or("<no @type>");
        let node: IrNode = serde_json::from_value(original.clone())
            .unwrap_or_else(|e| panic!("node {i} ({tag}): deserialize failed: {e}"));
        let reser = serde_json::to_value(&node)
            .unwrap_or_else(|e| panic!("node {i} ({tag}): serialize failed: {e}"));
        assert_eq!(&reser, original, "node {i} ({tag}) did not round-trip");
    }
}

#[test]
fn deck_effects_round_trip() {
    let nodes = fixture("deck_effects.json");
    assert!(!nodes.is_empty(), "deck_effects.json is empty");
    assert_roundtrips(&nodes);
}

#[test]
fn all_node_types_round_trip() {
    let nodes = fixture("all_nodes.json");
    assert_roundtrips(&nodes);
}

/// The exhaustive corpus must cover exactly the 106 node types in the schema —
/// a guard that the union stays complete as the contract evolves.
#[test]
fn all_nodes_covers_every_type() {
    let nodes = fixture("all_nodes.json");
    let mut tags: Vec<String> = nodes
        .iter()
        .map(|n| n["@type"].as_str().expect("@type is a string").to_owned())
        .collect();
    tags.sort();
    tags.dedup();
    assert_eq!(
        tags.len(),
        106,
        "expected 106 distinct node types, got {}",
        tags.len()
    );
}
