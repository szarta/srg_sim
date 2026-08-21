//! Effect IR — the DESIGN.md §3 contract, as Rust serde types.
//!
//! This is a faithful port of the Python `effects.py` / `conditions.py`
//! dataclasses. Every node is tag-serialized by its class name under the
//! `@type` key, exactly as the Python side emits it, so the same
//! `cards.ir.json` round-trips through both engines. The frozen JSON Schema
//! (`schemas/v1/effect_ir.schema.json`, task #62) is the authority; the
//! `tests/ir_roundtrip.rs` corpus guards the mapping.
//!
//! Structure mirrors the schema's four unions:
//!   * [`Trigger`]   — when an [`Effect`] fires (`Effect.trigger`)
//!   * [`Condition`] — the guard on an effect / choice
//!   * [`Action`]    — what an effect does (`Effect.actions`)
//!   * [`IrNode`]    — the top-level union of *all* node types, used to
//!     round-trip an arbitrary node (the schema root `IRNode`).
//!
//! Node structs carry only their payload fields; the `@type` tag is supplied
//! by the enclosing internally-tagged enum. Fields that are "required but
//! nullable" in the schema map to `Option<T>` **without** `skip_serializing_if`
//! so `None` serializes as an explicit `null`, matching the Python output.

use serde::{Deserialize, Serialize};

/// The Effect IR schema version — mirrors the `"version"` field of
/// `schemas/v1/effect_ir.schema.json` (the cross-language contract). Bumped in
/// lockstep with any IR node/field/enum-value change (CLAUDE.md §3 review gate);
/// `tests/schema_version.rs` guards that this equals the JSON schema's value.
pub const SCHEMA_VERSION: i64 = 156;

/// `skip_serializing_if` predicate for additive `bool` fields that default to `false`
/// (e.g. `BuffSkill.per_excludes_self`): absent-when-false keeps pre-field fixtures
/// byte-identical, the same low-churn tactic as `Option` fields with `Option::is_none`.
fn is_false(b: &bool) -> bool {
    !*b
}

/// Default `1` for a count that omits to "at least one" (e.g. [`Condition::HasInDiscard`]).
fn one_i64() -> i64 {
    1
}

fn is_one_i64(n: &i64) -> bool {
    *n == 1
}

fn is_default_search_source(s: &SearchSource) -> bool {
    *s == SearchSource::Deck
}

/// `skip_serializing_if` predicate for a `Who` field that carries the enum default
/// (`SelfSide`): used on per-count fields added to an EXISTING node so pre-field
/// fixtures — which never wrote a `per_who` — stay byte-identical. A per-count node
/// counting the OWNER's board (`SelfSide`) also omits it; only an `Opp` count writes it.
fn is_self_who(w: &Who) -> bool {
    matches!(w, Who::SelfSide)
}

/// `skip_serializing_if` predicate for a `CountZone` field at its default (`InPlay`) —
/// same low-churn tactic as [`is_self_who`] for per-count fields on an existing node.
fn is_in_play_zone(z: &CountZone) -> bool {
    matches!(z, CountZone::InPlay)
}

// ---------------------------------------------------------------------------
// `@type` tags for product structs
// ---------------------------------------------------------------------------
//
// The union nodes get their `@type` from the enclosing internally-tagged enum.
// The four *product* structs ([`Effect`], [`CardFilter`], [`FrequencyGuard`],
// [`ChoiceOption`]) are plain fields, so they carry the tag themselves: a ZST
// field that (de)serializes as a fixed string, exactly matching the Python
// `to_dict()` output. `Default` lets construction sites omit it.

macro_rules! type_tag {
    ($name:ident, $lit:literal) => {
        /// Zero-sized `@type` marker that (de)serializes as a fixed string.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
        pub struct $name;

        impl Serialize for $name {
            fn serialize<S: serde::Serializer>(
                &self,
                s: S,
            ) -> std::result::Result<S::Ok, S::Error> {
                s.serialize_str($lit)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(
                d: D,
            ) -> std::result::Result<Self, D::Error> {
                let s = String::deserialize(d)?;
                if s == $lit {
                    Ok($name)
                } else {
                    Err(serde::de::Error::custom(format!(
                        "expected @type {:?}, got {:?}",
                        $lit, s
                    )))
                }
            }
        }
    };
}

type_tag!(EffectTag, "Effect");
type_tag!(CardFilterTag, "CardFilter");
type_tag!(FrequencyGuardTag, "FrequencyGuard");
type_tag!(ChoiceOptionTag, "ChoiceOption");
type_tag!(RerollCostTag, "RerollCost");

// ---------------------------------------------------------------------------
// Scalar enums
// ---------------------------------------------------------------------------

/// The six skills (three attributes + three attack types). `Ord` follows the
/// canonical declaration order (`Power < Agility < … < Strike`), so a
/// `BTreeMap<Skill, _>` serializes finish bonuses in that fixed order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Skill {
    Power,
    Agility,
    Technique,
    Submission,
    Grapple,
    Strike,
}

impl Skill {
    /// All six skills, in the canonical order used by the finish/stop math.
    pub const ALL: [Skill; 6] = [
        Skill::Power,
        Skill::Agility,
        Skill::Technique,
        Skill::Submission,
        Skill::Grapple,
        Skill::Strike,
    ];

    /// The skill's canonical name — identical to its serialized `@type` value.
    pub fn name(self) -> &'static str {
        match self {
            Skill::Power => "Power",
            Skill::Agility => "Agility",
            Skill::Technique => "Technique",
            Skill::Submission => "Submission",
            Skill::Grapple => "Grapple",
            Skill::Strike => "Strike",
        }
    }
}

/// Attack type of a card (or `None` for non-attack cards).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AtkType {
    Strike,
    Grapple,
    Submission,
    None,
}

impl AtkType {
    /// The canonical name — identical to its serialized value.
    pub fn name(self) -> &'static str {
        match self {
            AtkType::Strike => "Strike",
            AtkType::Grapple => "Grapple",
            AtkType::Submission => "Submission",
            AtkType::None => "None",
        }
    }
}

/// Where a card sits in a play sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PlayOrder {
    #[default]
    Lead,
    Followup,
    Finish,
    None,
}

impl PlayOrder {
    /// The canonical name — identical to its serialized value.
    pub fn name(self) -> &'static str {
        match self {
            PlayOrder::Lead => "Lead",
            PlayOrder::Followup => "Followup",
            PlayOrder::Finish => "Finish",
            PlayOrder::None => "None",
        }
    }
}

/// Numeric comparison operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Comparator {
    #[serde(rename = ">")]
    Gt,
    #[serde(rename = ">=")]
    Ge,
    #[serde(rename = "=")]
    Eq,
    #[serde(rename = "<")]
    Lt,
    #[serde(rename = "<=")]
    Le,
}

/// Which end of a deck a draw/recur touches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DeckEnd {
    Top,
    Bottom,
}

/// Destination zone for a search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Dest {
    Hand,
    Discard,
    /// Put the searched card on TOP of the (shuffled) deck — "search your deck for a
    /// Strike and put it on top of your shuffled deck" (Heartache Kid).
    DeckTop,
}

/// Source zone a [`Action::Bury`] draws from. `Discard` (the default) is the
/// "pass and recycle" bury — discard pile to the bottom of the deck. `Hand` is
/// the card-text bury — "bury N cards in [your/their] hand" — hand to the bottom
/// of the deck.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BuryFrom {
    #[default]
    Discard,
    Hand,
}

/// Source zone a [`Action::ShuffleIntoDeck`] draws from. `Discard` (the default) is
/// the recur-from-discard shuffle; `InPlay` returns one of the owner's in-play cards
/// to the deck — "shuffle 1 Follow Up you have in play into your deck" (Cardona).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ShuffleSource {
    #[default]
    Discard,
    InPlay,
    /// From the actor's HAND — "shuffle any number of cards from your hand into your deck"
    /// (The Dudebuster). schema v129
    Hand,
}

/// What a play/land requirement counts on the actor's own board — the generic play-
/// restriction vocabulary. `Cards` = any card in play; `Leads`/`FollowUps` = by play
/// order. SRG's built-in defaults are `Leads`×1 to play a Follow Up and `FollowUps`×1
/// to land a Finish (encoded structurally in `playable_as`); a [`Action::FinishRequires`]
/// declaration is a DEFENDER-imposed override on top of that default. schema v125
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RequireKind {
    Cards,
    Leads,
    FollowUps,
}

/// Which zone(s) a [`Action::Search`] tutors from — `Deck` (the default, historical
/// behaviour) or `DeckOrDiscard` ("search your deck or discard pile for X"): the pool
/// is the union of both zones and the found card leaves whichever zone holds it. schema v115
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SearchSource {
    #[default]
    Deck,
    DeckOrDiscard,
}

/// A match stipulation ("this is a Steel Cage Match"). The default `Standard` is a
/// normal singles match; the rest are the recurring special-match keywords that gate
/// card text ("if this is a Steel Cage or Liger's Den Match, …"). Read by the
/// [`Condition::IsMatchType`] gate against `GameState.match_type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MatchType {
    #[default]
    Standard,
    SteelCage,
    LigersDen,
    RingOfFire,
    Triad,
    TagTeam,
    SteelChain,
    Lumberjack,
}

/// Which zone a [`Action::BuffSkill`] `per`-count ranges over — "for each card
/// you have **in play**" vs "in your **discard** pile".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CountZone {
    #[default]
    InPlay,
    Discard,
    /// Cards the target FLIPPED this turn (drained deck → discard by `Flip`), read
    /// while they are still the current turn's flips — "your Finish roll is +1 for
    /// each Strike card flipped" (Five Star Frog Splash); "for each Strike flipped:
    /// +1 to Strike and Power" (Five Star Heart Punch). Transient turn state,
    /// re-derived on replay. schema v74
    FlippedThisTurn,
    /// Cards in the target's **hand** — "+1 for every 4 cards in your hand" (GOAT's
    /// Dive Bomb Superkick, Party Package). Counted live; a `per_divisor` floors the
    /// count into groups. schema v151
    Hand,
}

/// Reach of a [`Action::DisqualificationRule`] toggle. `SelfSide` = "you cannot
/// be disqualified" (only the owner); `Match` = "this match has no
/// disqualifications" (every player).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DqScope {
    #[default]
    #[serde(rename = "SELF")]
    SelfSide,
    Match,
}

/// Which comparison [`Action::ConsideredCompare`] overrides "for card effects":
/// `Skill` forces every `SkillCompare` of the declaring player vs the opponent,
/// `Hand` forces every `HandSizeCompare`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CompareDomain {
    Skill,
    Hand,
}

/// How [`Action::ConsideredCompare`] resolves the declaring player vs the opponent:
/// `Greater` = the subject is always considered higher/more ("your skills are
/// considered higher" — RaRa Perre); `Less` = always considered lower/fewer ("you
/// are considered to have fewer cards in hand" — Theo the Greek Neo V2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CompareOrder {
    Greater,
    Less,
}

/// Which revealed cards count toward the draw in [`Action::RevealForDraw`].
/// `Stop` = each revealed Stop card (Bartholomew Hooke: "if it is a stop, draw
/// 2"); `RolledSkill` = each revealed card whose move type equals the skill the
/// actor just rolled (The Winning Ticket: "if the move type of the card revealed
/// is the same as the skill you rolled, draw 1").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RevealMatch {
    Stop,
    RolledSkill,
}

/// What a [`Action::Scry`] does with revealed cards that are neither taken to
/// hand nor buried by the fixed `bury` count. `Return` puts them back on top of
/// the deck (the actor reorders by value); `Choose` lets the actor decide, per
/// card, between returning it on top and burying it to the deck bottom
/// (Ricky Riot's "put the other back on top or bury it"); `Flip` mills them to
/// the discard pile ("look at the top N cards, add M to your hand and flip the
/// others"). `MayFlip` is the optional single-card variant — peek the top card,
/// then flip it *only when worthwhile* (mill the cards worth denying an opponent
/// / dumping from your own deck, leave the rest on top): "Look at the top card of
/// your opponent's deck, you may flip it." schema v69; MayFlip v96
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ScryRest {
    #[default]
    Return,
    Choose,
    Flip,
    MayFlip,
}

/// Where a [`Action::RevealRoute`] sends the revealed card. `Hand` = the deck
/// owner's hand; `Flip` = mill it to the discard pile; `Bury` = the deck bottom;
/// `Leave` = keep it on top (the declined "you may" branch).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RevealDest {
    #[default]
    Leave,
    Hand,
    Flip,
    Bury,
}

/// Which end of the deck a [`Action::RevealRoute`] reveals from. `Choose` is the
/// actor's pick ("the top or bottom card") — resolved blind to the top, since the
/// card is not yet known.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RevealFrom {
    #[default]
    Top,
    Bottom,
    Choose,
}

/// Where a [`Action::RevealMatch`] reveals its card from. `DeckTop` / `DeckBottom` =
/// the owner's own deck (non-destructive peek, the card stays unless `take_matched`);
/// `HandRandom` = a uniformly-random card in the owner's hand. schema v95
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RevealSource {
    #[default]
    DeckTop,
    DeckBottom,
    HandRandom,
}

/// Direction of a stop relative to the acting player.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Direction {
    Yours,
    Theirs,
}

/// How long a modifier persists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Duration {
    WhileInPlay,
    WhileGimmickActive,
    Instant,
    /// Active only while the source card sits in its owner's **discard pile** —
    /// "when this card is in your discard pile, …" (the in-discard Spotlight blanks).
    /// Scanned from the discard zone; inert while the card is in play.
    WhileInDiscard,
    /// TIMED: granted imperatively when the effect fires and swept at the END of the
    /// turn it was granted in — "until the end of the turn" (~81 cards). Unlike the
    /// `While*` durations this is NOT re-derived from a zone each read; it lives in
    /// [`PlayerState::timed_buffs`](crate::state::PlayerState) until its sweep.
    UntilEndOfTurn,
    /// TIMED: granted imperatively and swept at the start of the owner's next ACTIVE
    /// turn — "until the start of your next turn" (Snake Pitt Super Lucha, Arcade
    /// Addict Aaron, Caveman V1). A turn is shared and its active player is only known
    /// once the turn roll resolves, so the sweep runs immediately AFTER that roll: the
    /// buff still feeds the roll that makes the turn yours, then dies. It therefore
    /// survives every turn on which the owner is not the active player. Hand-
    /// adjudicated 2026-07-20; see DESIGN.md §3.
    UntilStartOfYourNextTurn,
    /// TIMED, event-swept: active until the TARGET player next LANDS A HIT — "your
    /// opponent's Gimmick is blank until they hit a card" (Sleep Paralysis). Granted
    /// imperatively (`blank_until_hit` on the target's `PlayerState`) and lifted the
    /// instant that player pushes a card to the board, so it can span several turns.
    /// schema v127
    UntilTargetHitsCard,
}

/// Where an effect originates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EffectSource {
    Card,
    Gimmick,
    Entrance,
}

/// How often an effect may fire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Frequency {
    Unlimited,
    OncePerTurn,
    OncePerMatch,
    NPerMatch,
}

/// A forced-loss condition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum LoseKind {
    Disqualification,
    Pinfall,
}

/// Whether a roll modifier applies to this roll or the next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RollWhen {
    #[default]
    This,
    Next,
}

/// Comparison operand for skill/hand-size compares. `Opp`/`OppSame` read the
/// opponent's skill (a different / the same skill); `Value` compares to a literal.
/// `SelfOther` compares two of the SAME player's skills — "your Agility skill is
/// greater than your Strike skill" (the #13/#14/#15 "equal-8" stops), where the
/// right operand is the subject's own `vs_skill` rather than the opponent's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Vs {
    Opp,
    OppSame,
    Value,
    SelfOther,
}

/// Which player a node targets. `SELF` is the acting player.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Who {
    #[default]
    #[serde(rename = "SELF")]
    SelfSide,
    #[serde(rename = "OPP")]
    Opp,
}

// ---------------------------------------------------------------------------
// Shared leaf nodes
// ---------------------------------------------------------------------------

/// A predicate over cards (name/number/tag/attack-type/play-order/raw).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardFilter {
    #[serde(rename = "@type", default)]
    pub node_type: CardFilterTag,
    pub number: Option<i64>,
    pub atk_type: Option<AtkType>,
    pub play_order: Option<PlayOrder>,
    /// OR-list form of [`Self::play_order`] — "1 **Lead or Follow Up** with 'Roll'
    /// in the name" (Cherie Von Danish; 53 cards phrase a play-order this way).
    /// Empty = no constraint. ANDs with `play_order` when both are set, though in
    /// practice authors set exactly one: `play_order` for the single-order case,
    /// `play_orders` for the disjunction. schema v41
    #[serde(default)]
    pub play_orders: Vec<PlayOrder>,
    pub tag: Option<String>,
    pub name: Option<String>,
    pub raw: Option<String>,
    /// Case-insensitive substring match on the card's **title** — "a card with
    /// 'X' (or 'Y') in the name". OR of substrings; empty = no constraint. Pure
    /// substring, so "Table" matches "Stable".
    #[serde(default)]
    pub name_contains: Vec<String>,
    /// Case-insensitive substring match on the card's **rules text** — "a card
    /// with 'X' in the text". OR of substrings; empty = no constraint.
    #[serde(default)]
    pub text_contains: Vec<String>,
    /// "a stop" / "N stops" / "for each stop …" — constrain to STOP cards (a card
    /// whose effects declare a [`Action::Stop`]). `Some(true)` = must be a stop,
    /// `Some(false)` = must NOT be a stop, `None` = unconstrained. schema v62
    #[serde(default)]
    pub is_stop: Option<bool>,
    /// Cross-field OR: the card must match AT LEAST ONE of these sub-filters, in
    /// addition to any base-field constraints on THIS filter. Empty = no disjunctive
    /// constraint. The one primitive for a selector that spans DIFFERENT fields — "a
    /// card with 'Light' in the name OR a Spotlight card" ({name_contains} vs {tag}) —
    /// which the ANDed base fields cannot express. schema v138
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub any_of: Vec<CardFilter>,
}

/// How a costed [`Action::Reroll`] is paid. `ShuffleInPlay` shuffles one card the
/// owner has in play (matching the cost's `filter`) into their deck — the original
/// re-roll cost (Mr. Hyde's "Potion"). `BuryFromHand` / `DiscardFromHand` are hand
/// payments (`count` cards, optionally matching `filter` for the discard case) — the
/// "bury 4 cards in your hand to re-roll" / "discard 1 Finish from your hand to
/// re-roll" family. `RevealFromHand` is a SOFT cost — the owner reveals `count` cards
/// matching `filter` from hand but keeps them (Whole Lotta Lariat's "reveal 2
/// Submissions from your hand to re-roll your Finish roll"); it gates the re-roll on
/// holding the cards without spending them. schema v103 (RevealFromHand: v155)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RerollCostKind {
    ShuffleInPlay,
    BuryFromHand,
    DiscardFromHand,
    RevealFromHand,
}

/// The cost of a [`Action::Reroll`] (the payment offered alongside the re-roll). Its
/// `kind` selects the payment; `count` is the hand-payment size (`None` for
/// `ShuffleInPlay`); `filter` scopes which card — the in-play card to shuffle, or the
/// hand cards to discard when typed ("discard 1 Finish"). schema v103
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RerollCost {
    #[serde(rename = "@type", default)]
    pub node_type: RerollCostTag,
    pub kind: RerollCostKind,
    pub count: Option<i64>,
    pub filter: Option<CardFilter>,
}

/// The frequency guard attached to every [`Effect`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrequencyGuard {
    #[serde(rename = "@type", default)]
    pub node_type: FrequencyGuardTag,
    pub kind: Frequency,
    pub n: Option<i64>,
}

// ---------------------------------------------------------------------------
// Triggers — `Effect.trigger`
// ---------------------------------------------------------------------------

