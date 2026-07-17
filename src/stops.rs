//! Skill-stop online logic — PORTED from `fae_comp/skill_stops.py` (DESIGN.md §6).
//!
//! Cards 13 / 14 / 15 are Follow Ups whose stop is only "online" depending on
//! skills. Each is keyed to a pair of skills; the three pairs partition all six
//! skills. **Do not re-derive this**; keep it in parity with the source
//! (`tests/stops.rs` checks golden facts).
//!
//! RPS: Strike stops Grapple, Grapple stops Submission, Submission stops Strike,
//! so each finish type is stopped by exactly one skill-stop card:
//!
//! * Grapple finish    <- card 13 (Strike-type),     pair (Strike, Agility)
//! * Submission finish <- card 14 (Grapple-type),    pair (Power, Grapple)
//! * Strike finish     <- card 15 (Submission-type), pair (Technique, Submission)
//!
//! A stop comes online via any of:
//!
//! * beat-opponent : your keyed skill value  >  opponent's SAME skill (strict)
//! * equal-8       : one paired skill `>= 8`, then your OWN two skills compared
//! * Colossal Smash: card 14 only; Power 10 AND Grapple 9 -> guaranteed
//!   stop-Submission

use crate::ir::Skill;
use crate::skills::Skills;
use num_rational::Rational64;

/// The card and skill pair that stops a finish of the given type, or `None` for
/// a non-stoppable skill. Only Grapple / Submission / Strike finishes have a
/// keyed stop card (13 / 14 / 15).
pub fn stop_card(finish_type: Skill) -> Option<(u32, (Skill, Skill))> {
    match finish_type {
        Skill::Grapple => Some((13, (Skill::Strike, Skill::Agility))),
        Skill::Submission => Some((14, (Skill::Power, Skill::Grapple))),
        Skill::Strike => Some((15, (Skill::Technique, Skill::Submission))),
        _ => None,
    }
}

/// The outcome of [`evaluate_stop`] — mirrors the Python result dict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopEvaluation {
    pub card: u32,
    pub pair: (Skill, Skill),
    pub online: bool,
    pub reasons: Vec<String>,
    pub offline_notes: Vec<String>,
    pub best_beat_key: (Skill, i64),
    pub random_online_prob: Rational64,
}

/// Evaluate the defender's skill stop against a finish of `finish_type`.
///
/// If `opponent` is `None`, only matchup-independent variants (equal-8,
/// Colossal) are decided; beat-opponent is reported as a probability vs a
/// random opponent via `random_online_prob`. Returns `None` if `finish_type`
/// has no keyed stop card.
pub fn evaluate_stop(
    defender: &Skills,
    finish_type: Skill,
    opponent: Option<&Skills>,
) -> Option<StopEvaluation> {
    let (card, (x, y)) = stop_card(finish_type)?;
    let d = defender;
    let mut reasons: Vec<String> = Vec::new();
    let mut offline: Vec<String> = Vec::new();

    // --- beat-opponent variants (one per paired skill) ---
    for k in [x, y] {
        if let Some(o) = opponent {
            if d.get(k) > o.get(k) {
                reasons.push(format!(
                    "beat-opp: your {} {} > their {} {}",
                    k.name(),
                    d.get(k),
                    k.name(),
                    o.get(k)
                ));
            } else {
                offline.push(format!(
                    "beat-opp {}: {} !> {}",
                    k.name(),
                    d.get(k),
                    o.get(k)
                ));
            }
        }
    }

    // --- equal-8 variants (self-referential): req A>=8, then B>A ---
    for (a, b) in [(x, y), (y, x)] {
        if d.get(a) >= 8 {
            if d.get(b) > d.get(a) {
                reasons.push(format!(
                    "equal-8 (req {}>=8): your {} {} > your {} {}",
                    a.name(),
                    b.name(),
                    d.get(b),
                    a.name(),
                    d.get(a)
                ));
            } else {
                offline.push(format!(
                    "equal-8 (req {}>=8): {} {} !> {} {}",
                    a.name(),
                    b.name(),
                    d.get(b),
                    a.name(),
                    d.get(a)
                ));
            }
        } else {
            offline.push(format!(
                "equal-8 (req {}>=8): {}={} fails requirement",
                a.name(),
                a.name(),
                d.get(a)
            ));
        }
    }

    // --- Colossal Smash (card 14 only) ---
    if finish_type == Skill::Submission {
        if d.get(Skill::Power) == 10 && d.get(Skill::Grapple) == 9 {
            // Power >= opp Power; Power is 10 so always satisfied.
            reasons.push("Colossal Smash: Power 10 >= opp Power (guaranteed)".to_string());
        } else {
            offline.push(format!(
                "Colossal Smash: needs Power 10 & Grapple 9 (have {}/{})",
                d.get(Skill::Power),
                d.get(Skill::Grapple)
            ));
        }
    }

    // --- vs-random-opponent probability for the best beat-opp key ---
    let best_key = if d.get(x) >= d.get(y) { x } else { y };
    let rand_prob = Rational64::new((d.get(best_key) - 5).max(0), 6); // opp same-skill ~ U{5..10}

    Some(StopEvaluation {
        card,
        pair: (x, y),
        online: !reasons.is_empty(),
        reasons,
        offline_notes: offline,
        best_beat_key: (best_key, d.get(best_key)),
        random_online_prob: rand_prob,
    })
}
