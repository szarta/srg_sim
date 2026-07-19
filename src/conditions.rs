//! Evaluate Effect-IR conditions against live game state (DESIGN.md §3).
//!
//! A faithful port of `conditions.py`. A [`Condition`] is a predicate on the
//! current [`GameState`], evaluated relative to the effect's **owner** (`SELF`
//! is the owner, `OPP` the other side). [`holds`] dispatches on the node type.
//!
//! `SkillCompare` reads the **derived** stats (base + unconditional buffs) via
//! [`GameState::effective_stats`] with no evaluator, which reflects active buffs
//! yet avoids a buff→condition→buff recursion. Roll-scoped conditions
//! (`RollWasSkill` / `RollGap*` / `RollValue`) need a [`RollContext`]; without
//! one they are false.

use crate::cards::Card;
use crate::ir::{
    Action, CardFilter, Comparator, CompareDomain, CompareOrder, Condition, Effect, Skill, Trigger,
    Vs, Who,
};
use crate::state::GameState;

/// The current turn roll, for roll-scoped conditions (from the owner's view).
///
/// `gap` is the **opponent's** rolled value minus the **owner's**, so a positive
/// gap means the owner rolled *lower* by that much. It is signed: rolling higher
/// gives a negative gap, which no `RollGap*(k>0)` matches.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RollContext {
    pub skill: Option<Skill>,
    pub gap: Option<i64>,
    pub value: Option<i64>,
    /// The *other* side's rolled skill this turn-roll — set only in the recorded
    /// post-roll context (`record_roll_ctx`), so `SameRolledSkill` ("you and your
    /// target rolled the same skill") can compare the two. `None` in the in-progress
    /// / re-roll contexts, which are single-sided.
    pub opp_skill: Option<Skill>,
}

fn cmp_apply(cmp: Comparator, a: i64, b: i64) -> bool {
    match cmp {
        Comparator::Gt => a > b,
        Comparator::Ge => a >= b,
        Comparator::Eq => a == b,
        Comparator::Lt => a < b,
        Comparator::Le => a <= b,
    }
}

/// Resolve a comparison whose left side is forced by a [`CompareOrder`] override:
/// `Greater` means the subject is treated as strictly higher/more than the opponent
/// (`>`/`>=` hold, everything else fails); `Less` as strictly lower/fewer. A strict
/// override so equality (`=`) is never satisfied — "considered higher" ≠ "equal".
fn forced_cmp(cmp: Comparator, order: CompareOrder) -> bool {
    match order {
        CompareOrder::Greater => matches!(cmp, Comparator::Gt | Comparator::Ge),
        CompareOrder::Less => matches!(cmp, Comparator::Lt | Comparator::Le),
    }
}

/// The active [`Action::ConsideredCompare`] override of `key` for `domain`, if any
/// (RaRa Perre "skills higher"; Theo V2 "hand fewer"). Scans `key`'s own active
/// static declarations — competitor gimmick (unless blanked), entrance, in-play —
/// honoring each declaration's condition. Read by `SkillCompare`/`HandSizeCompare`.
fn considered_compare(state: &GameState, key: &str, domain: CompareDomain) -> Option<CompareOrder> {
    let player = &state.players[key];
    let groups: [(&[Effect], bool); 2] = [
        (&player.competitor.effects, !state.is_gimmick_blanked(key)),
        (&player.entrance.effects, true),
    ];
    for (effects, active) in groups {
        if active {
            if let Some(o) = scan_considered(state, key, effects, domain) {
                return Some(o);
            }
        }
    }
    for card in &player.in_play {
        if let Some(o) = scan_considered(state, key, &card.effects, domain) {
            return Some(o);
        }
    }
    None
}

/// The first active `ConsideredCompare` of `domain` among `effects` (Static trigger,
/// condition holds), or `None`.
fn scan_considered(
    state: &GameState,
    key: &str,
    effects: &[Effect],
    domain: CompareDomain,
) -> Option<CompareOrder> {
    for eff in effects {
        if !matches!(eff.trigger, Trigger::Static) {
            continue;
        }
        for a in &eff.actions {
            if let Action::ConsideredCompare { domain: d, order } = a {
                if *d == domain && holds(&eff.condition, state, key, None) {
                    return Some(*order);
                }
            }
        }
    }
    None
}

