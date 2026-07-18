//! Cross-language RNG parity (task #68).
//!
//! `fixtures/rng/splitmix64.json` was generated from the canonical Python
//! splitmix64 reference (DESIGN.md §5). Every primitive — the raw `next()`
//! stream, `roll`, `shuffle`, `reveal`, `randint`, and the post-draw state —
//! must reproduce it bit-for-bit, so Rust and Python logs stay in lockstep.
//! Each field is drawn from a FRESH generator on the same seed, so it pins that
//! one primitive independently.

use serde::Deserialize;
use srg_core::rng::SeededRNG;
use std::path::PathBuf;

#[derive(Deserialize)]
struct Fixture {
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    seed: u64,
    next: Vec<u64>,
    rolls: Vec<String>,
    shuffle_in: Vec<i64>,
    shuffle_out: Vec<i64>,
    reveal_from: Vec<i64>,
    reveal_seq: Vec<i64>,
    randint_1_6: Vec<i64>,
    randint_7_7: Vec<i64>,
    state_after_10_next: u64,
}

fn load() -> Fixture {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/rng/splitmix64.json");
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&text).expect("valid rng fixture")
}

#[test]
fn raw_next_stream_matches() {
    for case in load().cases {
        let mut rng = SeededRNG::new(case.seed);
        let got: Vec<u64> = (0..case.next.len()).map(|_| rng.next_u64()).collect();
        assert_eq!(
            got, case.next,
            "next() stream diverged for seed {}",
            case.seed
        );
    }
}

#[test]
fn roll_maps_faces_identically() {
    for case in load().cases {
        let mut rng = SeededRNG::new(case.seed);
        let got: Vec<String> = (0..case.rolls.len())
            .map(|_| rng.roll().name().to_owned())
            .collect();
        assert_eq!(got, case.rolls, "roll() diverged for seed {}", case.seed);
    }
}

#[test]
fn shuffle_is_deterministic() {
    for case in load().cases {
        let mut rng = SeededRNG::new(case.seed);
        let mut items = case.shuffle_in.clone();
        rng.shuffle(&mut items);
        assert_eq!(
            items, case.shuffle_out,
            "shuffle() diverged for seed {}",
            case.seed
        );
    }
}

#[test]
fn reveal_picks_identically() {
    for case in load().cases {
        let mut rng = SeededRNG::new(case.seed);
        let got: Vec<i64> = (0..case.reveal_seq.len())
            .map(|_| *rng.reveal(&case.reveal_from).expect("non-empty"))
            .collect();
        assert_eq!(
            got, case.reveal_seq,
            "reveal() diverged for seed {}",
            case.seed
        );
    }
}

#[test]
fn randint_matches() {
    for case in load().cases {
        let mut rng = SeededRNG::new(case.seed);
        let got: Vec<i64> = (0..case.randint_1_6.len())
            .map(|_| rng.randint(1, 6))
            .collect();
        assert_eq!(
            got, case.randint_1_6,
            "randint(1,6) diverged for seed {}",
            case.seed
        );

        let mut rng = SeededRNG::new(case.seed);
        let got: Vec<i64> = (0..case.randint_7_7.len())
            .map(|_| rng.randint(7, 7))
            .collect();
        assert_eq!(
            got, case.randint_7_7,
            "randint(7,7) single-value range diverged for seed {}",
            case.seed
        );
    }
}

#[test]
fn snapshot_state_matches_and_resumes() {
    for case in load().cases {
        // The captured state after 10 draws must equal the Python reference.
        let mut rng = SeededRNG::new(case.seed);
        for _ in 0..10 {
            rng.next_u64();
        }
        assert_eq!(
            rng.snapshot().state,
            case.state_after_10_next,
            "snapshot state diverged for seed {}",
            case.seed
        );

        // Restore resumes the stream bit-for-bit from the captured point.
        let snap = rng.snapshot();
        let resumed: Vec<u64> = {
            let mut r = SeededRNG::new(case.seed);
            for _ in 0..10 {
                r.next_u64();
            }
            (0..5).map(|_| r.next_u64()).collect()
        };
        let mut r = SeededRNG::restore(&snap);
        let after: Vec<u64> = (0..5).map(|_| r.next_u64()).collect();
        assert_eq!(
            after, resumed,
            "restore() did not resume for seed {}",
            case.seed
        );
    }
}