/// When an [`Effect`] fires.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "@type")]
pub enum Trigger {
    OnPlay,
    OnRoll {
        skill: Option<Skill>,
        who: Who,
    },
    /// Fires on a FINISH roll (not the turn roll-off) — "when you roll `skill` for
    /// your Finish roll" (The Man from I.T.). `who` follows the finisher like
    /// `OnRoll`'s does; the parser never emits it (override-only), so existing
    /// turn-roll `OnRoll` nodes stay untouched. schema v47
    OnFinishRoll {
        skill: Option<Skill>,
        who: Who,
    },
    /// Fires each time `who` has rolled EVERY skill in `skills` as a turn roll since the
    /// last firing (General Lee Wong V2: "each time you roll Power, Agility, and
    /// Technique for your turn rolls"). The engine accumulates the distinct rolled
    /// skills per effect and resets on fire. Override-only. schema v49
    OnRolledAll {
        skills: Vec<Skill>,
        #[serde(default)]
        who: Who,
    },
    InRoll {
        skill: Option<Skill>,
        who: Who,
        either: bool,
    },
    OnRollBoost {
        skill: Option<Skill>,
        delta: i64,
        on_bump: bool,
    },
    OnWinTurn,
    OnLoseTurn {
        by: Option<i64>,
    },
    OnStop {
        dir: Direction,
        /// When set, fires only if the **stopped** card's play order matches — "when
        /// your opponent stops your *Finish*" (La Fenix Super Lucha). `None` = any
        /// stopped card, the backward-compatible default (the parser's DQ/pinfall
        /// "if this is stopped" clauses and Gia's "when you Stop a card").
        #[serde(default)]
        order: Option<PlayOrder>,
    },
    OnHit {
        atk_type: Option<AtkType>,
        /// Case-insensitive OR-substring match on the **hit** card's title —
        /// "when you hit a card with 'X' (or 'Y') in the name". Empty = no name
        /// gate. Combines (AND) with `atk_type` when both are set.
        #[serde(default)]
        name_contains: Vec<String>,
        /// Same, against the hit card's rules text — "…with 'X' in the text".
        #[serde(default)]
        text_contains: Vec<String>,
        /// "When you hit a card" (any card, no gate) as a standing gimmick — fires on
        /// every hit (Bartholomew Hooke). Override-only; a bare parser OnHit leaves it
        /// false so misattributed fragments stay inert. See `run_hit_gimmicks`.
        #[serde(default)]
        on_any: bool,
        /// Play-order gate on the HIT card — "when you hit a **Lead**" (Sticky
        /// Sailboat, Asia, Chip Day; 22 cards). `None` = any order, the
        /// backward-compatible default. Combines (AND) with `atk_type` and the
        /// name/text gates, and counts as a gate for the bare-OnHit skip rule.
        /// schema v38
        #[serde(default)]
        order: Option<PlayOrder>,
        /// WHOSE hit fires this, from the owner's POV. `SelfSide` (the default, and
        /// every pre-v43 node) = "when YOU hit a card"; `Opp` = "after your OPPONENT
        /// hits a Follow Up" (El Super Hombre V2). Same scoping convention as
        /// [`Trigger::OnBreakout`] / [`Trigger::OnBury`]. schema v43
        #[serde(default)]
        who: Who,
        /// Dispatch this OnHit from the owner's HAND, not the board — a "reveal this
        /// card from your hand when your opponent hits <X>" reactive (The Mailman
        /// Always Delivers). `hand_self_triggers` scans hand cards carrying it and binds
        /// `self_card` so a self-referential body (`ShuffleSelfIntoDeck`) works. `false`
        /// (the default) = the ordinary in-play/gimmick standing OnHit. schema v128
        #[serde(default, skip_serializing_if = "is_false")]
        from_hand: bool,
    },
    OnBump,
    /// "When a card or Gimmick causes you to bury any number of cards" (The Cyclone
    /// V1) / "when you bury OR discard cards from your hand from a card effect or
    /// Gimmick" (Tommy Stillwell). Fires ONLY after an EFFECT-caused bury (`act_bury`)
    /// / effect-caused hand discard (`act_discard`) — never the mechanical pass-and-
    /// recycle (`do_pass`) or the hand-cap trim, which bypass those paths. `who` =
    /// whose bury fires it (SELF = "causes you"). `from_hand_only` limits to hand
    /// buries (Tommy); `also_discard` additionally fires on an effect-caused hand
    /// DISCARD (Tommy's "bury or discard"). Fires once per bury/discard event.
    OnBury {
        who: Who,
        #[serde(default)]
        from_hand_only: bool,
        #[serde(default)]
        also_discard: bool,
    },
    StartOfTurn,
    /// Fires for the NON-active player during the active player's turn — "once during
    /// your opponent's turn, you may …" (Memes Dealer V1). The mirror of `StartOfTurn`;
    /// offered once, at the opponent's turn start. Override-only. schema v52
    DuringOpponentTurn,
    StartOfMatch,
    OnBreakout {
        /// Whose breakout fires this: `None` = any breakout ("after a breakout" —
        /// Copy Kat V2); `Some(SelfSide)` = you broke out; `Some(Opp)` = your
        /// opponent broke out ("if your opponent breaks out" — the Spotlight recur).
        #[serde(default)]
        who: Option<Who>,
    },
    /// Fires for each of `who`'s breakout ROLLS (up to `BREAKOUT_ATTEMPTS` per finish),
    /// as each is made — distinct from [`Trigger::OnBreakout`], which fires once on a
    /// SUCCESSFUL breakout. `who` is read from the effect owner's POV: `Opp` = "your
    /// opponent's breakout roll" (the defender rolling against the owner's finish),
    /// `SelfSide` = the owner's own breakout roll. The rolled value/skill is exposed via
    /// the `RollContext`, so a `RollValue` / `RollWasSkill` condition gates on it ("if
    /// your opponent rolls 10 for their Breakout roll, you lose"). schema v72
    OnBreakoutRoll {
        who: Who,
        /// Ordinal gate on WHICH of the defender's breakout rolls fire this — the 1-based
        /// attempt numbers ("your opponent's 1st or 2nd breakout roll" -> `[1, 2]`; "their
        /// 3rd breakout roll" -> `[3]`). Empty (the default, every pre-v128 node) = every
        /// roll regardless of ordinal. schema v128
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attempts: Vec<i64>,
    },
    /// Fires when the `who`-side re-rolls their TURN roll (at the roll-off, after the
    /// re-rolled die lands). `who` from the owner's POV: `SelfSide` = "when you re-roll
    /// your turn roll", `Opp` = "when your opponent/target re-rolls their turn roll". A
    /// roll-modifier body ("their roll is -1", "your roll is +2") adjusts the re-rolled
    /// value; other bodies (draw, shuffle-self from discard) resolve normally. Fired by
    /// `run_on_reroll` at the `offer_rerolls` site. schema v104
    OnReroll {
        who: Who,
    },
    /// Fires when the `who`-side's deck is shuffled by a card/gimmick EFFECT (any
    /// effect-caused shuffle: explicit "shuffle your deck", or the incidental shuffle
    /// after a search/tutor/shuffle-into-deck/hand-into-deck). NOT the match-start
    /// setup shuffle, nor the private bury-ordering shuffle. `who` = whose shuffle
    /// fires it from the owner's POV (OPP = "when your opponent shuffles their deck" —
    /// Memes Dealer V2). Override-only.
    OnShuffle {
        who: Who,
    },
    /// Fires right after the `who`-side DRAWS one or more cards (`run_on_draw` at the
    /// `draw` chokepoint). `who = SelfSide` = "when you draw". Used by a WhileInDiscard
    /// recur gated on how many cards were drawn this turn — "when this card is in your
    /// discard pile, if you drew 1 or more cards this turn, you may add it to your hand"
    /// (The Gobstopper); `self_card` is bound so `AddSelfToHand` resurrects the source.
    /// schema v129
    OnDraw {
        who: Who,
    },
    /// Fires when the `who`-side flips one or more cards (`Flip` mills deck→discard).
    /// `count` = a size gate: `None` fires on any flip; `Some(n)` with `at_least = false`
    /// only on exactly `n` ("flip exactly 3 cards" — Evee Laveaux), with `at_least = true`
    /// on `n` or more ("flip 2 or more cards"). `who` follows the shuffle convention
    /// (SELF = you flipped).
    ///
    /// `on_self` splits the two intents that share this trigger: `true` = a per-card
    /// self-trigger ("if THIS card is flipped, …"), fired by `run_self_flips` for each
    /// just-flipped card carrying it; `false` = a standing trigger ("when YOU flip …"),
    /// fired by `run_on_flip` from in-play/gimmick effects. The split keeps a standing
    /// "when you flip" effect from firing merely because its own card was milled. schema v89
    OnFlip {
        who: Who,
        #[serde(default)]
        count: Option<i64>,
        #[serde(default)]
        at_least: bool,
        #[serde(default)]
        on_self: bool,
    },
    /// Fires when one or more cards LEAVE the `who`-side's discard pile because of a
    /// card/gimmick EFFECT — "when your opponent moves any number of cards from their
    /// discard pile with their card effect or Gimmick" (Brumeister V2). Covers every
    /// effect-driven exit: recur-to-hand, shuffle-into-deck, recur-to-deck-top, the
    /// hand/discard swap, and an effect-caused discard-pile bury. Fires ONCE per
    /// action, not per card ("any number of cards"). Deliberately NOT fired by the
    /// mechanical pass-and-recycle (`do_pass`), which is not a card effect. `who` is
    /// read as the owner of the PILE, from the effect owner's POV (OPP = "your
    /// opponent['s] discard pile"). Override-only.
    OnDiscardMove {
        who: Who,
    },
    /// Fires whenever the (shared) Crowd Meter goes UP — "when the Crowd Meter
    /// increases, <body>" (Khloe Mai's gimmick, plus a small DB family). The meter is
    /// global, so both players' standing effects carrying it fire on any positive swing,
    /// however caused: the per-turn +1 after a breakout, or an effect-driven `CrowdMeter`
    /// swing. A decrease never fires it. Dispatched by `run_on_cm_increase`. schema v131
    OnCrowdMeterIncrease,
    /// Fires once at the START of every turn, AFTER the per-turn state rotation but
    /// BEFORE the roll-off — so a `MultiTurnRollBonus` armed here still lands on this
    /// turn's roll, and last-turn gates (`EndedTurnNoPlay`, `BuriedSpotlightLastTurn`)
    /// read the just-ended turn. Scans both players' entrance + in-play effects.
    /// Dispatched by `run_start_of_turn_triggers`. schema v140
    OnTurnStart,
    Static,
}

// ---------------------------------------------------------------------------
// Conditions — the effect guard
// ---------------------------------------------------------------------------

/// A boolean guard on an effect or choice option.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "@type")]
pub enum Condition {
    Always,
    And {
        items: Vec<Condition>,
    },
    Or {
        items: Vec<Condition>,
    },
    Not {
        item: Box<Condition>,
    },
    SkillCompare {
        skill: Skill,
        cmp: Comparator,
        who: Who,
        vs: Vs,
        value: Option<i64>,
        vs_skill: Option<Skill>,
    },
    HandSizeCompare {
        cmp: Comparator,
        vs: Vs,
        value: Option<i64>,
        who: Who,
    },
    CrowdMeterCompare {
        cmp: Comparator,
        value: i64,
    },
    /// `who`'s remaining deck size compared to `value` — "if you have 0 cards in your
    /// deck" (Foxworthy V3's finish double). Reads `PlayerState.deck.len()`. schema v82
    DeckSizeCompare {
        cmp: Comparator,
        value: i64,
        #[serde(default)]
        who: Who,
    },
    /// The match currently has no disqualifications — neither player can be DQ'd
    /// (`GameState.match_has_no_dq`). "If this match has No Disqualifications, your
    /// Finish roll is +1" (Cardona's Pizza Cutter; a 16-clause family). schema v83
    MatchHasNoDisqualifications,
    /// The current match is one of the listed stipulations ("if this is a Steel Cage
    /// or Liger's Den Match, …"). Holds iff `GameState.match_type` is in `types`; a
    /// disjunction over the OR-joined keywords. A 156-clause gate family. schema v92
    IsMatchType {
        types: Vec<MatchType>,
    },
    HasInPlay {
        who: Who,
        filter: CardFilter,
        count: i64,
        cmp: Comparator,
    },
    HasInHand {
        who: Who,
        filter: CardFilter,
        count: i64,
    },
    HasInDiscard {
        who: Who,
        filter: CardFilter,
        /// How many matching cards the discard pile must hold — "if you have `count`
        /// Finishes in your discard pile" (Fortress's Tower of Strength: count 2). Defaults
        /// to 1 ("has ≥1"), so the boolean forms neither carry nor churn it. schema v136
        #[serde(default = "one_i64", skip_serializing_if = "is_one_i64")]
        count: i64,
    },
    /// Cross-board in-play count compare: `who`'s count of cards in play matching
    /// `filter` compared (`cmp`) against `vs_who`'s count of the same filter. "When
    /// your target has more Strikes in play [than you]" (Snake Pitt V3): `who=OPP`,
    /// `vs_who=SELF`, `cmp=">"`, filter `atk_type=Strike`. Honors `CountsAsInPlay`
    /// on both boards (via `count_in_play`).
    InPlayCompare {
        filter: CardFilter,
        cmp: Comparator,
        who: Who,
        vs_who: Who,
    },
    /// True while `who`'s [`Action::ChooseName`] binding equals `name` — the gate that
    /// turns "when you hit a card with THAT in the name" into one concrete effect per
    /// option (Raven). Case-sensitive equality against the stored binding; false when
    /// nothing has been chosen yet. schema v37
    ChosenNameIs {
        name: String,
        who: Who,
    },
    RollWasSkill {
        skill: Skill,
        /// Whose turn-roll skill this checks. `SELF` (default) = the owner's rolled
        /// skill; `OPP` reads the other side's skill from the roll context's
        /// `opp_skill`. Composed under And/Or, this expresses "if **both** players
        /// rolled X" / "if **either** player rolled X for their turn roll" (Tomato
        /// Tomato Jr.). schema v75
        #[serde(default)]
        who: Who,
    },
    RollGapExactly {
        k: i64,
    },
    RollGapAtLeast {
        k: i64,
    },
    /// The owner rolled at least `k` *higher* than the opponent — mirror of
    /// `RollGapAtLeast` (owner `k` lower). A lead of `k` is `gap <= -k`.
    RollLeadAtLeast {
        k: i64,
    },
    RollValue {
        cmp: Comparator,
        value: i64,
        /// Whose turn-roll VALUE (die + stat + mods) to compare. `SelfSide` (the
        /// default) = "you rolled N for your turn roll"; `Opp` = "your opponent's turn
        /// roll is N" (Scott Prime's The Loaded Glove — a 12-clause family of
        /// opp-turn-roll-value gates). The opponent's value is read from the actor's
        /// [`RollContext`] as `value + gap` (`gap` = opp − self). schema v130
        #[serde(default, skip_serializing_if = "is_self_who")]
        who: Who,
    },
    /// The rolled skill's **printed** (base, unbuffed) stat on the `who`-side's
    /// competitor equals `value` — "when your opponent rolls their printed 8 skill"
    /// (Collin the Chrononaut). Needs a roll context; `who` picks whose printed stat
    /// to read (the roller), following the trigger's `who` like `RollValue`.
    PrintedRollValue {
        who: Who,
        value: i64,
    },
    /// The owner and their target rolled the **same skill** for this turn-roll (Hex,
    /// Nic Nemeth). Reads the post-roll context's `skill` vs `opp_skill`; needs a
    /// roll context (false without one, and in single-sided re-roll/switch contexts).
    SameRolledSkill,
    /// This is the first turn of the game (`GameState.turn_no <= 1` — 0 at setup, 1 once
    /// the first turn begins). Gates the "if this is the first turn of the game, …" riders
    /// (this card is also a `<order>` / cannot be stopped). A "considered first turn"
    /// override that re-labels a later turn is a separate, unmodeled action. schema v119
    FirstTurn,
    /// The card the owner most recently stopped had NEITHER a competitor logo NOR a skill
    /// requirement — i.e. it carried the `Logoless` tag AND lacked `SkillRequirement`.
    /// Read from `PlayerState.flags["stopped_card_no_logo_no_req"]`, stamped on the
    /// stopping side by `apply_stop` (the same flags recipe as `StoppedCard`'s turn stamp).
    /// Gates "if the stopped card did not have a competitor logo or skill requirement, this
    /// card is also a Finish". schema v144
    StoppedCardNoLogoNoReq,
    /// The owner's opponent won the *previous* turn's roll-off
    /// (`GameState.last_roll_winner`); false before turn 1. Gates Dunn's re-roll.
    OppWonLastRoll,
    /// The PREVIOUS turn's roll-off bumped (`GameState.last_turn_bumped`); false before
    /// turn 1. Gates Mack-a-Tack's "if you bumped on the last turn roll" re-roll.
    BumpedLastTurnRoll,
    /// `who` ended the **previous** turn without playing a card — they were the
    /// roll-off winner on turn `turn_no - 1` and passed (chose not to play, or had
    /// nothing playable). Reads `who`'s `PlayerState.flags["last_pass_turn"]`, stamped
    /// by `do_pass`; false before turn 1 and whenever they instead played a card or
    /// lost the previous roll-off. `who` defaults to SELF (skip-when-self, so The SRG
    /// Boss's pre-v139 SELF nodes round-trip byte-identically); the OPP form gates
    /// "when your opponent ended their turn without playing a card" (Impact is Family
    /// V1). schema v78; `who` added v140
    EndedTurnNoPlay {
        #[serde(default, skip_serializing_if = "is_self_who")]
        who: Who,
    },
    /// `who` buried a Spotlight card on the **previous** turn. Reads `who`'s
    /// `PlayerState.flags["buried_spotlight_turn"]` (== `turn_no - 1`), stamped when a
    /// `Bury` moves a Spotlight-tagged card; false otherwise. Gates Impact is Family
    /// V1's "and they buried a Spotlight card" rider. schema v140
    BuriedSpotlightLastTurn {
        who: Who,
    },
    /// `who` broke out on the PREVIOUS turn — they were the defender of a Finish on turn
    /// `turn_no - 1` and survived every Breakout roll. Reads `PlayerState.flags`
    /// `["broke_out_turn"]`, stamped by `breakout` on success; false before turn 1 and
    /// whenever `who` did not break out last turn. Gates "if you broke out last turn, this
    /// card is also a Lead" ("either/any player" -> Or of both sides). schema v120
    BrokeOutLastTurn {
        who: Who,
    },
    /// `who` performed a stop (played a stop card that stopped an attack) on the PREVIOUS
    /// turn (`last_turn = true`) or THIS turn (`last_turn = false`). Reads
    /// `PlayerState.flags["stopped_card_turn"]` (the most-recent turn `who` stopped,
    /// stamped by `apply_stop` for the stopping side); true iff it equals `turn_no - 1`
    /// (last) or `turn_no` (this). Gates "if you stopped a card last turn, …"
    /// ("either/any player" -> Or of both sides). schema v121
    StoppedCard {
        who: Who,
        last_turn: bool,
    },
    /// The owner re-rolled their turn roll **this** turn — any of their turn dice was
    /// re-rolled at the roll-off (a granted "re-roll your next turn roll", a standing
    /// `Reroll{This}`, or a bump re-roll). Reads `PlayerState.flags["rerolled_turn"]`,
    /// stamped in `offer_rerolls` for the re-rolled side; false otherwise. Gates King
    /// Brian Cage's finish riders ("if you rolled Power for your turn roll or you
    /// re-rolled your turn roll, …"), OR'd with `RollWasSkill{Power}`. schema v80
    RerolledTurnRoll,
    /// "flipped for your Gimmick" — the flip currently resolving was caused by a
    /// Gimmick-source effect. Reads [`GameState::flip_provenance`]; only meaningful on a
    /// flipped card's own `OnFlip{SELF}` self-trigger. schema v87
    FlippedForGimmick,
    /// "flipped by \"<X>\"" — the flip currently resolving was caused by a card whose
    /// name contains one of `names` (case-insensitive OR-substring; the Set-Up-the-Ladder
    /// ladder-match cards). Reads [`GameState::flip_provenance.source_name`]. schema v87
    FlippedByName {
        names: Vec<String>,
    },
    GimmickFlipped {
        who: Who,
    },
    /// It is currently `who`'s turn — the active player (roll-off winner) is the
    /// `who`-side. Gates a continuous effect to a turn phase ("during your opponent's
    /// turn: …" — La Fenix). Reads `GameState.active`.
    DuringTurn {
        who: Who,
    },
    /// The owner's competitor's name contains any of `name_contains` (case-insensitive
    /// substring) — "you are Paul Walter Hauser". A clause that references a specific
    /// wrestler is inert on every other competitor. schema v72
    CompetitorIs {
        name_contains: Vec<String>,
    },
    /// `who` has hit (landed) at least one card this turn — "if you hit another card
    /// this turn" (task #94). Reads `PlayerState.hits_this_turn`, reset at each turn
    /// start; the current (stopped) card is not yet counted. schema v72
    HitThisTurn {
        who: Who,
    },
    /// `who` has DRAWN at least `at_least` cards this turn — "if you drew 1 or more cards
    /// this turn, …" (The Gobstopper recur; Brotherly Love's "drew 3 or more → also a
    /// Lead"). Reads `PlayerState.drew_this_turn`, incremented at the `draw` chokepoint and
    /// reset at turn start. schema v129
    DrewThisTurn {
        #[serde(default)]
        who: Who,
        at_least: i64,
    },
    /// `who` has LOST at least `at_least` turn rolls IN A ROW — "if you lose 2 Turn Rolls
    /// in a row, …" (Me Against the World's discard recur). Reads
    /// `PlayerState.turn_losses_in_a_row`, incremented on a turn-roll loss and reset on a
    /// win. schema v134
    LostTurnRollsInARow {
        #[serde(default)]
        who: Who,
        at_least: i64,
    },
    /// `who` hit (landed) a card matching `filter` this turn (`last_turn = false`) or the
    /// PREVIOUS turn (`last_turn = true`) — "if you hit a Grapple last turn, …" / "if you
    /// hit a card with 'Dragon' in the name this turn, …". Reads `PlayerState.hit_this_turn`
    /// / `hit_last_turn` (full cards, rotated at turn start); a bare/empty `filter` matches
    /// any hit. Distinct from [`Self::HitThisTurn`] (a count with no filter). schema v91
    HitCard {
        filter: CardFilter,
        #[serde(default)]
        who: Who,
        #[serde(default)]
        last_turn: bool,
    },
}

// ---------------------------------------------------------------------------
// Actions — `Effect.actions` / `ChoiceOption.actions`
// ---------------------------------------------------------------------------

