//! Coverage/parity tests for the ported skill-stop logic (DESIGN.md §6, §11).
//!
//! Golden facts come from `fae_comp/skill_stops.py` (via the Python port);
//! these mirror `tests/test_stops.py` on the Python branch.

use num_rational::Rational64;
use srg_core::ir::Skill;
use srg_core::skills::Skills;
use srg_core::stops::{evaluate_stop, stop_card};

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
        agility: 6,
        strike: 8,
        submission: 9,
        grapple: 5,
        technique: 7,
    }
}

fn eval(defender: &Skills, finish: Skill, opp: Option<&Skills>) -> srg_core::stops::StopEvaluation {
    evaluate_stop(defender, finish, opp).expect("stoppable finish type")
}

#[test]
fn stop_cards_partition_the_six_skills() {
    let mut paired: Vec<&str> = [Skill::Grapple, Skill::Submission, Skill::Strike]
        .into_iter()
        .flat_map(|ft| {
            let (_, (x, y)) = stop_card(ft).expect("keyed stop");
            [x.name(), y.name()]
        })
        .collect();
    paired.sort_unstable();
    assert_eq!(
        paired,
        [
            "Agility",
            "Grapple",
            "Power",
            "Strike",
            "Submission",
            "Technique"
        ]
    );
}

#[test]
fn non_stoppable_finish_has_no_card() {
    assert!(stop_card(Skill::Power).is_none());
    assert!(stop_card(Skill::Agility).is_none());
    assert!(stop_card(Skill::Technique).is_none());
    assert!(evaluate_stop(&bull(), Skill::Power, None).is_none());
}

#[test]
fn bull_vs_fae_only_submission_online() {
    assert!(!eval(&bull(), Skill::Strike, Some(&fae())).online);
    assert!(!eval(&bull(), Skill::Grapple, Some(&fae())).online);
    let sub = eval(&bull(), Skill::Submission, Some(&fae()));
    assert!(sub.online);
    assert_eq!(sub.card, 14);
    assert_eq!(sub.pair, (Skill::Power, Skill::Grapple));
    assert_eq!(sub.reasons.len(), 3); // beat-opp Grapple + equal-8 + Colossal
}

#[test]
fn colossal_smash_always_on_without_opponent() {
    // Power 10 & Grapple 9 -> stop-Submission is guaranteed, matchup-proof.
    let result = eval(&bull(), Skill::Submission, None);
    assert!(result.online);
    assert!(result.reasons.iter().any(|r| r.contains("Colossal Smash")));
}

#[test]
fn colossal_smash_offline_when_stats_wrong() {
    let weak = bull().with(Skill::Grapple, 8); // Grapple 8, not 9 -> no Colossal
    let result = eval(&weak, Skill::Submission, None);
    assert!(result
        .offline_notes
        .iter()
        .any(|n| n.contains("Colossal Smash: needs Power 10 & Grapple 9")));
}

#[test]
fn fae_vs_bull_coverage() {
    assert!(eval(&fae(), Skill::Strike, Some(&bull())).online);
    assert!(eval(&fae(), Skill::Grapple, Some(&bull())).online);
    assert!(!eval(&fae(), Skill::Submission, Some(&bull())).online);
}

#[test]
fn beat_opponent_is_strict() {
    // Equal keyed skills do NOT bring a beat-opp stop online (strict >).
    let even = Skills {
        power: 10,
        technique: 8,
        agility: 7,
        strike: 6,
        submission: 5,
        grapple: 9,
    };
    // card 13 (Grapple finish) keys on Strike/Agility; make them tie the opponent.
    let opp = even;
    let result = eval(&even, Skill::Grapple, Some(&opp));
    assert!(!result.reasons.iter().any(|r| r.contains("beat-opp")));
}

#[test]
fn random_online_prob() {
    // best beat key for Submission stop is Power 10 -> (10-5)/6.
    let result = eval(&bull(), Skill::Submission, None);
    assert_eq!(result.best_beat_key, (Skill::Power, 10));
    assert_eq!(result.random_online_prob, Rational64::new(5, 6));
}
