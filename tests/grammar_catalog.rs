//! Guards the grammar catalog against silent drift.
//!
//! The DB-free rule inventory (`fixtures/parser/rule_index.json`) must equal what
//! the live parser produces. If you add, remove, or reorder a grammar rule — or
//! change a regex — this fails; regenerate with `invoke grammar-catalog` so the
//! readable reference (`docs/development/grammar-catalog.md`) stays in lockstep.
//! Unlike the parser golden, this needs no card DB: it only reads the rule table.

#[test]
fn rule_index_matches_committed() {
    let committed = std::fs::read_to_string("fixtures/parser/rule_index.json")
        .expect("read fixtures/parser/rule_index.json");
    let live = srg_core::parser::rule_index_json();
    assert_eq!(
        committed, live,
        "grammar rule inventory drifted from fixtures/parser/rule_index.json — run \
         `invoke grammar-catalog` to regenerate the catalog (readable doc + inventory)."
    );
}
