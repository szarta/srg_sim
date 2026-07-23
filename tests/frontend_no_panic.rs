//! Frontend-integration guard (task #124, `FRONTEND_INTEGRATION_BRIEF.md`).
//!
//! The single worst failure mode for the in-browser "Run It Back" play screen is a
//! WASM panic: it poisons the module and kills the session mid-match. This test
//! plays the golden-path test decks — **The Bull** vs **Fae Dragon**, seat B
//! `heuristic` — from `open` to a terminal `Done` across many seeds, driving seat A
//! exactly as the browser does (a remote seat answered via `submit`), and asserts
//! the match never panics and always terminates. Unsupported clauses (~36% of the
//! main deck) must degrade to no-ops, never `unwrap()`/`panic!`.
//!
//! It drives the native [`Session`] rather than `WasmSession`, which is a thin JSON
//! wrapper over exactly this logic (`src/wasm.rs`) — a panic here is a panic there.
//! The decks are the committed UI fixtures `web/src/sample/deck{A,B}.json`, so this
//! also guards that they stay valid against the current `Deck` schema.

use srg_core::cards::Deck;
use srg_core::engine::{DecisionResponse, Step};
use srg_core::session::{Seat, Session};
use std::collections::BTreeMap;
use std::path::PathBuf;

fn sample_deck(name: &str) -> Deck {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("web/src/sample")
        .join(name);
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

fn seats() -> BTreeMap<String, Seat> {
    BTreeMap::from([
        ("A".to_owned(), Seat::from_spec("remote")), // the human, driven by submit
        ("B".to_owned(), Seat::from_spec("heuristic")),
    ])
}

/// A full human-vs-AI match must reach `Done` without panicking, for every seed —
/// the exact wire path the browser drives. Seat A's choices are varied per seed and
/// per decision to exercise many `legal[]` branches (not just `legal[0]`).
#[test]
fn bull_vs_fae_remote_never_panics_across_seeds() {
    const STEP_CAP: usize = 5_000; // a real match resolves in well under this
    for seed in 0..40u64 {
        let (mut session, mut step) = Session::open(
            sample_deck("deckA.json"),
            sample_deck("deckB.json"),
            seats(),
            seed,
            String::new(),
            "real".to_owned(),
        )
        .unwrap_or_else(|e| panic!("open (seed {seed}): {e}"));

        let mut steps = 0usize;
        let result = loop {
            match step {
                Step::Done(res) => break res,
                Step::Decision(req) => {
                    assert!(
                        !req.legal.is_empty(),
                        "seed {seed}: empty legal set at point {:?}",
                        req.point
                    );
                    // Vary the pick by seed + decision seq to walk many option branches.
                    let idx =
                        ((seed as usize).wrapping_mul(7) + req.seq as usize * 13) % req.legal.len();
                    let chosen = req.legal[idx].clone();
                    step = session.submit(DecisionResponse {
                        request_id: req.request_id,
                        chosen,
                    });
                    steps += 1;
                    assert!(steps < STEP_CAP, "seed {seed}: match did not terminate");
                }
            }
        };

        assert!(
            ["A", "B", "draw"].contains(&result.winner.as_str()),
            "seed {seed}: unexpected winner {:?}",
            result.winner
        );
        assert!(result.turns >= 0, "seed {seed}: negative turn count");
    }
}

/// Both seats resolved by local policies (including `random`, the broadest branch
/// fuzzer) run straight to `Done` without suspending or panicking.
#[test]
fn bull_vs_fae_local_policies_never_panic() {
    for (pa, pb) in [
        ("random", "random"),
        ("aggressive", "smart"),
        ("newbie", "heuristic"),
    ] {
        for seed in 0..20u64 {
            let map = BTreeMap::from([
                ("A".to_owned(), Seat::from_spec(pa)),
                ("B".to_owned(), Seat::from_spec(pb)),
            ]);
            let (session, step) = Session::open(
                sample_deck("deckA.json"),
                sample_deck("deckB.json"),
                map,
                seed,
                String::new(),
                "sim".to_owned(),
            )
            .unwrap_or_else(|e| panic!("open {pa} vs {pb} (seed {seed}): {e}"));

            match step {
                Step::Done(res) => assert!(
                    ["A", "B", "draw"].contains(&res.winner.as_str()),
                    "{pa} vs {pb} seed {seed}: unexpected winner {:?}",
                    res.winner
                ),
                Step::Decision(req) => panic!(
                    "{pa} vs {pb} seed {seed}: local policies suspended at {:?}",
                    req.point
                ),
            }
            let _ = session.result();
        }
    }
}
