//! Parity tests for the ported finish/breakout math (DESIGN.md §6, §11).
//!
//! Golden `Rational64` values were computed from the authoritative
//! `fae_comp/supershow.py` (via the Python port) and are baked in, so the Rust
//! port is validated even where that source is unavailable (CI). These mirror
//! `tests/test_finish.py` on the Python branch one-for-one.

use num_rational::Rational64;
use srg_core::finish::{finish_odds, is_auto_success, stat_breaks_out, FinishParams};
use srg_core::ir::Skill;
use srg_core::skills::Skills;

fn bull() -> Skills {
    Skills {
        power: 10,
        technique: 6,
        agility: 5,
        strike: 7,
        submission: 8,
        grapple: 9,
    }
}

fn fae() -> Skills {
    Skills {
        power: 10,
        technique: 7,
        agility: 6,
        strike: 8,
        submission: 9,
        grapple: 5,
    }
}

fn frac(n: i64, d: i64) -> Rational64 {
    Rational64::new(n, d)
}

// --- golden case batch (mirrors CASES / GOLDEN in test_finish.py) -----------

#[test]
fn golden_default() {
    let p = FinishParams::new(bull(), fae());
    assert_eq!(finish_odds(&p), frac(25, 144));
}

#[test]
fn golden_cm1() {
    let p = FinishParams {
        crowd_meter: 1,
        ..FinishParams::new(bull(), fae())
    };
    assert_eq!(finish_odds(&p), frac(49, 144));
}

#[test]
fn golden_cm5() {
    let p = FinishParams {
        crowd_meter: 5,
        ..FinishParams::new(bull(), fae())
    };
    assert_eq!(finish_odds(&p), frac(1205, 1296));
}

#[test]
fn golden_attempts1() {
    let p = FinishParams {
        breakout_attempts: 1,
        ..FinishParams::new(bull(), fae())
    };
    assert_eq!(finish_odds(&p), frac(5, 12));
}

#[test]
fn golden_penalties_011() {
    let p = FinishParams {
        breakout_penalties: vec![0, 1, 1],
        ..FinishParams::new(bull(), fae())
    };
    assert_eq!(finish_odds(&p), frac(295, 1296));
}

#[test]
fn golden_reroll() {
    let p = FinishParams {
        allow_reroll: true,
        ..FinishParams::new(bull(), fae())
    };
    assert_eq!(finish_odds(&p), frac(2183, 7776));
}

#[test]
fn golden_bonus_strike5() {
    let p = FinishParams {
        finish_bonus: Skills::default().with(Skill::Strike, 5),
        ..FinishParams::new(bull(), fae())
    };
    assert_eq!(finish_odds(&p), frac(19, 72));
}

#[test]
fn golden_bonus_all4_cm1() {
    let p = FinishParams {
        finish_bonus: Skills::splat(4),
        crowd_meter: 1,
        ..FinishParams::new(bull(), fae())
    };
    assert_eq!(finish_odds(&p), frac(1205, 1296));
}

#[test]
fn golden_oppmod_neg1() {
    let p = FinishParams {
        opponent_modifiers: Skills::splat(-1),
        ..FinishParams::new(bull(), fae())
    };
    assert_eq!(finish_odds(&p), frac(49, 144));
}

#[test]
fn golden_fae_vs_bull_default() {
    let p = FinishParams::new(fae(), bull());
    assert_eq!(finish_odds(&p), frac(25, 144));
}

// --- primitive self-checks --------------------------------------------------

#[test]
fn auto_success_rule() {
    assert!(is_auto_success(11, 1)); // >=11 at CM>0
    assert!(is_auto_success(12, 3));
    assert!(!is_auto_success(11, 0)); // CM0 has no auto-success
    assert!(!is_auto_success(10, 5));
}

#[test]
fn stat_breaks_out_cm0_ten_always() {
    // At CM0 a raw 10 breaks out even a finish it "shouldn't", ignoring penalty.
    assert!(stat_breaks_out(10, 12, 5, 0));
    // ...but a 9 at CM0 follows the normal rule.
    assert!(!stat_breaks_out(9, 12, 0, 0));
}

#[test]
fn stat_breaks_out_normal_rule() {
    assert!(stat_breaks_out(9, 9, 0, 1)); // 9 - 0 >= 9
    assert!(!stat_breaks_out(9, 10, 1, 1)); // 9 - 1 = 8, not >= 10
    assert!(stat_breaks_out(8, 7, 0, 3)); // 8 >= 7
}

#[test]
fn finish_probability_bounds() {
    // A crushing finish (all-skill +4 at CM1) should be near-certain success.
    let p = FinishParams {
        finish_bonus: Skills::splat(4),
        crowd_meter: 1,
        ..FinishParams::new(bull(), fae())
    };
    let odds = finish_odds(&p);
    assert!(frac(9, 10) < odds && odds <= Rational64::from_integer(1));
}