/// One primitive game action performed by an [`Effect`].
///
/// This is the superset used by `Effect.actions`; `ChoiceOption.actions`
/// excludes only [`Action::Unsupported`], which never appears inside a choice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "@type")]
pub enum Action {
    Draw {
        n: i64,
        source: DeckEnd,
        who: Who,
        per: Option<CardFilter>,
        per_who: Who,
        /// Clamps the per-count product — "draw 1 card for each … (Max 3)". Ignored
        /// without `per`. schema v38
        #[serde(default)]
        cap: Option<i64>,
        /// Drop the card that TRIGGERED this effect from the `per` count — "for each
        /// **other** Lead you have in play". Needed only when the trigger puts the
        /// card on the board before firing (an `OnHit` gimmick; `run_hit_gimmicks`
        /// runs after the hit card is in play). The usual "each other" clause is
        /// authored `OnPlay`, where the source is not yet on the board and no
        /// exclusion is needed, so this defaults false. schema v38
        #[serde(default)]
        per_excludes_trigger: bool,
        /// Count is the Crowd Meter (plus `n` as a signed offset), clamped to `cap` —
        /// "draw cards equal to the Crowd Meter", "… equal to the Crowd Meter +1 (Max
        /// +5)". Mutually exclusive with `per`. `n` is the offset here, not a flat count.
        /// Additive/skip-when-false. schema v108
        #[serde(default, skip_serializing_if = "is_false")]
        from_crowd: bool,
    },
    Bury {
        selector: CardFilter,
        count: i64,
        who: Who,
        random: bool,
        #[serde(default)]
        source: BuryFrom,
        /// `BuryFrom::Discard` only: the actor picks WHICH card, from EITHER player's
        /// discard pile — "bury 1 card in any player's discard pile" (Cherry
        /// Glamazon). The default discard bury is the mechanical pass-and-recycle,
        /// which takes the top `count` and ignores `selector`; this one is a targeted
        /// choice (it can deny a specific recursion target). `who` is ignored when
        /// set, and the card returns to ITS OWNER's deck bottom. schema v39
        #[serde(default)]
        choose: bool,
        /// Per-count scaling like [`Action::Draw::per`]: when set, `count` is multiplied
        /// by the number of `per_who`'s in-play cards matching this filter — "bury 1 card
        /// in your opponent's discard pile for each Strike you have in play" / "…for each
        /// Lead you have in play" (Cardona; a 34-clause family). schema v83
        #[serde(default)]
        per: Option<CardFilter>,
        #[serde(default)]
        per_who: Who,
        /// Which zone the `per` filter counts (like [`Action::FinishRollBonus::per_zone`]).
        /// `InPlay` (default) = "for each `<X>` you have in play" (Cardona); `FlippedThisTurn`
        /// = "your opponent buries 1 card in their hand for each Strike flipped" (Scott
        /// Prime's Five Star Heart Punch — count the finisher's flips, not the board).
        /// schema v130
        #[serde(default, skip_serializing_if = "is_in_play_zone")]
        per_zone: CountZone,
        /// Bury EVERY card matching `selector` in the target's hand, ignoring `count`
        /// (and `per`) — "Look at your opponent's hand, they bury all Strike cards"
        /// (a 12-clause family). `BuryFrom::Hand` only; the dispatch sets the effective
        /// count to the target's hand size, and the per-card loop stops when no matching
        /// card remains. schema v90
        #[serde(default)]
        all: bool,
    },
    /// Bury the TRIGGERING card — "bury this card" on an `OnStop` clause (task #94:
    /// "If stopped, discard 1 card from your hand and bury this card or lose ..."). The
    /// referent is [`Engine::stopped_card`], the card whose stop fired the effect;
    /// burying moves it from the discard pile to the bottom of its owner's deck. A
    /// no-op outside a stop context. schema v72
    BuryThisCard,
    /// Add the TRIGGERING (flipped) card to its owner's hand — "If this card is
    /// flipped, [you may] add it to your hand." The referent is
    /// [`Engine::self_card`], set per-card while an `OnFlip` clause carried by a
    /// just-flipped card is dispatched; the card moves from the discard pile (where a
    /// flip lands it) to its owner's hand. The "you may" lives on [`Effect::optional`].
    /// A no-op outside a flip context or if the card has already left the discard.
    /// schema v85
    AddSelfToHand,
    /// Shuffle the TRIGGERING (flipped) card back into its owner's deck — "If this card
    /// is flipped, [you may] shuffle it [back] into your deck." Sibling of
    /// [`Action::AddSelfToHand`]: the referent is [`Engine::self_card`]; the card
    /// moves from the discard pile to the deck, which is then shuffled (firing
    /// `OnShuffle`). "you may" lives on [`Effect::optional`]. schema v86
    ShuffleSelfIntoDeck,
    /// Put the TRIGGERING/self card on TOP of its owner's deck (drawn next) — "[you may]
    /// put this card on top of your deck." Referent is [`Engine::self_card`] (a discard-
    /// resident card firing its WHILE_IN_DISCARD trigger) falling back to
    /// [`Engine::stopped_card`] (the "If stopped, put this card on top of your deck"
    /// family); the card moves from wherever it sits (discard/hand) to the deck front,
    /// unshuffled. "you may" lives on [`Effect::optional`]. schema v141
    PutSelfOnDeckTop,
    /// Put `count` cards from the owner's HAND on TOP of their deck (drawn next),
    /// unshuffled — "put N card(s) from your hand on top of your deck." The owner chooses
    /// which (their hidden hand); the loop stops early when the hand runs dry. Tails the
    /// [`Action::PutSelfOnDeckTop`] recycle ("put this card on top of your deck, then put
    /// 1 card from your hand on top of your deck"). schema v142
    PutFromHandOnDeckTop {
        count: i64,
    },
    /// Play the TRIGGERING (flipped) card immediately — "If this card is flipped, [you
    /// may] play it[ as an additional card this turn]." The referent is
    /// [`Engine::self_card`]; the card leaves the discard pile and resolves as a
    /// normal play by its owner (stop window, OnPlay/OnHit), a bonus action outside the
    /// turn's one-card play. "you may" lives on [`Effect::optional`]. schema v86
    PlaySelf,
    /// Add cards from the just-flipped pool to hand — "add N of the flipped cards to
    /// your hand" / "add all flipped Strikes to your hand" / "randomly add 1 of the
    /// flipped cards…". Selects from `PlayerState.flipped_this_turn` (the turn's flips,
    /// recorded by `act_flip`) that are still in the discard and match `filter`; `count`
    /// = how many (`None` = all matching), `random` picks by RNG instead of by the
    /// owner. Distinct from [`Action::AddFromDiscard`] (whole discard, one card): this is
    /// scoped to the flip pool. schema v88
    AddFlippedToHand {
        #[serde(default)]
        count: Option<i64>,
        #[serde(default)]
        filter: CardFilter,
        #[serde(default)]
        random: bool,
    },
    /// "You may switch 1 card in your hand with 1 card in your discard pile" (Collin,
    /// Mr. Rey): the owner picks one hand card out (→ discard) and one discard card in
    /// (→ hand). A no-op if either zone is empty. The "you may" lives on
    /// [`Effect::optional`]. Picks route to the `discard` (shed) / `target` (tutor)
    /// decision points.
    SwapHandDiscard,
    /// Grant `who` a deferred, one-shot optional hand↔discard swap on their next
    /// turn (Mr. Rey: "When you roll Technique for your turn roll: Once on the next
    /// turn, you may switch 1 card in your hand with 1 card in your discard pile").
    /// Sets a next-turn grant that promotes to usable at the start of the grantee's
    /// following turn (SET, not accumulate — an unused grant expires after that one
    /// turn) and is offered as an optional [`SwapHandDiscard`] before they act.
    GrantSwapNextTurn {
        who: Who,
    },
    Flip {
        n: i64,
        who: Who,
        /// Per-count: flip `n` times the number of `per_who`'s cards matching this
        /// filter ("Flip N cards for each Follow Up you have in play").
        #[serde(default)]
        per: Option<CardFilter>,
        #[serde(default)]
        per_who: Who,
        /// Flip-until (variable count): when set, ignore `n` and mill the target's
        /// deck one card at a time until a flipped card matches this filter (or the
        /// deck empties). "Flip cards until you flip a Submission[, add it to your
        /// hand]." The matching card goes to the hand when `until_to_hand`, else to
        /// the discard with the rest. schema v68
        #[serde(default)]
        until: Option<CardFilter>,
        #[serde(default)]
        until_to_hand: bool,
    },
    /// Move `count` card(s) from the `from` end of `who`'s DECK to their discard pile
    /// — "Each player discards the bottom card of their deck." Unlike [`Self::Flip`]
    /// (which mills the TOP and fires flip triggers / records `flipped_this_turn`),
    /// this is a plain deck-to-discard mill with no flip semantics. schema v101
    MillDeck {
        who: Who,
        count: i64,
        from: DeckEnd,
    },
    /// One-shot roll-conditional draw — "if your [opponent's] next turn roll is `<S>`,
    /// draw N". Arms on play; the engine watches `who`'s NEXT turn roll (`SelfSide` =
    /// your own, `Opp` = your opponent's) and, if it resolves to `skill`, the effect
    /// owner draws `count`. Fires-or-fizzles on that one turn roll and is consumed —
    /// distinct from `ModifyRoll{on_skill}`, which waits until its skill comes up.
    /// schema v109
    RollDraw {
        who: Who,
        skill: Skill,
        count: i64,
    },
    /// One-turn skill-gated turn-roll bonus — "+N to `<S>`, `<S>` during your next turn
    /// roll" / "if your [opponent's] next turn roll is `<S>`, it is +N". Arms on play;
    /// `delta` applies to `who`'s (`SelfSide` = your own, `Opp` = your opponent's)
    /// IMMEDIATELY-next turn roll if it comes up one of `skills`, then the whole pending
    /// queue is drained — a one-turn window, so a non-match fizzles. Distinct from
    /// `ModifyRoll{on_skill}` (waits indefinitely for one skill). schema v110
    NextRollSkillBonus {
        who: Who,
        skills: Vec<Skill>,
        delta: i64,
    },
    /// Multi-turn turn-roll bonus — "your [opponent's] next N turn rolls are +/-N".
    /// Arms on play; `delta` applies to each of `who`'s (`SelfSide` = your own, `Opp` =
    /// your opponent's) next `rolls` turn rolls, decrementing once per roll-off until
    /// exhausted. Skill-agnostic and self-expiring (unlike the standing `TurnRollBonus`).
    /// schema v111
    MultiTurnRollBonus {
        who: Who,
        rolls: i64,
        delta: i64,
    },
    /// "Bury up to `max` cards in your hand to draw the same number of cards +`bonus`"
    /// (Stolen Valor, Back Cracker Potion, Win When You Can…). `who` buries their least
    /// valuable hand cards (to the deck bottom) and then draws that many PLUS `bonus`;
    /// the draw is coupled to the ACTUAL bury count, so zero buries still draw `bonus`.
    /// The "up to" collapses to burying min(`max`, hand size), matching the "bury up to N"
    /// family convention. schema v149
    BuryToDraw {
        max: i64,
        bonus: i64,
        who: Who,
    },
    Discard {
        selector: CardFilter,
        count: i64,
        who: Who,
        random: bool,
        per: Option<CardFilter>,
        per_who: Who,
        /// Like [`Action::Bury`]'s `choose`: the EFFECT OWNER looks at the target's
        /// hand and picks which card(s) to discard ("Look at your opponent's hand,
        /// choose 1 card and discard it"), rather than the hand owner shedding their
        /// own. Only meaningful with `who == Opp`; ignored when `random`. schema v60
        #[serde(default)]
        choose: bool,
        /// Discard EVERY card matching `selector` from the target's hand, ignoring
        /// `count` (and `per`) — "Look at your opponent's hand, they discard all
        /// Strikes". Mirrors [`Action::Bury`]'s `all`; the dispatch sets the effective
        /// count to the target's hand size. schema v90
        #[serde(default)]
        all: bool,
    },
    Search {
        filter: CardFilter,
        dest: Dest,
        count: i64,
        /// Which zone(s) to tutor from. Default `Deck`; `DeckOrDiscard` also scans the
        /// discard pile. Skip-when-default, so pre-v115 fixtures round-trip identically.
        #[serde(default, skip_serializing_if = "is_default_search_source")]
        source: SearchSource,
    },
    ShuffleDeck {
        who: Who,
    },
    ShuffleIntoDeck {
        selector: CardFilter,
        /// Which zone the shuffled card comes from — `Discard` (default) or `InPlay`
        /// ("shuffle 1 Follow Up you have in play into your deck"). schema v83
        #[serde(default)]
        source: ShuffleSource,
        /// Whose zone the shuffle acts on — `SelfSide` (default) or `Opp`/each-player
        /// ("each player shuffles 1 Grapple from their discard pile into their deck" emits
        /// one per side). Each player recurs THEIR OWN zone into THEIR OWN deck. schema v143
        #[serde(default, skip_serializing_if = "is_self_who")]
        who: Who,
        /// Shuffle EVERY matching card in the source zone (not just one chosen card).
        /// "Take any number of Lead cards … and shuffle them into your deck" — the
        /// "any number" is the whole matching set. schema v124
        #[serde(default, skip_serializing_if = "is_false")]
        all: bool,
        /// After shuffling, draw as many cards as were shuffled ("… then draw the same
        /// number of cards"). Coupled to the actual shuffled count. schema v124
        #[serde(default, skip_serializing_if = "is_false")]
        then_draw: bool,
        /// After shuffling, bury as many cards from HAND as were shuffled ("… then bury the
        /// same number of cards from your hand" — Double Leg Death Lock). Coupled to the
        /// actual shuffled count; mutually exclusive with `then_draw` in practice. schema v129
        #[serde(default, skip_serializing_if = "is_false")]
        then_bury: bool,
    },
    AddFromDiscard {
        filter: CardFilter,
    },
    RecurToDeckTop {
        selector: CardFilter,
        count: i64,
    },
    CountsAsInPlay {
        selector: CardFilter,
        count: i64,
    },
    RemoveFromPlay {
        selector: CardFilter,
        who: Who,
        count: i64,
        /// Like [`Action::ReturnToHand`]'s: the actor picks from EITHER board —
        /// "choose 1 card in play and discard it" (Cherry Glamazon), where the card
        /// does not restrict whose board. `who` is ignored when set. schema v39
        #[serde(default)]
        choose: bool,
        /// Send the removed card to its owner's DECK BOTTOM instead of their discard —
        /// "choose 1 card your opponent has in play and BURY it" (JT Dunn's gimmick; a
        /// 6-card family). `false` (the default) = the ordinary discard removal. schema v133
        #[serde(default, skip_serializing_if = "is_false")]
        to_deck: bool,
        /// Remove EVERY matching in-play card of the target at once, with no per-card
        /// pick ("Discard all cards in play", Apocalypse) — there is no real choice, so
        /// this suppresses the phantom decisions a `count`-many loop would emit. `count`
        /// is ignored when set. `false` (the default) = the ordinary N-many aimed removal.
        /// schema v135
        #[serde(default, skip_serializing_if = "is_false")]
        all: bool,
    },
    /// The per-player halves of an "each player …" board effect (Apocalypse's board
    /// clear, Rejected!'s discard-bury, Derailed's hand cycle), wrapped so a competitor
    /// with a matching [`Action::RedirectAuthority`] (Emo Mam) may choose which players
    /// they affect. Absent an active authority the wrapper applies every inner action —
    /// byte-identical to a plain each-player effect — so wrapping is safe DB-wide. The
    /// authority match is by the RESOLVING card's name, so only the cards it lists are
    /// ever redirected. schema v135
    RedirectBoardEffect {
        actions: Vec<Action>,
    },
    /// A passive gimmick marker (Emo Mam): "when you or your opponent hit one of
    /// `groups`, you may choose who it affects." Read by [`Action::RedirectBoardEffect`]
    /// via the resolving card's name (trailing-`!`/case-insensitive, so "Rejected"
    /// matches the card "Rejected!"). Never executes on its own. schema v135
    RedirectAuthority {
        groups: Vec<String>,
    },
    /// Discard 1 of the owner's own in-play cards, then discard 1 of the OPPONENT's
    /// in-play cards of the SAME play order (Candyman Dan). The second target's filter
    /// is bound at runtime to the first pick's play order — a trade the actor chooses
    /// both ends of. No-op if the owner has nothing in play; the second discard is
    /// skipped if the opponent has no same-order card. schema v51
    DiscardInPlayMatch,
    /// "Discard any number of cards from your hand, your opponent discards the same
    /// number of cards from their hand `offset`" (Defector's Dismantler: offset -1;
    /// 2 cards). The actor's chosen count N is a heuristic in `act_coupled_discard`
    /// (strip the opponent's hand when affordable: N = min(self_hand, opp_hand+1)),
    /// since no policy count-choice hook exists; the self-discard fires OnBury so a
    /// discard-recur gimmick still triggers, then the opponent sheds max(0, N+offset).
    /// schema v76
    CoupledDiscard {
        offset: i64,
    },
    /// "Add `count` card(s) in play to their hand" (Fox Assassin V2): return matching
    /// in-play cards to their OWNER's hand (bounce). `who` picks the board; `choose`
    /// (like [`ShuffleHandDraw`]) lets the actor pick from EITHER board — "any player
    /// has in play". A no-op when no matching card exists.
    ReturnToHand {
        selector: CardFilter,
        who: Who,
        count: i64,
        #[serde(default)]
        choose: bool,
    },
    RevealAndDiscard {
        count: i64,
        who: Who,
    },
    /// "Your opponent randomly reveals `count` card(s) in their hand: if it is a stop,
    /// draw `draw` cards" (Bartholomew Hooke). Reveals stay in hand; the actor draws
    /// `draw` for each revealed stop.
    RevealForDraw {
        who: Who,
        count: i64,
        draw: i64,
        match_on: RevealMatch,
    },
    Peek {
        who: Who,
    },
    /// `who` reveals `count` card(s) from their OWN hand to the opponent — a fog-of-war
    /// effect ("Each player reveals 1 card in their hand"). The revealing player CHOOSES
    /// which (a `reveal` decision); the chosen cards become visible to the opponent in
    /// the observable projection while they remain in hand. No zone change. schema v100
    Reveal {
        who: Who,
        count: i64,
        /// "Reveal your (whole) hand to your opponent" (Bermuda Triangle): expose
        /// EVERY card in `who`'s hand, ignoring `count` and the per-card choice.
        /// `false` (the default, every pre-v127 node) = the fog-of-war "reveal N of
        /// your choosing" form. schema v127
        #[serde(default, skip_serializing_if = "is_false")]
        whole_hand: bool,
    },
    /// Arm a deferred, mandatory "forced reveal-and-play" on `who` for their next
    /// turn (Father Light: "during your opponent's next turn, they randomly reveal
    /// a card in their hand until they reveal a playable card; they must play that
    /// card"). Sets a one-shot flag on the target; at the start of that player's
    /// next won turn the engine reveals their hand in random order until a card is
    /// playable (Lead / Follow-Up-with-Lead / Finish-with-Follow-Up, stops count as
    /// their play order) and force-plays it. Idempotent: re-arming before the target
    /// takes a turn still fires once.
    ForceRevealPlay {
        who: Who,
    },
    /// Copy `who`'s Entrance onto the actor's (El Ganso Ruso: "Copy your target's
    /// Entrance"): append the target entrance's effects to the actor's own
    /// entrance, so the actor gains that entrance's ability (in addition to their
    /// own). Resolved live — the engine sees both loaded entrances. Authored under
    /// a `StartOfMatch` `Choice`; copied *ongoing* abilities (OnRoll/Static) fire
    /// naturally, but a copied `StartOfMatch` ability has already missed its window.
    CopyEntrance {
        who: Who,
    },
    /// Look at / reveal cards from the top (and/or bottom) of `deck`'s deck, then
    /// route them: the effect owner (the "actor") takes `to_hand` of them to the
    /// deck owner's hand, buries `bury` to the deck bottom, and disposes of the
    /// leftovers per `rest`. The actor picks by card value — best-to-hand, and
    /// bury the *worst* on their own deck or the *best* on an opponent's deck
    /// (sabotage, e.g. The Oracle). `reveal=true` makes the seen cards public
    /// (logged); `reveal=false` is a private "look at". Covers reveal-top-of-deck
    /// gimmicks (Perfect Assistant, Split, Ricky Riot, The Oracle).
    Scry {
        deck: Who,
        #[serde(default)]
        top: i64,
        #[serde(default)]
        bottom: i64,
        #[serde(default)]
        reveal: bool,
        #[serde(default)]
        to_hand: i64,
        #[serde(default)]
        bury: i64,
        #[serde(default)]
        rest: ScryRest,
        /// Restrict which revealed cards `to_hand` may take — "add 1 STOP to your hand and
        /// bury the others" (Fortress): only a matching card goes to hand (best-first among
        /// matches), the rest fall through to `bury`/`rest`. `None` (the default) = take the
        /// `to_hand` best regardless of kind. schema v136
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to_hand_filter: Option<CardFilter>,
    },
    /// Reveal the top card of `deck`'s deck and route it by a runtime predicate: if
    /// the card's `atk_type` equals `match_atk` it goes to `on_match`, otherwise to
    /// `on_fail` (taken only when worthwhile if `fail_optional` — "you may flip/bury
    /// it"). Destinations: HAND (deck owner's hand), FLIP (mill to discard), BURY
    /// (deck bottom), LEAVE (keep on top). Covers "reveal the top card; if the move
    /// type matches the rolled skill …" gimmicks (Candy MaM, Flame Fighter) — one
    /// effect per rolled skill, `match_atk` baked to that skill's move type.
    RevealRoute {
        deck: Who,
        match_atk: AtkType,
        on_match: RevealDest,
        on_fail: RevealDest,
        #[serde(default)]
        fail_optional: bool,
        #[serde(default)]
        reveal: bool,
        #[serde(default)]
        reveal_from: RevealFrom,
        /// When set, the predicate is a number-parity match instead of `atk_type`:
        /// `Some(true)` = the revealed card matches iff its number is even,
        /// `Some(false)` iff odd (the actor's blind odd/even guess — Smart Mark
        /// Sterling). `None` keeps the `atk_type == match_atk` predicate.
        #[serde(default)]
        match_parity: Option<bool>,
    },
    /// Reveal card(s) and conditionally fire a nested consequence. Reveal `count`
    /// card(s) from `from` — the top/bottom of the owner's deck (a non-destructive
    /// peek; the card stays unless taken) or a uniformly-random card in the owner's
    /// hand; if a revealed card matches `filter` (name substring / attack type), run
    /// the consequence: move that card to the owner's hand when `take_matched` ("add
    /// that card to your hand"), then apply `then` (extra actions parsed from the
    /// tail — draw, roll bonus, bury, re-roll, …). `then_optional` makes the whole
    /// consequence a "you may". A non-match reveals nothing further and leaves every
    /// card in place. Covers "Reveal the top card of your deck: if it has 'X' in the
    /// name, add that card to your hand" and "Randomly reveal 1 card in your hand: if
    /// it has 'X' in the name, draw 1 card". schema v95
    RevealThen {
        reveal_from: RevealSource,
        count: i64,
        filter: CardFilter,
        #[serde(default)]
        take_matched: bool,
        #[serde(default)]
        then: Vec<Action>,
        #[serde(default)]
        then_optional: bool,
    },
    /// Shuffle a player's hand back into their deck, shuffle it, then draw `count`
    /// fresh cards — a mid-match hand refresh (Cyclone V2, on a bump). `choose`
    /// lets the actor pick which player ("either player"); otherwise `who` selects.
    ShuffleHandDraw {
        who: Who,
        count: i64,
        #[serde(default)]
        choose: bool,
        /// How many hand cards to shuffle in: `None` = the WHOLE hand (Cyclone V2);
        /// `Some(n)` = the owner reveals and shuffles `n` chosen cards (Memes Dealer V1:
        /// "reveal 1 card in your hand, shuffle it into your deck, and draw 1"). schema v52
        #[serde(default)]
        hand_count: Option<i64>,
    },
    ModifyRoll {
        who: Who,
        delta: i64,
        when: RollWhen,
        per: Option<CardFilter>,
        per_who: Who,
        /// Which zone the `per` count reads — `InPlay` (the default, "for each Lead
        /// you have in play") or `Discard` ("+2 for each Finish in your discard
        /// pile"). Only meaningful when `per` is set. schema v70
        #[serde(default)]
        per_zone: CountZone,
        /// When set (with `when = Next`), a SKILL-KEYED pending mod: it waits, across
        /// however many turns, until `who` next rolls this skill for their turn roll,
        /// applies `delta` to THAT roll, and is consumed — "the next time you roll
        /// Technique for your turn roll, it is +2". `None` = the plain next/this mod.
        /// schema v99
        #[serde(default, skip_serializing_if = "Option::is_none")]
        on_skill: Option<Skill>,
    },
    /// Add `delta` to the owner's CURRENT roll value, mid-roll-off. Unlike
    /// `ModifyRoll{when=This}` (a pending mod consumed at roll start), this applies to a
    /// roll ALREADY made — a choice branch inside an `OnRollBoost` offer (El Super Hombre
    /// V3: "when you roll Agility … or your roll is +1"). Read by `offer_roll_boost` via
    /// the engine's `pending_roll_boost`. schema v54
    RollBoost {
        delta: i64,
    },
    BuffSkill {
        skill: Skill,
        delta: i64,
        who: Who,
        duration: Duration,
        target_highest: bool,
        /// Retarget the buff to the target's LOWEST base skill (ties -> earlier in
        /// canonical order), mirroring `target_highest`. "+N to your lowest skill".
        /// schema v93
        #[serde(default)]
        target_lowest: bool,
        /// Retarget the buff to the skill the OWNER bound via [`Action::ChooseSkill`]
        /// ("Choose a skill: Your opponent's skill of that type is -1" — Catch These
        /// Hands): the delta lands on the owner's `chosen_skill`, read live in derived
        /// stats. Inert (contributes nothing) until a choice is bound. schema v150
        #[serde(default, skip_serializing_if = "is_false")]
        target_chosen: bool,
        per_crowd: bool,
        /// Clamps the bonus. Under a `While*` duration this bounds the per-read
        /// `per`/`per_crowd` product (see `per`). Under a TIMED duration
        /// (`UntilEndOfTurn` / `UntilStartOfYourNextTurn`) it instead bounds the
        /// ACCUMULATED total this buff has granted while live: repeat firings stack
        /// `delta` and clamp to `cap` — "+1 to Strike and +5 to Submission … (Max +5
        /// to each)" (Snake Pitt Super Lucha). Hand-adjudicated 2026-07-20.
        cap: Option<i64>,
        /// When set, the bonus is `delta * (count of the target's cards in
        /// `per_zone` matching this filter)`, clamped to `cap` — "your Technique is
        /// +1 for each card you have in play with 'Chin' in the name (Max +3)".
        #[serde(default)]
        per: Option<CardFilter>,
        #[serde(default)]
        per_zone: CountZone,
        /// Exclude the SOURCE card (the one carrying this buff) from the `per` count —
        /// "for each OTHER card you have in play with 'X' in the name". Only meaningful
        /// with `per` over the owner's own board; the count-in-zone drops the source by
        /// pointer identity. Additive/skip-when-false (mirrors `ModifyRoll.on_skill`),
        /// so pre-v105 fixtures round-trip byte-identically. schema v105
        #[serde(default, skip_serializing_if = "is_false")]
        per_excludes_self: bool,
    },
    MaxHandSize {
        delta: i64,
        who: Who,
        duration: Duration,
        /// Absolute maximum ("your opponent's maximum handsize is N") — overrides the
        /// base cap rather than shifting it; the LOWEST active set wins, then `delta`
        /// mods apply on top. `None` = a pure delta modifier (the historical form).
        /// Additive/skip-when-none, so pre-set fixtures round-trip byte-identically.
        /// schema v114
        #[serde(default, skip_serializing_if = "Option::is_none")]
        set: Option<i64>,
    },
    /// Minimum-handsize modifier (Quadruple H). NOT a draw-up floor: per the SRG
    /// ruling the minimum is a floor on the MAXIMUM, folded in `effective_hand_cap`.
    /// Read there, never executed. schema v44
    MinHandSize {
        delta: i64,
        who: Who,
        duration: Duration,
    },
    /// Static declaration that the declarer mirrors the opponent's skill increases
    /// (Mimic: "when your opponent increases their skills, your skills are also
    /// increased the same amount"). Read in `effective_stats` — for each skill the
    /// declarer gains the positive part of the opponent's `effective - base`. A
    /// derived-stats fold like `BuffSkill`, never executed. schema v46
    MirrorOpponentIncrease,
    AddText {
        name_contains: Vec<String>,
        effects: Vec<Effect>,
    },
    /// Add a chosen competitor's Gimmick to the actor's own (The SRG Boss — "add
    /// their Gimmick to yours"): append `effects` to the actor's competitor
    /// effects, so they become standing effects (and are suppressed together if
    /// the actor's gimmick is blanked). Authored under a `StartOfMatch` `Choice`
    /// whose branches carry each absorbable variant's baked IR; the engine has no
    /// card index, so the candidate gimmicks are baked, not resolved at runtime.
    AbsorbGimmick {
        effects: Vec<Effect>,
    },
    /// POISON/DOPING (srgpc): "Your opponent's **next** Grapple has the added text:
    /// 'If stopped, you lose the match via disqualification'" (the Madness trio).
    /// Attaches `effects` to the NEXT card `who` plays matching `selector`, then is
    /// consumed. Unlike [`Action::AddText`] — a continuous, gimmick-sourced,
    /// name-matched injection re-derived on every play — this is a ONE-SHOT queued on
    /// the target player (`PlayerState.pending_text`), so per the ruling it "stays
    /// active until fulfilled even if [the source is] removed from the board".
    /// Materialized onto the played card itself, so the added text also reaches the
    /// stop exchange (where `injected_text` never did). schema v40
    AddTextToNext {
        who: Who,
        selector: CardFilter,
        effects: Vec<Effect>,
    },
    Reroll {
        /// Whose die is re-rolled: `SelfSide` (your own — Dunn/Jay White) or `Opp`
        /// ("force your opponent to re-roll" — Reverend/Macho Manny). Overridden by
        /// `choose`.
        who: Who,
        once: bool,
        /// "Choose any player to re-roll": the owner picks which side re-rolls
        /// (overrides `who`). Grim Librarian.
        #[serde(default)]
        choose: bool,
        /// `This` re-rolls the current roll (structural, read in the roll-off);
        /// `Next` grants a one-shot re-roll for the owner's NEXT turn roll ("you
        /// may re-roll your next turn roll" — King Brian Cage / El Gato Shinobi).
        #[serde(default)]
        when: RollWhen,
        /// The payment required to re-roll (`None` = free). A `ShuffleInPlay` cost is
        /// offered only while a matching in-play card exists and shuffles one away (Mr.
        /// Hyde's "Potion"); a `BuryFromHand`/`DiscardFromHand` cost is offered only
        /// while the hand can pay and sheds `count` cards ("bury 4 cards in your hand to
        /// re-roll", "discard 1 Finish from your hand to re-roll"). schema v103
        #[serde(default)]
        cost: Option<RerollCost>,
        /// Scope: `false` (default) = the turn-roll off, offered in `offer_reroll`;
        /// `true` = the FINISH roll, offered in `offer_finish_reroll` inside the
        /// finish sequence ("you may re-roll your Finish roll" — 59 cards, e.g.
        /// Tomato Tomato Jr.). Keeps the two roll paths from cross-firing. schema v76
        #[serde(default)]
        finish: bool,
        /// Scope: `true` = a BREAKOUT roll, offered in `offer_breakout_reroll` inside
        /// the breakout loop ("re-roll your Breakout roll" / "force your opponent to
        /// re-roll their Breakout roll"). Mutually exclusive with `finish`; a bare
        /// `Reroll` (both `false`) is the turn roll. Both re-roll the DEFENDER's die —
        /// `who: SelfSide` means the effect owner IS the defender, `who: Opp` means the
        /// finisher forces the defender to re-roll. schema v102
        #[serde(default)]
        breakout: bool,
    },
    /// "When you roll `from` for your turn roll or Finish roll, you may switch it to
    /// `to`" (Scott Prime V1/V2). Read structurally in BOTH roll paths (the turn
    /// roll-off and the Finish roll), a no-op in `apply_action`; fires when the
    /// rolled skill == `from`. The "you may" lives on the [`Effect::optional`] flag.
    /// A switched turn die keeps its roll mods (value is recomputed on `to`'s stat);
    /// a switched Finish die recomputes base + combo from `to`.
    SwitchRolledSkill {
        from_skill: Skill,
        to: Skill,
    },
    WinTie {
        who: Who,
    },
    Bump {
        who: Who,
    },
    ElectBumpOnSameSkill {
        uses: i64,
    },
    /// A persistent, FORCED same-skill bump when the owner would LOSE the turn roll
    /// (Brock Smith V1's start-of-match gimmick: "when you and that opponent roll the
    /// same skill and they would win the turn roll, bump instead"). Unlike the elective
    /// [`Action::ElectBumpOnSameSkill`] (a per-match charge the owner MAY spend on any
    /// same-skill roll), this is unlimited and mandatory, but only fires on a
    /// same-skill roll the owner is losing — the exact case the card converts into a
    /// bump. The "choose an opponent" targeting is the sole opponent in a 1v1 match.
    /// schema v151
    BumpInsteadOnSameSkillLoss,
    Stop {
        order: Option<PlayOrder>,
        atk_type: Option<AtkType>,
        source_is_skillreq: bool,
        /// "Stop any Finish Strike that cannot be stopped" / "… even if it cannot be
        /// stopped" — this Stop bypasses the attack's own `Unstoppable` declaration,
        /// answering an otherwise-unstoppable finisher. Read in `card_can_stop`.
        /// schema v63
        #[serde(default)]
        even_unstoppable: bool,
        /// Extra constraint on the stopped attack beyond `order`/`atk_type` — "Stop
        /// any Submission with \"Over the Top\" in the name" / "… with \"X\" in the
        /// text". Only `name_contains`/`text_contains` are set here (order/type stay
        /// on the flat fields); matched via `card_matches` in `stop_matches_for`.
        /// `None` = no extra filter. schema v66
        #[serde(default)]
        target: Option<CardFilter>,
        /// "Stop any Finish Strike that is also a Lead [or a Follow Up]" — the stopped
        /// attack must ALSO count as one of these play orders via an `AlsoLead`
        /// declaration whose condition currently holds (a multi-order card, e.g. a printed
        /// Finish that is "also a Lead"). Empty = no such constraint. AND-ed with
        /// `order`/`atk_type`/`target` in `stop_matches_for`. schema v146
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        also_order: Vec<PlayOrder>,
    },
    StopRequiresTag {
        tag: String,
        /// The gate is ALSO satisfied when the stopped attack is itself unstoppable —
        /// "Stop any Finish Submission that has a Spotlight OR that cannot be stopped":
        /// the tag requirement is OR-ed with unstoppability rather than being a hard
        /// AND. Pair with `Stop.even_unstoppable` so the stop can actually catch the
        /// unstoppable case. Read in `card_can_stop`. schema v137
        #[serde(default, skip_serializing_if = "is_false")]
        or_unstoppable: bool,
    },
    BlankGimmick {
        who: Who,
        duration: Duration,
    },
    FlipGimmick {
        who: Who,
    },
    BlankText {
        selector: CardFilter,
        who: Who,
        /// Restrict the blank to cards SITTING IN the target's discard pile — "cards in
        /// your opponent's discard pile have blank text" (neutralises their WhileInDiscard
        /// abilities). `false` = blank the matching card wherever it is (the in-play
        /// Spotlight-blank form). Additive/skip-when-false, so pre-v116 fixtures round-trip
        /// byte-identically. schema v116
        #[serde(default, skip_serializing_if = "is_false")]
        discard_only: bool,
    },
    /// REST-OF-MATCH ("poison") text blank — "Blank all Spotlights for the rest of the
    /// match" (ee0defe5). Unlike the standing [`Action::BlankText`] (re-derived each read
    /// from the source card's presence), this is EXECUTED once when its effect fires: it
    /// resolves `who` to the absolute target and stamps a `(selector, owner)` entry into
    /// `GameState.permanent_blanks`, which persists for the rest of the match — surviving
    /// the source leaving play and catching matching cards played later. "All" (both
    /// boards) is two clauses, `who: SELF` + `who: OPP`. schema v139
    BlankTextPermanent {
        selector: CardFilter,
        who: Who,
    },
    /// "Un-blank your Finishes." — the inverse of [`Action::BlankText`]: a one-shot that
    /// RESTORES the text of `who`'s cards matching `selector`, overriding any blank on
    /// them for the rest of the match (the 6 Splash / "your opponent buries … un-blank
    /// your Finishes" Followups; `who` is always `SelfSide`, `selector` the Finish
    /// order). The engine records the `selector` in `PlayerState.text_unblank`, which
    /// [`GameState::is_text_blanked`](crate::state::GameState) consults FIRST — an
    /// un-blank wins over every blank source (a continuous `BlankText`, a stop's
    /// per-identity `blanked_text`). Duration is the rest of the match: a played card
    /// with no stated end, and the blank it counters is a standing opponent declaration,
    /// so the override must persist to be useful. schema v117
    Unblank {
        selector: CardFilter,
        who: Who,
    },
    /// "This card copies the text of …" — the Spotlight text-copy family (#2 "A Trip
    /// to the Upside Down", #9 "The D-Roll", #16 "Your Worst Nightmare!"). A passive
    /// marker read (never executed) by [`GameState::copied_effects`](crate::state::GameState):
    /// the effects of every card matching `selector` in `who`'s `zone` are re-homed
    /// onto the copier and fire for as long as this clause's own duration is active
    /// (a `WhileInDiscard` copy projects them from the copier's discard pile),
    /// regardless of the source effect's original duration — the copier becomes the
    /// new `self`. `copy_tags` also grafts the source's tags (its "Spotlight-ness")
    /// onto the copier (#16 only; #2/#9 set `false`). "…then blanks them" (#2) is a
    /// *separate* [`Action::BlankText`] clause against the same selector, not a flag
    /// here. Bounded against copy→copy recursion by `copy_guard`. schema v71
    CopyText {
        selector: CardFilter,
        who: Who,
        zone: CountZone,
        copy_tags: bool,
    },
    /// "The stopped card has blank text until the end of the turn" — blank the text of
    /// the specific card instance that was JUST stopped, for the rest of the turn (21
    /// cards; the Jurassic / "If Stopped" stop-card family). Unlike [`Action::BlankText`],
    /// which is a continuous selector-driven scan re-derived from the board, this
    /// blanks ONE card by identity and is held in `GameState.blanked_text` until the
    /// turn-boundary sweep — the stop card stays in play afterwards, so a continuous
    /// blank would never end. Fired from the stop card's `OnStop`; resolved BEFORE the
    /// stopped card's own `OnStop`, so it suppresses that card's "If Stopped" text
    /// (which is the entire point of the family — several members read "stop any card
    /// with 'If Stopped' in the text: that card has blank text …"). schema v36
    BlankStoppedText,
    /// "… that card has blank text" — blank the card that JUST triggered this `OnHit`
    /// (the hit referent `GameState.hit_card`), by identity, for as long as it stays IN
    /// PLAY (the card was just hit, so it is now on the board; the blank self-expires when
    /// it leaves play). Jax, Pet of the Year: "when your opponent hits a card with \"…\" in
    /// the name, that card has blank text and their next turn roll is -1". Authored on an
    /// `OnHit{who:Opp, name_contains}` gimmick effect (a blanked gimmick never reaches
    /// here). Stamps `GameState.blanked_in_play`. schema v148
    BlankHitCard,
    /// "If played as a Stop, this card is also a Finish" — the FINISH-OFF-STOP marker.
    /// A stop card carrying this runs a full finish sequence (finish roll → opponent
    /// breakout roll → win/resume) off a SUCCESSFUL stop, with the stopper as the
    /// finisher and the stopped attacker as the target. A passive marker: read in
    /// `apply_stop` (`maybe_finish_off_stop`), never executed as a mutation. Authored on
    /// an `OnStop{Theirs}` effect whose condition carries the gate — `Always` for "if
    /// played as a Stop", or `StoppedCardNoLogoNoReq` for "if the stopped card had no
    /// competitor logo or skill requirement". A sibling `CrowdMeter` action in the same
    /// effect handles the "the Crowd Meter is +N and …" variants. schema v145
    FinishIfStop,
    /// "… and end the current turn" — end the ACTIVE player's turn immediately (Boot Off
    /// the Apron / Capture Headlock / Take You for a Ride, on stopping a "Double Team"
    /// card). Executed: sets the active player's `turn_ended` flag, which the turn loop's
    /// extra-play loop honours (cancelling any remaining `PlayExtraCard` grants). Authored
    /// on an `OnStop{Theirs}` effect so it fires when this card stops. schema v147
    EndTurn,
    /// "Choose 1: "Kendo Stick", "Steel Chair", or "Trash Can"" (Raven) — bind ONE of
    /// `options` for the rest of the match, stored as `PlayerState.chosen_name`.
    /// Authored under `StartOfMatch`; the binding is then read by
    /// [`Condition::ChosenNameIs`] to gate the sibling effects that reference "that"
    /// name. A no-op if `options` is empty. schema v37
    ChooseName {
        options: Vec<String>,
    },
    /// "Choose a skill: …" (Catch These Hands) — the owner binds ONE of the six skills
    /// for the rest of the match, stored as `PlayerState.chosen_skill`. Read by
    /// [`Action::BuffSkill`]'s `target_chosen` (the debuff on "your opponent's skill of
    /// that type") and by [`Action::RollDrawChosen`] ("the next time you roll that
    /// skill"). Authored on the card's OnHit; a no-op if already bound. schema v150
    ChooseSkill,
    /// "The next time you roll that skill draw 1 card" (Catch These Hands) — arms a
    /// PERSISTENT one-shot draw keyed to the owner's `chosen_skill`: the next time `who`
    /// (SelfSide) rolls that skill for a turn roll, the owner draws `count`, and it is
    /// then consumed. Unlike [`Action::RollDraw`] it does NOT fizzle on a non-matching
    /// roll — it waits until the chosen skill comes up. A no-op if no skill is bound.
    /// schema v150
    RollDrawChosen {
        who: Who,
        count: i64,
    },
    LoseBy {
        kind: LoseKind,
        who: Who,
    },
    /// A Static match-rule toggle: `enabled=false` = "no disqualifications",
    /// `enabled=true` re-enables them. `scope` is who it reaches (see [`DqScope`]).
    /// Read at the disqualification-loss point, not executed.
    DisqualificationRule {
        enabled: bool,
        scope: DqScope,
    },
    /// A Static match-rule toggle for count-out losses: `enabled=false` = "no count
    /// outs" (a player emptying deck+hand no longer loses/wins by count-out), a
    /// standing rule several Crowd Meter match types impose (No DQ / Submission /
    /// Psycho Circus / Liger's Den). `scope` reuses [`DqScope`] (Match = every
    /// player; SelfSide = only the owner). Read at the count-out point in
    /// `draw_for_turn`, never executed as a mutation. schema v59
    CountOutRule {
        enabled: bool,
        scope: DqScope,
    },
    /// A Static poison: while the declaring card sits in play/discard, the `who`-side
    /// (Bleeding Out: `Opp` = "an opponent") must resolve every card-/Gimmick-driven
    /// move of a card OUT of their OWN discard pile RANDOMLY, losing the normal free
    /// choice of which card to recur. Read at the discard-move choice sites
    /// (`bury_from_discard`, `act_add_from_discard`) via `GameState::
    /// force_random_discard_move`, never executed as a mutation. schema v131
    ForceRandomDiscardMove {
        who: Who,
    },
    /// A Static poison: while the declaring card sits in play/discard, an OPPONENT
    /// cannot move ANY card out of the `who`-side's discard pile (Split Personality:
    /// "your opponent cannot move other cards from your discard pile", `who = SelfSide`
    /// = the owner's own pile). Read at the discard-move choice site (`bury_from_discard`,
    /// the only path that reaches the OTHER player's pile) via `GameState::
    /// discard_move_locked`, never executed as a mutation. Distinct from
    /// [`Action::ForceRandomDiscardMove`], which merely randomises the choice. schema v132
    LockDiscard {
        who: Who,
    },
    /// Install a Crowd Meter match-type's standing rules (GM Calace V1: "replace all
    /// Crowd Meter cards with … Steel Cage / Psycho Circus / Lumberjack / No DQ /
    /// Submission"). Appends `effects` to the owner's **Entrance** effects so they are
    /// always-active — a global match condition that survives the owner's gimmick
    /// being blanked (unlike [`Action::AbsorbGimmick`], which installs into the
    /// blankable competitor gimmick). `name` labels the swapped-in match type in the
    /// log. Authored under a `StartOfMatch` `Choice`; clauses the engine cannot yet
    /// model are carried as explicit `Unsupported` sub-effects. schema v59
    SwapCrowdMeter {
        name: String,
        effects: Vec<Effect>,
    },
    /// A Static meta-comparison override "for card effects": the declaring player's
    /// `domain` comparison vs the opponent always resolves as `order` regardless of
    /// the real values (RaRa Perre "skills considered higher"; Theo V2 "considered
    /// fewer cards in hand"). Read in `conditions::holds`, not executed.
    ConsideredCompare {
        domain: CompareDomain,
        order: CompareOrder,
    },
    /// A Static declaration: "your opponent does not draw for your card effects"
    /// (Sami "The Draw" Callihan). Read at `act_draw` — a `Draw{who=OPP}` resolved by
    /// the declaring player is voided. Not executed as a mutation.
    SuppressOpponentDraw,
    /// The mirror declaration: "you do not bury or discard cards from your hand for
    /// your OWN card effects" (Sami "Death Machine" V2; one branch of Sami WR's
    /// start-of-match choice). Read at the two hand-loss chokepoints — `act_bury`'s
    /// `BuryFrom::Hand` branch and `act_discard` — and only when the declaring player
    /// is BOTH the effect's owner and the one losing cards, so an opponent's effect
    /// still takes them. Not executed as a mutation. schema v42
    SuppressSelfHandLoss,
    /// Static declaration that on a BUMP the declarer's opponent discards 1 card
    /// instead of drawing (Mack-a-Tack: "when you bump, your opponent discards 1 card
    /// instead of drawing"). Read in `do_bump`, never executed. schema v50
    BumpDrawReplace,
    /// Static declaration that, `uses` times per match, the declarer MAY replace a
    /// bump they would take with drawing `draw` cards and re-rolling both turn rolls
    /// (Pretty Paul Says "Let It Rip!": "Once per match: When you would bump, draw 2
    /// cards instead and each player re-rolls their turn roll"). Read structurally in
    /// `roll_off` (`try_bump_replacement`), never executed: the bump is *replaced*, so
    /// neither side's `OnBump` gimmick fires and the turn is not counted as bumped —
    /// the whole point when a sign-flipper (Cassandra) has turned the owner's own
    /// bump-punish against them. The per-match charge is tracked in `freq_counters`
    /// under `match:bump_replace` (like `ElectBumpOnSameSkill`'s `uses`), not via the
    /// frequency guard. schema v73
    BumpReplacement {
        uses: i64,
        draw: i64,
    },
    /// Static declaration that multiplies every number in the owner's Entrance card's
    /// effects by `factor`, when the entrance name matches `name_contains` (Pedro
    /// Valiant: "triple the numbers in the text of your Entrance cards with 'Training
    /// with' in the name"). Applied to the entrance effects in `gimmick_standing_effects`
    /// (like Cassandra's sign-flip), never executed. Inert while the matching entrances
    /// parse to `Unsupported`; forward-compatible when they are modeled. schema v53
    ScaleEntranceNumbers {
        name_contains: Vec<String>,
        factor: i64,
    },
    CrowdMeter {
        delta: i64,
    },
    PlayExtraCard {
        order: Option<PlayOrder>,
    },
    SetFinishRoll {
        value: i64,
        condition: Condition,
    },
    FinishBonus {
        skill: Skill,
        delta: i64,
    },
    FinishRollBonus {
        delta: i64,
        when_skill: Option<Skill>,
        either: bool,
        /// Base-roll gate: the bonus applies only when the BASE Finish roll (the
        /// rolled skill's stat, BEFORE combo/gimmick/Crowd-Meter bonuses) is
        /// `<= when_base_le` and/or `>= when_base_ge` — "If your Finish roll is 6 or
        /// less, it is +2". `None` = ungated. schema v61
        #[serde(default)]
        when_base_le: Option<i64>,
        #[serde(default)]
        when_base_ge: Option<i64>,
        /// When set, the bonus is `delta * (count of `per_who`'s cards in `per_zone`
        /// matching this filter)` — "your Finish roll is +1 for each Spotlight you
        /// have in play / in your opponent's discard pile". `None` = flat `delta`.
        #[serde(default)]
        per: Option<CardFilter>,
        #[serde(default)]
        per_who: Who,
        #[serde(default)]
        per_zone: CountZone,
        /// Integer divisor on the per-count before scaling by `delta` — the count is
        /// `floor(matches / per_divisor)`. `None`/`Some(1)` = one bonus per match;
        /// `Some(3)` = "your Finish roll is +1 for every 3 Strikes you have in play"
        /// (The Ride Along). Only meaningful with `per` set. schema v74
        #[serde(default)]
        per_divisor: Option<i64>,
        /// Clamps the per-count product ("… (Max +2)") — the `per`-scaled bonus never
        /// exceeds `cap`. `None` = uncapped. Additive/skip-when-none. schema v106
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cap: Option<i64>,
        /// Exclude the SOURCE card from the `per` count — "for each OTHER `<X>` you have
        /// in play", the FinishRollBonus analogue of `BuffSkill.per_excludes_self`.
        /// Additive/skip-when-false. schema v106
        #[serde(default, skip_serializing_if = "is_false")]
        per_excludes_self: bool,
        /// Dynamic delta = the Crowd Meter (clamped to `cap`), added ON TOP of the Crowd
        /// Meter the finish math already folds into every roll — "Your Finish roll is + the
        /// Crowd Meter (Max +N)", a SECOND crowd-meter addend. Mutually exclusive with the
        /// flat `delta` / `per` count. `finish_bonus_from` reads the live Crowd Meter each
        /// roll. Additive/skip-when-false, so pre-`per_crowd` fixtures round-trip.
        /// schema v123
        #[serde(default, skip_serializing_if = "is_false")]
        per_crowd: bool,
    },
    /// A standing bonus to the owner's TURN roll, applied only when the randomly
    /// rolled skill equals `skill`: "Your Power is +N during turn rolls." Read by
    /// `turn_roll_bonus` in the roll-off — the parallel of [`Action::FinishRollBonus`]
    /// / [`Action::BreakoutModifier`] for the turn roll — and never executed as a
    /// mutation. Because it lives in the turn-roll phase, it does NOT touch finish
    /// rolls, stops, or skill comparisons the way a plain `BuffSkill` would. schema v97
    TurnRollBonus {
        skill: Skill,
        delta: i64,
        /// Whose turn roll this modifies, from the OWNER's point of view. `SelfSide`
        /// (the default) = the owner's own turn roll ("your Power is +N during turn
        /// rolls"); `Opp` = the owner's opponent's ("your opponent's Power is -N during
        /// their turn rolls"). Read by `turn_roll_bonus`, which sums a roller's own
        /// `SelfSide` mods with their opponent's `Opp` mods. Skip-when-`SelfSide`, so
        /// pre-`who` fixtures round-trip byte-identical. schema v122
        #[serde(default, skip_serializing_if = "is_self_who")]
        who: Who,
        /// Symmetric modifier: when set, the bonus applies to WHOEVER rolls `skill`
        /// for their turn roll, not just the owner — "if either player rolls Power for
        /// their turn roll, their roll is +1". `turn_roll_bonus` picks up an
        /// `either` bonus from the opponent's board too. Additive/skip-when-false.
        /// schema v107
        #[serde(default, skip_serializing_if = "is_false")]
        either: bool,
        /// Dynamic delta = the Crowd Meter (clamped to `cap`), instead of the flat
        /// `delta` — "your Technique is + the Crowd Meter (Max +3) during your turn
        /// roll" (the roll-off parallel of a `per_crowd` [`Action::BuffSkill`]). A
        /// turn-roll-scoped skill mod that must NOT leak into `effective_stats` (finish
        /// rolls, skill requirements, comparisons), so it rides `TurnRollBonus` rather
        /// than a full-time buff. `turn_roll_bonus` reads the live Crowd Meter each
        /// roll-off. Additive/skip-when-false, so pre-v118 fixtures round-trip.
        /// schema v118
        #[serde(default, skip_serializing_if = "is_false")]
        per_crowd: bool,
        /// Clamps the `per_crowd` delta ("Max +N"). `None` = uncapped. Ignored when
        /// `per_crowd` is false. schema v118
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cap: Option<i64>,
    },
    BreakoutModifier {
        delta: i64,
        attempts: Option<i64>,
        /// Skill gate on the breakout roll — the bonus applies only when the defender's
        /// rolled breakout skill equals `when_skill` ("+1 to Strike during your breakout
        /// rolls", Pineapple; "Power is +1 during your breakout rolls", The SRG Boss V3).
        /// `None` = every breakout roll regardless of the rolled skill. schema v79
        #[serde(default)]
        when_skill: Option<Skill>,
        /// Whose breakout rolls this modifies, from the OWNER's point of view. `SelfSide`
        /// (the default) = the owner's own breakout rolls ("your breakout rolls are +N");
        /// `Opp` = the owner's opponent's ("your opponent's breakout rolls are -N"). Read
        /// by `breakout_bonus`, which sums a defender's own `SelfSide` mods with their
        /// opponent's `Opp` mods. schema v94
        #[serde(default)]
        who: Who,
        /// Symmetric modifier: when set, the bonus applies to WHOEVER is rolling the
        /// breakout (the defender), regardless of `who` or which board it sits on — "if
        /// either player rolls Agility for their breakout roll, their roll is -1".
        /// `breakout_mods_from` admits an `either` mod on top of the `who` match.
        /// Additive/skip-when-false. schema v107
        #[serde(default, skip_serializing_if = "is_false")]
        either: bool,
        /// When set, `delta` is scaled by `count of `per_who`'s cards in `per_zone`
        /// matching this filter` — "your opponent's breakout rolls are +1 for each Stop
        /// they have in play", the `BreakoutModifier` analogue of
        /// [`Action::FinishRollBonus::per`]. `None` = flat `delta`. All the per-count
        /// fields are additive/skip-when-default so pre-per breakout fixtures stay
        /// byte-identical. schema v112
        #[serde(default, skip_serializing_if = "Option::is_none")]
        per: Option<CardFilter>,
        #[serde(default, skip_serializing_if = "is_self_who")]
        per_who: Who,
        #[serde(default, skip_serializing_if = "is_in_play_zone")]
        per_zone: CountZone,
        /// Integer divisor on the per-count before scaling by `delta` (the count is
        /// `floor(matches / per_divisor)`); `None`/`Some(1)` = one bonus per match.
        /// schema v112
        #[serde(default, skip_serializing_if = "Option::is_none")]
        per_divisor: Option<i64>,
        /// Clamps the per-count product ("… (Max +M)") — the `per`-scaled bonus never
        /// exceeds `cap`. `None` = uncapped. schema v112
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cap: Option<i64>,
        /// Exclude the SOURCE card from the `per` count — "for each OTHER `<X>` you have
        /// in play". schema v112
        #[serde(default, skip_serializing_if = "is_false")]
        per_excludes_self: bool,
    },
    /// Grant the actor a TIMED, imperative breakout-roll bonus of `delta`, swept at the
    /// end of the turn — "add +1 to your breakout rolls until the end of the turn" (The
    /// Mailman Always Delivers). Unlike [`Action::BreakoutModifier`] (a Static bonus read
    /// off an in-play card), this accumulates onto the actor's `breakout_bonus_eot` store
    /// and so survives the SOURCE card leaving play — needed because Mailman shuffles
    /// itself away as it grants the bonus. `breakout_bonus` adds the store for the
    /// defender. `who` names WHOSE breakout rolls it lands on from the actor's POV:
    /// `SelfSide` (the default, Mailman) = the actor's own; `Opp` = "your opponent's
    /// breakout rolls are -N" (Shattered Split's Why So Serious?!?, revealed as a Strike).
    /// schema v132
    GrantBreakoutBonus {
        delta: i64,
        #[serde(default, skip_serializing_if = "is_self_who")]
        who: Who,
    },
    /// Modifies the NUMBER of breakout attempts (rolls) the affected player gets this
    /// turn — the "reduced / extra breakout rolls" family, distinct from
    /// [`Action::BreakoutModifier`] (which shifts a roll's VALUE, not the count). `set`
    /// overrides the base `BREAKOUT_ATTEMPTS` ("your opponent gets 2 Breakout rolls this
    /// turn"); `delta` shifts it ("gets 1 additional / 1 fewer Breakout roll"). `who`
    /// names the affected side from the OWNER's POV — `Opp` = "your opponent gets …",
    /// `SelfSide` = "you get …". Read by `breakout_attempts_for`, which sums both boards
    /// and clamps the result. schema v113
    BreakoutAttempts {
        /// Additive shift: +N "additional/more", -N "fewer". 0 when only `set` applies.
        delta: i64,
        /// Absolute override of the base attempt count ("gets N Breakout rolls"); `None`
        /// = shift the base by `delta` only. When several effects set, the smallest wins.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        set: Option<i64>,
        /// Affected side from the owner's POV: `SelfSide` = the owner's own breakout
        /// attempts, `Opp` = the owner's opponent's. Default `SelfSide`.
        #[serde(default, skip_serializing_if = "is_self_who")]
        who: Who,
        /// Per-count scaling of `delta` — "1 additional Breakout roll for each Skill
        /// Requirement card they have in play". Same machinery as
        /// [`Action::BreakoutModifier::per`]; all skip-when-default. schema v113
        #[serde(default, skip_serializing_if = "Option::is_none")]
        per: Option<CardFilter>,
        #[serde(default, skip_serializing_if = "is_self_who")]
        per_who: Who,
        #[serde(default, skip_serializing_if = "is_in_play_zone")]
        per_zone: CountZone,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        per_divisor: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cap: Option<i64>,
        #[serde(default, skip_serializing_if = "is_false")]
        per_excludes_self: bool,
    },
    LowestRollWins,
    FlipGimmickSigns {
        who: Who,
    },
    Unstoppable {
        by_order: Option<PlayOrder>,
        /// "Cannot be stopped by \"X\"" — unstoppable specifically against a stopper
        /// whose NAME equals this (AND-ed with `by_order`). `None` = no name gate.
        /// schema v64
        #[serde(default)]
        by_name: Option<String>,
        /// "Cannot be stopped by Skill Requirement cards" — unstoppable against a
        /// stopper that carries a skill requirement (AND-ed with the other gates).
        /// Authored on a main-deck card = this card; on a gimmick/entrance = every
        /// one of the owner's cards. schema v65
        #[serde(default)]
        by_skillreq: bool,
        /// "Your cards with \"X\" in the name cannot be stopped" — a player-scope
        /// declaration (gimmick/competitor/entrance) that only shields the owner's
        /// attacks whose NAME contains this substring; `None` = every card. Matched
        /// against the ATTACK, not the stopper (distinct from `by_name`). schema v152
        #[serde(default, skip_serializing_if = "Option::is_none")]
        applies_name: Option<String>,
        /// "Your cards cannot be stopped by …" (vs "This card …"): the shield covers
        /// EVERY one of the owner's cards, so the engine reads it even from an in-play
        /// main-deck source (Cat/Dog/Sheep Uprising's printed-Finish shield). A
        /// self-scope `Unstoppable` (`false`) only ever shields its own card and never
        /// leaks to siblings from in play. schema v153
        #[serde(default, skip_serializing_if = "is_false")]
        player_scope: bool,
    },
    AlsoLead {
        condition: Condition,
        /// Which play-order slot this card may ALSO be played in while `condition`
        /// holds. `Lead` (the default) = "this card is also a Lead"; `Followup` =
        /// "… also a Follow Up" (playable when a Lead is in play); `Finish` = "…
        /// also a Finish". Read in `also_playable_now`. schema v70
        #[serde(default)]
        order: PlayOrder,
    },
    /// Static stop-reframe (Jokerfish V2: "your opponent's Finishes are also Follow
    /// Ups for your Stop cards"). For the DECLARER-as-defender, an attack whose order
    /// is `attack_order` also satisfies a `Stop{order: as_order}`. Read in
    /// `card_can_stop`, never executed. schema v45
    StopCountsOrderAs {
        attack_order: PlayOrder,
        as_order: PlayOrder,
    },
    /// Static declaration that the declarer's OWN cards whose deck number is in
    /// `[number_min, number_max]` cannot act as Stops (Jokerfish V2: "your cards
    /// #19-21 cannot stop cards"). The rest of each card's text is unaffected — only
    /// its Stop ability is suppressed. Read in `card_can_stop`, never executed. schema v45
    SuppressStop {
        number_min: i64,
        number_max: i64,
    },
    /// A player-scope standing declaration that the DECLARER's Stops may stop an
    /// attack even when it "cannot be stopped" ("You can stop cards that cannot be
    /// stopped" — Pixel Palace Plancha / Throw Into the Turnbuckle / That's Cheesy
    /// Chinlock; JT Dunn). The per-`Stop` `even_unstoppable` flag says "THIS stop
    /// bypasses"; this node says "ALL of your stops bypass" while it is in play (or
    /// declared on a gimmick). Read in `card_can_stop` via `can_stop_unstoppable`,
    /// never executed. schema v154
    ///
    /// `only_order` narrows the bypass to attacks whose PRINTED play order matches
    /// ("Ignore any \"Cannot be stopped\" text on your opponent's Finish cards" —
    /// Pineapple/Trash Can/Sledgehammer Uprising); `None` is the original blanket
    /// enabler. schema v156
    CanStopUnstoppable {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        only_order: Option<PlayOrder>,
    },
    DoubleFinishIfBumped,
    /// Double this card's own printed Finish-roll bonuses when `condition` holds —
    /// the conditional generalization of [`Self::DoubleFinishIfBumped`] ("double
    /// these bonuses if you have another Submission in play / rolled Power / …";
    /// kenzie, king-cage, foxworthy, srg-boss). Read in `card_finish_bonus` against
    /// the owner's turn-roll context, never executed. schema v77
    DoubleFinishIf {
        condition: Condition,
    },
    /// This card can only be stopped by `count` Stops at once — the defender must
    /// commit `count` legal stop cards to stop it, or it lands (King Brian Cage's
    /// "This card will only be stopped by 2 Stops"). A Static self-effect read from
    /// the attack's own effects in `offer_stop`; never executed. schema v80
    RequireStops {
        count: i64,
    },
    /// This card ALSO counts as attack type `atk_type` in addition to its printed
    /// type — "This card is also a Finish Grapple" (King Brian Cage). A Static
    /// self-effect read via `Card::counts_as_atk_type` at every atk-type test
    /// (stop-matching, CardFilter, hit gimmicks); never executed. schema v81
    AlsoAtkType {
        atk_type: AtkType,
    },
    /// A DEFENDER declaration that the OPPONENT must have at least `count` cards of
    /// `kind` in their OWN play to LAND a Finish against you — D3 (V1)'s "your opponent
    /// needs 3 cards in play to hit you with a Finish" (`Cards`, 3). A Static effect
    /// read in `playable_options` (so Stops, which bypass play restrictions, stay
    /// exempt); never executed. On top of the built-in `FollowUps`×1 default. schema v125
    FinishRequires {
        kind: RequireKind,
        count: i64,
    },
    /// Look at `who`'s hand and move one chosen `selector`-matching card to the TOP of
    /// `who`'s deck — D3 (V1)'s Claw: "Look at your opponent's hand, choose 1 card and
    /// put it on top of their deck" (`who: Opp`). The actor picks (they've seen the
    /// hand); tempo/info denial (the target must redraw it). schema v126
    HandToDeckTop {
        who: Who,
        selector: CardFilter,
    },
    Choice {
        options: Vec<ChoiceOption>,
    },
    Unsupported {
        raw_text: String,
        reason: String,
    },
}

