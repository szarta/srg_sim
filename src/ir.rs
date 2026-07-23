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
pub const SCHEMA_VERSION: i64 = 70;

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

/// Which zone a [`Action::BuffSkill`] `per`-count ranges over — "for each card
/// you have **in play**" vs "in your **discard** pile".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CountZone {
    #[default]
    InPlay,
    Discard,
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
/// others"). schema v69
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ScryRest {
    #[default]
    Return,
    Choose,
    Flip,
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

/// Comparison operand for skill/hand-size compares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Vs {
    Opp,
    OppSame,
    Value,
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
    /// Fires when the `who`-side's deck is shuffled by a card/gimmick EFFECT (any
    /// effect-caused shuffle: explicit "shuffle your deck", or the incidental shuffle
    /// after a search/tutor/shuffle-into-deck/hand-into-deck). NOT the match-start
    /// setup shuffle, nor the private bury-ordering shuffle. `who` = whose shuffle
    /// fires it from the owner's POV (OPP = "when your opponent shuffles their deck" —
    /// Memes Dealer V2). Override-only.
    OnShuffle {
        who: Who,
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
    /// The owner's opponent won the *previous* turn's roll-off
    /// (`GameState.last_roll_winner`); false before turn 1. Gates Dunn's re-roll.
    OppWonLastRoll,
    /// The PREVIOUS turn's roll-off bumped (`GameState.last_turn_bumped`); false before
    /// turn 1. Gates Mack-a-Tack's "if you bumped on the last turn roll" re-roll.
    BumpedLastTurnRoll,
    GimmickFlipped {
        who: Who,
    },
    /// It is currently `who`'s turn — the active player (roll-off winner) is the
    /// `who`-side. Gates a continuous effect to a turn phase ("during your opponent's
    /// turn: …" — La Fenix). Reads `GameState.active`.
    DuringTurn {
        who: Who,
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
    },
    Search {
        filter: CardFilter,
        dest: Dest,
        count: i64,
    },
    ShuffleDeck {
        who: Who,
    },
    ShuffleIntoDeck {
        selector: CardFilter,
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
    },
    /// Discard 1 of the owner's own in-play cards, then discard 1 of the OPPONENT's
    /// in-play cards of the SAME play order (Candyman Dan). The second target's filter
    /// is bound at runtime to the first pick's play order — a trade the actor chooses
    /// both ends of. No-op if the owner has nothing in play; the second discard is
    /// skipped if the opponent has no same-order card. schema v51
    DiscardInPlayMatch,
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
    },
    MaxHandSize {
        delta: i64,
        who: Who,
        duration: Duration,
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
        /// An in-play card the owner must shuffle into their deck to re-roll (Mr.
        /// Hyde: "shuffle 1 card with 'Potion' in the name that you have in play into
        /// your deck to re-roll"). `None` = free. When set, the re-roll is offered
        /// only while a matching card is in play, and taking it shuffles one away.
        #[serde(default)]
        cost: Option<CardFilter>,
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
    },
    StopRequiresTag {
        tag: String,
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
    /// "Choose 1: "Kendo Stick", "Steel Chair", or "Trash Can"" (Raven) — bind ONE of
    /// `options` for the rest of the match, stored as `PlayerState.chosen_name`.
    /// Authored under `StartOfMatch`; the binding is then read by
    /// [`Condition::ChosenNameIs`] to gate the sibling effects that reference "that"
    /// name. A no-op if `options` is empty. schema v37
    ChooseName {
        options: Vec<String>,
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
    },
    BreakoutModifier {
        delta: i64,
        attempts: Option<i64>,
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
    DoubleFinishIfBumped,
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
    /// Fires when the `who`-side's deck is shuffled by a card/gimmick EFFECT (any
    /// effect-caused shuffle: explicit "shuffle your deck", or the incidental shuffle
    /// after a search/tutor/shuffle-into-deck/hand-into-deck). NOT the match-start
    /// setup shuffle, nor the private bury-ordering shuffle. `who` = whose shuffle
    /// fires it from the owner's POV (OPP = "when your opponent shuffles their deck" —
    /// Memes Dealer V2). Override-only.
    OnShuffle {
        who: Who,
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
    /// The owner's opponent won the *previous* turn's roll-off
    /// (`GameState.last_roll_winner`); false before turn 1. Gates Dunn's re-roll.
    OppWonLastRoll,
    /// The PREVIOUS turn's roll-off bumped (`GameState.last_turn_bumped`); false before
    /// turn 1. Gates Mack-a-Tack's "if you bumped on the last turn roll" re-roll.
    BumpedLastTurnRoll,
    GimmickFlipped {
        who: Who,
    },
    DuringTurn {
        who: Who,
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
    },
    Search {
        filter: CardFilter,
        dest: Dest,
        count: i64,
    },
    ShuffleDeck {
        who: Who,
    },
    ShuffleIntoDeck {
        selector: CardFilter,
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
    },
    /// Discard 1 of the owner's own in-play cards, then discard 1 of the OPPONENT's
    /// in-play cards of the SAME play order (Candyman Dan). The second target's filter
    /// is bound at runtime to the first pick's play order — a trade the actor chooses
    /// both ends of. No-op if the owner has nothing in play; the second discard is
    /// skipped if the opponent has no same-order card. schema v51
    DiscardInPlayMatch,
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
    },
    MaxHandSize {
        delta: i64,
        who: Who,
        duration: Duration,
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
        /// An in-play card the owner must shuffle into their deck to re-roll (Mr.
        /// Hyde: "shuffle 1 card with 'Potion' in the name that you have in play into
        /// your deck to re-roll"). `None` = free. When set, the re-roll is offered
        /// only while a matching card is in play, and taking it shuffles one away.
        #[serde(default)]
        cost: Option<CardFilter>,
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
    },
    StopRequiresTag {
        tag: String,
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
    /// "Choose 1: "Kendo Stick", "Steel Chair", or "Trash Can"" (Raven) — bind ONE of
    /// `options` for the rest of the match, stored as `PlayerState.chosen_name`.
    /// Authored under `StartOfMatch`; the binding is then read by
    /// [`Condition::ChosenNameIs`] to gate the sibling effects that reference "that"
    /// name. A no-op if `options` is empty. schema v37
    ChooseName {
        options: Vec<String>,
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
    },
    BreakoutModifier {
        delta: i64,
        attempts: Option<i64>,
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
    DoubleFinishIfBumped,
    Choice {
        options: Vec<ChoiceOption>,
    },
    Unsupported {
        raw_text: String,
        reason: String,
    },
}
