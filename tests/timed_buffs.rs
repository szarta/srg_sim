//! Timed skill buffs (task #79 / Snake Pitt Super Lucha): the `UntilEndOfTurn` and
//! `UntilStartOfYourNextTurn` durations, their stacking-with-cap accumulation, and
//! their sweeps. Driven against bull_fae_fresh (positions.json) by granting buffs
//! directly onto the state and reading the derived stats — the one view that feeds
//! turn rolls, Finish rolls and breakout rolls alike.

use serde_json::Value;
use srg_core::ir::{Duration, Skill};
use srg_core::state::{GameState, TimedBuff};
use std::path::PathBuf;

fn fresh() -> GameState {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/state/positions.json");
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    GameState::from_dict(doc["positions"][0]["state"].clone()).expect("from_dict")
}

fn buff(skill: Skill, delta: i64, until: Duration, granted_turn: i64) -> TimedBuff {
    TimedBuff {
        skill,
        delta,
        until,
        source: "test clause".to_owned(),
        cap: Some(5),
        granted_turn,
    }
}

#[test]
fn a_timed_buff_folds_into_the_derived_stats() {
    let mut gs = fresh();
    let base = gs.effective_stats("A", None).get(Skill::Submission);
    gs.players.get_mut("A").unwrap().timed_buffs.push(buff(
        Skill::Submission,
        5,
        Duration::UntilEndOfTurn,
        1,
    ));
    assert_eq!(
        gs.effective_stats("A", None).get(Skill::Submission) - base,
        5,
        "a live timed buff raises the derived stat"
    );
}

#[test]
fn a_timed_buff_is_scoped_to_its_owner() {
    // Buffs live on the TARGET, so the opponent's derived stats are untouched.
    let mut gs = fresh();
    let base_b = gs.effective_stats("B", None).get(Skill::Submission);
    gs.players.get_mut("A").unwrap().timed_buffs.push(buff(
        Skill::Submission,
        5,
        Duration::UntilEndOfTurn,
        1,
    ));
    assert_eq!(
        gs.effective_stats("B", None).get(Skill::Submission),
        base_b,
        "A's timed buff does not touch B"
    );
}

#[test]
fn timed_buffs_stack_additively() {
    // Two entries from DIFFERENT sources both apply; the cap is per-entry, so it
    // does not merge them (the grant path is what merges same-source repeats).
    let mut gs = fresh();
    let base = gs.effective_stats("A", None).get(Skill::Strike);
    let p = gs.players.get_mut("A").unwrap();
    p.timed_buffs
        .push(buff(Skill::Strike, 2, Duration::UntilEndOfTurn, 1));
    let mut other = buff(Skill::Strike, 3, Duration::UntilEndOfTurn, 1);
    other.source = "another clause".to_owned();
    p.timed_buffs.push(other);
    assert_eq!(
        gs.effective_stats("A", None).get(Skill::Strike) - base,
        5,
        "distinct sources sum"
    );
}

#[test]
fn snapshot_round_trips_timed_buffs() {
    // The buff is real state, so it must survive a snapshot/restore (DESIGN.md §5).
    let mut gs = fresh();
    gs.players.get_mut("A").unwrap().timed_buffs.push(buff(
        Skill::Submission,
        5,
        Duration::UntilStartOfYourNextTurn,
        3,
    ));
    let restored = GameState::from_dict(gs.to_dict()).expect("round-trip");
    assert_eq!(
        restored.players["A"].timed_buffs, gs.players["A"].timed_buffs,
        "timed buffs survive a snapshot"
    );
    assert_eq!(
        restored.effective_stats("A", None).get(Skill::Submission),
        gs.effective_stats("A", None).get(Skill::Submission)
    );
}
