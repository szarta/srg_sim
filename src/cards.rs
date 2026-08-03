//! Domain model: Card, Competitor, EntranceCard, Deck (DESIGN.md §2).
//!
//! A faithful port of `cards.py`. The skill / attack-type / play-order enums
//! live in [`crate::ir`] (the card database's exact strings), and the six-value
//! stat block is [`crate::skills::Skills`]; this module adds the card and deck
//! records that carry compiled [`Effect`] IR. Every type is serde-serializable
//! with the same field names the Python `to_dict()` emits, so snapshots and the
//! embedded fixture decks round-trip unchanged.

use crate::ir::{Action, AtkType, Effect, PlayOrder, Skill};
use crate::skills::Skills;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A legal main deck holds exactly one card of each number 1..=30.
pub const DECK_SIZE: usize = 30;

/// Format-legality cap: a legal deck holds at most this many skill-requirement
/// cards (cards carrying a `requirements:` block). A deck-build rule, enforced by
/// [`Deck::format_problems`] (the optional offline validator), not the engine.
pub const MAX_SKILL_REQUIREMENT_CARDS: usize = 2;

/// Synthetic tag marking a card that carries a `requirements:` block (a "Skill
/// Requirement card"). Folded in at load time by the loader so the stop-resolution
/// `Unstoppable { by_skillreq }` gate can read it off a stopper's tags.
pub const SKILL_REQUIREMENT_TAG: &str = "SkillRequirement";

/// One `min_<skill>: N` entry of a card's `requirements:` block — the owner needs
/// effective `skill` >= `min` for the card to be online. A card may carry more than
/// one (e.g. "Field of Fire" needs Strike >= 10 AND Agility >= 9); ALL must hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillRequirement {
    pub skill: Skill,
    pub min: i64,
}

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
    /// The card's `requirements:` block, parsed to `(skill, min)` pairs. Empty for
    /// the vast majority of cards. A skill-requirement card is BLANK (text inert)
    /// whenever the owner's effective skill is below ANY of these thresholds, and
    /// un-blanks live when restored — read by `GameState::is_text_blanked`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skill_requirements: Vec<SkillRequirement>,
    #[serde(default)]
    pub raw_text: String,
    #[serde(default)]
    pub effects: Vec<Effect>,
    /// Global play-sequence stamp: the monotonic tick at which this physical card
    /// was played onto the board (`GameState::bump_play_seq`), retained as it moves
    /// on to the discard pile. `None` for a card that never went through the play
    /// step (a setup card, or one milled straight to discard). Used only to resolve
    /// competing match-rule toggles by LAST-PLAYED order (task #93): a standing
    /// no-DQ declaration vs. a "this match has Disqualifications" re-enable. Transient
    /// engine bookkeeping, never serialized — it must not leak into the observable
    /// protocol or any golden, and re-derives as the engine replays a match.
    #[serde(skip)]
    pub played_seq: Option<u64>,
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

    /// Whether this card counts as attack type `want` — its printed type, or an
    /// additional type granted by an `AlsoAtkType` effect ("this card is also a
    /// Finish Grapple", King Brian Cage). Used at every atk-type test so an aliased
    /// type is stoppable/countable/hit-gimmick-triggering like the printed one.
    pub fn counts_as_atk_type(&self, want: AtkType) -> bool {
        self.atk_type == want
            || self
                .effects
                .iter()
                .flat_map(|e| &e.actions)
                .any(|a| matches!(a, Action::AlsoAtkType { atk_type } if *atk_type == want))
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

    /// Return a list of **format-legality** problems (empty means the deck is
    /// format-legal). Distinct from [`Self::validate`], which checks structural
    /// integrity (30 cards, unique numbers): this enforces card-pool / deck-build
    /// rules and is the "optional offline validator" of DESIGN.md §1 — deliberately
    /// NOT run in the engine preflight, since the engine plays whatever decklists it
    /// is handed. Currently one rule: at most [`MAX_SKILL_REQUIREMENT_CARDS`]
    /// skill-requirement cards (cards carrying a `requirements:` block).
    pub fn format_problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        let req_count = self
            .cards
            .iter()
            .filter(|c| !c.skill_requirements.is_empty())
            .count();
        if req_count > MAX_SKILL_REQUIREMENT_CARDS {
            problems.push(format!(
                "too many skill-requirement cards: {req_count} (max {MAX_SKILL_REQUIREMENT_CARDS})"
            ));
        }
        problems
    }

    /// True iff the deck is format-legal (no [`Self::format_problems`]).
    pub fn is_format_legal(&self) -> bool {
        self.format_problems().is_empty()
    }

    /// The card with the given number, if present.
    pub fn card_by_number(&self, number: i64) -> Option<&Card> {
        self.cards.iter().find(|c| c.number == number)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A minimal main-deck card; `reqs` is its `skill_requirements` list.
    fn card(number: i64, reqs: serde_json::Value) -> Card {
        serde_json::from_value(json!({
            "db_uuid": format!("c{number}"), "name": format!("c{number}"),
            "number": number, "atk_type": "Strike", "play_order": "Lead",
            "finish_bonuses": {}, "tags": [], "skill_requirements": reqs,
            "raw_text": "", "effects": []
        }))
        .unwrap()
    }

    /// A deck carrying exactly `cards` (integrity aside — these tests exercise
    /// format legality, which is independent of the 30-card size check).
    fn deck_with(cards: Vec<Card>) -> Deck {
        let mut deck: Deck = serde_json::from_value(json!({
            "competitor": {"db_uuid": "A", "name": "A", "division": "World Championship",
                "stats": {"Power": 5, "Agility": 5, "Technique": 5,
                          "Submission": 5, "Grapple": 5, "Strike": 5}},
            "entrance": {"db_uuid": "E", "name": "E"},
            "cards": []
        }))
        .unwrap();
        deck.cards = cards;
        deck
    }

    #[test]
    fn format_problems_caps_skill_requirement_cards_at_two() {
        let req = || json!([{"skill": "Strike", "min": 8}]);
        // Two skill-requirement cards (plus a plain one) is format-legal.
        let two = deck_with(vec![card(1, req()), card(2, req()), card(3, json!([]))]);
        assert!(two.format_problems().is_empty());
        assert!(two.is_format_legal());

        // Three trips the cap, with a message naming the count and the limit.
        let three = deck_with(vec![card(1, req()), card(2, req()), card(3, req())]);
        let problems = three.format_problems();
        assert_eq!(problems.len(), 1);
        assert!(
            problems[0].contains('3') && problems[0].contains("max 2"),
            "got {problems:?}"
        );
        assert!(!three.is_format_legal());
    }

    #[test]
    fn format_legality_is_independent_of_integrity() {
        // A one-card deck fails the integrity check but is still format-legal — the
        // two validators are separate (DESIGN.md §1: format legality is not enforced
        // in the engine preflight).
        let deck = deck_with(vec![card(1, json!([{"skill": "Grapple", "min": 8}]))]);
        assert!(!deck.is_valid(), "one card fails integrity");
        assert!(
            deck.is_format_legal(),
            "one skill-requirement card is format-legal"
        );
    }
}