/// True iff `card` satisfies every set criterion of `filt` (AND; `raw` ignored).
pub fn card_matches(card: &Card, filt: &CardFilter) -> bool {
    if filt.number.is_some() && Some(card.number) != filt.number {
        return false;
    }
    if filt.atk_type.is_some() && Some(card.atk_type) != filt.atk_type {
        return false;
    }
    if filt.play_order.is_some() && Some(card.play_order) != filt.play_order {
        return false;
    }
    if let Some(tag) = &filt.tag {
        if !card.tags.contains(tag) {
            return false;
        }
    }
    if filt.name.is_some() && filt.name.as_ref() != Some(&card.name) {
        return false;
    }
    if !filt.name_contains.is_empty() && !any_substr_ci(&filt.name_contains, &card.name) {
        return false;
    }
    !(!filt.text_contains.is_empty() && !any_substr_ci(&filt.text_contains, &card.raw_text))
}

/// True iff `haystack` contains any of `needles` as a case-insensitive substring
/// (pure substring — "Table" matches "Stable"; OR over the needle list).
fn any_substr_ci(needles: &[String], haystack: &str) -> bool {
    let hay = haystack.to_lowercase();
    needles.iter().any(|n| hay.contains(&n.to_lowercase()))
}

/// True iff every card matching `sel` necessarily matches `query` — i.e. `query`
/// is no more restrictive than `sel`. So a Lead-Strike declaration implies the
/// looser "Lead" and "Strike" queries, but not "Follow up" (`raw` ignored).
fn filter_implies(sel: &CardFilter, query: &CardFilter) -> bool {
    if query.number.is_some() && sel.number != query.number {
        return false;
    }
    if query.atk_type.is_some() && sel.atk_type != query.atk_type {
        return false;
    }
    if query.play_order.is_some() && sel.play_order != query.play_order {
        return false;
    }
    if query.tag.is_some() && sel.tag != query.tag {
        return false;
    }
    !(query.name.is_some() && sel.name != query.name)
}

/// The largest `CountsAsInPlay` count this card declares for a `query` its
/// selector implies (0 if none).
fn counts_as(card: &Card, query: &CardFilter) -> i64 {
    let mut best = 0;
    for eff in &card.effects {
        for action in &eff.actions {
            if let Action::CountsAsInPlay { selector, count } = action {
                if filter_implies(selector, query) {
                    best = best.max(*count);
                }
            }
        }
    }
    best
}

/// Count cards in a board matching `query`, honoring `CountsAsInPlay` self-
/// declarations (a card that "counts as N" contributes N instead of 1).
/// `exclude` drops one card object (the just-played source, for "each **other**
/// … in play").
pub fn count_in_play(cards: &[Card], query: &CardFilter, exclude: Option<&Card>) -> i64 {
    let mut total = 0;
    for card in cards {
        if let Some(ex) = exclude {
            if std::ptr::eq(card, ex) {
                continue;
            }
        }
        let base = if card_matches(card, query) { 1 } else { 0 };
        total += base.max(counts_as(card, query));
    }
    total
}

fn who_key<'a>(state: &'a GameState, owner: &'a str, who: Who) -> String {
    match who {
        Who::SelfSide => owner.to_owned(),
        Who::Opp => state.opponent_of(owner),
    }
}

// No evaluator: derived stats reflect unconditional buffs and cannot recurse.
fn skill_value(state: &GameState, key: &str, skill: Skill) -> i64 {
    state.effective_stats(key, None).get(skill)
}