/// One labelled branch of a [`Action::Choice`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChoiceOption {
    #[serde(rename = "@type", default)]
    pub node_type: ChoiceOptionTag,
    pub label: String,
    pub actions: Vec<Action>,
}

// ---------------------------------------------------------------------------
// Effect — the compiled unit of card text
// ---------------------------------------------------------------------------

/// A single compiled clause: a trigger, a guard, and the actions it performs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Effect {
    #[serde(rename = "@type", default)]
    pub node_type: EffectTag,
    pub trigger: Trigger,
    pub condition: Condition,
    pub actions: Vec<Action>,
    pub duration: Duration,
    pub frequency: FrequencyGuard,
    pub raw_clause: String,
    pub source: EffectSource,
    pub optional: bool,
}

// ---------------------------------------------------------------------------
// IrNode — the top-level union (schema root `IRNode`)
// ---------------------------------------------------------------------------

/// Any IR node, tag-dispatched by `@type`. This is the schema root: it
/// round-trips an arbitrary node regardless of where it sits in the tree.
///
/// The sub-union enums ([`Trigger`], [`Condition`], [`Action`]) are the typed
/// slots used *inside* [`Effect`]; `IrNode` is the untyped envelope used when a
/// node's kind is not known ahead of time (e.g. reading `cards.ir.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "@type")]
#[allow(clippy::large_enum_variant)]
pub enum IrNode {
    // Structural
    Effect(Effect),
    CardFilter(CardFilter),
    ChoiceOption(ChoiceOption),
    FrequencyGuard(FrequencyGuard),
    RerollCost(RerollCost),

