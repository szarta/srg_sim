//! Domain model: Card, Competitor, EntranceCard, Deck (DESIGN.md §2).
//!
//! A faithful port of `cards.py`. The skill / attack-type / play-order enums
//! live in [`crate::ir`] (the card database's exact strings), and the six-value
//! stat block is [`crate::skills::Skills`]; this module adds the card and deck
//! records that carry compiled [`Effect`] IR. Every type is serde-serializable
//! with the same field names the Python `to_dict()` emits, so snapshots and the
//! embedded fixture decks round-trip unchanged.

use crate::ir::{AtkType, Effect, PlayOrder, Skill};
use crate::skills::Skills;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A legal main deck holds exactly one card of each number 1..=30.
pub const DECK_SIZE: usize = 30;

/// Attack type implied by a main-deck card number (DESIGN.md §2).
///
/// `n mod 3`: 1 → Strike, 2 → Grapple, 0 → Submission. Cards come in triples
/// (one of each type per consecutive triple).
pub fn atk_type_from_number(number: i64) -> AtkType {
    [AtkType::Submission, AtkType::Strike, AtkType::Grapple][number.rem_euclid(3) as usize]
}

/// A main-deck card (`number` 1–30). `finish_bonuses` and `effects` are
/// populated by the rules parser; the raw text is retained for audit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Card {
    pub db_uuid: String,
    pub name: String,
    pub number: i64,
    pub atk_type: AtkType,
    pub play_order: PlayOrder,
    /// Finish bonus per rolled skill. A `BTreeMap<Skill, _>` keeps the keys in
    /// canonical skill order, matching the Python `__post_init__` normalization.
    #[serde(default)]
    pub finish_bonuses: BTreeMap<Skill, i64>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub raw_text: String,
    #[serde(default)]
    pub effects: Vec<Effect>,
}

impl Card {
    /// Finish bonus added when `skill` is rolled for the finish (0 if none).
    pub fn bonus_for(&self, skill: Skill) -> i64 {
        self.finish_bonuses.get(&skill).copied().unwrap_or(0)
    }

    /// The attack type implied by `number` (DESIGN.md §2 cross-check).
    pub fn expected_atk_type(&self) -> AtkType {
        atk_type_from_number(self.number)
    }

    /// True iff `atk_type` agrees with `number` (the loader logs mismatches).
    pub fn atk_type_matches_number(&self) -> bool {
        self.atk_type == self.expected_atk_type()
    }
}

/// A single competitor (one per side in a SingleCompetitor game).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Competitor {
    pub db_uuid: String,
    pub name: String,
    pub division: String,
    pub stats: Skills,
    #[serde(default)]
    pub gimmick_text: String,
    #[serde(default)]
    pub effects: Vec<Effect>,
    #[serde(default)]
    pub related_finishes: Vec<String>,
}

/// A competitor's Entrance card (no attack type, no ordering stage).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntranceCard {
    pub db_uuid: String,
    pub name: String,
    #[serde(default)]
    pub raw_text: String,
    #[serde(default)]
    pub effects: Vec<Effect>,
}

/// One side's deck: a competitor, an entrance, and exactly 30 cards.
///
/// Format legality (card-pool rules) is **not** enforced here (DESIGN.md §2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deck {
    pub competitor: Competitor,
    pub entrance: EntranceCard,
    #[serde(default)]
    pub cards: Vec<Card>,
}

impl Deck {
    /// Return a list of integrity problems (empty means the deck is legal).
    pub fn validate(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if self.cards.len() != DECK_SIZE {
            problems.push(format!(
                "expected {DECK_SIZE} cards, got {}",
                self.cards.len()
            ));
        }
        let numbers: Vec<i64> = self.cards.iter().map(|c| c.number).collect();
        let mut missing: Vec<i64> = (1..=DECK_SIZE as i64)
            .filter(|n| !numbers.contains(n))
            .collect();
        missing.sort_unstable();
        if !missing.is_empty() {
            problems.push(format!("missing card numbers: {missing:?}"));
        }
        let mut dupes: Vec<i64> = numbers
            .iter()
            .filter(|&&n| numbers.iter().filter(|&&m| m == n).count() > 1)
            .copied()
            .collect();
        dupes.sort_unstable();
        dupes.dedup();
        if !dupes.is_empty() {
            problems.push(format!("duplicate card numbers: {dupes:?}"));
        }
        problems
    }

    /// True iff the deck has no integrity problems.
    pub fn is_valid(&self) -> bool {
        self.validate().is_empty()
    }

    /// The card with the given number, if present.
    pub fn card_by_number(&self, number: i64) -> Option<&Card> {
        self.cards.iter().find(|c| c.number == number)
    }
}
