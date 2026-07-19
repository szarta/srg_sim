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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlayOrder {
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

/// What a [`Action::Scry`] does with revealed cards that are neither taken to
/// hand nor buried by the fixed `bury` count. `Return` puts them back on top of
/// the deck (the actor reorders by value); `Choose` lets the actor decide, per
/// card, between returning it on top and burying it to the deck bottom
/// (Ricky Riot's "put the other back on top or bury it").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ScryRest {
    #[default]
    Return,
    Choose,
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

/// Text-blank expiry window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Until {
    EndOfTurn,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Who {
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
    },
    OnBump,
    StartOfTurn,
    StartOfMatch,
    OnBreakout,
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
    /// The owner's opponent won the *previous* turn's roll-off
    /// (`GameState.last_roll_winner`); false before turn 1. Gates Dunn's re-roll.
    OppWonLastRoll,
    GimmickFlipped {
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
    },
    Bury {
        selector: CardFilter,
        count: i64,
        who: Who,
        random: bool,
        #[serde(default)]
        source: BuryFrom,
    },
    Flip {
        n: i64,
        who: Who,
    },
    Discard {
        selector: CardFilter,
        count: i64,
        who: Who,
        random: bool,
        per: Option<CardFilter>,
        per_who: Who,
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
    },
    RevealAndDiscard {
        count: i64,
        who: Who,
    },
    Peek {
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
    },
    ModifyRoll {
        who: Who,
        delta: i64,
        when: RollWhen,
        per: Option<CardFilter>,
        per_who: Who,
    },
    BuffSkill {
        skill: Skill,
        delta: i64,
        who: Who,
        duration: Duration,
        target_highest: bool,
        per_crowd: bool,
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
        until: Until,
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
    },
    AlsoLead {
        condition: Condition,
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
    },
    OnBump,
    StartOfTurn,
    StartOfMatch,
    OnBreakout,
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
    /// The owner's opponent won the *previous* turn's roll-off
    /// (`GameState.last_roll_winner`); false before turn 1. Gates Dunn's re-roll.
    OppWonLastRoll,
    GimmickFlipped {
        who: Who,
    },

    // Actions
    Draw {
        n: i64,
        source: DeckEnd,
        who: Who,
        per: Option<CardFilter>,
        per_who: Who,
    },
    Bury {
        selector: CardFilter,
        count: i64,
        who: Who,
        random: bool,
        #[serde(default)]
        source: BuryFrom,
    },
    Flip {
        n: i64,
        who: Who,
    },
    Discard {
        selector: CardFilter,
        count: i64,
        who: Who,
        random: bool,
        per: Option<CardFilter>,
        per_who: Who,
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
    },
    RevealAndDiscard {
        count: i64,
        who: Who,
    },
    Peek {
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
    },
    ModifyRoll {
        who: Who,
        delta: i64,
        when: RollWhen,
        per: Option<CardFilter>,
        per_who: Who,
    },
    BuffSkill {
        skill: Skill,
        delta: i64,
        who: Who,
        duration: Duration,
        target_highest: bool,
        per_crowd: bool,
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
        until: Until,
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
    },
    AlsoLead {
        condition: Condition,
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
