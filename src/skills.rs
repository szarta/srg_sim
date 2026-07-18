//! `Skills` — a competitor's six stat values.
//!
//! A plain, `Copy` block of the six [`Skill`](crate::ir::Skill) values, keyed by
//! the canonical skill order. The finish-odds and skill-stop math (`finish`,
//! `stops`) read stats through [`Skills::get`]; sparse "bonus" / "modifier"
//! maps from the Python API (`{skill: delta}`, defaulting to 0) are modelled as
//! a `Skills` of deltas — [`Skills::default`] is all-zero, so an absent skill
//! reads as 0 exactly like `dict.get(skill, 0)`.

use crate::ir::Skill;
use serde::{Deserialize, Serialize};

/// The six stat values of a competitor (or a sparse delta block, all-zero by
/// default).
///
/// Serializes as `{"Power": …, "Agility": …, …}` — the exact form the Python
/// `Stats.to_dict()` produces — so it doubles as the domain-model stat block
/// (`cards::Competitor::stats`) and the derived-stats view.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skills {
    #[serde(rename = "Power")]
    pub power: i64,
    #[serde(rename = "Agility")]
    pub agility: i64,
    #[serde(rename = "Technique")]
    pub technique: i64,
    #[serde(rename = "Submission")]
    pub submission: i64,
    #[serde(rename = "Grapple")]
    pub grapple: i64,
    #[serde(rename = "Strike")]
    pub strike: i64,
}

impl Skills {
    /// A block with every skill set to `value` (used for uniform modifiers).
    pub fn splat(value: i64) -> Self {
        Self {
            power: value,
            agility: value,
            technique: value,
            submission: value,
            grapple: value,
            strike: value,
        }
    }

    /// The value of a single skill.
    pub fn get(&self, skill: Skill) -> i64 {
        match skill {
            Skill::Power => self.power,
            Skill::Agility => self.agility,
            Skill::Technique => self.technique,
            Skill::Submission => self.submission,
            Skill::Grapple => self.grapple,
            Skill::Strike => self.strike,
        }
    }

    /// Set a single skill, returning `self` (builder-style).
    pub fn with(mut self, skill: Skill, value: i64) -> Self {
        match skill {
            Skill::Power => self.power = value,
            Skill::Agility => self.agility = value,
            Skill::Technique => self.technique = value,
            Skill::Submission => self.submission = value,
            Skill::Grapple => self.grapple = value,
            Skill::Strike => self.strike = value,
        }
        self
    }
}