    // Triggers
    OnPlay,
    OnRoll {
        skill: Option<Skill>,
        who: Who,
    },
    /// Fires on a FINISH roll (not the turn roll-off) — "when you roll `skill` for
    /// your Finish roll" (The Man from I.T.). `who` follows the finisher like
    /// `OnRoll`'s does; the parser never emits it (override-only), so existing
    /// turn-roll `OnRoll` nodes stay untouched. schema v47
    OnFinishRoll {
        skill: Option<Skill>,
        who: Who,
    },
    /// Fires each time `who` has rolled EVERY skill in `skills` as a turn roll since the
    /// last firing (General Lee Wong V2: "each time you roll Power, Agility, and
    /// Technique for your turn rolls"). The engine accumulates the distinct rolled
    /// skills per effect and resets on fire. Override-only. schema v49
    OnRolledAll {
        skills: Vec<Skill>,
        #[serde(default)]
        who: Who,
    },
    InRoll {
        skill: Option<Skill>,
        who: Who,
        either: bool,
    },
    OnRollBoost {
        skill: Option<Skill>,
        delta: i64,
        on_bump: bool,
    },
    OnWinTurn,
    OnLoseTurn {
        by: Option<i64>,
    },
    OnStop {
        dir: Direction,
        /// When set, fires only if the **stopped** card's play order matches — "when
        /// your opponent stops your *Finish*" (La Fenix Super Lucha). `None` = any
        /// stopped card, the backward-compatible default (the parser's DQ/pinfall
        /// "if this is stopped" clauses and Gia's "when you Stop a card").
        #[serde(default)]
        order: Option<PlayOrder>,
    },
    OnHit {
        atk_type: Option<AtkType>,
        /// Case-insensitive OR-substring match on the **hit** card's title —
        /// "when you hit a card with 'X' (or 'Y') in the name". Empty = no name
        /// gate. Combines (AND) with `atk_type` when both are set.
        #[serde(default)]
        name_contains: Vec<String>,
        /// Same, against the hit card's rules text — "…with 'X' in the text".
        #[serde(default)]
        text_contains: Vec<String>,
        /// "When you hit a card" (any card, no gate) as a standing gimmick — fires on
        /// every hit (Bartholomew Hooke). Override-only; a bare parser OnHit leaves it
        /// false so misattributed fragments stay inert. See `run_hit_gimmicks`.
        #[serde(default)]
        on_any: bool,
        /// Play-order gate on the HIT card — "when you hit a **Lead**" (Sticky
        /// Sailboat, Asia, Chip Day; 22 cards). `None` = any order, the
        /// backward-compatible default. Combines (AND) with `atk_type` and the
        /// name/text gates, and counts as a gate for the bare-OnHit skip rule.
        /// schema v38
        #[serde(default)]
        order: Option<PlayOrder>,
        /// WHOSE hit fires this, from the owner's POV. `SelfSide` (the default, and
        /// every pre-v43 node) = "when YOU hit a card"; `Opp` = "after your OPPONENT
        /// hits a Follow Up" (El Super Hombre V2). Same scoping convention as
        /// [`Trigger::OnBreakout`] / [`Trigger::OnBury`]. schema v43
        #[serde(default)]
        who: Who,
        /// Dispatch this OnHit from the owner's HAND, not the board — a "reveal this
        /// card from your hand when your opponent hits <X>" reactive (The Mailman
        /// Always Delivers). `hand_self_triggers` scans hand cards carrying it and binds
        /// `self_card` so a self-referential body (`ShuffleSelfIntoDeck`) works. `false`
        /// (the default) = the ordinary in-play/gimmick standing OnHit. schema v128
        #[serde(default, skip_serializing_if = "is_false")]
        from_hand: bool,
    },
    OnBump,
    /// "When a card or Gimmick causes you to bury any number of cards" (The Cyclone
    /// V1) / "when you bury OR discard cards from your hand from a card effect or
    /// Gimmick" (Tommy Stillwell). Fires ONLY after an EFFECT-caused bury (`act_bury`)
    /// / effect-caused hand discard (`act_discard`) — never the mechanical pass-and-
    /// recycle (`do_pass`) or the hand-cap trim, which bypass those paths. `who` =
    /// whose bury fires it (SELF = "causes you"). `from_hand_only` limits to hand
    /// buries (Tommy); `also_discard` additionally fires on an effect-caused hand
    /// DISCARD (Tommy's "bury or discard"). Fires once per bury/discard event.
    OnBury {
        who: Who,
        #[serde(default)]
        from_hand_only: bool,
        #[serde(default)]
        also_discard: bool,
    },
    StartOfTurn,
    /// Fires for the NON-active player during the active player's turn — "once during
    /// your opponent's turn, you may …" (Memes Dealer V1). The mirror of `StartOfTurn`;
    /// offered once, at the opponent's turn start. Override-only. schema v52
    DuringOpponentTurn,
    StartOfMatch,
    OnBreakout {
        /// Whose breakout fires this: `None` = any breakout ("after a breakout" —
        /// Copy Kat V2); `Some(SelfSide)` = you broke out; `Some(Opp)` = your
        /// opponent broke out ("if your opponent breaks out" — the Spotlight recur).
        #[serde(default)]
        who: Option<Who>,
    },
    /// Fires for each of `who`'s breakout ROLLS (up to `BREAKOUT_ATTEMPTS` per finish),
    /// as each is made — distinct from [`Trigger::OnBreakout`], which fires once on a
    /// SUCCESSFUL breakout. `who` is read from the effect owner's POV: `Opp` = "your
    /// opponent's breakout roll" (the defender rolling against the owner's finish),
    /// `SelfSide` = the owner's own breakout roll. The rolled value/skill is exposed via
    /// the `RollContext`, so a `RollValue` / `RollWasSkill` condition gates on it ("if
    /// your opponent rolls 10 for their Breakout roll, you lose"). schema v72
    OnBreakoutRoll {
        who: Who,
        /// Ordinal gate on WHICH of the defender's breakout rolls fire this — the 1-based
        /// attempt numbers ("your opponent's 1st or 2nd breakout roll" -> `[1, 2]`; "their
        /// 3rd breakout roll" -> `[3]`). Empty (the default, every pre-v128 node) = every
        /// roll regardless of ordinal. schema v128
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attempts: Vec<i64>,
    },
    /// Fires when the `who`-side re-rolls their TURN roll (at the roll-off, after the
    /// re-rolled die lands). `who` from the owner's POV: `SelfSide` = "when you re-roll
    /// your turn roll", `Opp` = "when your opponent/target re-rolls their turn roll". A
    /// roll-modifier body ("their roll is -1", "your roll is +2") adjusts the re-rolled
    /// value; other bodies (draw, shuffle-self from discard) resolve normally. Fired by
    /// `run_on_reroll` at the `offer_rerolls` site. schema v104
    OnReroll {
        who: Who,
    },
    /// Fires when the `who`-side's deck is shuffled by a card/gimmick EFFECT (any
    /// effect-caused shuffle: explicit "shuffle your deck", or the incidental shuffle
    /// after a search/tutor/shuffle-into-deck/hand-into-deck). NOT the match-start
    /// setup shuffle, nor the private bury-ordering shuffle. `who` = whose shuffle
    /// fires it from the owner's POV (OPP = "when your opponent shuffles their deck" —
    /// Memes Dealer V2). Override-only.
    OnShuffle {
        who: Who,
    },
    /// Fires right after the `who`-side DRAWS one or more cards (`run_on_draw` at the
    /// `draw` chokepoint). `who = SelfSide` = "when you draw". Used by a WhileInDiscard
    /// recur gated on how many cards were drawn this turn — "when this card is in your
    /// discard pile, if you drew 1 or more cards this turn, you may add it to your hand"
    /// (The Gobstopper); `self_card` is bound so `AddSelfToHand` resurrects the source.
    /// schema v129
    OnDraw {
        who: Who,
    },
    /// Fires when the `who`-side flips one or more cards (`Flip` mills deck→discard).
    /// `count` = a size gate: `None` fires on any flip; `Some(n)` with `at_least = false`
    /// only on exactly `n` ("flip exactly 3 cards" — Evee Laveaux), with `at_least = true`
    /// on `n` or more ("flip 2 or more cards"). `who` follows the shuffle convention
    /// (SELF = you flipped).
    ///
    /// `on_self` splits the two intents that share this trigger: `true` = a per-card
    /// self-trigger ("if THIS card is flipped, …"), fired by `run_self_flips` for each
    /// just-flipped card carrying it; `false` = a standing trigger ("when YOU flip …"),
    /// fired by `run_on_flip` from in-play/gimmick effects. The split keeps a standing
    /// "when you flip" effect from firing merely because its own card was milled. schema v89
    OnFlip {
        who: Who,
        #[serde(default)]
        count: Option<i64>,
        #[serde(default)]
        at_least: bool,
        #[serde(default)]
        on_self: bool,
    },
    /// Fires when one or more cards LEAVE the `who`-side's discard pile because of a
    /// card/gimmick EFFECT — "when your opponent moves any number of cards from their
    /// discard pile with their card effect or Gimmick" (Brumeister V2). Covers every
    /// effect-driven exit: recur-to-hand, shuffle-into-deck, recur-to-deck-top, the
    /// hand/discard swap, and an effect-caused discard-pile bury. Fires ONCE per
    /// action, not per card ("any number of cards"). Deliberately NOT fired by the
    /// mechanical pass-and-recycle (`do_pass`), which is not a card effect. `who` is
    /// read as the owner of the PILE, from the effect owner's POV (OPP = "your
    /// opponent['s] discard pile"). Override-only.
    OnDiscardMove {
        who: Who,
    },
    /// Mirror of [`Trigger::OnCrowdMeterIncrease`]. schema v131
    OnCrowdMeterIncrease,
    /// Fires once at the START of every turn, before the roll-off (see the `Trigger`
    /// copy). Dispatched by `run_start_of_turn_triggers`. schema v140
    OnTurnStart,
    Static,