/// Whether `cond` holds for `owner` in `state` (unknown/unsupported nodes →
/// false).
pub fn holds(cond: &Condition, state: &GameState, owner: &str, roll: Option<&RollContext>) -> bool {
    match cond {
        Condition::Always => true,
        Condition::And { items } => items.iter().all(|x| holds(x, state, owner, roll)),
        Condition::Or { items } => items.iter().any(|x| holds(x, state, owner, roll)),
        Condition::Not { item } => !holds(item, state, owner, roll),
        Condition::SkillCompare {
            skill,
            cmp,
            who,
            vs,
            value,
            vs_skill,
        } => {
            let subject = who_key(state, owner, *who);
            // "Your skills are considered higher than your opponent's" (RaRa Perre):
            // a vs-opponent skill comparison of `subject` resolves a fixed way.
            if *vs != Vs::Value {
                if let Some(order) = considered_compare(state, &subject, CompareDomain::Skill) {
                    return forced_cmp(*cmp, order);
                }
            }
            let left = skill_value(state, &subject, *skill);
            let right = if *vs == Vs::Value {
                value.unwrap_or(0)
            } else {
                let opp = state.opponent_of(&subject);
                skill_value(state, &opp, vs_skill.unwrap_or(*skill))
            };
            cmp_apply(*cmp, left, right)
        }
        Condition::HandSizeCompare {
            cmp,
            vs,
            value,
            who,
        } => {
            let subject = who_key(state, owner, *who);
            // "You are considered to have fewer cards in hand" (Theo V2): a
            // vs-opponent hand-size comparison of `subject` resolves a fixed way.
            if *vs != Vs::Value {
                if let Some(order) = considered_compare(state, &subject, CompareDomain::Hand) {
                    return forced_cmp(*cmp, order);
                }
            }
            let left = state.players[&subject].hand.len() as i64;
            let right = if *vs == Vs::Value {
                value.unwrap_or(0)
            } else {
                let opp = state.opponent_of(&subject);
                state.players[&opp].hand.len() as i64
            };
            cmp_apply(*cmp, left, right)
        }
        Condition::CrowdMeterCompare { cmp, value } => cmp_apply(*cmp, state.crowd_meter, *value),
        Condition::HasInPlay {
            who,
            filter,
            count,
            cmp,
        } => {
            let subject = who_key(state, owner, *who);
            let n = count_in_play(&state.players[&subject].in_play, filter, None);
            cmp_apply(*cmp, n, *count)
        }
        Condition::HasInHand { who, filter, count } => {
            let subject = who_key(state, owner, *who);
            let n = state.players[&subject]
                .hand
                .iter()
                .filter(|c| card_matches(c, filter))
                .count() as i64;
            n >= *count
        }
        Condition::HasInDiscard { who, filter } => {
            let subject = who_key(state, owner, *who);
            state.players[&subject]
                .discard
                .iter()
                .any(|c| card_matches(c, filter))
        }
        Condition::RollWasSkill { skill } => roll.is_some_and(|r| r.skill == Some(*skill)),
        Condition::RollGapExactly { k } => roll.is_some_and(|r| r.gap == Some(*k)),
        Condition::RollGapAtLeast { k } => roll.is_some_and(|r| r.gap.is_some_and(|g| g >= *k)),
        // A lead of k = the owner rolled k higher = gap (opp - owner) <= -k.
        Condition::RollLeadAtLeast { k } => roll.is_some_and(|r| r.gap.is_some_and(|g| g <= -*k)),
        Condition::RollValue { cmp, value } => {
            roll.is_some_and(|r| r.value.is_some_and(|v| cmp_apply(*cmp, v, *value)))
        }
        Condition::PrintedRollValue { who, value } => roll.is_some_and(|r| {
            r.skill.is_some_and(|sk| {
                let subject = who_key(state, owner, *who);
                state.players[&subject].competitor.stats.get(sk) == *value
            })
        }),
        Condition::SameRolledSkill => {
            roll.is_some_and(|r| r.skill.is_some() && r.skill == r.opp_skill)
        }
        Condition::OppWonLastRoll => {
            state.last_roll_winner.as_deref() == Some(state.opponent_of(owner).as_str())
        }
        Condition::GimmickFlipped { who } => {
            let subject = who_key(state, owner, *who);
            state.players[&subject].gimmick_flipped
        }
        Condition::DuringTurn { who } => state.active == who_key(state, owner, *who),
    }
}
