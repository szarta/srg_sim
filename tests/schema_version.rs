//! Guards that the Rust schema-version constants stay in lockstep with the pinned
//! JSON Schema contracts (`schemas/v1/`). The engine reports these to the frontend
//! via `version_info()` / `srg info` / `WasmSession.version()`; if a constant drifts
//! from its schema file, the client's no-skew assertion is comparing a stale number.

use serde_json::Value;
use std::path::PathBuf;

fn schema(name: &str) -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("schemas/v1")
        .join(name);
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

#[test]
fn effect_ir_schema_version_matches_constant() {
    let v = schema("effect_ir.schema.json")["version"].as_i64();
    assert_eq!(
        v,
        Some(srg_core::ir::SCHEMA_VERSION),
        "ir::SCHEMA_VERSION must equal schemas/v1/effect_ir.schema.json version"
    );
}

#[test]
fn version_info_exposes_engine_commit_and_schemas() {
    let info = srg_core::version_info();
    assert!(info["engine"].is_string(), "engine version present");
    assert!(info["commit"].is_string(), "git commit stamp present");
    assert_eq!(
        info["schemas"]["effect_ir"].as_i64(),
        Some(srg_core::ir::SCHEMA_VERSION)
    );
    // The policy roster is non-empty and includes the golden-path opponent.
    let policies = info["policies"].as_array().expect("policies array");
    assert!(policies.iter().any(|p| p == "heuristic"));
}