    // Conditions
    Always,
    And {
        items: Vec<Condition>,
    },
    Or {
        items: Vec<Condition>,
    },
    Not {
        item: Box<Condition>,
    },
    SkillCompare {
        skill: Skill,
        cmp: Comparator,
        who: Who,
        vs: Vs,
        value: Option<i64>,
        vs_skill: Option<Skill>,
    },
    HandSizeCompare {
        cmp: Comparator,
        vs: Vs,
        value: Option<i64>,
        who: Who,
    },
    CrowdMeterCompare {
        cmp: Comparator,
        value: i64,
    },
    /// `who`'s remaining deck size compared to `value` — "if you have 0 cards in your
    /// deck" (Foxworthy V3's finish double). Reads `PlayerState.deck.len()`. schema v82
    DeckSizeCompare {
        cmp: Comparator,
        value: i64,
        #[serde(default)]
        who: Who,
    },
    /// The match currently has no disqualifications — neither player can be DQ'd
    /// (`GameState.match_has_no_dq`). "If this match has No Disqualifications, your
    /// Finish roll is +1" (Cardona's Pizza Cutter; a 16-clause family). schema v83
    MatchHasNoDisqualifications,
    /// The current match is one of the listed stipulations ("if this is a Steel Cage
    /// or Liger's Den Match, …"). Holds iff `GameState.match_type` is in `types`; a
    /// disjunction over the OR-joined keywords. A 156-clause gate family. schema v92
    IsMatchType {
        types: Vec<MatchType>,
    },
    HasInPlay {
        who: Who,
        filter: CardFilter,
        count: i64,
        cmp: Comparator,
    },
    HasInHand {
        who: Who,
        filter: CardFilter,
        count: i64,
    },
    HasInDiscard {
        who: Who,
        filter: CardFilter,
        /// How many matching cards the discard pile must hold — "if you have `count`
        /// Finishes in your discard pile" (Fortress's Tower of Strength: count 2). Defaults
        /// to 1 ("has ≥1"), so the boolean forms neither carry nor churn it. schema v136
        #[serde(default = "one_i64", skip_serializing_if = "is_one_i64")]
        count: i64,
    },
    /// Cross-board in-play count compare: `who`'s count of cards in play matching
    /// `filter` compared (`cmp`) against `vs_who`'s count of the same filter. "When
    /// your target has more Strikes in play [than you]" (Snake Pitt V3): `who=OPP`,
    /// `vs_who=SELF`, `cmp=">"`, filter `atk_type=Strike`. Honors `CountsAsInPlay`
    /// on both boards (via `count_in_play`).
    InPlayCompare {
        filter: CardFilter,
        cmp: Comparator,
        who: Who,
        vs_who: Who,
    },
    /// True while `who`'s [`Action::ChooseName`] binding equals `name` — the gate that
    /// turns "when you hit a card with THAT in the name" into one concrete effect per
    /// option (Raven). Case-sensitive equality against the stored binding; false when
    /// nothing has been chosen yet. schema v37
    ChosenNameIs {
        name: String,
        who: Who,
    },
    RollWasSkill {
        skill: Skill,
        /// Whose turn-roll skill this checks. `SELF` (default) = the owner's rolled
        /// skill; `OPP` reads the other side's skill from the roll context's
        /// `opp_skill`. Composed under And/Or, this expresses "if **both** players
        /// rolled X" / "if **either** player rolled X for their turn roll" (Tomato
        /// Tomato Jr.). schema v75
        #[serde(default)]
        who: Who,
    },
    RollGapExactly {
        k: i64,
    },
    RollGapAtLeast {
        k: i64,
    },
    /// The owner rolled at least `k` *higher* than the opponent — mirror of
    /// `RollGapAtLeast` (owner `k` lower). A lead of `k` is `gap <= -k`.
    RollLeadAtLeast {
        k: i64,
    },
    RollValue {
        cmp: Comparator,
        value: i64,
        /// Whose turn-roll VALUE (die + stat + mods) to compare. `SelfSide` (the
        /// default) = "you rolled N for your turn roll"; `Opp` = "your opponent's turn
        /// roll is N" (Scott Prime's The Loaded Glove — a 12-clause family of
        /// opp-turn-roll-value gates). The opponent's value is read from the actor's
        /// [`RollContext`] as `value + gap` (`gap` = opp − self). schema v130
        #[serde(default, skip_serializing_if = "is_self_who")]
        who: Who,
    },
    /// The rolled skill's **printed** (base, unbuffed) stat on the `who`-side's
    /// competitor equals `value` — "when your opponent rolls their printed 8 skill"
    /// (Collin the Chrononaut). Needs a roll context; `who` picks whose printed stat
    /// to read (the roller), following the trigger's `who` like `RollValue`.
    PrintedRollValue {
        who: Who,
        value: i64,
    },
    /// The owner and their target rolled the **same skill** for this turn-roll (Hex,
    /// Nic Nemeth). Reads the post-roll context's `skill` vs `opp_skill`; needs a
    /// roll context (false without one, and in single-sided re-roll/switch contexts).
    SameRolledSkill,
    /// This is the first turn of the game (`GameState.turn_no <= 1`). Gates the "if this is
    /// the first turn of the game, …" riders. schema v119
    FirstTurn,
    /// The card the owner most recently stopped had neither a competitor logo nor a skill
    /// requirement (`Logoless` tag AND no `SkillRequirement`). Read from
    /// `flags["stopped_card_no_logo_no_req"]`, stamped by `apply_stop`. schema v144
    StoppedCardNoLogoNoReq,
    /// The owner's opponent won the *previous* turn's roll-off
    /// (`GameState.last_roll_winner`); false before turn 1. Gates Dunn's re-roll.
    OppWonLastRoll,
    /// The PREVIOUS turn's roll-off bumped (`GameState.last_turn_bumped`); false before
    /// turn 1. Gates Mack-a-Tack's "if you bumped on the last turn roll" re-roll.
    BumpedLastTurnRoll,
    /// `who` ended the previous turn without playing a card (roll-off winner on
    /// `turn_no - 1` who passed). Reads `flags["last_pass_turn"]`. `who` defaults SELF
    /// (skip-when-self). schema v78; `who` added v140
    EndedTurnNoPlay {
        #[serde(default, skip_serializing_if = "is_self_who")]
        who: Who,
    },
    /// `who` buried a Spotlight card on the previous turn. Reads
    /// `flags["buried_spotlight_turn"]`. schema v140
    BuriedSpotlightLastTurn {
        who: Who,
    },
    /// `who` broke out on the previous turn (defender of a Finish on `turn_no - 1` who
    /// survived every Breakout roll). Reads `flags["broke_out_turn"]`. schema v120
    BrokeOutLastTurn {
        who: Who,
    },
    /// `who` performed a stop last turn (`last_turn = true`) or this turn. Reads
    /// `flags["stopped_card_turn"]`, stamped by `apply_stop`. schema v121
    StoppedCard {
        who: Who,
        last_turn: bool,
    },
    /// The owner re-rolled their turn roll this turn. Reads `flags["rerolled_turn"]`,
    /// stamped in `offer_rerolls`. Gates King Brian Cage's finish riders. schema v80
    RerolledTurnRoll,
    /// The flip currently resolving was caused by a Gimmick-source effect. schema v87
    FlippedForGimmick,
    /// The flip currently resolving was caused by a card whose name contains one of
    /// `names` (case-insensitive OR-substring). schema v87
    FlippedByName {
        names: Vec<String>,
    },
    GimmickFlipped {
        who: Who,
    },
    DuringTurn {
        who: Who,
    },
    CompetitorIs {
        name_contains: Vec<String>,
    },
    HitThisTurn {
        who: Who,
    },
    DrewThisTurn {
        #[serde(default)]
        who: Who,
        at_least: i64,
    },
    /// `who` has LOST at least `at_least` turn rolls IN A ROW — "if you lose 2 Turn Rolls
    /// in a row, …" (Me Against the World's discard recur). Reads
    /// `PlayerState.turn_losses_in_a_row`, incremented on a turn-roll loss and reset on a
    /// win. schema v134
    LostTurnRollsInARow {
        #[serde(default)]
        who: Who,
        at_least: i64,
    },
    HitCard {
        filter: CardFilter,
        #[serde(default)]
        who: Who,
        #[serde(default)]
        last_turn: bool,
    },

