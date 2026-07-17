//! Finish / breakout math — PORTED from `fae_comp/supershow.py` (DESIGN.md §6).
//!
//! This is the authoritative finish-roll model, itself a mirror of the
//! validated web tool `FinishCalculator.jsx`. **Do not re-derive it**; keep it
//! in parity with the source (`tests/finish.rs` checks a golden case batch).
//!
//! The per-die-face rules (exposed as public primitives so the engine's
//! seeded-roll finish sequence shares the exact same logic):
//!
//! * Finish value per face = finisher stat + finish bonus + crowd meter
//!   (uncapped).
//! * A defender stat breaks out iff `stat - penalty >= finish_value`, EXCEPT at
//!   Crowd Meter 0 a raw 10 always breaks out (ignoring penalty).
//! * A finish value `>= 11` at Crowd Meter `> 0` is unbreakoutable
//!   (auto-success).
//!
//! [`finish_odds`] returns the exact [`Rational64`] probability the finisher
//! succeeds (the defender fails to break out), enumerating the finisher's six
//! die faces.

use crate::ir::Skill;
use crate::skills::Skills;
use num_rational::Rational64;

/// A finish value `>= 11` at Crowd Meter `> 0` is unbreakoutable (auto-success).
pub fn is_auto_success(finish_value: i64, crowd_meter: i64) -> bool {
    finish_value >= 11 && crowd_meter > 0
}

/// Whether a single defender stat breaks out of a `finish_value` finish.
///
/// At Crowd Meter 0 a raw 10 always breaks out (ignoring penalty); otherwise a
/// stat breaks out iff `stat - penalty >= finish_value`.
pub fn stat_breaks_out(stat_value: i64, finish_value: i64, penalty: i64, crowd_meter: i64) -> bool {
    if crowd_meter == 0 && stat_value == 10 {
        return true;
    }
    stat_value - penalty >= finish_value
}

/// How many of the opponent's six stats break out of a `finish_value` finish.
fn count_breakout_stats(opp: &Skills, finish_value: i64, penalty: i64, crowd_meter: i64) -> i64 {
    Skill::ALL
        .iter()
        .filter(|&&s| stat_breaks_out(opp.get(s), finish_value, penalty, crowd_meter))
        .count() as i64
}

/// P(opponent breaks out) against one finish face.
fn breakout_prob_for_finish(
    finish_value: i64,
    opp: &Skills,
    attempts: u32,
    crowd_meter: i64,
    penalties: &[i64],
) -> Rational64 {
    let one = Rational64::from_integer(1);
    if is_auto_success(finish_value, crowd_meter) {
        return Rational64::from_integer(0); // auto-success, unbreakoutable
    }
    if !penalties.is_empty() {
        let mut prob_all_fail = one;
        for i in 0..attempts as usize {
            let pen = penalties.get(i).copied().unwrap_or(0);
            let can = count_breakout_stats(opp, finish_value, pen, crowd_meter);
            prob_all_fail *= one - Rational64::new(can, 6);
        }
        return one - prob_all_fail;
    }
    let can = count_breakout_stats(opp, finish_value, 0, crowd_meter);
    let base = one - Rational64::new(can, 6);
    let mut all_fail = one;
    for _ in 0..attempts {
        all_fail *= base;
    }
    one - all_fail
}

/// Average breakout prob over the finisher's six die faces. With a reroll the
/// finisher rolls two faces and keeps the better (lower breakout prob).
fn average_breakout(probs: &[Rational64], allow_reroll: bool) -> Rational64 {
    if !allow_reroll {
        let sum: Rational64 = probs.iter().copied().sum();
        return sum / Rational64::from_integer(6);
    }
    let mut total = Rational64::from_integer(0);
    for &p1 in probs {
        for &p2 in probs {
            total += p1.min(p2);
        }
    }
    total / Rational64::from_integer(36)
}

/// Inputs to [`finish_odds`], mirroring the Python keyword arguments.
///
/// `finish_bonus` / `opponent_modifiers` are delta blocks (all-zero by
/// default). `breakout_penalties` is a per-attempt penalty SUBTRACTED from
/// opponent stats — POSITIVE hurts the opponent, e.g. `[0, 1, 1]` = "-1 to their
/// 2nd and 3rd breakout rolls".
#[derive(Debug, Clone)]
pub struct FinishParams {
    pub finisher: Skills,
    pub defender: Skills,
    pub finish_bonus: Skills,
    pub crowd_meter: i64,
    pub breakout_attempts: u32,
    pub breakout_penalties: Vec<i64>,
    pub opponent_modifiers: Skills,
    pub allow_reroll: bool,
}

impl Default for FinishParams {
    fn default() -> Self {
        Self {
            finisher: Skills::default(),
            defender: Skills::default(),
            finish_bonus: Skills::default(),
            crowd_meter: 0,
            breakout_attempts: 3,
            breakout_penalties: Vec::new(),
            opponent_modifiers: Skills::default(),
            allow_reroll: false,
        }
    }
}

impl FinishParams {
    /// A finish of `finisher` against `defender` with all defaults (CM 0, three
    /// breakout attempts, no bonuses/penalties/reroll).
    pub fn new(finisher: Skills, defender: Skills) -> Self {
        Self {
            finisher,
            defender,
            ..Self::default()
        }
    }
}

/// Exact probability the FINISHER succeeds (opponent fails to break out).
///
/// Mirrors `FinishCalculator.jsx`. Finish value per face = finisher stat +
/// `finish_bonus[skill]` + `crowd_meter` (uncapped). The opponent breaks out on
/// a face if any of up to `breakout_attempts` rolls succeeds. Returns
/// `1 - P(breakout)`.
pub fn finish_odds(p: &FinishParams) -> Rational64 {
    let mut opp = Skills::default();
    for s in Skill::ALL {
        opp = opp.with(s, p.defender.get(s) + p.opponent_modifiers.get(s));
    }
    let probs: Vec<Rational64> = Skill::ALL
        .iter()
        .map(|&s| {
            let finish_value = p.finisher.get(s) + p.finish_bonus.get(s) + p.crowd_meter;
            breakout_prob_for_finish(
                finish_value,
                &opp,
                p.breakout_attempts,
                p.crowd_meter,
                &p.breakout_penalties,
            )
        })
        .collect();
    let breakout = average_breakout(&probs, p.allow_reroll);
    Rational64::from_integer(1) - breakout
}