    // Actions
    Draw {
        n: i64,
        source: DeckEnd,
        who: Who,
        per: Option<CardFilter>,
        per_who: Who,
        /// Clamps the per-count product — "draw 1 card for each … (Max 3)". Ignored
        /// without `per`. schema v38
        #[serde(default)]
        cap: Option<i64>,
        /// Drop the card that TRIGGERED this effect from the `per` count — "for each
        /// **other** Lead you have in play". Needed only when the trigger puts the
        /// card on the board before firing (an `OnHit` gimmick; `run_hit_gimmicks`
        /// runs after the hit card is in play). The usual "each other" clause is
        /// authored `OnPlay`, where the source is not yet on the board and no
        /// exclusion is needed, so this defaults false. schema v38
        #[serde(default)]
        per_excludes_trigger: bool,
        /// Count is the Crowd Meter (plus `n` as a signed offset), clamped to `cap` —
        /// "draw cards equal to the Crowd Meter", "… equal to the Crowd Meter +1 (Max
        /// +5)". Mutually exclusive with `per`. `n` is the offset here, not a flat count.
        /// Additive/skip-when-false. schema v108
        #[serde(default, skip_serializing_if = "is_false")]
        from_crowd: bool,
    },
    Bury {
        selector: CardFilter,
        count: i64,
        who: Who,
        random: bool,
        #[serde(default)]
        source: BuryFrom,
        /// `BuryFrom::Discard` only: the actor picks WHICH card, from EITHER player's
        /// discard pile — "bury 1 card in any player's discard pile" (Cherry
        /// Glamazon). The default discard bury is the mechanical pass-and-recycle,
        /// which takes the top `count` and ignores `selector`; this one is a targeted
        /// choice (it can deny a specific recursion target). `who` is ignored when
        /// set, and the card returns to ITS OWNER's deck bottom. schema v39
        #[serde(default)]
        choose: bool,
        /// Per-count scaling like [`Action::Draw::per`]: when set, `count` is multiplied
        /// by the number of `per_who`'s in-play cards matching this filter — "bury 1 card
        /// in your opponent's discard pile for each Strike you have in play" / "…for each
        /// Lead you have in play" (Cardona; a 34-clause family). schema v83
        #[serde(default)]
        per: Option<CardFilter>,
        #[serde(default)]
        per_who: Who,
        /// Which zone the `per` filter counts (like [`Action::FinishRollBonus::per_zone`]).
        /// `InPlay` (default) = "for each `<X>` you have in play" (Cardona); `FlippedThisTurn`
        /// = "your opponent buries 1 card in their hand for each Strike flipped" (Scott
        /// Prime's Five Star Heart Punch — count the finisher's flips, not the board).
        /// schema v130
        #[serde(default, skip_serializing_if = "is_in_play_zone")]
        per_zone: CountZone,
        /// Bury EVERY card matching `selector` in the target's hand, ignoring `count`
        /// (and `per`) — "Look at your opponent's hand, they bury all Strike cards"
        /// (a 12-clause family). `BuryFrom::Hand` only; the dispatch sets the effective
        /// count to the target's hand size, and the per-card loop stops when no matching
        /// card remains. schema v90
        #[serde(default)]
        all: bool,
    },
    /// Bury the TRIGGERING card — "bury this card" on an `OnStop` clause (task #94:
    /// "If stopped, discard 1 card from your hand and bury this card or lose ..."). The
    /// referent is [`Engine::stopped_card`], the card whose stop fired the effect;
    /// burying moves it from the discard pile to the bottom of its owner's deck. A
    /// no-op outside a stop context. schema v72
    BuryThisCard,
    /// Add the TRIGGERING (flipped) card to its owner's hand — "If this card is
    /// flipped, [you may] add it to your hand." The referent is
    /// [`Engine::self_card`], set per-card while an `OnFlip` clause carried by a
    /// just-flipped card is dispatched; the card moves from the discard pile (where a
    /// flip lands it) to its owner's hand. The "you may" lives on [`Effect::optional`].
    /// A no-op outside a flip context or if the card has already left the discard.
    /// schema v85
    AddSelfToHand,
    /// Shuffle the TRIGGERING (flipped) card back into its owner's deck — "If this card
    /// is flipped, [you may] shuffle it [back] into your deck." Sibling of
    /// [`Action::AddSelfToHand`]: the referent is [`Engine::self_card`]; the card
    /// moves from the discard pile to the deck, which is then shuffled (firing
    /// `OnShuffle`). "you may" lives on [`Effect::optional`]. schema v86
    ShuffleSelfIntoDeck,
    /// Put the TRIGGERING/self card on TOP of its owner's deck (drawn next) — "[you may]
    /// put this card on top of your deck." Referent is [`Engine::self_card`] (a discard-
    /// resident card firing its WHILE_IN_DISCARD trigger) falling back to
    /// [`Engine::stopped_card`] (the "If stopped, put this card on top of your deck"
    /// family); the card moves from wherever it sits (discard/hand) to the deck front,
    /// unshuffled. "you may" lives on [`Effect::optional`]. schema v141
    PutSelfOnDeckTop,
    /// Put `count` cards from the owner's HAND on TOP of their deck (drawn next),
    /// unshuffled — "put N card(s) from your hand on top of your deck." The owner chooses
    /// which (their hidden hand); the loop stops early when the hand runs dry. Tails the
    /// [`Action::PutSelfOnDeckTop`] recycle ("put this card on top of your deck, then put
    /// 1 card from your hand on top of your deck"). schema v142
    PutFromHandOnDeckTop {
        count: i64,
    },
    /// Play the TRIGGERING (flipped) card immediately — "If this card is flipped, [you
    /// may] play it[ as an additional card this turn]." The referent is
    /// [`Engine::self_card`]; the card leaves the discard pile and resolves as a
    /// normal play by its owner (stop window, OnPlay/OnHit), a bonus action outside the
    /// turn's one-card play. "you may" lives on [`Effect::optional`]. schema v86
    PlaySelf,
    /// Add cards from the just-flipped pool to hand — "add N of the flipped cards to
    /// your hand" / "add all flipped Strikes to your hand" / "randomly add 1 of the
    /// flipped cards…". Selects from `PlayerState.flipped_this_turn` (the turn's flips,
    /// recorded by `act_flip`) that are still in the discard and match `filter`; `count`
    /// = how many (`None` = all matching), `random` picks by RNG instead of by the
    /// owner. Distinct from [`Action::AddFromDiscard`] (whole discard, one card): this is
    /// scoped to the flip pool. schema v88
    AddFlippedToHand {
        #[serde(default)]
        count: Option<i64>,
        #[serde(default)]
        filter: CardFilter,
        #[serde(default)]
        random: bool,
    },
    /// "You may switch 1 card in your hand with 1 card in your discard pile" (Collin,
    /// Mr. Rey): the owner picks one hand card out (→ discard) and one discard card in
    /// (→ hand). A no-op if either zone is empty. The "you may" lives on
    /// [`Effect::optional`]. Picks route to the `discard` (shed) / `target` (tutor)
    /// decision points.
    SwapHandDiscard,
    /// Grant `who` a deferred, one-shot optional hand↔discard swap on their next
    /// turn (Mr. Rey: "When you roll Technique for your turn roll: Once on the next
    /// turn, you may switch 1 card in your hand with 1 card in your discard pile").
    /// Sets a next-turn grant that promotes to usable at the start of the grantee's
    /// following turn (SET, not accumulate — an unused grant expires after that one
    /// turn) and is offered as an optional [`SwapHandDiscard`] before they act.
    GrantSwapNextTurn {
        who: Who,
    },
    Flip {
        n: i64,
        who: Who,
        /// Per-count: flip `n` times the number of `per_who`'s cards matching this
        /// filter ("Flip N cards for each Follow Up you have in play").
        #[serde(default)]
        per: Option<CardFilter>,
        #[serde(default)]
        per_who: Who,
        /// Flip-until (variable count): when set, ignore `n` and mill the target's
        /// deck one card at a time until a flipped card matches this filter (or the
        /// deck empties). "Flip cards until you flip a Submission[, add it to your
        /// hand]." The matching card goes to the hand when `until_to_hand`, else to
        /// the discard with the rest. schema v68
        #[serde(default)]
        until: Option<CardFilter>,
        #[serde(default)]
        until_to_hand: bool,
    },
    /// Move `count` card(s) from the `from` end of `who`'s DECK to their discard pile
    /// — "Each player discards the bottom card of their deck." Unlike [`Self::Flip`]
    /// (which mills the TOP and fires flip triggers / records `flipped_this_turn`),
    /// this is a plain deck-to-discard mill with no flip semantics. schema v101
    MillDeck {
        who: Who,
        count: i64,
        from: DeckEnd,
    },
    /// One-shot roll-conditional draw — "if your [opponent's] next turn roll is `<S>`,
    /// draw N". Arms on play; the engine watches `who`'s NEXT turn roll (`SelfSide` =
    /// your own, `Opp` = your opponent's) and, if it resolves to `skill`, the effect
    /// owner draws `count`. Fires-or-fizzles on that one turn roll and is consumed —
    /// distinct from `ModifyRoll{on_skill}`, which waits until its skill comes up.
    /// schema v109
    RollDraw {
        who: Who,
        skill: Skill,
        count: i64,
    },
    /// One-turn skill-gated turn-roll bonus — "+N to `<S>`, `<S>` during your next turn
    /// roll" / "if your [opponent's] next turn roll is `<S>`, it is +N". Arms on play;
    /// `delta` applies to `who`'s (`SelfSide` = your own, `Opp` = your opponent's)
    /// IMMEDIATELY-next turn roll if it comes up one of `skills`, then the whole pending
    /// queue is drained — a one-turn window, so a non-match fizzles. Distinct from
    /// `ModifyRoll{on_skill}` (waits indefinitely for one skill). schema v110
    NextRollSkillBonus {
        who: Who,
        skills: Vec<Skill>,
        delta: i64,
    },
    /// Multi-turn turn-roll bonus — "your [opponent's] next N turn rolls are +/-N".
    /// Arms on play; `delta` applies to each of `who`'s (`SelfSide` = your own, `Opp` =
    /// your opponent's) next `rolls` turn rolls, decrementing once per roll-off until
    /// exhausted. Skill-agnostic and self-expiring (unlike the standing `TurnRollBonus`).
    /// schema v111
    MultiTurnRollBonus {
        who: Who,
        rolls: i64,
        delta: i64,
    },
    /// "Bury up to `max` cards in your hand to draw the same number of cards +`bonus`"
    /// (Stolen Valor, Back Cracker Potion, Win When You Can…). `who` buries their least
    /// valuable hand cards (to the deck bottom) and then draws that many PLUS `bonus`;
    /// the draw is coupled to the ACTUAL bury count, so zero buries still draw `bonus`.
    /// The "up to" collapses to burying min(`max`, hand size), matching the "bury up to N"
    /// family convention. schema v149
    BuryToDraw {
        max: i64,
        bonus: i64,
        who: Who,
    },
    Discard {
        selector: CardFilter,
        count: i64,
        who: Who,
        random: bool,
        per: Option<CardFilter>,
        per_who: Who,
        /// Like [`Action::Bury`]'s `choose`: the EFFECT OWNER looks at the target's
        /// hand and picks which card(s) to discard ("Look at your opponent's hand,
        /// choose 1 card and discard it"), rather than the hand owner shedding their
        /// own. Only meaningful with `who == Opp`; ignored when `random`. schema v60
        #[serde(default)]
        choose: bool,
        /// Discard EVERY card matching `selector` from the target's hand, ignoring
        /// `count` (and `per`) — "Look at your opponent's hand, they discard all
        /// Strikes". Mirrors [`Action::Bury`]'s `all`; the dispatch sets the effective
        /// count to the target's hand size. schema v90
        #[serde(default)]
        all: bool,
    },
    Search {
        filter: CardFilter,
        dest: Dest,
        count: i64,
        /// Which zone(s) to tutor from. Default `Deck`; `DeckOrDiscard` also scans the
        /// discard pile. Skip-when-default, so pre-v115 fixtures round-trip identically.
        #[serde(default, skip_serializing_if = "is_default_search_source")]
        source: SearchSource,
    },
    ShuffleDeck {
        who: Who,
    },
    ShuffleIntoDeck {
        selector: CardFilter,
        /// Which zone the shuffled card comes from — `Discard` (default) or `InPlay`
        /// ("shuffle 1 Follow Up you have in play into your deck"). schema v83
        #[serde(default)]
        source: ShuffleSource,
        /// Whose zone the shuffle acts on — `SelfSide` (default) or `Opp`/each-player
        /// ("each player shuffles 1 Grapple from their discard pile into their deck" emits
        /// one per side). Each player recurs THEIR OWN zone into THEIR OWN deck. schema v143
        #[serde(default, skip_serializing_if = "is_self_who")]
        who: Who,
        /// Shuffle EVERY matching card in the source zone (not just one chosen card).
        /// "Take any number of Lead cards … and shuffle them into your deck" — the
        /// "any number" is the whole matching set. schema v124
        #[serde(default, skip_serializing_if = "is_false")]
        all: bool,
        /// After shuffling, draw as many cards as were shuffled ("… then draw the same
        /// number of cards"). Coupled to the actual shuffled count. schema v124
        #[serde(default, skip_serializing_if = "is_false")]
        then_draw: bool,
        /// After shuffling, bury as many cards from HAND as were shuffled ("… then bury the
        /// same number of cards from your hand" — Double Leg Death Lock). Coupled to the
        /// actual shuffled count; mutually exclusive with `then_draw` in practice. schema v129
        #[serde(default, skip_serializing_if = "is_false")]
        then_bury: bool,
    },
    AddFromDiscard {
        filter: CardFilter,
    },
    RecurToDeckTop {
        selector: CardFilter,
        count: i64,
    },
    CountsAsInPlay {
        selector: CardFilter,
        count: i64,
    },
    RemoveFromPlay {
        selector: CardFilter,
        who: Who,
        count: i64,
        /// Like [`Action::ReturnToHand`]'s: the actor picks from EITHER board —
        /// "choose 1 card in play and discard it" (Cherry Glamazon), where the card
        /// does not restrict whose board. `who` is ignored when set. schema v39
        #[serde(default)]
        choose: bool,
        /// Send the removed card to its owner's DECK BOTTOM instead of their discard —
        /// "choose 1 card your opponent has in play and BURY it" (JT Dunn's gimmick; a
        /// 6-card family). `false` (the default) = the ordinary discard removal. schema v133
        #[serde(default, skip_serializing_if = "is_false")]
        to_deck: bool,
        /// Remove EVERY matching in-play card of the target at once, with no per-card
        /// pick ("Discard all cards in play", Apocalypse) — there is no real choice, so
        /// this suppresses the phantom decisions a `count`-many loop would emit. `count`
        /// is ignored when set. `false` (the default) = the ordinary N-many aimed removal.
        /// schema v135
        #[serde(default, skip_serializing_if = "is_false")]
        all: bool,
    },
    /// The per-player halves of an "each player …" board effect (Apocalypse's board
    /// clear, Rejected!'s discard-bury, Derailed's hand cycle), wrapped so a competitor
    /// with a matching [`Action::RedirectAuthority`] (Emo Mam) may choose which players
    /// they affect. Absent an active authority the wrapper applies every inner action —
    /// byte-identical to a plain each-player effect — so wrapping is safe DB-wide. The
    /// authority match is by the RESOLVING card's name, so only the cards it lists are
    /// ever redirected. schema v135
    RedirectBoardEffect {
        actions: Vec<Action>,
    },
    /// A passive gimmick marker (Emo Mam): "when you or your opponent hit one of
    /// `groups`, you may choose who it affects." Read by [`Action::RedirectBoardEffect`]
    /// via the resolving card's name (trailing-`!`/case-insensitive, so "Rejected"
    /// matches the card "Rejected!"). Never executes on its own. schema v135
    RedirectAuthority {
        groups: Vec<String>,
    },
    /// Discard 1 of the owner's own in-play cards, then discard 1 of the OPPONENT's
    /// in-play cards of the SAME play order (Candyman Dan). The second target's filter
    /// is bound at runtime to the first pick's play order — a trade the actor chooses
    /// both ends of. No-op if the owner has nothing in play; the second discard is
    /// skipped if the opponent has no same-order card. schema v51
    DiscardInPlayMatch,
    /// "Discard any number of cards from your hand, your opponent discards the same
    /// number of cards from their hand `offset`" (Defector's Dismantler: offset -1;
    /// 2 cards). The actor's chosen count N is a heuristic in `act_coupled_discard`
    /// (strip the opponent's hand when affordable: N = min(self_hand, opp_hand+1)),
    /// since no policy count-choice hook exists; the self-discard fires OnBury so a
    /// discard-recur gimmick still triggers, then the opponent sheds max(0, N+offset).
    /// schema v76
    CoupledDiscard {
        offset: i64,
    },
    /// "Add `count` card(s) in play to their hand" (Fox Assassin V2): return matching
    /// in-play cards to their OWNER's hand (bounce). `who` picks the board; `choose`
    /// (like [`ShuffleHandDraw`]) lets the actor pick from EITHER board — "any player
    /// has in play". A no-op when no matching card exists.
    ReturnToHand {
        selector: CardFilter,
        who: Who,
        count: i64,
        #[serde(default)]
        choose: bool,
    },
    RevealAndDiscard {
        count: i64,
        who: Who,
    },
    /// "Your opponent randomly reveals `count` card(s) in their hand: if it is a stop,
    /// draw `draw` cards" (Bartholomew Hooke). Reveals stay in hand; the actor draws
    /// `draw` for each revealed stop.
    RevealForDraw {
        who: Who,
        count: i64,
        draw: i64,
        match_on: RevealMatch,
    },
    Peek {
        who: Who,
    },
    /// `who` reveals `count` card(s) from their OWN hand to the opponent — a fog-of-war
    /// effect ("Each player reveals 1 card in their hand"). The revealing player CHOOSES
    /// which (a `reveal` decision); the chosen cards become visible to the opponent in
    /// the observable projection while they remain in hand. No zone change. schema v100
    Reveal {
        who: Who,
        count: i64,
        /// "Reveal your (whole) hand to your opponent" (Bermuda Triangle): expose
        /// EVERY card in `who`'s hand, ignoring `count` and the per-card choice.
        /// `false` (the default, every pre-v127 node) = the fog-of-war "reveal N of
        /// your choosing" form. schema v127
        #[serde(default, skip_serializing_if = "is_false")]
        whole_hand: bool,
    },
    /// Arm a deferred, mandatory "forced reveal-and-play" on `who` for their next
    /// turn (Father Light: "during your opponent's next turn, they randomly reveal
    /// a card in their hand until they reveal a playable card; they must play that
    /// card"). Sets a one-shot flag on the target; at the start of that player's
    /// next won turn the engine reveals their hand in random order until a card is
    /// playable (Lead / Follow-Up-with-Lead / Finish-with-Follow-Up, stops count as
    /// their play order) and force-plays it. Idempotent: re-arming before the target
    /// takes a turn still fires once.
    ForceRevealPlay {
        who: Who,
    },
    /// Copy `who`'s Entrance onto the actor's (El Ganso Ruso: "Copy your target's
    /// Entrance"): append the target entrance's effects to the actor's own
    /// entrance, so the actor gains that entrance's ability (in addition to their
    /// own). Resolved live — the engine sees both loaded entrances. Authored under
    /// a `StartOfMatch` `Choice`; copied *ongoing* abilities (OnRoll/Static) fire
    /// naturally, but a copied `StartOfMatch` ability has already missed its window.
    CopyEntrance {
        who: Who,
    },
    /// Look at / reveal cards from the top (and/or bottom) of `deck`'s deck, then
    /// route them: the effect owner (the "actor") takes `to_hand` of them to the
    /// deck owner's hand, buries `bury` to the deck bottom, and disposes of the
    /// leftovers per `rest`. The actor picks by card value — best-to-hand, and
    /// bury the *worst* on their own deck or the *best* on an opponent's deck
    /// (sabotage, e.g. The Oracle). `reveal=true` makes the seen cards public
    /// (logged); `reveal=false` is a private "look at". Covers reveal-top-of-deck
    /// gimmicks (Perfect Assistant, Split, Ricky Riot, The Oracle).
    Scry {
        deck: Who,
        #[serde(default)]
        top: i64,
        #[serde(default)]
        bottom: i64,
        #[serde(default)]
        reveal: bool,
        #[serde(default)]
        to_hand: i64,
        #[serde(default)]
        bury: i64,
        #[serde(default)]
        rest: ScryRest,
        /// Restrict which revealed cards `to_hand` may take — "add 1 STOP to your hand and
        /// bury the others" (Fortress): only a matching card goes to hand (best-first among
        /// matches), the rest fall through to `bury`/`rest`. `None` (the default) = take the
        /// `to_hand` best regardless of kind. schema v136
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to_hand_filter: Option<CardFilter>,
    },
    /// Reveal the top card of `deck`'s deck and route it by a runtime predicate: if
    /// the card's `atk_type` equals `match_atk` it goes to `on_match`, otherwise to
    /// `on_fail` (taken only when worthwhile if `fail_optional` — "you may flip/bury
    /// it"). Destinations: HAND (deck owner's hand), FLIP (mill to discard), BURY
    /// (deck bottom), LEAVE (keep on top). Covers "reveal the top card; if the move
    /// type matches the rolled skill …" gimmicks (Candy MaM, Flame Fighter) — one
    /// effect per rolled skill, `match_atk` baked to that skill's move type.
    RevealRoute {
        deck: Who,
        match_atk: AtkType,
        on_match: RevealDest,
        on_fail: RevealDest,
        #[serde(default)]
        fail_optional: bool,
        #[serde(default)]
        reveal: bool,
        #[serde(default)]
        reveal_from: RevealFrom,
        /// When set, the predicate is a number-parity match instead of `atk_type`:
        /// `Some(true)` = the revealed card matches iff its number is even,
        /// `Some(false)` iff odd (the actor's blind odd/even guess — Smart Mark
        /// Sterling). `None` keeps the `atk_type == match_atk` predicate.
        #[serde(default)]
        match_parity: Option<bool>,
    },
    /// Reveal card(s) and conditionally fire a nested consequence. Reveal `count`
    /// card(s) from `from` — the top/bottom of the owner's deck (a non-destructive
    /// peek; the card stays unless taken) or a uniformly-random card in the owner's
    /// hand; if a revealed card matches `filter` (name substring / attack type), run
    /// the consequence: move that card to the owner's hand when `take_matched` ("add
    /// that card to your hand"), then apply `then` (extra actions parsed from the
    /// tail — draw, roll bonus, bury, re-roll, …). `then_optional` makes the whole
    /// consequence a "you may". A non-match reveals nothing further and leaves every
    /// card in place. Covers "Reveal the top card of your deck: if it has 'X' in the
    /// name, add that card to your hand" and "Randomly reveal 1 card in your hand: if
    /// it has 'X' in the name, draw 1 card". schema v95
    RevealThen {
        reveal_from: RevealSource,
        count: i64,
        filter: CardFilter,
        #[serde(default)]
        take_matched: bool,
        #[serde(default)]
        then: Vec<Action>,
        #[serde(default)]
        then_optional: bool,
    },
    /// Shuffle a player's hand back into their deck, shuffle it, then draw `count`
    /// fresh cards — a mid-match hand refresh (Cyclone V2, on a bump). `choose`
    /// lets the actor pick which player ("either player"); otherwise `who` selects.
    ShuffleHandDraw {
        who: Who,
        count: i64,
        #[serde(default)]
        choose: bool,
        /// How many hand cards to shuffle in: `None` = the WHOLE hand (Cyclone V2);
        /// `Some(n)` = the owner reveals and shuffles `n` chosen cards (Memes Dealer V1:
        /// "reveal 1 card in your hand, shuffle it into your deck, and draw 1"). schema v52
        #[serde(default)]
        hand_count: Option<i64>,
    },
    ModifyRoll {
        who: Who,
        delta: i64,
        when: RollWhen,
        per: Option<CardFilter>,
        per_who: Who,
        /// Which zone the `per` count reads — `InPlay` (the default, "for each Lead
        /// you have in play") or `Discard` ("+2 for each Finish in your discard
        /// pile"). Only meaningful when `per` is set. schema v70
        #[serde(default)]
        per_zone: CountZone,
        /// When set (with `when = Next`), a SKILL-KEYED pending mod: it waits, across
        /// however many turns, until `who` next rolls this skill for their turn roll,
        /// applies `delta` to THAT roll, and is consumed — "the next time you roll
        /// Technique for your turn roll, it is +2". `None` = the plain next/this mod.
        /// schema v99
        #[serde(default, skip_serializing_if = "Option::is_none")]
        on_skill: Option<Skill>,
    },
    /// Add `delta` to the owner's CURRENT roll value, mid-roll-off. Unlike
    /// `ModifyRoll{when=This}` (a pending mod consumed at roll start), this applies to a
    /// roll ALREADY made — a choice branch inside an `OnRollBoost` offer (El Super Hombre
    /// V3: "when you roll Agility … or your roll is +1"). Read by `offer_roll_boost` via
    /// the engine's `pending_roll_boost`. schema v54
    RollBoost {
        delta: i64,
    },
    BuffSkill {
        skill: Skill,
        delta: i64,
        who: Who,
        duration: Duration,
        target_highest: bool,
        /// Retarget the buff to the target's LOWEST base skill (ties -> earlier in
        /// canonical order), mirroring `target_highest`. "+N to your lowest skill".
        /// schema v93
        #[serde(default)]
        target_lowest: bool,
        /// Retarget the buff to the skill the OWNER bound via [`Action::ChooseSkill`]
        /// ("Choose a skill: Your opponent's skill of that type is -1" — Catch These
        /// Hands): the delta lands on the owner's `chosen_skill`, read live in derived
        /// stats. Inert (contributes nothing) until a choice is bound. schema v150
        #[serde(default, skip_serializing_if = "is_false")]
        target_chosen: bool,
        per_crowd: bool,
        /// Clamps the bonus. Under a `While*` duration this bounds the per-read
        /// `per`/`per_crowd` product (see `per`). Under a TIMED duration
        /// (`UntilEndOfTurn` / `UntilStartOfYourNextTurn`) it instead bounds the
        /// ACCUMULATED total this buff has granted while live: repeat firings stack
        /// `delta` and clamp to `cap` — "+1 to Strike and +5 to Submission … (Max +5
        /// to each)" (Snake Pitt Super Lucha). Hand-adjudicated 2026-07-20.
        cap: Option<i64>,
        /// When set, the bonus is `delta * (count of the target's cards in
        /// `per_zone` matching this filter)`, clamped to `cap` — "your Technique is
        /// +1 for each card you have in play with 'Chin' in the name (Max +3)".
        #[serde(default)]
        per: Option<CardFilter>,
        #[serde(default)]
        per_zone: CountZone,
        /// Exclude the SOURCE card (the one carrying this buff) from the `per` count —
        /// "for each OTHER card you have in play with 'X' in the name". Only meaningful
        /// with `per` over the owner's own board; the count-in-zone drops the source by
        /// pointer identity. Additive/skip-when-false (mirrors `ModifyRoll.on_skill`),
        /// so pre-v105 fixtures round-trip byte-identically. schema v105
        #[serde(default, skip_serializing_if = "is_false")]
        per_excludes_self: bool,
    },
    MaxHandSize {
        delta: i64,
        who: Who,
        duration: Duration,
        /// Absolute maximum ("your opponent's maximum handsize is N") — overrides the
        /// base cap rather than shifting it; the LOWEST active set wins, then `delta`
        /// mods apply on top. `None` = a pure delta modifier (the historical form).
        /// Additive/skip-when-none, so pre-set fixtures round-trip byte-identically.
        /// schema v114
        #[serde(default, skip_serializing_if = "Option::is_none")]
        set: Option<i64>,
    },
    /// Minimum-handsize modifier (Quadruple H). NOT a draw-up floor: per the SRG
    /// ruling the minimum is a floor on the MAXIMUM, folded in `effective_hand_cap`.
    /// Read there, never executed. schema v44
    MinHandSize {
        delta: i64,
        who: Who,
        duration: Duration,
    },
    /// Static declaration that the declarer mirrors the opponent's skill increases
    /// (Mimic: "when your opponent increases their skills, your skills are also
    /// increased the same amount"). Read in `effective_stats` — for each skill the
    /// declarer gains the positive part of the opponent's `effective - base`. A
    /// derived-stats fold like `BuffSkill`, never executed. schema v46
    MirrorOpponentIncrease,
    AddText {
        name_contains: Vec<String>,
        effects: Vec<Effect>,
    },
    /// Add a chosen competitor's Gimmick to the actor's own (The SRG Boss — "add
    /// their Gimmick to yours"): append `effects` to the actor's competitor
    /// effects, so they become standing effects (and are suppressed together if
    /// the actor's gimmick is blanked). Authored under a `StartOfMatch` `Choice`
    /// whose branches carry each absorbable variant's baked IR; the engine has no
    /// card index, so the candidate gimmicks are baked, not resolved at runtime.
    AbsorbGimmick {
        effects: Vec<Effect>,
    },
    /// POISON/DOPING (srgpc): "Your opponent's **next** Grapple has the added text:
    /// 'If stopped, you lose the match via disqualification'" (the Madness trio).
    /// Attaches `effects` to the NEXT card `who` plays matching `selector`, then is
    /// consumed. Unlike [`Action::AddText`] — a continuous, gimmick-sourced,
    /// name-matched injection re-derived on every play — this is a ONE-SHOT queued on
    /// the target player (`PlayerState.pending_text`), so per the ruling it "stays
    /// active until fulfilled even if [the source is] removed from the board".
    /// Materialized onto the played card itself, so the added text also reaches the
    /// stop exchange (where `injected_text` never did). schema v40
    AddTextToNext {
        who: Who,
        selector: CardFilter,
        effects: Vec<Effect>,
    },
    Reroll {
        /// Whose die is re-rolled: `SelfSide` (your own — Dunn/Jay White) or `Opp`
        /// ("force your opponent to re-roll" — Reverend/Macho Manny). Overridden by
        /// `choose`.
        who: Who,
        once: bool,
        /// "Choose any player to re-roll": the owner picks which side re-rolls
        /// (overrides `who`). Grim Librarian.
        #[serde(default)]
        choose: bool,
        /// `This` re-rolls the current roll (structural, read in the roll-off);
        /// `Next` grants a one-shot re-roll for the owner's NEXT turn roll ("you
        /// may re-roll your next turn roll" — King Brian Cage / El Gato Shinobi).
        #[serde(default)]
        when: RollWhen,
        /// The payment required to re-roll (`None` = free). A `ShuffleInPlay` cost is
        /// offered only while a matching in-play card exists and shuffles one away (Mr.
        /// Hyde's "Potion"); a `BuryFromHand`/`DiscardFromHand` cost is offered only
        /// while the hand can pay and sheds `count` cards ("bury 4 cards in your hand to
        /// re-roll", "discard 1 Finish from your hand to re-roll"). schema v103
        #[serde(default)]
        cost: Option<RerollCost>,
        /// Scope: `false` (default) = the turn-roll off, offered in `offer_reroll`;
        /// `true` = the FINISH roll, offered in `offer_finish_reroll` inside the
        /// finish sequence ("you may re-roll your Finish roll" — 59 cards, e.g.
        /// Tomato Tomato Jr.). Keeps the two roll paths from cross-firing. schema v76
        #[serde(default)]
        finish: bool,
        /// Scope: `true` = a BREAKOUT roll, offered in `offer_breakout_reroll` inside
        /// the breakout loop ("re-roll your Breakout roll" / "force your opponent to
        /// re-roll their Breakout roll"). Mutually exclusive with `finish`; a bare
        /// `Reroll` (both `false`) is the turn roll. Both re-roll the DEFENDER's die —
        /// `who: SelfSide` means the effect owner IS the defender, `who: Opp` means the
        /// finisher forces the defender to re-roll. schema v102
        #[serde(default)]
        breakout: bool,
    },
    /// "When you roll `from` for your turn roll or Finish roll, you may switch it to
    /// `to`" (Scott Prime V1/V2). Read structurally in BOTH roll paths (the turn
    /// roll-off and the Finish roll), a no-op in `apply_action`; fires when the
    /// rolled skill == `from`. The "you may" lives on the [`Effect::optional`] flag.
    /// A switched turn die keeps its roll mods (value is recomputed on `to`'s stat);
    /// a switched Finish die recomputes base + combo from `to`.
    SwitchRolledSkill {
        from_skill: Skill,
        to: Skill,
    },
    WinTie {
        who: Who,
    },
    Bump {
        who: Who,
    },
    ElectBumpOnSameSkill {
        uses: i64,
    },
    /// A persistent, FORCED same-skill bump when the owner would LOSE the turn roll
    /// (Brock Smith V1's start-of-match gimmick: "when you and that opponent roll the
    /// same skill and they would win the turn roll, bump instead"). Unlike the elective
    /// [`Action::ElectBumpOnSameSkill`] (a per-match charge the owner MAY spend on any
    /// same-skill roll), this is unlimited and mandatory, but only fires on a
    /// same-skill roll the owner is losing — the exact case the card converts into a
    /// bump. The "choose an opponent" targeting is the sole opponent in a 1v1 match.
    /// schema v151
    BumpInsteadOnSameSkillLoss,
    Stop {
        order: Option<PlayOrder>,
        atk_type: Option<AtkType>,
        source_is_skillreq: bool,
        /// "Stop any Finish Strike that cannot be stopped" / "… even if it cannot be
        /// stopped" — this Stop bypasses the attack's own `Unstoppable` declaration,
        /// answering an otherwise-unstoppable finisher. Read in `card_can_stop`.
        /// schema v63
        #[serde(default)]
        even_unstoppable: bool,
        /// Extra constraint on the stopped attack beyond `order`/`atk_type` — "Stop
        /// any Submission with \"Over the Top\" in the name" / "… with \"X\" in the
        /// text". Only `name_contains`/`text_contains` are set here (order/type stay
        /// on the flat fields); matched via `card_matches` in `stop_matches_for`.
        /// `None` = no extra filter. schema v66
        #[serde(default)]
        target: Option<CardFilter>,
        /// "Stop any Finish Strike that is also a Lead [or a Follow Up]" — the stopped
        /// attack must ALSO count as one of these play orders via an `AlsoLead`
        /// declaration whose condition currently holds (a multi-order card, e.g. a printed
        /// Finish that is "also a Lead"). Empty = no such constraint. AND-ed with
        /// `order`/`atk_type`/`target` in `stop_matches_for`. schema v146
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        also_order: Vec<PlayOrder>,
    },
    StopRequiresTag {
        tag: String,
        /// The gate is ALSO satisfied when the stopped attack is itself unstoppable —
        /// "Stop any Finish Submission that has a Spotlight OR that cannot be stopped":
        /// the tag requirement is OR-ed with unstoppability rather than being a hard
        /// AND. Pair with `Stop.even_unstoppable` so the stop can actually catch the
        /// unstoppable case. Read in `card_can_stop`. schema v137
        #[serde(default, skip_serializing_if = "is_false")]
        or_unstoppable: bool,
    },
    BlankGimmick {
        who: Who,
        duration: Duration,
    },
    FlipGimmick {
        who: Who,
    },
    BlankText {
        selector: CardFilter,
        who: Who,
        /// Restrict the blank to cards SITTING IN the target's discard pile — "cards in
        /// your opponent's discard pile have blank text" (neutralises their WhileInDiscard
        /// abilities). `false` = blank the matching card wherever it is (the in-play
        /// Spotlight-blank form). Additive/skip-when-false, so pre-v116 fixtures round-trip
        /// byte-identically. schema v116
        #[serde(default, skip_serializing_if = "is_false")]
        discard_only: bool,
    },
    /// REST-OF-MATCH ("poison") text blank — "Blank all Spotlights for the rest of the
    /// match" (ee0defe5). Unlike the standing [`Action::BlankText`] (re-derived each read
    /// from the source card's presence), this is EXECUTED once when its effect fires: it
    /// resolves `who` to the absolute target and stamps a `(selector, owner)` entry into
    /// `GameState.permanent_blanks`, which persists for the rest of the match — surviving
    /// the source leaving play and catching matching cards played later. "All" (both
    /// boards) is two clauses, `who: SELF` + `who: OPP`. schema v139
    BlankTextPermanent {
        selector: CardFilter,
        who: Who,
    },
    /// "Un-blank your Finishes." — the inverse of [`Action::BlankText`]: a one-shot that
    /// RESTORES the text of `who`'s cards matching `selector`, overriding any blank on
    /// them for the rest of the match (the 6 Splash / "your opponent buries … un-blank
    /// your Finishes" Followups; `who` is always `SelfSide`, `selector` the Finish
    /// order). The engine records the `selector` in `PlayerState.text_unblank`, which
    /// [`GameState::is_text_blanked`](crate::state::GameState) consults FIRST — an
    /// un-blank wins over every blank source (a continuous `BlankText`, a stop's
    /// per-identity `blanked_text`). Duration is the rest of the match: a played card
    /// with no stated end, and the blank it counters is a standing opponent declaration,
    /// so the override must persist to be useful. schema v117
    Unblank {
        selector: CardFilter,
        who: Who,
    },
    /// "This card copies the text of …" — the Spotlight text-copy family (#2 "A Trip
    /// to the Upside Down", #9 "The D-Roll", #16 "Your Worst Nightmare!"). A passive
    /// marker read (never executed) by [`GameState::copied_effects`](crate::state::GameState):
    /// the effects of every card matching `selector` in `who`'s `zone` are re-homed
    /// onto the copier and fire for as long as this clause's own duration is active
    /// (a `WhileInDiscard` copy projects them from the copier's discard pile),
    /// regardless of the source effect's original duration — the copier becomes the
    /// new `self`. `copy_tags` also grafts the source's tags (its "Spotlight-ness")
    /// onto the copier (#16 only; #2/#9 set `false`). "…then blanks them" (#2) is a
    /// *separate* [`Action::BlankText`] clause against the same selector, not a flag
    /// here. Bounded against copy→copy recursion by `copy_guard`. schema v71
    CopyText {
        selector: CardFilter,
        who: Who,
        zone: CountZone,
        copy_tags: bool,
    },
    /// "The stopped card has blank text until the end of the turn" — blank the text of
    /// the specific card instance that was JUST stopped, for the rest of the turn (21
    /// cards; the Jurassic / "If Stopped" stop-card family). Unlike [`Action::BlankText`],
    /// which is a continuous selector-driven scan re-derived from the board, this
    /// blanks ONE card by identity and is held in `GameState.blanked_text` until the
    /// turn-boundary sweep — the stop card stays in play afterwards, so a continuous
    /// blank would never end. Fired from the stop card's `OnStop`; resolved BEFORE the
    /// stopped card's own `OnStop`, so it suppresses that card's "If Stopped" text
    /// (which is the entire point of the family — several members read "stop any card
    /// with 'If Stopped' in the text: that card has blank text …"). schema v36
    BlankStoppedText,
    /// "… that card has blank text" — blank the card that JUST triggered this `OnHit`
    /// (the hit referent `GameState.hit_card`), by identity, for as long as it stays IN
    /// PLAY (the card was just hit, so it is now on the board; the blank self-expires when
    /// it leaves play). Jax, Pet of the Year: "when your opponent hits a card with \"…\" in
    /// the name, that card has blank text and their next turn roll is -1". Authored on an
    /// `OnHit{who:Opp, name_contains}` gimmick effect (a blanked gimmick never reaches
    /// here). Stamps `GameState.blanked_in_play`. schema v148
    BlankHitCard,
    /// "If played as a Stop, this card is also a Finish" — the FINISH-OFF-STOP marker.
    /// A stop card carrying this runs a full finish sequence (finish roll → opponent
    /// breakout roll → win/resume) off a SUCCESSFUL stop, with the stopper as the
    /// finisher and the stopped attacker as the target. A passive marker: read in
    /// `apply_stop` (`maybe_finish_off_stop`), never executed as a mutation. Authored on
    /// an `OnStop{Theirs}` effect whose condition carries the gate — `Always` for "if
    /// played as a Stop", or `StoppedCardNoLogoNoReq` for "if the stopped card had no
    /// competitor logo or skill requirement". A sibling `CrowdMeter` action in the same
    /// effect handles the "the Crowd Meter is +N and …" variants. schema v145
    FinishIfStop,
    /// "… and end the current turn" — end the ACTIVE player's turn immediately (Boot Off
    /// the Apron / Capture Headlock / Take You for a Ride, on stopping a "Double Team"
    /// card). Executed: sets the active player's `turn_ended` flag, which the turn loop's
    /// extra-play loop honours (cancelling any remaining `PlayExtraCard` grants). Authored
    /// on an `OnStop{Theirs}` effect so it fires when this card stops. schema v147
    EndTurn,
    /// "Choose 1: "Kendo Stick", "Steel Chair", or "Trash Can"" (Raven) — bind ONE of
    /// `options` for the rest of the match, stored as `PlayerState.chosen_name`.
    /// Authored under `StartOfMatch`; the binding is then read by
    /// [`Condition::ChosenNameIs`] to gate the sibling effects that reference "that"
    /// name. A no-op if `options` is empty. schema v37
    ChooseName {
        options: Vec<String>,
    },
    /// "Choose a skill: …" (Catch These Hands) — the owner binds ONE of the six skills
    /// for the rest of the match, stored as `PlayerState.chosen_skill`. Read by
    /// [`Action::BuffSkill`]'s `target_chosen` (the debuff on "your opponent's skill of
    /// that type") and by [`Action::RollDrawChosen`] ("the next time you roll that
    /// skill"). Authored on the card's OnHit; a no-op if already bound. schema v150
    ChooseSkill,
    /// "The next time you roll that skill draw 1 card" (Catch These Hands) — arms a
    /// PERSISTENT one-shot draw keyed to the owner's `chosen_skill`: the next time `who`
    /// (SelfSide) rolls that skill for a turn roll, the owner draws `count`, and it is
    /// then consumed. Unlike [`Action::RollDraw`] it does NOT fizzle on a non-matching
    /// roll — it waits until the chosen skill comes up. A no-op if no skill is bound.
    /// schema v150
    RollDrawChosen {
        who: Who,
        count: i64,
    },
    LoseBy {
        kind: LoseKind,
        who: Who,
    },
    /// A Static match-rule toggle: `enabled=false` = "no disqualifications",
    /// `enabled=true` re-enables them. `scope` is who it reaches (see [`DqScope`]).
    /// Read at the disqualification-loss point, not executed.
    DisqualificationRule {
        enabled: bool,
        scope: DqScope,
    },
    /// A Static match-rule toggle for count-out losses: `enabled=false` = "no count
    /// outs" (a player emptying deck+hand no longer loses/wins by count-out), a
    /// standing rule several Crowd Meter match types impose (No DQ / Submission /
    /// Psycho Circus / Liger's Den). `scope` reuses [`DqScope`] (Match = every
    /// player; SelfSide = only the owner). Read at the count-out point in
    /// `draw_for_turn`, never executed as a mutation. schema v59
    CountOutRule {
        enabled: bool,
        scope: DqScope,
    },
    /// A Static poison: while the declaring card sits in play/discard, the `who`-side
    /// (Bleeding Out: `Opp` = "an opponent") must resolve every card-/Gimmick-driven
    /// move of a card OUT of their OWN discard pile RANDOMLY, losing the normal free
    /// choice of which card to recur. Read at the discard-move choice sites
    /// (`bury_from_discard`, `act_add_from_discard`) via `GameState::
    /// force_random_discard_move`, never executed as a mutation. schema v131
    ForceRandomDiscardMove {
        who: Who,
    },
    /// A Static poison: while the declaring card sits in play/discard, an OPPONENT
    /// cannot move ANY card out of the `who`-side's discard pile (Split Personality:
    /// "your opponent cannot move other cards from your discard pile", `who = SelfSide`
    /// = the owner's own pile). Read at the discard-move choice site (`bury_from_discard`,
    /// the only path that reaches the OTHER player's pile) via `GameState::
    /// discard_move_locked`, never executed as a mutation. Distinct from
    /// [`Action::ForceRandomDiscardMove`], which merely randomises the choice. schema v132
    LockDiscard {
        who: Who,
    },
    /// Install a Crowd Meter match-type's standing rules (GM Calace V1: "replace all
    /// Crowd Meter cards with … Steel Cage / Psycho Circus / Lumberjack / No DQ /
    /// Submission"). Appends `effects` to the owner's **Entrance** effects so they are
    /// always-active — a global match condition that survives the owner's gimmick
    /// being blanked (unlike [`Action::AbsorbGimmick`], which installs into the
    /// blankable competitor gimmick). `name` labels the swapped-in match type in the
    /// log. Authored under a `StartOfMatch` `Choice`; clauses the engine cannot yet
    /// model are carried as explicit `Unsupported` sub-effects. schema v59
    SwapCrowdMeter {
        name: String,
        effects: Vec<Effect>,
    },
    /// A Static meta-comparison override "for card effects": the declaring player's
    /// `domain` comparison vs the opponent always resolves as `order` regardless of
    /// the real values (RaRa Perre "skills considered higher"; Theo V2 "considered
    /// fewer cards in hand"). Read in `conditions::holds`, not executed.
    ConsideredCompare {
        domain: CompareDomain,
        order: CompareOrder,
    },
    /// A Static declaration: "your opponent does not draw for your card effects"
    /// (Sami "The Draw" Callihan). Read at `act_draw` — a `Draw{who=OPP}` resolved by
    /// the declaring player is voided. Not executed as a mutation.
    SuppressOpponentDraw,
    /// The mirror declaration: "you do not bury or discard cards from your hand for
    /// your OWN card effects" (Sami "Death Machine" V2; one branch of Sami WR's
    /// start-of-match choice). Read at the two hand-loss chokepoints — `act_bury`'s
    /// `BuryFrom::Hand` branch and `act_discard` — and only when the declaring player
    /// is BOTH the effect's owner and the one losing cards, so an opponent's effect
    /// still takes them. Not executed as a mutation. schema v42
    SuppressSelfHandLoss,
    /// Static declaration that on a BUMP the declarer's opponent discards 1 card
    /// instead of drawing (Mack-a-Tack: "when you bump, your opponent discards 1 card
    /// instead of drawing"). Read in `do_bump`, never executed. schema v50
    BumpDrawReplace,
    /// Static declaration that, `uses` times per match, the declarer MAY replace a
    /// bump they would take with drawing `draw` cards and re-rolling both turn rolls
    /// (Pretty Paul Says "Let It Rip!": "Once per match: When you would bump, draw 2
    /// cards instead and each player re-rolls their turn roll"). Read structurally in
    /// `roll_off` (`try_bump_replacement`), never executed: the bump is *replaced*, so
    /// neither side's `OnBump` gimmick fires and the turn is not counted as bumped —
    /// the whole point when a sign-flipper (Cassandra) has turned the owner's own
    /// bump-punish against them. The per-match charge is tracked in `freq_counters`
    /// under `match:bump_replace` (like `ElectBumpOnSameSkill`'s `uses`), not via the
    /// frequency guard. schema v73
    BumpReplacement {
        uses: i64,
        draw: i64,
    },
    /// Static declaration that multiplies every number in the owner's Entrance card's
    /// effects by `factor`, when the entrance name matches `name_contains` (Pedro
    /// Valiant: "triple the numbers in the text of your Entrance cards with 'Training
    /// with' in the name"). Applied to the entrance effects in `gimmick_standing_effects`
    /// (like Cassandra's sign-flip), never executed. Inert while the matching entrances
    /// parse to `Unsupported`; forward-compatible when they are modeled. schema v53
    ScaleEntranceNumbers {
        name_contains: Vec<String>,
        factor: i64,
    },
    CrowdMeter {
        delta: i64,
    },
    PlayExtraCard {
        order: Option<PlayOrder>,
    },
    SetFinishRoll {
        value: i64,
        condition: Condition,
    },
    FinishBonus {
        skill: Skill,
        delta: i64,
    },
    FinishRollBonus {
        delta: i64,
        when_skill: Option<Skill>,
        either: bool,
        /// Base-roll gate: the bonus applies only when the BASE Finish roll (the
        /// rolled skill's stat, BEFORE combo/gimmick/Crowd-Meter bonuses) is
        /// `<= when_base_le` and/or `>= when_base_ge` — "If your Finish roll is 6 or
        /// less, it is +2". `None` = ungated. schema v61
        #[serde(default)]
        when_base_le: Option<i64>,
        #[serde(default)]
        when_base_ge: Option<i64>,
        /// When set, the bonus is `delta * (count of `per_who`'s cards in `per_zone`
        /// matching this filter)` — "your Finish roll is +1 for each Spotlight you
        /// have in play / in your opponent's discard pile". `None` = flat `delta`.
        #[serde(default)]
        per: Option<CardFilter>,
        #[serde(default)]
        per_who: Who,
        #[serde(default)]
        per_zone: CountZone,
        /// Integer divisor on the per-count before scaling by `delta` — the count is
        /// `floor(matches / per_divisor)`. `None`/`Some(1)` = one bonus per match;
        /// `Some(3)` = "your Finish roll is +1 for every 3 Strikes you have in play"
        /// (The Ride Along). Only meaningful with `per` set. schema v74
        #[serde(default)]
        per_divisor: Option<i64>,
        /// Clamps the per-count product ("… (Max +2)") — the `per`-scaled bonus never
        /// exceeds `cap`. `None` = uncapped. Additive/skip-when-none. schema v106
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cap: Option<i64>,
        /// Exclude the SOURCE card from the `per` count — "for each OTHER `<X>` you have
        /// in play", the FinishRollBonus analogue of `BuffSkill.per_excludes_self`.
        /// Additive/skip-when-false. schema v106
        #[serde(default, skip_serializing_if = "is_false")]
        per_excludes_self: bool,
        /// Dynamic delta = the Crowd Meter (clamped to `cap`), added ON TOP of the Crowd
        /// Meter the finish math already folds into every roll — "Your Finish roll is + the
        /// Crowd Meter (Max +N)", a SECOND crowd-meter addend. Mutually exclusive with the
        /// flat `delta` / `per` count. `finish_bonus_from` reads the live Crowd Meter each
        /// roll. Additive/skip-when-false, so pre-`per_crowd` fixtures round-trip.
        /// schema v123
        #[serde(default, skip_serializing_if = "is_false")]
        per_crowd: bool,
    },
    /// A standing bonus to the owner's TURN roll, applied only when the randomly
    /// rolled skill equals `skill`: "Your Power is +N during turn rolls." Read by
    /// `turn_roll_bonus` in the roll-off — the parallel of [`Action::FinishRollBonus`]
    /// / [`Action::BreakoutModifier`] for the turn roll — and never executed as a
    /// mutation. Because it lives in the turn-roll phase, it does NOT touch finish
    /// rolls, stops, or skill comparisons the way a plain `BuffSkill` would. schema v97
    TurnRollBonus {
        skill: Skill,
        delta: i64,
        /// Whose turn roll this modifies, from the OWNER's point of view. `SelfSide`
        /// (the default) = the owner's own turn roll ("your Power is +N during turn
        /// rolls"); `Opp` = the owner's opponent's ("your opponent's Power is -N during
        /// their turn rolls"). Read by `turn_roll_bonus`, which sums a roller's own
        /// `SelfSide` mods with their opponent's `Opp` mods. Skip-when-`SelfSide`, so
        /// pre-`who` fixtures round-trip byte-identical. schema v122
        #[serde(default, skip_serializing_if = "is_self_who")]
        who: Who,
        /// Symmetric modifier: when set, the bonus applies to WHOEVER rolls `skill`
        /// for their turn roll, not just the owner — "if either player rolls Power for
        /// their turn roll, their roll is +1". `turn_roll_bonus` picks up an
        /// `either` bonus from the opponent's board too. Additive/skip-when-false.
        /// schema v107
        #[serde(default, skip_serializing_if = "is_false")]
        either: bool,
        /// Dynamic delta = the Crowd Meter (clamped to `cap`), instead of the flat
        /// `delta` — "your Technique is + the Crowd Meter (Max +3) during your turn
        /// roll" (the roll-off parallel of a `per_crowd` [`Action::BuffSkill`]). A
        /// turn-roll-scoped skill mod that must NOT leak into `effective_stats` (finish
        /// rolls, skill requirements, comparisons), so it rides `TurnRollBonus` rather
        /// than a full-time buff. `turn_roll_bonus` reads the live Crowd Meter each
        /// roll-off. Additive/skip-when-false, so pre-v118 fixtures round-trip.
        /// schema v118
        #[serde(default, skip_serializing_if = "is_false")]
        per_crowd: bool,
        /// Clamps the `per_crowd` delta ("Max +N"). `None` = uncapped. Ignored when
        /// `per_crowd` is false. schema v118
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cap: Option<i64>,
    },
    BreakoutModifier {
        delta: i64,
        attempts: Option<i64>,
        /// Skill gate on the breakout roll — the bonus applies only when the defender's
        /// rolled breakout skill equals `when_skill` ("+1 to Strike during your breakout
        /// rolls", Pineapple; "Power is +1 during your breakout rolls", The SRG Boss V3).
        /// `None` = every breakout roll regardless of the rolled skill. schema v79
        #[serde(default)]
        when_skill: Option<Skill>,
        /// Whose breakout rolls this modifies, from the OWNER's point of view. `SelfSide`
        /// (the default) = the owner's own breakout rolls ("your breakout rolls are +N");
        /// `Opp` = the owner's opponent's ("your opponent's breakout rolls are -N"). Read
        /// by `breakout_bonus`, which sums a defender's own `SelfSide` mods with their
        /// opponent's `Opp` mods. schema v94
        #[serde(default)]
        who: Who,
        /// Symmetric modifier: when set, the bonus applies to WHOEVER is rolling the
        /// breakout (the defender), regardless of `who` or which board it sits on — "if
        /// either player rolls Agility for their breakout roll, their roll is -1".
        /// `breakout_mods_from` admits an `either` mod on top of the `who` match.
        /// Additive/skip-when-false. schema v107
        #[serde(default, skip_serializing_if = "is_false")]
        either: bool,
        /// When set, `delta` is scaled by `count of `per_who`'s cards in `per_zone`
        /// matching this filter` — "your opponent's breakout rolls are +1 for each Stop
        /// they have in play", the `BreakoutModifier` analogue of
        /// [`Action::FinishRollBonus::per`]. `None` = flat `delta`. All the per-count
        /// fields are additive/skip-when-default so pre-per breakout fixtures stay
        /// byte-identical. schema v112
        #[serde(default, skip_serializing_if = "Option::is_none")]
        per: Option<CardFilter>,
        #[serde(default, skip_serializing_if = "is_self_who")]
        per_who: Who,
        #[serde(default, skip_serializing_if = "is_in_play_zone")]
        per_zone: CountZone,
        /// Integer divisor on the per-count before scaling by `delta` (the count is
        /// `floor(matches / per_divisor)`); `None`/`Some(1)` = one bonus per match.
        /// schema v112
        #[serde(default, skip_serializing_if = "Option::is_none")]
        per_divisor: Option<i64>,
        /// Clamps the per-count product ("… (Max +M)") — the `per`-scaled bonus never
        /// exceeds `cap`. `None` = uncapped. schema v112
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cap: Option<i64>,
        /// Exclude the SOURCE card from the `per` count — "for each OTHER `<X>` you have
        /// in play". schema v112
        #[serde(default, skip_serializing_if = "is_false")]
        per_excludes_self: bool,
    },
    /// Grant the actor a TIMED, imperative breakout-roll bonus of `delta`, swept at the
    /// end of the turn — "add +1 to your breakout rolls until the end of the turn" (The
    /// Mailman Always Delivers). Unlike [`Action::BreakoutModifier`] (a Static bonus read
    /// off an in-play card), this accumulates onto the actor's `breakout_bonus_eot` store
    /// and so survives the SOURCE card leaving play — needed because Mailman shuffles
    /// itself away as it grants the bonus. `breakout_bonus` adds the store for the
    /// defender. `who` names WHOSE breakout rolls it lands on from the actor's POV:
    /// `SelfSide` (the default, Mailman) = the actor's own; `Opp` = "your opponent's
    /// breakout rolls are -N" (Shattered Split's Why So Serious?!?, revealed as a Strike).
    /// schema v132
    GrantBreakoutBonus {
        delta: i64,
        #[serde(default, skip_serializing_if = "is_self_who")]
        who: Who,
    },
    /// Modifies the NUMBER of breakout attempts (rolls) the affected player gets this
    /// turn — the "reduced / extra breakout rolls" family, distinct from
    /// [`Action::BreakoutModifier`] (which shifts a roll's VALUE, not the count). `set`
    /// overrides the base `BREAKOUT_ATTEMPTS` ("your opponent gets 2 Breakout rolls this
    /// turn"); `delta` shifts it ("gets 1 additional / 1 fewer Breakout roll"). `who`
    /// names the affected side from the OWNER's POV — `Opp` = "your opponent gets …",
    /// `SelfSide` = "you get …". Read by `breakout_attempts_for`, which sums both boards
    /// and clamps the result. schema v113
    BreakoutAttempts {
        /// Additive shift: +N "additional/more", -N "fewer". 0 when only `set` applies.
        delta: i64,
        /// Absolute override of the base attempt count ("gets N Breakout rolls"); `None`
        /// = shift the base by `delta` only. When several effects set, the smallest wins.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        set: Option<i64>,
        /// Affected side from the owner's POV: `SelfSide` = the owner's own breakout
        /// attempts, `Opp` = the owner's opponent's. Default `SelfSide`.
        #[serde(default, skip_serializing_if = "is_self_who")]
        who: Who,
        /// Per-count scaling of `delta` — "1 additional Breakout roll for each Skill
        /// Requirement card they have in play". Same machinery as
        /// [`Action::BreakoutModifier::per`]; all skip-when-default. schema v113
        #[serde(default, skip_serializing_if = "Option::is_none")]
        per: Option<CardFilter>,
        #[serde(default, skip_serializing_if = "is_self_who")]
        per_who: Who,
        #[serde(default, skip_serializing_if = "is_in_play_zone")]
        per_zone: CountZone,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        per_divisor: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cap: Option<i64>,
        #[serde(default, skip_serializing_if = "is_false")]
        per_excludes_self: bool,
    },
    LowestRollWins,
    FlipGimmickSigns {
        who: Who,
    },
    Unstoppable {
        by_order: Option<PlayOrder>,
        /// "Cannot be stopped by \"X\"" — unstoppable specifically against a stopper
        /// whose NAME equals this (AND-ed with `by_order`). `None` = no name gate.
        /// schema v64
        #[serde(default)]
        by_name: Option<String>,
        /// "Cannot be stopped by Skill Requirement cards" — unstoppable against a
        /// stopper that carries a skill requirement (AND-ed with the other gates).
        /// Authored on a main-deck card = this card; on a gimmick/entrance = every
        /// one of the owner's cards. schema v65
        #[serde(default)]
        by_skillreq: bool,
        /// "Your cards with \"X\" in the name cannot be stopped" — a player-scope
        /// declaration (gimmick/competitor/entrance) that only shields the owner's
        /// attacks whose NAME contains this substring; `None` = every card. Matched
        /// against the ATTACK, not the stopper (distinct from `by_name`). schema v152
        #[serde(default, skip_serializing_if = "Option::is_none")]
        applies_name: Option<String>,
        /// "Your cards cannot be stopped by …" (vs "This card …"): the shield covers
        /// EVERY one of the owner's cards, so the engine reads it even from an in-play
        /// main-deck source (Cat/Dog/Sheep Uprising's printed-Finish shield). A
        /// self-scope `Unstoppable` (`false`) only ever shields its own card and never
        /// leaks to siblings from in play. schema v153
        #[serde(default, skip_serializing_if = "is_false")]
        player_scope: bool,
    },
    AlsoLead {
        condition: Condition,
        /// Which play-order slot this card may ALSO be played in while `condition`
        /// holds. `Lead` (the default) = "this card is also a Lead"; `Followup` =
        /// "… also a Follow Up" (playable when a Lead is in play); `Finish` = "…
        /// also a Finish". Read in `also_playable_now`. schema v70
        #[serde(default)]
        order: PlayOrder,
    },
    /// Static stop-reframe (Jokerfish V2: "your opponent's Finishes are also Follow
    /// Ups for your Stop cards"). For the DECLARER-as-defender, an attack whose order
    /// is `attack_order` also satisfies a `Stop{order: as_order}`. Read in
    /// `card_can_stop`, never executed. schema v45
    StopCountsOrderAs {
        attack_order: PlayOrder,
        as_order: PlayOrder,
    },
    /// Static declaration that the declarer's OWN cards whose deck number is in
    /// `[number_min, number_max]` cannot act as Stops (Jokerfish V2: "your cards
    /// #19-21 cannot stop cards"). The rest of each card's text is unaffected — only
    /// its Stop ability is suppressed. Read in `card_can_stop`, never executed. schema v45
    SuppressStop {
        number_min: i64,
        number_max: i64,
    },
    /// A player-scope standing declaration that the DECLARER's Stops may stop an
    /// attack even when it "cannot be stopped" ("You can stop cards that cannot be
    /// stopped" — Pixel Palace Plancha / Throw Into the Turnbuckle / That's Cheesy
    /// Chinlock; JT Dunn). The per-`Stop` `even_unstoppable` flag says "THIS stop
    /// bypasses"; this node says "ALL of your stops bypass" while it is in play (or
    /// declared on a gimmick). Read in `card_can_stop` via `can_stop_unstoppable`,
    /// never executed. schema v154
    ///
    /// `only_order` narrows the bypass to attacks whose PRINTED play order matches
    /// ("Ignore any \"Cannot be stopped\" text on your opponent's Finish cards" —
    /// Pineapple/Trash Can/Sledgehammer Uprising); `None` is the original blanket
    /// enabler. schema v156
    CanStopUnstoppable {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        only_order: Option<PlayOrder>,
    },
    DoubleFinishIfBumped,
    /// Double this card's own printed Finish-roll bonuses when `condition` holds —
    /// the conditional generalization of [`Self::DoubleFinishIfBumped`] ("double
    /// these bonuses if you have another Submission in play / rolled Power / …";
    /// kenzie, king-cage, foxworthy, srg-boss). Read in `card_finish_bonus` against
    /// the owner's turn-roll context, never executed. schema v77
    DoubleFinishIf {
        condition: Condition,
    },
    /// This card can only be stopped by `count` Stops at once (King Brian Cage). A
    /// Static self-effect read in `offer_stop`; never executed. schema v80
    RequireStops {
        count: i64,
    },
    /// This card ALSO counts as attack type `atk_type` (King Brian Cage's "also a
    /// Finish Grapple"). Read via `Card::counts_as_atk_type`; never executed. schema v81
    AlsoAtkType {
        atk_type: AtkType,
    },
    /// Defender declaration: the opponent needs `count` cards of `kind` in play to land
    /// a Finish against you (D3 V1). Read in `playable_options`; never executed. schema v125
    FinishRequires {
        kind: RequireKind,
        count: i64,
    },
    /// Look at `who`'s hand, move one chosen `selector` card to the top of `who`'s deck
    /// (D3 V1's Claw, `who: Opp`). schema v126
    HandToDeckTop {
        who: Who,
        selector: CardFilter,
    },
    Choice {
        options: Vec<ChoiceOption>,
    },
    Unsupported {
        raw_text: String,
        reason: String,
    },
}
