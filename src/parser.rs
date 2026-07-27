//! rules_text -> [Effect]: pattern grammar + overrides + coverage (DESIGN.md §4).
//!
//! A faithful port of `rules_parser.py`. Three layers, tried in order:
//!
//! 1. **Pattern grammar** — a library of whole-clause regexes for the recurring
//!    shapes, each paired with a builder that emits the [`Effect`] IR.
//! 2. **Curated overrides** (keyed by `db_uuid`) — hand-authored IR for cards the
//!    grammar can't parse. The source is `overrides.yaml`; the machine-read form
//!    is the pre-expanded `overrides.ir.json` (defaults filled), loaded strictly.
//! 3. **`Unsupported(raw_clause, reason)`** — anything left over, so it is logged
//!    and measurable, never silently dropped.
//!
//! [`coverage`] tallies grammar / override / unsupported over a record set;
//! [`enrich_card`] / [`enrich_deck`] attach compiled IR (and finish bonuses) to
//! loaded domain objects.

use crate::cards::{Card, Competitor, Deck, EntranceCard, DECK_SIZE};
use crate::ir::{
    Action, AtkType, BuryFrom, CardFilter, ChoiceOption, ChoiceOptionTag, Comparator, Condition,
    CountZone, DeckEnd, Direction, Duration, Effect, EffectSource, EffectTag, Frequency,
    FrequencyGuard, FrequencyGuardTag, LoseKind, PlayOrder, RollWhen, ScryRest, ShuffleSource,
    Skill, Trigger, Vs, Who,
};
use regex::{Captures, Regex};
use std::collections::BTreeMap;
use std::sync::LazyLock;

/// The hand-authored override table: `db_uuid -> compiled effects`.
pub type Overrides = BTreeMap<String, Vec<Effect>>;

// ---------------------------------------------------------------------------
// Small constructors mirroring the effects.py dataclass defaults
// ---------------------------------------------------------------------------

fn guard() -> FrequencyGuard {
    FrequencyGuard {
        node_type: FrequencyGuardTag,
        kind: Frequency::Unlimited,
        n: None,
    }
}

/// A partial Effect; provenance/frequency are filled in by [`compile`].
fn eff(trigger: Trigger, actions: Vec<Action>, condition: Condition, duration: Duration) -> Effect {
    Effect {
        node_type: EffectTag,
        trigger,
        condition,
        actions,
        duration,
        frequency: guard(),
        raw_clause: String::new(),
        source: EffectSource::Card,
        optional: false,
    }
}

fn on_hit() -> Trigger {
    Trigger::OnHit {
        order: None,
        atk_type: None,
        name_contains: Vec::new(),
        text_contains: Vec::new(),
        on_any: false,
        who: Who::SelfSide, // the parser only ever produces "when YOU hit"
    }
}

fn cf_atk(a: AtkType) -> CardFilter {
    CardFilter {
        atk_type: Some(a),
        ..Default::default()
    }
}

/// "When `who` rolls `skill` for their turn roll" — a standing trigger fired on the
/// turn roll-off (the effect owner's card must be in play). `who == SelfSide` = "when
/// you roll"; `who == Opp` = "when your opponent rolls".
fn on_roll(s: Skill, who: Who) -> Trigger {
    Trigger::OnRoll {
        skill: Some(s),
        who,
    }
}

fn cf_order(o: PlayOrder) -> CardFilter {
    CardFilter {
        play_order: Some(o),
        ..Default::default()
    }
}

/// Quoted names from a `with "X" [or "Y"] in the name` phrase (case-insensitive
/// OR-substring — same convention as the name-substring override family).
fn quoted_names(text: &str) -> Vec<String> {
    static Q: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#""([^"]+)""#).unwrap());
    Q.captures_iter(text).map(|c| c[1].to_owned()).collect()
}

fn cf_name(names: Vec<String>) -> CardFilter {
    CardFilter {
        name_contains: names,
        ..Default::default()
    }
}

/// A card-substring filter over the title (`"X" in the name`) or the rules text
/// (`"X" in the text`) — picks the attribute from the phrasing captured as `attr`.
fn name_or_text_filter(attr: &str, names: Vec<String>) -> CardFilter {
    if attr == "text" {
        CardFilter {
            text_contains: names,
            ..Default::default()
        }
    } else {
        cf_name(names)
    }
}

fn cf_tag(tag: &str) -> CardFilter {
    CardFilter {
        tag: Some(tag.to_owned()),
        ..Default::default()
    }
}

/// The card-selector inside a recur/discard clause ("N cards", "N Finish", "N cards
/// with \"X\" in the name", "N Follow Up Strike"). `None` if the descriptor is one we
/// don't model (e.g. "stop", which has no CardFilter attribute).
fn recur_filter(desc: &str) -> Option<CardFilter> {
    let d = desc.trim();
    if d.eq_ignore_ascii_case("card") || d.eq_ignore_ascii_case("cards") {
        return Some(CardFilter::default());
    }
    if d.contains("in the name") {
        let names = quoted_names(d);
        return (!names.is_empty()).then(|| cf_name(names));
    }
    // count_filter lowercases + strips a trailing 's' ("Strikes"->"strike"); the
    // `es`-fallback covers sibilant plurals it misses ("Finishes"->"finish").
    count_filter(d).or_else(|| d.strip_suffix("es").and_then(count_filter))
}

/// The card-selector inside a "for each `<X>` flipped" per-count over the turn's
/// flips — an attack type, a Stop, an "X in the name" substring, or bare "card"
/// (every flip). `None` for a descriptor with no filter.
fn flipped_filter(desc: &str) -> Option<CardFilter> {
    let d = desc.trim();
    if d.eq_ignore_ascii_case("stop") || d.eq_ignore_ascii_case("stops") {
        return Some(CardFilter {
            is_stop: Some(true),
            ..Default::default()
        });
    }
    recur_filter(d)
}

/// "If you have a(nother) `<desc>` in play, …" as a `HasInPlay(SELF, …, ≥1)` gate.
/// `None` for descriptors with no CardFilter (e.g. "stop").
fn has_in_play_desc(desc: &str) -> Option<Condition> {
    Some(has_in_play(Who::SelfSide, count_filter(desc.trim())?, 1))
}

fn draw(n: i64, who: Who, source: DeckEnd, per: Option<CardFilter>, per_who: Who) -> Action {
    Action::Draw {
        cap: None,
        per_excludes_trigger: false,
        n,
        source,
        who,
        per,
        per_who,
    }
}

fn modify_roll(
    who: Who,
    delta: i64,
    when: RollWhen,
    per: Option<CardFilter>,
    per_who: Who,
) -> Action {
    Action::ModifyRoll {
        who,
        delta,
        when,
        per,
        per_who,
        per_zone: CountZone::InPlay,
    }
}

fn discard(count: i64, who: Who, random: bool, per: Option<CardFilter>, per_who: Who) -> Action {
    Action::Discard {
        selector: CardFilter::default(),
        count,
        who,
        random,
        per,
        per_who,
        choose: false,
    }
}

/// "Look at your opponent's hand, choose N card(s) and discard it/them" — the
/// effect owner picks from the opponent's hand. `selector` gates which cards.
fn discard_choose(count: i64, selector: CardFilter) -> Action {
    Action::Discard {
        selector,
        count,
        who: Who::Opp,
        random: false,
        per: None,
        per_who: Who::SelfSide,
        choose: true,
    }
}

fn flip(n: i64, who: Who) -> Action {
    Action::Flip {
        n,
        who,
        per: None,
        per_who: Who::SelfSide,
        until: None,
        until_to_hand: false,
    }
}

/// "If this card is flipped, [you may] <self-action>" — the per-card flip
/// self-trigger family. `OnFlip{who:SELF}` (no count gate) fires per flipped card
/// during `run_self_flips`; the paired self-action ([`Action::AddSelfToHand`] /
/// `ShuffleSelfIntoDeck` / `PlaySelf`) acts on the just-flipped referent. `cond`
/// gates the fire ("during your turn" -> `DuringTurn{SELF}`); the "you may" rides on
/// [`Effect::optional`].
fn flip_self(action: Action, optional: bool, cond: Condition) -> Effect {
    Effect {
        optional,
        ..eff(
            Trigger::OnFlip {
                who: Who::SelfSide,
                count: None,
            },
            vec![action],
            cond,
            Duration::Instant,
        )
    }
}

/// "If this card is flipped for your Gimmick, <action>" — a per-card flip self-trigger
/// gated on the flip being gimmick-caused ([`Condition::FlippedForGimmick`], read from
/// `GameState::flip_provenance`).
fn flip_self_gimmick(action: Action, optional: bool) -> Effect {
    flip_self(action, optional, Condition::FlippedForGimmick)
}

/// "[randomly] add N of the flipped cards to your hand" -> [`Action::AddFlippedToHand`].
/// `count_word` is a number / "one" / "all" / "the" (`all`/`the` -> all matching);
/// `filter_word` is `cards` (any) or an attack type.
fn add_flipped_action(count_word: &str, filter_word: &str, random: bool) -> Action {
    let count = match count_word.to_ascii_lowercase().as_str() {
        "all" | "the" => None,
        "one" => Some(1),
        d => d.parse::<i64>().ok(),
    };
    let f = filter_word.to_ascii_lowercase();
    let f = f.strip_suffix('s').unwrap_or(&f);
    let filter = if f == "card" {
        CardFilter::default()
    } else {
        cf_atk(count_atk(f))
    };
    Action::AddFlippedToHand {
        count,
        filter,
        random,
    }
}

/// "<flipper> flips N cards for each <desc> <per_who> ha(s|ve) in play" — the
/// per-count flip family, mirroring [`per_draw`].
fn per_flip(n: i64, who: Who, desc: &str, per_who: Who) -> Option<Effect> {
    let per = count_filter(desc)?;
    Some(eff(
        on_hit(),
        vec![Action::Flip {
            n,
            who,
            per: Some(per),
            per_who,
            until: None,
            until_to_hand: false,
        }],
        Condition::Always,
        Duration::Instant,
    ))
}

/// "Flip cards until you flip a <desc>[, add that <desc> to your hand]" — the
/// flip-until family. Mills the deck one card at a time until a card matching
/// `desc` surfaces; that card goes to the hand when `to_hand`, else to the
/// discard with the rest. Returns `None` when `desc` is not a recognized filter.
fn flip_until(desc: &str, to_hand: bool) -> Option<Effect> {
    let until = count_filter(desc)?;
    Some(eff(
        on_hit(),
        vec![Action::Flip {
            n: 0,
            who: Who::SelfSide,
            per: None,
            per_who: Who::SelfSide,
            until: Some(until),
            until_to_hand: to_hand,
        }],
        Condition::Always,
        Duration::Instant,
    ))
}

/// "Look at / Reveal the top N cards of your deck, add M to your hand and flip
/// the others" — a self-deck [`Action::Scry`] that mills its leftovers
/// ([`ScryRest::Flip`]). "Look at" keeps the window private; "Reveal" makes the
/// ids public.
fn scry_flip(reveal: bool, top: i64, to_hand: i64) -> Action {
    Action::Scry {
        deck: Who::SelfSide,
        top,
        bottom: 0,
        reveal,
        to_hand,
        bury: 0,
        rest: ScryRest::Flip,
    }
}

fn bury(count: i64, who: Who) -> Action {
    Action::Bury {
        choose: false,
        selector: CardFilter::default(),
        count,
        who,
        random: false,
        source: BuryFrom::Discard,
        per: None,
        per_who: Who::SelfSide,
    }
}

/// "Bury `count` per `per`-matching card you have in play" (schema v83). `random` is
/// forced on for a HAND source (the hand owner sheds without choosing). The per-count
/// always ranges over the SELF board ("… for each `<X>` you have in play").
fn bury_per(count: i64, who: Who, source: BuryFrom, per: CardFilter, random: bool) -> Action {
    Action::Bury {
        choose: false,
        selector: CardFilter::default(),
        count,
        who,
        random: random || source == BuryFrom::Hand,
        source,
        per: Some(per),
        per_who: Who::SelfSide,
    }
}

/// The per-count / shuffle selector for a "… `<pre>` you have in play [with `<names>`
/// in the name]" clause — a name-substring filter when the name qualifier is present,
/// else the `<pre>` descriptor via [`recur_filter`] (card / type / order). `None` for
/// a descriptor with no CardFilter.
fn in_play_filter(pre: &str, names: Option<&str>) -> Option<CardFilter> {
    if let Some(n) = names {
        let names = quoted_names(n);
        return (!names.is_empty()).then(|| cf_name(names));
    }
    recur_filter(pre)
}

/// An `OnPlay` per-count bury effect (Cardona family); `None` if the per-descriptor
/// has no CardFilter.
fn per_bury(
    count: i64,
    who: Who,
    source: BuryFrom,
    pre: &str,
    names: Option<&str>,
    random: bool,
) -> Option<Effect> {
    let per = in_play_filter(pre, names)?;
    Some(eff(
        Trigger::OnPlay,
        vec![bury_per(count, who, source, per, random)],
        Condition::Always,
        Duration::Instant,
    ))
}

/// Bury `count` card(s) from a player's HAND (SRG hand disruption). `random` = the
/// hand owner loses a random card; `choose` = the EFFECT OWNER looks and picks (only
/// meaningful with `who == Opp`). Routes to the engine's `bury_from_hand`.
fn bury_hand(count: i64, who: Who, random: bool, choose: bool) -> Action {
    Action::Bury {
        choose,
        selector: CardFilter::default(),
        count,
        who,
        random,
        source: BuryFrom::Hand,
        per: None,
        per_who: Who::SelfSide,
    }
}

/// Bury a player's ENTIRE discard pile at random (Rejected!: "Each player
/// randomly buries their discard pile"). `count == DECK_SIZE` is the whole-pile
/// idiom (the engine's `bury_from_discard` clamps by breaking when the pile is
/// empty); `random` routes each pick through the RNG.
fn bury_whole_discard(who: Who) -> Action {
    Action::Bury {
        choose: false,
        selector: CardFilter::default(),
        count: DECK_SIZE as i64,
        who,
        random: true,
        source: BuryFrom::Discard,
        per: None,
        per_who: Who::SelfSide,
    }
}

fn buff(skill: Skill, delta: i64, who: Who) -> Action {
    Action::BuffSkill {
        skill,
        delta,
        who,
        duration: Duration::WhileInPlay,
        target_highest: false,
        per_crowd: false,
        cap: None,
        per: None,
        per_zone: CountZone::InPlay,
    }
}

/// A `BuffSkill` scaled by the count of the owner's in-play cards matching `per`
/// (clamped to `cap`) — "your Technique and Grapple are +1 for each card you have
/// in play with 'Breaker' in the name". `per: None` = a flat +`delta`.
fn buff_per(skill: Skill, delta: i64, per: Option<CardFilter>, cap: Option<i64>) -> Action {
    Action::BuffSkill {
        skill,
        delta,
        who: Who::SelfSide,
        duration: Duration::WhileInPlay,
        target_highest: false,
        per_crowd: false,
        cap,
        per,
        per_zone: CountZone::InPlay,
    }
}

fn max_hand(delta: i64, who: Who) -> Action {
    Action::MaxHandSize {
        delta,
        who,
        duration: Duration::WhileInPlay,
    }
}

/// The DQ-CAUSE trigger: "if [this card is] stopped" (the stopped card's own
/// side), shared by the whole family (task #94).
fn on_your_stop() -> Trigger {
    Trigger::OnStop {
        dir: Direction::Yours,
        order: None,
    }
}

/// "If stopped, you lose the match via `kind`" — an OnStop(Yours) self-loss gated on
/// `cond`: the loss fires only while `cond` holds. Pass `Always` for the plain form,
/// or `Not(escape)` for an "... unless <escape>" variant (the loss is voided when the
/// escape holds).
fn lose_via(kind: LoseKind, cond: Condition) -> Effect {
    eff(
        on_your_stop(),
        vec![Action::LoseBy {
            kind,
            who: Who::SelfSide,
        }],
        cond,
        Duration::Instant,
    )
}

/// "If stopped, discard N card(s) from your hand OR you lose the match via
/// disqualification" — the stopped player may pay the discard cost instead of taking
/// the DQ loss (task #94). Offered cost-first; a player who cannot pay is still
/// offered the discard (a minor fidelity gap on an empty hand). Existing nodes only.
fn discard_or_lose(count: i64) -> Effect {
    let pay = ChoiceOption {
        node_type: ChoiceOptionTag,
        label: format!("Discard {count} from your hand"),
        actions: vec![discard(count, Who::SelfSide, false, None, Who::SelfSide)],
    };
    let lose = ChoiceOption {
        node_type: ChoiceOptionTag,
        label: "Lose the match via disqualification".to_owned(),
        actions: vec![Action::LoseBy {
            kind: LoseKind::Disqualification,
            who: Who::SelfSide,
        }],
    };
    eff(
        on_your_stop(),
        vec![Action::Choice {
            options: vec![pay, lose],
        }],
        Condition::Always,
        Duration::Instant,
    )
}

/// "If stopped, unless you discard N <type> from your hand, you lose ..." — a
/// pay-or-lose where the cost is discarding a specific attack type (task #94). Like
/// [`discard_or_lose`] but the discard carries a type `selector`.
fn discard_type_or_lose(count: i64, atk_type: AtkType) -> Effect {
    let pay = ChoiceOption {
        node_type: ChoiceOptionTag,
        label: format!("Discard {count} {} from your hand", atk_type.name()),
        actions: vec![Action::Discard {
            selector: cf_atk(atk_type),
            count,
            who: Who::SelfSide,
            random: false,
            per: None,
            per_who: Who::SelfSide,
            choose: false,
        }],
    };
    let lose = ChoiceOption {
        node_type: ChoiceOptionTag,
        label: "Lose the match via disqualification".to_owned(),
        actions: vec![Action::LoseBy {
            kind: LoseKind::Disqualification,
            who: Who::SelfSide,
        }],
    };
    eff(
        on_your_stop(),
        vec![Action::Choice {
            options: vec![pay, lose],
        }],
        Condition::Always,
        Duration::Instant,
    )
}

/// The "take the loss" branch shared by the pay-or-lose Choice family (task #94).
fn dq_lose_option() -> ChoiceOption {
    ChoiceOption {
        node_type: ChoiceOptionTag,
        label: "Lose the match via disqualification".to_owned(),
        actions: vec![Action::LoseBy {
            kind: LoseKind::Disqualification,
            who: Who::SelfSide,
        }],
    }
}

/// An OnStop "pay `pay_actions` (labelled `label`) OR lose via disqualification"
/// Choice effect (task #94).
fn pay_or_lose(label: String, pay_actions: Vec<Action>) -> Effect {
    let pay = ChoiceOption {
        node_type: ChoiceOptionTag,
        label,
        actions: pay_actions,
    };
    eff(
        on_your_stop(),
        vec![Action::Choice {
            options: vec![pay, dq_lose_option()],
        }],
        Condition::Always,
        Duration::Instant,
    )
}

/// "If stopped, discard N from your hand and bury this card or lose ..." — pay the
/// discard-plus-bury-the-stopped-card cost, or take the loss.
fn discard_bury_or_lose(count: i64) -> Effect {
    pay_or_lose(
        format!("Discard {count} and bury this card"),
        vec![
            discard(count, Who::SelfSide, false, None, Who::SelfSide),
            Action::BuryThisCard,
        ],
    )
}

/// "If stopped, randomly bury your hand or you lose ..." — bury the ENTIRE hand at
/// random (the count caps the whole possible hand; the bury loops until the hand is
/// empty), or take the loss.
fn bury_hand_or_lose() -> Effect {
    pay_or_lose(
        "Randomly bury your hand".to_owned(),
        vec![bury_hand(DECK_SIZE as i64, Who::SelfSide, true, false)],
    )
}

fn has_in_play(who: Who, filter: CardFilter, count: i64) -> Condition {
    Condition::HasInPlay {
        who,
        filter,
        count,
        cmp: Comparator::Ge,
    }
}

/// `who`'s hand size `cmp` a literal `value` — "you have N or more cards in your
/// hand" (`Ge`, SELF) / "your opponent has N or fewer" (`Le`, OPP).
fn hand_size(cmp: Comparator, who: Who, value: i64) -> Condition {
    Condition::HandSizeCompare {
        cmp,
        vs: Vs::Value,
        value: Some(value),
        who,
    }
}

// ---------------------------------------------------------------------------
// Enum lookups
// ---------------------------------------------------------------------------

fn skill(text: &str) -> Skill {
    match text {
        "Power" => Skill::Power,
        "Agility" => Skill::Agility,
        "Technique" => Skill::Technique,
        "Submission" => Skill::Submission,
        "Grapple" => Skill::Grapple,
        "Strike" => Skill::Strike,
        other => unreachable!("skill regex admitted {other:?}"),
    }
}

/// A conjunction of skill names — "Power", "Technique and Grapple", "Power,
/// Technique, and Agility" (an optional trailing " skill"/" skills" is tolerated).
/// Empty if any token isn't a skill name (the caller then declines the rule).
fn skill_list(text: &str) -> Vec<Skill> {
    let normalized = text
        .replace(" skills", "")
        .replace(" skill", "")
        .replace(", and ", ", ")
        .replace(" and ", ", ");
    let mut out = Vec::new();
    for tok in normalized.split(',') {
        let t = tok.trim();
        if t.is_empty() {
            continue;
        }
        match t {
            "Power" | "Agility" | "Technique" | "Submission" | "Grapple" | "Strike" => {
                out.push(skill(t));
            }
            _ => return Vec::new(),
        }
    }
    out
}

fn atk(text: &str) -> AtkType {
    match text {
        "Strike" => AtkType::Strike,
        "Grapple" => AtkType::Grapple,
        "Submission" => AtkType::Submission,
        other => unreachable!("atk regex admitted {other:?}"),
    }
}

fn order(text: &str) -> PlayOrder {
    match text {
        "Lead" => PlayOrder::Lead,
        "Follow Up" => PlayOrder::Followup,
        "Finish" => PlayOrder::Finish,
        other => unreachable!("order regex admitted {other:?}"),
    }
}

/// Integer capture group `i` (handles a leading `+`/`-` sign).
fn num(c: &Captures, i: usize) -> i64 {
    c[i].parse().expect("numeric capture parses")
}

// ---------------------------------------------------------------------------
// Count / stop-target helper parsers
// ---------------------------------------------------------------------------

static COUNT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:(lead|follow up|finish) )?(strike|grapple|submission)$").unwrap()
});
static STOP_PART_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:(Lead|Follow Up|Finish) )?(Strike|Grapple|Submission)$").unwrap()
});

fn count_order(text: &str) -> PlayOrder {
    match text {
        "lead" => PlayOrder::Lead,
        "follow up" => PlayOrder::Followup,
        "finish" => PlayOrder::Finish,
        other => unreachable!("count order {other:?}"),
    }
}

fn count_atk(text: &str) -> AtkType {
    match text {
        "strike" => AtkType::Strike,
        "grapple" => AtkType::Grapple,
        "submission" => AtkType::Submission,
        other => unreachable!("count atk {other:?}"),
    }
}

/// Parse a count descriptor ("Lead", "Strike", "Lead Strike"), case-insensitive
/// with an optional trailing "s", into a [`CardFilter`], or `None`.
fn count_filter(text: &str) -> Option<CardFilter> {
    let t = text.trim().to_lowercase();
    let t = t.trim_end_matches('s');
    if let Some(m) = COUNT_RE.captures(t) {
        let order = m.get(1).map(|g| count_order(g.as_str()));
        return Some(CardFilter {
            play_order: order,
            atk_type: Some(count_atk(&m[2])),
            ..Default::default()
        });
    }
    // "stop" / "stops": a stop-card constraint (has no atk/order). This flows
    // through every caller — per-count draws/discards, HasInPlay gates, recur adds.
    if t == "stop" {
        return Some(CardFilter {
            is_stop: Some(true),
            ..Default::default()
        });
    }
    let play_order = match t {
        "lead" => PlayOrder::Lead,
        "follow up" => PlayOrder::Followup,
        "finish" => PlayOrder::Finish,
        _ => return None,
    };
    Some(CardFilter {
        play_order: Some(play_order),
        ..Default::default()
    })
}

/// Normalize one "stop any …" target part to a bare `<order?> <type>`: drop a
/// leading "any " (repeated in "Lead Submission or any Finish Submission") and a
/// trailing " card"/" cards" ("stop any Grapple card"), so STOP_PART_RE matches.
fn norm_stop_part(part: &str) -> &str {
    let p = part.trim();
    let p = p.strip_prefix("any ").unwrap_or(p);
    let p = p
        .strip_suffix(" cards")
        .or_else(|| p.strip_suffix(" card"))
        .unwrap_or(p);
    p.trim()
}

fn unstoppable(by_order: Option<PlayOrder>, by_name: Option<String>) -> Action {
    Action::Unstoppable {
        by_order,
        by_name,
        by_skillreq: false,
    }
}

/// "Cannot be stopped by Skill Requirement cards" — an `Unstoppable` keyed on the
/// stopper carrying a skill requirement.
fn unstoppable_skillreq() -> Action {
    Action::Unstoppable {
        by_order: None,
        by_name: None,
        by_skillreq: true,
    }
}

/// Parse a stopper play-order word ("Follow Ups" / "Leads" / "Finishes", with an
/// optional hyphen/plural) to a [`PlayOrder`].
fn stopper_order(s: &str) -> PlayOrder {
    let t = s.replace('-', " ").to_lowercase();
    let t = t
        .strip_suffix("es")
        .or_else(|| t.strip_suffix('s'))
        .unwrap_or(&t);
    match t {
        "lead" => PlayOrder::Lead,
        "follow up" => PlayOrder::Followup,
        "finish" => PlayOrder::Finish,
        other => unreachable!("stopper order {other:?}"),
    }
}

/// Strip a trailing "even if it cannot be stopped" / "that cannot be stopped"
/// override off a stop-any target, returning the bare target and whether the
/// override was present (every produced Stop then bypasses `Unstoppable`).
fn strip_stop_override(t: &str) -> (&str, bool) {
    for suf in [
        ", even if it cannot be stopped",
        " even if it cannot be stopped",
        " that cannot be stopped",
    ] {
        if let Some(head) = t.strip_suffix(suf) {
            return (head.trim(), true);
        }
    }
    (t, false)
}

/// Parse the guard of a conditional "If/When `<cond>`, this card cannot be stopped"
/// into a [`Condition`], covering the common gate shapes (Crowd Meter, skill-vs-opp,
/// hand size, in-play count / name-count / none, turn-roll value/skill, same skill).
/// `None` (the rule declines → stays `Unsupported`) for any shape not covered. The
/// engine evaluates this from the CARD OWNER's side with their turn roll context.
fn stop_condition(text: &str) -> Option<Condition> {
    static CROWD: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^the [Cc]rowd [Mm]eter is (\d+) or (greater|less)$").unwrap()
    });
    static SKILL_GT: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(&format!(
            r"^your {SK}(?: skill)? is greater than your opponent'?s {SK}(?: skill)?$"
        ))
        .unwrap()
    });
    static SKILL_GE_DELTA: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(&format!(
            r"^your {SK}(?: skill)? is at least (\d+) greater than your opponent'?s {SK}(?: skill)?$"
        ))
        .unwrap()
    });
    static HAND_SELF: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^you have (\d+) or more cards in (?:your )?hand$").unwrap());
    static HAND_OPP: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^your opponent has (\d+) (?:or fewer cards|cards?) in their hand$").unwrap()
    });
    static PLAY_CNT: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(&format!(
            r"^you have (\d+) other {ATK}s?(?: cards)? in play$"
        ))
        .unwrap()
    });
    static PLAY_NONE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^you have no (Lead|Follow Up|Finish|Strike|Grapple|Submission)s? in play$")
            .unwrap()
    });
    static PLAY_NAME: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"^you have (\d+) cards? in play with "([^"]+)" in the name$"#).unwrap()
    });
    // "you have a card in play with "X"[, "Y",] or "Z" in the name" — OR-list of
    // quoted names, ≥1 in play (the count form is PLAY_NAME above).
    static PLAY_NAMELIST: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"^you have a card in play with (.+) in the name$"#).unwrap());
    // "you do not have "X" in play" — the negated in-play gate.
    static NOT_HAVE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"^you do not have "([^"]+)" in play$"#).unwrap());
    // "you have at least N <Tag> cards in play" — a tag-count in-play gate (Spotlight).
    static PLAY_TAG: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^you have at least (\d+) (Spotlight) cards? in play$").unwrap()
    });
    // "you hit another card this turn" — the per-turn hit gate.
    static HIT_TURN: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^you hit another card this turn$").unwrap());
    // "you are not <Competitor>" — a competitor-identity gate (capitalized name).
    static COMPETITOR_NOT: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^you are not ([A-Z].*)$").unwrap());
    static ROLL_VAL: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^you rolled (\d+) for your turn roll$").unwrap());
    static ROLL_SK: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(&format!(r"^you rolled {SK} for your turn roll$")).unwrap());

    let t = text.trim();
    if let Some(c) = CROWD.captures(t) {
        let cmp = if &c[2] == "greater" {
            Comparator::Ge
        } else {
            Comparator::Le
        };
        return Some(Condition::CrowdMeterCompare {
            cmp,
            value: c[1].parse().ok()?,
        });
    }
    if let Some(c) = SKILL_GT.captures(t) {
        let (s1, s2) = (skill(&c[1]), skill(&c[2]));
        return Some(Condition::SkillCompare {
            skill: s1,
            cmp: Comparator::Gt,
            who: Who::SelfSide,
            vs: Vs::OppSame,
            value: None,
            vs_skill: (s1 != s2).then_some(s2),
        });
    }
    if let Some(c) = SKILL_GE_DELTA.captures(t) {
        let (s1, s2) = (skill(&c[1]), skill(&c[3]));
        return Some(Condition::SkillCompare {
            skill: s1,
            cmp: Comparator::Ge,
            who: Who::SelfSide,
            vs: Vs::OppSame,
            value: Some(c[2].parse().ok()?),
            vs_skill: (s1 != s2).then_some(s2),
        });
    }
    if let Some(c) = HAND_SELF.captures(t) {
        return Some(hand_size(Comparator::Ge, Who::SelfSide, c[1].parse().ok()?));
    }
    if let Some(c) = HAND_OPP.captures(t) {
        return Some(hand_size(Comparator::Le, Who::Opp, c[1].parse().ok()?));
    }
    if let Some(c) = PLAY_CNT.captures(t) {
        return Some(has_in_play(
            Who::SelfSide,
            cf_atk(atk(&c[2])),
            c[1].parse().ok()?,
        ));
    }
    if let Some(c) = PLAY_NONE.captures(t) {
        return Some(Condition::HasInPlay {
            who: Who::SelfSide,
            filter: count_filter(&c[1])?,
            count: 1,
            cmp: Comparator::Lt,
        });
    }
    if let Some(c) = PLAY_NAME.captures(t) {
        return Some(has_in_play(
            Who::SelfSide,
            cf_name(vec![c[2].to_owned()]),
            c[1].parse().ok()?,
        ));
    }
    if let Some(c) = PLAY_NAMELIST.captures(t) {
        let names = quoted_names(&c[1]);
        if !names.is_empty() {
            return Some(has_in_play(Who::SelfSide, cf_name(names), 1));
        }
    }
    if let Some(c) = NOT_HAVE.captures(t) {
        return Some(Condition::Not {
            item: Box::new(has_in_play(
                Who::SelfSide,
                cf_name(vec![c[1].to_owned()]),
                1,
            )),
        });
    }
    if let Some(c) = PLAY_TAG.captures(t) {
        return Some(has_in_play(
            Who::SelfSide,
            cf_tag(&c[2]),
            c[1].parse().ok()?,
        ));
    }
    if HIT_TURN.is_match(t) {
        return Some(Condition::HitThisTurn { who: Who::SelfSide });
    }
    if let Some(c) = COMPETITOR_NOT.captures(t) {
        return Some(Condition::Not {
            item: Box::new(Condition::CompetitorIs {
                name_contains: vec![c[1].to_owned()],
            }),
        });
    }
    if let Some(c) = ROLL_VAL.captures(t) {
        return Some(Condition::RollValue {
            cmp: Comparator::Eq,
            value: c[1].parse().ok()?,
        });
    }
    if let Some(c) = ROLL_SK.captures(t) {
        return Some(Condition::RollWasSkill {
            skill: skill(&c[1]),
            who: Who::SelfSide,
        });
    }
    if t == "you and your opponent rolled the same skill for your turn roll" {
        return Some(Condition::SameRolledSkill);
    }
    None
}

/// Peel a trailing `with "X" in the (name|text)` qualifier off a stop-target part,
/// returning the bare `<order?> <type>` head and the name/text `CardFilter` (or
/// `None`) — "Submission with \"Over the Top\" in the name".
fn strip_target_filter(part: &str) -> (&str, Option<CardFilter>) {
    static RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"^(.*?) with "([^"]+)" in the (name|text)$"#).unwrap());
    let p = part.trim();
    if let Some(c) = RE.captures(p) {
        let names = vec![c[2].to_owned()];
        let filter = if &c[3] == "name" {
            CardFilter {
                name_contains: names,
                ..Default::default()
            }
        } else {
            CardFilter {
                text_contains: names,
                ..Default::default()
            }
        };
        return (c.get(1).unwrap().as_str(), Some(filter));
    }
    (p, None)
}

/// Parse a "stop any …" target into `Stop` actions, or `None` if any part is not
/// a plain `<type>` / `<order> <type>` (handles the "X or Y" two-target form). A
/// trailing "(that / even if it) cannot be stopped" flags every Stop to bypass the
/// attack's `Unstoppable`; a `with "X" in the name/text` qualifier sets `target`.
fn stop_targets(text: &str) -> Option<Vec<Action>> {
    static OR_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+or\s+").unwrap());
    let (body, even_unstoppable) = strip_stop_override(text.trim());
    let mut stops = Vec::new();
    for part in OR_RE.split(body) {
        let (head, target) = strip_target_filter(part);
        let m = STOP_PART_RE.captures(norm_stop_part(head))?;
        stops.push(Action::Stop {
            order: m.get(1).map(|g| order(g.as_str())),
            atk_type: Some(atk(&m[2])),
            source_is_skillreq: false,
            even_unstoppable,
            target,
        });
    }
    if stops.is_empty() {
        None
    } else {
        Some(stops)
    }
}

fn stop_eff(target: &str, condition: Condition) -> Option<Effect> {
    let stops = stop_targets(target)?;
    Some(eff(Trigger::OnPlay, stops, condition, Duration::Instant))
}

fn per_roll(delta: i64, desc: &str, per_who: Who, trigger: Trigger) -> Option<Effect> {
    let per = count_filter(desc)?;
    Some(eff(
        trigger,
        vec![modify_roll(
            Who::SelfSide,
            delta,
            RollWhen::Next,
            Some(per),
            per_who,
        )],
        Condition::Always,
        Duration::Instant,
    ))
}

fn per_draw(n: i64, desc: &str, per_who: Who) -> Option<Effect> {
    let per = count_filter(desc)?;
    Some(eff(
        Trigger::OnPlay,
        vec![draw(n, Who::SelfSide, DeckEnd::Top, Some(per), per_who)],
        Condition::Always,
        Duration::Instant,
    ))
}

fn per_discard(n: i64, desc: &str) -> Option<Effect> {
    let per = count_filter(desc)?;
    Some(eff(
        Trigger::OnPlay,
        vec![discard(n, Who::Opp, false, Some(per), Who::SelfSide)],
        Condition::Always,
        Duration::Instant,
    ))
}

/// Remove N `selector` card(s) from the opponent's board to their discard. The
/// effect owner picks WHICH opponent card (`choose:false` + `who:Opp`), so "discard
/// N" and "choose N … and discard it" are the same node.
fn remove_opp(count: i64, selector: CardFilter) -> Action {
    Action::RemoveFromPlay {
        selector,
        who: Who::Opp,
        count,
        choose: false,
    }
}

/// The unconditional "Discard / choose N … your opponent has in play" (on-hit).
fn remove_opp_play(count: i64, selector: CardFilter) -> Option<Effect> {
    Some(eff(
        on_hit(),
        vec![remove_opp(count, selector)],
        Condition::Always,
        Duration::Instant,
    ))
}

// ---------------------------------------------------------------------------
// Grammar: (anchored regex, builder). Order is significant — first match wins.
// ---------------------------------------------------------------------------

type Builder = fn(&Captures) -> Option<Effect>;

const SK: &str = r"(Power|Technique|Agility|Strike|Submission|Grapple)";
const ATK: &str = r"(Strike|Grapple|Submission)";

fn rule(pattern: &str, builder: Builder) -> (Regex, Builder) {
    (
        Regex::new(&format!("^(?:{pattern})$")).expect("grammar regex compiles"),
        builder,
    )
}

fn finish_roll_bonus(delta: i64) -> Vec<Action> {
    vec![finish_bonus(delta, None, false)]
}

/// A [`Action::FinishRollBonus`] with the base-roll gate and per-count fields at
/// their defaults (the common case; base-gated riders build the node inline).
fn finish_bonus(delta: i64, when_skill: Option<Skill>, either: bool) -> Action {
    Action::FinishRollBonus {
        delta,
        when_skill,
        either,
        when_base_le: None,
        when_base_ge: None,
        per: None,
        per_who: Who::SelfSide,
        per_zone: CountZone::InPlay,
        per_divisor: None,
    }
}

/// A rolled-skill-gated breakout-roll bonus ("+1 to Strike during your breakout
/// rolls", Pineapple). `when_skill` = None applies to every breakout roll. schema v79
fn breakout_mod(delta: i64, when_skill: Option<Skill>) -> Action {
    Action::BreakoutModifier {
        delta,
        attempts: None,
        when_skill,
    }
}

#[allow(clippy::too_many_lines)]
fn build_rules() -> Vec<(Regex, Builder)> {
    vec![
        // "If this match has [No] [Dd]isqualifications, <effect>" — the DQ-state gate
        // (schema v83, MatchHasNoDisqualifications). Cardona's Pizza Cutter family; the
        // compound "…and the Crowd Meter is N or greater, …" variants fall to Unsupported.
        rule(
            r"If this match has [Nn]o [Dd]isqualifications,? your next turn roll is \+(\d+)",
            |c| {
                Some(eff(
                    on_hit(),
                    vec![modify_roll(
                        Who::SelfSide,
                        num(c, 1),
                        RollWhen::Next,
                        None,
                        Who::Opp,
                    )],
                    Condition::MatchHasNoDisqualifications,
                    Duration::Instant,
                ))
            },
        ),
        rule(
            r"If this match has [Nn]o [Dd]isqualifications,? your Finish rolls? (?:is|are) \+(\d+)",
            |c| {
                Some(eff(
                    Trigger::Static,
                    finish_roll_bonus(num(c, 1)),
                    Condition::MatchHasNoDisqualifications,
                    Duration::WhileInPlay,
                ))
            },
        ),
        rule(
            &format!(r"If this match has [Nn]o [Dd]isqualifications,? \+(\d+) to {SK}"),
            |c| {
                Some(eff(
                    Trigger::Static,
                    vec![finish_bonus(num(c, 1), Some(skill(&c[2])), false)],
                    Condition::MatchHasNoDisqualifications,
                    Duration::WhileInPlay,
                ))
            },
        ),
        rule(r"\+(\d+) to (?:your )?Finish rolls?", |c| {
            Some(eff(
                Trigger::Static,
                finish_roll_bonus(num(c, 1)),
                Condition::Always,
                Duration::WhileInPlay,
            ))
        }),
        rule(r"Your Finish rolls? (?:is|are) ([+-]\d+)", |c| {
            Some(eff(
                Trigger::Static,
                finish_roll_bonus(num(c, 1)),
                Condition::Always,
                Duration::WhileInPlay,
            ))
        }),
        rule(&format!(r"\+(\d+) to {SK}"), |c| {
            Some(eff(
                Trigger::Static,
                vec![Action::FinishBonus {
                    skill: skill(&c[2]),
                    delta: num(c, 1),
                }],
                Condition::Always,
                Duration::WhileInPlay,
            ))
        }),
        // Per-count skill buff keyed to an OR-list of name substrings, e.g. D2's
        // Thud!: "Your Agility skill is +1 for each card you have in play with
        // 'Hammer', 'Smash', 'High', or 'Strike' in the name" (154 cards use the
        // "for each card ... in the name" shape; the Chin/Spotlight scalers too).
        rule(
            &format!(
                r#"Your {SK} skill is \+(\d+) for each card you have in play with (.+?) in the name(?: \(Max \+(\d+)\))?"#
            ),
            |c| {
                let names = quoted_names(&c[3]);
                if names.is_empty() {
                    return None;
                }
                let cap = c.get(4).map(|m| m.as_str().parse::<i64>().unwrap());
                Some(eff(
                    Trigger::Static,
                    vec![Action::BuffSkill {
                        skill: skill(&c[1]),
                        delta: num(c, 2),
                        who: Who::SelfSide,
                        duration: Duration::WhileInPlay,
                        target_highest: false,
                        per_crowd: false,
                        cap,
                        per: Some(cf_name(names)),
                        per_zone: CountZone::InPlay,
                    }],
                    Condition::Always,
                    Duration::WhileInPlay,
                ))
            },
        ),
        rule(&format!(r"Your {SK} is \+(\d+)"), |c| {
            Some(eff(
                Trigger::Static,
                vec![buff(skill(&c[1]), num(c, 2), Who::SelfSide)],
                Condition::Always,
                Duration::WhileInPlay,
            ))
        }),
        // Count-of-type gate → a real skill buff: "When you have at least 2 Grapples
        // in play, your Submission is +1" (Ivelisse). "your X is +N" is a general
        // skill buff (folds into effective stats); gated on ≥N of an attack type.
        rule(
            &format!(
                r"(?:When|If) you have at least (\d+) {ATK}s? in play, your {SK} (?:is|are) \+(\d+)"
            ),
            |c| {
                Some(eff(
                    Trigger::Static,
                    vec![buff(skill(&c[3]), num(c, 4), Who::SelfSide)],
                    has_in_play(Who::SelfSide, cf_atk(atk(&c[2])), num(c, 1)),
                    Duration::WhileInPlay,
                ))
            },
        ),
        // Count-of-type gate → a finish-roll bonus: "If you have at least 2 Grapples
        // in play, +3 to Agility" (Ivelisse's Desert Eagle). "+N to X" is the printed
        // finish bonus form (finish-roll only); gated on ≥N of an attack type.
        rule(
            &format!(r"(?:When|If) you have at least (\d+) {ATK}s? in play, \+(\d+) to {SK}"),
            |c| {
                Some(eff(
                    Trigger::Static,
                    vec![Action::FinishBonus {
                        skill: skill(&c[4]),
                        delta: num(c, 3),
                    }],
                    has_in_play(Who::SelfSide, cf_atk(atk(&c[2])), num(c, 1)),
                    Duration::WhileInPlay,
                ))
            },
        ),
        // Name-in-play (≥1) gate → a skill buff on one or more skills: "When you have
        // a card with 'High' in the name in play, your Power, Technique, and Agility
        // are +1" (RVD); "…'Fire'…, your Power is +1" (Shattered Split).
        rule(
            r#"When you have a card with (.+?) in the name in play, your (.+?) (?:is|are) \+(\d+)"#,
            |c| {
                let names = quoted_names(&c[1]);
                let skills = skill_list(&c[2]);
                if names.is_empty() || skills.is_empty() {
                    return None;
                }
                let delta = num(c, 3);
                let actions = skills
                    .into_iter()
                    .map(|s| buff(s, delta, Who::SelfSide))
                    .collect();
                Some(eff(
                    Trigger::Static,
                    actions,
                    has_in_play(Who::SelfSide, cf_name(names), 1),
                    Duration::WhileInPlay,
                ))
            },
        ),
        // Multi-skill per-count buff, "Your X [and Y] are +N for each card you have in
        // play with 'Z' in the name/text" (Evee's Spell Breaker). The single-skill
        // "Your X skill is +N for each …" rule above wins first for its exact shape.
        rule(
            r#"Your (.+?) (?:is|are) \+(\d+) for each card you have in play with (.+?) in the (name|text)(?: \(Max \+(\d+)\))?"#,
            |c| {
                let skills = skill_list(&c[1]);
                let names = quoted_names(&c[3]);
                if skills.is_empty() || names.is_empty() {
                    return None;
                }
                let delta = num(c, 2);
                let cap = c.get(5).map(|m| m.as_str().parse::<i64>().unwrap());
                let filter = name_or_text_filter(&c[4], names);
                let actions = skills
                    .into_iter()
                    .map(|s| buff_per(s, delta, Some(filter.clone()), cap))
                    .collect();
                Some(eff(
                    Trigger::Static,
                    actions,
                    Condition::Always,
                    Duration::WhileInPlay,
                ))
            },
        ),
        // Same per-count buff phrased "+N to X [and Y] for each card … in the
        // name/text" (Witch's My Most Powerful Spell; Postal's "…in the text (Max +4)").
        rule(
            r#"\+(\d+) to (.+?) for each card you have in play with (.+?) in the (name|text)(?: \(Max \+(\d+)\))?"#,
            |c| {
                let skills = skill_list(&c[2]);
                let names = quoted_names(&c[3]);
                if skills.is_empty() || names.is_empty() {
                    return None;
                }
                let delta = num(c, 1);
                let cap = c.get(5).map(|m| m.as_str().parse::<i64>().unwrap());
                let filter = name_or_text_filter(&c[4], names);
                let actions = skills
                    .into_iter()
                    .map(|s| buff_per(s, delta, Some(filter.clone()), cap))
                    .collect();
                Some(eff(
                    Trigger::Static,
                    actions,
                    Condition::Always,
                    Duration::WhileInPlay,
                ))
            },
        ),
        rule(r"Each player draws? (\d+) cards?", |c| {
            let n = num(c, 1);
            Some(eff(
                on_hit(),
                vec![
                    draw(n, Who::SelfSide, DeckEnd::Top, None, Who::SelfSide),
                    draw(n, Who::Opp, DeckEnd::Top, None, Who::SelfSide),
                ],
                Condition::Always,
                Duration::Instant,
            ))
        }),
        rule(
            r"Each player reveals the top card of their deck and adds it to their hand",
            |_| {
                Some(eff(
                    on_hit(),
                    vec![
                        draw(1, Who::SelfSide, DeckEnd::Top, None, Who::SelfSide),
                        draw(1, Who::Opp, DeckEnd::Top, None, Who::SelfSide),
                    ],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        rule(
            r"Each player buries (\d+) cards? in their opponent'?s discard pile",
            |c| {
                let n = num(c, 1);
                Some(eff(
                    on_hit(),
                    vec![bury(n, Who::Opp), bury(n, Who::SelfSide)],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        // Rejected! (D2 backstop): nuke both discard piles at random — denies
        // discard recursion (D2's whole hammer engine).
        rule(r"Each player randomly buries their discard pile", |_| {
            Some(eff(
                on_hit(),
                vec![
                    bury_whole_discard(Who::SelfSide),
                    bury_whole_discard(Who::Opp),
                ],
                Condition::Always,
                Duration::Instant,
            ))
        }),
        // "Each player randomly discards N cards from their hand" (Defector's
        // Disruptor + 5 more) — symmetric random hand loss; `Who` has no EACH, so
        // it is two Discard actions in one effect.
        rule(
            r"Each player randomly discards (\d+) cards? from their hand",
            |c| {
                Some(eff(
                    on_hit(),
                    vec![
                        discard(num(c, 1), Who::SelfSide, true, None, Who::SelfSide),
                        discard(num(c, 1), Who::Opp, true, None, Who::SelfSide),
                    ],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        // Impact is Family (V2) entrance: blank the opponent's Spotlight Finishes
        // (continuous selector scan; mirrors A Trip to the Upside Down's Spotlight
        // blank). V1's broader "Spotlight cards" variant stays its own clause.
        rule(
            r"Your opponent'?s Spotlight Finishes have blank text",
            |_| {
                Some(eff(
                    Trigger::Static,
                    vec![Action::BlankText {
                        selector: CardFilter {
                            play_order: Some(PlayOrder::Finish),
                            tag: Some("Spotlight".to_owned()),
                            ..Default::default()
                        },
                        who: Who::Opp,
                    }],
                    Condition::Always,
                    Duration::WhileInPlay,
                ))
            },
        ),
        rule(r"Your opponent draws? (\d+) cards?", |c| {
            Some(eff(
                on_hit(),
                vec![draw(num(c, 1), Who::Opp, DeckEnd::Top, None, Who::SelfSide)],
                Condition::Always,
                Duration::Instant,
            ))
        }),
        rule(r"Draw (\d+) cards?", |c| {
            Some(eff(
                on_hit(),
                vec![draw(
                    num(c, 1),
                    Who::SelfSide,
                    DeckEnd::Top,
                    None,
                    Who::SelfSide,
                )],
                Condition::Always,
                Duration::Instant,
            ))
        }),
        rule(r"Draw the bottom (\d+) cards? of your deck", |c| {
            Some(eff(
                on_hit(),
                vec![draw(
                    num(c, 1),
                    Who::SelfSide,
                    DeckEnd::Bottom,
                    None,
                    Who::SelfSide,
                )],
                Condition::Always,
                Duration::Instant,
            ))
        }),
        rule(r"Shuffle your deck", |_| {
            Some(eff(
                on_hit(),
                vec![Action::ShuffleDeck { who: Who::SelfSide }],
                Condition::Always,
                Duration::Instant,
            ))
        }),
        rule(r"Look at your opponent'?s hand", |_| {
            Some(eff(
                on_hit(),
                vec![Action::Peek { who: Who::Opp }],
                Condition::Always,
                Duration::Instant,
            ))
        }),
        rule(
            r"Your next turn roll is \+(\d+) for each (?:other )?(.+?) you have in play",
            |c| {
                let per = count_filter(&c[2])?;
                Some(eff(
                    on_hit(),
                    vec![modify_roll(
                        Who::SelfSide,
                        num(c, 1),
                        RollWhen::Next,
                        Some(per),
                        Who::SelfSide,
                    )],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        rule(
            r"Your next turn roll is \+(\d+) for each (.+?) in your discard pile",
            |c| {
                let per = count_filter(&c[2])?;
                Some(eff(
                    on_hit(),
                    vec![Action::ModifyRoll {
                        who: Who::SelfSide,
                        delta: num(c, 1),
                        when: RollWhen::Next,
                        per: Some(per),
                        per_who: Who::SelfSide,
                        per_zone: CountZone::Discard,
                    }],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        rule(r"Your next turn roll is \+(\d+)", |c| {
            Some(eff(
                on_hit(),
                vec![modify_roll(
                    Who::SelfSide,
                    num(c, 1),
                    RollWhen::Next,
                    None,
                    Who::Opp,
                )],
                Condition::Always,
                Duration::Instant,
            ))
        }),
        rule(r"\+(\d+) to your next turn roll", |c| {
            Some(eff(
                on_hit(),
                vec![modify_roll(
                    Who::SelfSide,
                    num(c, 1),
                    RollWhen::Next,
                    None,
                    Who::Opp,
                )],
                Condition::Always,
                Duration::Instant,
            ))
        }),
        rule(r"Your turn roll is \+(\d+)", |c| {
            Some(eff(
                on_hit(),
                vec![modify_roll(
                    Who::SelfSide,
                    num(c, 1),
                    RollWhen::This,
                    None,
                    Who::Opp,
                )],
                Condition::Always,
                Duration::Instant,
            ))
        }),
        rule(r"Your opponent's next turn roll is -(\d+)", |c| {
            Some(eff(
                on_hit(),
                vec![modify_roll(
                    Who::Opp,
                    -num(c, 1),
                    RollWhen::Next,
                    None,
                    Who::Opp,
                )],
                Condition::Always,
                Duration::Instant,
            ))
        }),
        rule(&format!(r"Your opponent's {SK} is -(\d+)"), |c| {
            Some(eff(
                Trigger::Static,
                vec![buff(skill(&c[1]), -num(c, 2), Who::Opp)],
                Condition::Always,
                Duration::WhileInPlay,
            ))
        }),
        rule(r"Each player's maximum hand ?size is ([+-]\d+)", |c| {
            let d = num(c, 1);
            Some(eff(
                Trigger::Static,
                vec![max_hand(d, Who::SelfSide), max_hand(d, Who::Opp)],
                Condition::Always,
                Duration::WhileInPlay,
            ))
        }),
        rule(
            r"(?:Your opponent's|Your target's|Their) maximum hand ?size is ([+-]\d+)",
            |c| {
                Some(eff(
                    Trigger::Static,
                    vec![max_hand(num(c, 1), Who::Opp)],
                    Condition::Always,
                    Duration::WhileInPlay,
                ))
            },
        ),
        rule(r"Your maximum hand ?size is ([+-]\d+)", |c| {
            Some(eff(
                Trigger::Static,
                vec![max_hand(num(c, 1), Who::SelfSide)],
                Condition::Always,
                Duration::WhileInPlay,
            ))
        }),
        // DQ-CAUSE family (task #94). The plain "If [this card is] stopped[,] you lose
        // the match via disqualification[s]" self-loss — case/punctuation/plural
        // insensitive, so the many DB spellings collapse to one rule.
        rule(
            r"(?i)If (?:this card is )?stopped,? you lose the match via disqualifications?",
            |_| Some(lose_via(LoseKind::Disqualification, Condition::Always)),
        ),
        // "If stopped, unless <cond>, you lose ..." / "... you lose ... unless <cond>":
        // the loss is voided when the escape condition holds. The condition delegates
        // to `stop_condition`; an unparsed escape declines (stays Unsupported) rather
        // than silently dropping the guard.
        rule(
            r"(?i)If stopped, unless (.+?),? you lose the match via disqualifications?",
            |c| {
                let cond = stop_condition(&c[1])?;
                Some(lose_via(
                    LoseKind::Disqualification,
                    Condition::Not {
                        item: Box::new(cond),
                    },
                ))
            },
        ),
        rule(
            r"(?i)If stopped, you lose the match via disqualifications? unless (.+)",
            |c| {
                let cond = stop_condition(&c[1])?;
                Some(lose_via(
                    LoseKind::Disqualification,
                    Condition::Not {
                        item: Box::new(cond),
                    },
                ))
            },
        ),
        // "If stopped, and <cond>, you lose ..." — the loss fires when <cond> holds
        // (the phrase carries its own polarity, e.g. "you do not have X in play").
        rule(
            r"(?i)If stopped,? and (.+?), you lose the match via disqualifications?",
            |c| Some(lose_via(LoseKind::Disqualification, stop_condition(&c[1])?)),
        ),
        // "If stopped, unless you discard N <type> from your hand, you lose ..." —
        // pay a typed-discard cost or take the loss.
        rule(
            r"(?i)If stopped, unless you discard (\d+) (Strike|Grapple|Submission)s? from your hand, you lose the match via disqualifications?",
            |c| Some(discard_type_or_lose(num(c, 1), atk(&c[2]))),
        ),
        // "If stopped, discard N from your hand and bury this card or lose ..." —
        // pay the discard + bury-the-stopped-card cost, or take the loss.
        rule(
            r"(?i)If stopped, discard (\d+) cards? from your hand and bury this card or lose the match via disqualifications?",
            |c| Some(discard_bury_or_lose(num(c, 1))),
        ),
        // "If stopped, randomly bury your hand or you lose ..." — bury the whole hand.
        rule(
            r"(?i)If stopped, randomly bury your hand or you lose the match via disqualifications?",
            |_| Some(bury_hand_or_lose()),
        ),
        // "If your opponent rolls N for their Breakout roll, you lose ..." — an
        // OnBreakoutRoll(Opp) loss gated on the rolled value (task #94).
        rule(
            r"(?i)If your opponent rolls (\d+) for their Breakout roll, you (?:immediately )?lose the match via disqualifications?",
            |c| {
                Some(eff(
                    Trigger::OnBreakoutRoll { who: Who::Opp },
                    vec![Action::LoseBy {
                        kind: LoseKind::Disqualification,
                        who: Who::SelfSide,
                    }],
                    Condition::RollValue {
                        cmp: Comparator::Eq,
                        value: num(c, 1),
                    },
                    Duration::Instant,
                ))
            },
        ),
        // "If stopped, discard N card(s) from your hand or you lose ..." — pay-or-lose.
        rule(
            r"(?i)If stopped, discard (\d+) cards? from your hand or you lose the match via disqualifications?",
            |c| Some(discard_or_lose(num(c, 1))),
        ),
        // "If stopped, discard N card(s) from your hand and you lose ..." — both the
        // discard and the loss happen (an AND rider, not a choice).
        rule(
            r"(?i)If stopped, discard (\d+) cards? from your hand and you lose the match via disqualifications?",
            |c| {
                Some(eff(
                    on_your_stop(),
                    vec![
                        discard(num(c, 1), Who::SelfSide, false, None, Who::SelfSide),
                        Action::LoseBy {
                            kind: LoseKind::Disqualification,
                            who: Who::SelfSide,
                        },
                    ],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        rule(r"If stopped, you lose the match via pinfall", |_| {
            Some(eff(
                Trigger::OnStop {
                    dir: Direction::Yours,
                    order: None,
                },
                vec![Action::LoseBy {
                    kind: LoseKind::Pinfall,
                    who: Who::SelfSide,
                }],
                Condition::Always,
                Duration::Instant,
            ))
        }),
        rule(r"Flip (?:up to )?(\d+) cards?", |c| {
            Some(eff(
                on_hit(),
                vec![flip(num(c, 1), Who::SelfSide)],
                Condition::Always,
                Duration::Instant,
            ))
        }),
        rule(r"Your opponent flips (\d+) cards?", |c| {
            Some(eff(
                on_hit(),
                vec![flip(num(c, 1), Who::Opp)],
                Condition::Always,
                Duration::Instant,
            ))
        }),
        rule(r"Each player flips (\d+) cards?", |c| {
            let n = num(c, 1);
            Some(eff(
                on_hit(),
                vec![flip(n, Who::SelfSide), flip(n, Who::Opp)],
                Condition::Always,
                Duration::Instant,
            ))
        }),
        rule(
            r"Flip cards? until you(?:r)? flip a (.+?), add (?:that .+?|it) to your hand",
            |c| flip_until(&c[1], true),
        ),
        rule(r"Flip cards? until you(?:r)? flip a (.+)", |c| {
            flip_until(&c[1], false)
        }),
        rule(
            r"(Look at|Reveal) the top (\d+) cards? of your deck[,;] ?(?:and )?(?:add|put) (\d+)(?: cards?)? (?:to|in) your hand,?(?: and)? flip the others?",
            |c| {
                Some(eff(
                    on_hit(),
                    vec![scry_flip(&c[1] == "Reveal", num(c, 2), num(c, 3))],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        // Gated reveal+flip: "If you have a(nother) <order> in play, look at the top N
        // cards of your deck; put M in your hand, and flip the others" -> the same
        // scry_flip Scry, but condition-gated on HasInPlay{<order>}. Reuses the body
        // pattern above with a lowercase "look"/"reveal" (mid-sentence).
        rule(
            r"If you have a(?:nother)? (.+?) in play, (look at|reveal) the top (\d+) cards? of your deck[,;] ?(?:and )?(?:add|put) (\d+)(?: cards?)? (?:to|in) your hand,?(?: and)? flip the others?",
            |c| {
                let cond = has_in_play_desc(&c[1])?;
                let reveal = c[2].eq_ignore_ascii_case("reveal");
                Some(eff(
                    on_hit(),
                    vec![scry_flip(reveal, num(c, 3), num(c, 4))],
                    cond,
                    Duration::Instant,
                ))
            },
        ),
        // Compound flip + recur-to-hand: "Flip N cards, then take/add M <filter>
        // from your discard pile [and add it] to your hand" -> Flip then
        // AddFromDiscard (which pulls one, as the standalone recur rule does; the
        // flipped cards land in discard first, so they are eligible to be recurred).
        rule(
            r"Flip (\d+) cards?,(?: and)?(?: then)? (?:take|add) \d+ (.+?) (?:from|in) your discard pile (?:and add (?:it|them) )?to your hand",
            |c| {
                let filter = recur_filter(&c[2])?;
                Some(eff(
                    on_hit(),
                    vec![
                        flip(num(c, 1), Who::SelfSide),
                        Action::AddFromDiscard { filter },
                    ],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        // Per-card flip self-trigger: "If this card is flipped, [you may] <self-action>."
        // Trigger OnFlip{SELF} fires per flipped card; the self-action acts on the
        // referent. "you may" -> Effect::optional. (Comma optional; "flipped you may"
        // appears both with and without it.)
        rule(
            r"If this card is flipped,?(?: (you may))? add it to your hand",
            |c| {
                Some(flip_self(
                    Action::AddSelfToHand,
                    c.get(1).is_some(),
                    Condition::Always,
                ))
            },
        ),
        // "shuffle it [back] into your deck" / "shuffle it from your discard pile back
        // into your deck" (mandatory) / the "shuffleit" typo. -> ShuffleSelfIntoDeck.
        rule(
            r"If this card is flipped,?(?: (you may))? shuffle ?it(?: from your discard pile)?(?: back)? into your deck",
            |c| {
                Some(flip_self(
                    Action::ShuffleSelfIntoDeck,
                    c.get(1).is_some(),
                    Condition::Always,
                ))
            },
        ),
        // "you may play it[ as an additional card this turn]" -> PlaySelf (the play is
        // itself the bonus action, so "as an additional card" folds in).
        rule(
            r"If this card is flipped,?(?: (you may))? play it(?: as an additional card this turn)?",
            |c| {
                Some(flip_self(
                    Action::PlaySelf,
                    c.get(1).is_some(),
                    Condition::Always,
                ))
            },
        ),
        // "If this card is flipped during your turn, you may play it" -> gated on
        // DuringTurn{SELF} (the flip must land on the owner's turn). Also the
        // "During your turn, if this card is flipped …" prefix form (same semantics).
        rule(
            r"If this card is flipped during your turn,?(?: (you may))? play it",
            |c| {
                Some(flip_self(
                    Action::PlaySelf,
                    c.get(1).is_some(),
                    Condition::DuringTurn { who: Who::SelfSide },
                ))
            },
        ),
        rule(
            r"During your turn, if this card is flipped,?(?: (you may))? play it",
            |c| {
                Some(flip_self(
                    Action::PlaySelf,
                    c.get(1).is_some(),
                    Condition::DuringTurn { who: Who::SelfSide },
                ))
            },
        ),
        // Flip-pool select (schema v88): "Flip N cards, [randomly] add M of the flipped
        // cards to your hand" / "Flip 6, add all flipped Strikes to your hand" — the flip
        // fills the pool (flipped_this_turn), then AddFlippedToHand pulls M matching from
        // it. ("all"/"the" -> all matching; "randomly" -> RNG pick.) Trigger-prefixed
        // ("When you flip …") and stat-gated variants are the deferred tail.
        rule(
            r"Flip (\d+)(?: cards?)?,? (?:and |then )?(randomly )?add (\d+|all|the|[Oo]ne) (?:of the )?flipped (cards?|Strikes?|Grapples?|Submissions?) to your hand",
            |c| {
                Some(eff(
                    on_hit(),
                    vec![
                        flip(num(c, 1), Who::SelfSide),
                        add_flipped_action(&c[3], &c[4], c.get(2).is_some()),
                    ],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        // Provenance-gated flip self-trigger (schema v87). "flipped by \"<X>\"": the
        // flip must have been caused by a card whose name matches -> FlippedByName. All
        // are the Set-Up-the-Ladder ladder-match cards; add-to-hand. (Comma optional.)
        rule(
            r#"If this card is flipped by ("[^"]+"),? add it to your hand"#,
            |c| {
                Some(flip_self(
                    Action::AddSelfToHand,
                    false,
                    Condition::FlippedByName {
                        names: quoted_names(&c[1]),
                    },
                ))
            },
        ),
        // "flipped for your Gimmick, <action>" -> FlippedForGimmick gate. The action
        // varies per card; each reuses an existing node. ("If flipped …" drops "this
        // card is"; "play it as a Follow Up" folds into PlaySelf, order-override
        // dropped.)
        rule(
            r"If flipped for your Gimmick,?(?: (you may))? shuffle your deck",
            |c| {
                Some(flip_self_gimmick(
                    Action::ShuffleDeck { who: Who::SelfSide },
                    c.get(1).is_some(),
                ))
            },
        ),
        rule(
            r"[Ii]f this card is flipped for your Gimmick,?(?: (you may))? play it(?: as a Follow Up)?",
            |c| Some(flip_self_gimmick(Action::PlaySelf, c.get(1).is_some())),
        ),
        rule(
            r"If this card is flipped for your Gimmick, your opponent randomly discards (\d+) cards? (?:from|in) their hand",
            |c| {
                Some(flip_self_gimmick(
                    discard(num(c, 1), Who::Opp, true, None, Who::SelfSide),
                    false,
                ))
            },
        ),
        rule(
            r"If this card is flipped for your Gimmick your turn roll is \+(\d+)",
            |c| {
                Some(flip_self_gimmick(
                    modify_roll(
                        Who::SelfSide,
                        num(c, 1),
                        RollWhen::Next,
                        None,
                        Who::SelfSide,
                    ),
                    false,
                ))
            },
        ),
        rule(
            r"Flip (\d+) cards? for each (?:other )?(.+?) you have in play",
            |c| per_flip(num(c, 1), Who::SelfSide, &c[2], Who::SelfSide),
        ),
        rule(
            r"Your opponent flips (\d+) cards? for each (?:other )?(.+?) you have in play",
            |c| per_flip(num(c, 1), Who::Opp, &c[2], Who::SelfSide),
        ),
        // Per-count buries "… for each <X> you have in play" (schema v83, Bury.per).
        // The count scales by the SELF board's matching cards; "other" is dropped (the
        // finish card is not yet in play at OnPlay time). Placed before the plain bury
        // rules — full-anchored, so these only claim the "for each" phrasings.
        rule(
            r"Bury (\d+) cards? in your opponent's discard pile for each (?:other )?(.+?) you have in play",
            |c| per_bury(num(c, 1), Who::Opp, BuryFrom::Discard, &c[2], None, false),
        ),
        rule(
            r"Your opponent (randomly )?buries (\d+) cards? in their hand for each (?:other )?(.+?) you have in play(?: with (.+?) in the name)?",
            |c| {
                let random = c.get(1).is_some();
                per_bury(
                    num(c, 2),
                    Who::Opp,
                    BuryFrom::Hand,
                    &c[3],
                    c.get(4).map(|m| m.as_str()),
                    random,
                )
            },
        ),
        rule(
            r"Bury (\d+) cards? in your hand for each (?:other )?(.+?) you have in play",
            |c| per_bury(num(c, 1), Who::SelfSide, BuryFrom::Hand, &c[2], None, false),
        ),
        rule(
            r"Bury (?:up to )?(\d+) cards? in your opponent's discard pile",
            |c| {
                Some(eff(
                    on_hit(),
                    vec![bury(num(c, 1), Who::Opp)],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        rule(
            r"Bury (?:up to )?(\d+) cards?(?: in your discard pile)?",
            |c| {
                Some(eff(
                    on_hit(),
                    vec![bury(num(c, 1), Who::SelfSide)],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        rule(
            r"[Yy]our opponent randomly discards (\d+) cards?(?: (?:from|in) their hand)?",
            |c| {
                Some(eff(
                    on_hit(),
                    vec![discard(num(c, 1), Who::Opp, true, None, Who::SelfSide)],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        rule(
            r"[Yy]our opponent discards (\d+) random cards?(?: (?:from|in) their hand)?",
            |c| {
                Some(eff(
                    on_hit(),
                    vec![discard(num(c, 1), Who::Opp, true, None, Who::SelfSide)],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        rule(
            r"[Yy]our opponent discards (\d+) cards?(?: (?:from|in) their hand)?",
            |c| {
                Some(eff(
                    on_hit(),
                    vec![discard(num(c, 1), Who::Opp, false, None, Who::SelfSide)],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        rule(
            r"[Rr]andomly discard (\d+) cards?(?: from your hand)?",
            |c| {
                Some(eff(
                    on_hit(),
                    vec![discard(num(c, 1), Who::SelfSide, true, None, Who::SelfSide)],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        rule(r"[Dd]iscard (\d+) random cards?(?: from your hand)?", |c| {
            Some(eff(
                on_hit(),
                vec![discard(num(c, 1), Who::SelfSide, true, None, Who::SelfSide)],
                Condition::Always,
                Duration::Instant,
            ))
        }),
        rule(r"[Dd]iscard (\d+) cards?(?: from your hand)?", |c| {
            Some(eff(
                on_hit(),
                vec![discard(
                    num(c, 1),
                    Who::SelfSide,
                    false,
                    None,
                    Who::SelfSide,
                )],
                Condition::Always,
                Duration::Instant,
            ))
        }),
        // --- In-play removal: discard an opponent's in-play card (task #121) ---
        // "Discard N cards your opponent has in play" / "Choose N ... and discard
        // it/them" are the same IR; the filtered form gates by order/atk.
        rule(r"[Dd]iscard (\d+) cards? your opponent has in play", |c| {
            remove_opp_play(num(c, 1), CardFilter::default())
        }),
        rule(
            r"[Cc]hoose (\d+) cards? your opponent has in play and discard (?:it|them)",
            |c| remove_opp_play(num(c, 1), CardFilter::default()),
        ),
        rule(r"[Dd]iscard (\d+) (.+?) your opponent has in play", |c| {
            remove_opp_play(num(c, 1), count_filter(&c[2])?)
        }),
        // Conditional / OnRoll in-play removal.
        rule(
            &format!(
                r"If you have another {ATK} in play, choose (\d+) cards? your opponent has in play and discard (?:it|them)"
            ),
            |c| {
                Some(eff(
                    Trigger::OnPlay,
                    vec![remove_opp(num(c, 2), CardFilter::default())],
                    has_in_play(Who::SelfSide, cf_atk(atk(&c[1])), 1),
                    Duration::Instant,
                ))
            },
        ),
        rule(
            r"If you have another (Lead|Follow Up|Finish) in play, choose (\d+) cards? your opponent has in play and discard (?:it|them)",
            |c| {
                Some(eff(
                    Trigger::OnPlay,
                    vec![remove_opp(num(c, 2), CardFilter::default())],
                    has_in_play(Who::SelfSide, cf_order(order(&c[1])), 1),
                    Duration::Instant,
                ))
            },
        ),
        rule(
            &format!(
                r"When you roll {SK} for your turn roll, choose (\d+) cards? your opponent has in play and discard (?:it|them)"
            ),
            |c| {
                Some(eff(
                    on_roll(skill(&c[1]), Who::SelfSide),
                    vec![remove_opp(num(c, 2), CardFilter::default())],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        // "If your opponent has a <X> in play, …" (X via count_filter, incl. "stop").
        rule(
            r"If your opponent has a (.+?) in play, draw (\d+) cards?",
            |c| {
                Some(eff(
                    Trigger::OnPlay,
                    vec![draw(
                        num(c, 2),
                        Who::SelfSide,
                        DeckEnd::Top,
                        None,
                        Who::SelfSide,
                    )],
                    has_in_play(Who::Opp, count_filter(&c[1])?, 1),
                    Duration::Instant,
                ))
            },
        ),
        rule(
            r"If your opponent has a (.+?) in play, choose (\d+) cards? your opponent has in play and discard (?:it|them)",
            |c| {
                Some(eff(
                    Trigger::OnPlay,
                    vec![remove_opp(num(c, 2), CardFilter::default())],
                    has_in_play(Who::Opp, count_filter(&c[1])?, 1),
                    Duration::Instant,
                ))
            },
        ),
        // --- Hand disruption: bury from a player's HAND (task #39) ------------
        // Opponent-hand-bury. Random variants first (word-order both ways),
        // then the plain (hand owner sheds), then the look-and-choose form.
        rule(
            r"[Yy]our opponent randomly buries (\d+) cards? in their hand",
            |c| {
                Some(eff(
                    on_hit(),
                    vec![bury_hand(num(c, 1), Who::Opp, true, false)],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        rule(
            r"[Yy]our opponent buries (\d+) random cards? in their hand",
            |c| {
                Some(eff(
                    on_hit(),
                    vec![bury_hand(num(c, 1), Who::Opp, true, false)],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        rule(r"[Yy]our opponent buries (\d+) cards? in their hand", |c| {
            Some(eff(
                on_hit(),
                vec![bury_hand(num(c, 1), Who::Opp, false, false)],
                Condition::Always,
                Duration::Instant,
            ))
        }),
        rule(
            r"[Ll]ook at your opponent'?s hand, choose (\d+) cards? and bury (?:it|them)",
            |c| {
                Some(eff(
                    on_hit(),
                    vec![bury_hand(num(c, 1), Who::Opp, false, true)],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        // Draw-then-bury-self rider ("Draw N cards, then bury M in your hand"):
        // dig for a card, then shed the least useful. Two independent counts.
        rule(
            r"[Dd]raw (\d+) cards?,? then bury (\d+) cards? in your hand",
            |c| {
                Some(eff(
                    on_hit(),
                    vec![
                        draw(num(c, 1), Who::SelfSide, DeckEnd::Top, None, Who::SelfSide),
                        bury_hand(num(c, 2), Who::SelfSide, false, false),
                    ],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        // Self-hand-bury and both-players.
        rule(r"[Bb]ury (\d+) cards? in your hand", |c| {
            Some(eff(
                on_hit(),
                vec![bury_hand(num(c, 1), Who::SelfSide, false, false)],
                Condition::Always,
                Duration::Instant,
            ))
        }),
        rule(
            r"[Ee]ach player randomly buries (\d+) cards? in their hand",
            |c| {
                let n = num(c, 1);
                Some(eff(
                    on_hit(),
                    vec![
                        bury_hand(n, Who::SelfSide, true, false),
                        bury_hand(n, Who::Opp, true, false),
                    ],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        rule(r"[Ee]ach player buries (\d+) cards? in their hand", |c| {
            let n = num(c, 1);
            Some(eff(
                on_hit(),
                vec![
                    bury_hand(n, Who::SelfSide, false, false),
                    bury_hand(n, Who::Opp, false, false),
                ],
                Condition::Always,
                Duration::Instant,
            ))
        }),
        // Conditional prefix: "If you have another <play order/skill> in play, your
        // opponent buries N card(s) in their hand."
        rule(
            &format!(
                r"If you have another {ATK} in play, your opponent buries (\d+) cards? in their hand"
            ),
            |c| {
                Some(eff(
                    Trigger::OnPlay,
                    vec![bury_hand(num(c, 2), Who::Opp, false, false)],
                    has_in_play(Who::SelfSide, cf_atk(atk(&c[1])), 1),
                    Duration::Instant,
                ))
            },
        ),
        rule(
            r"If you have another (Lead|Follow Up|Finish) in play, your opponent buries (\d+) cards? in their hand",
            |c| {
                let filter = CardFilter {
                    play_order: Some(order(&c[1])),
                    ..Default::default()
                };
                Some(eff(
                    Trigger::OnPlay,
                    vec![bury_hand(num(c, 2), Who::Opp, false, false)],
                    has_in_play(Who::SelfSide, filter, 1),
                    Duration::Instant,
                ))
            },
        ),
        // Look-and-choose discard from the opponent's hand (effect owner picks).
        // Filtered form ("... choose N Follow Up Strike and discard it") first.
        rule(
            &format!(
                r"[Ll]ook at your opponent'?s hand, choose (\d+) (?:(Lead|Follow Up|Finish) )?{ATK}(?: cards?)? and discard (?:it|them)"
            ),
            |c| {
                let filter = CardFilter {
                    play_order: c.get(2).map(|m| order(m.as_str())),
                    atk_type: Some(atk(&c[3])),
                    ..Default::default()
                };
                Some(eff(
                    on_hit(),
                    vec![discard_choose(num(c, 1), filter)],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        rule(
            r"[Ll]ook at your opponent'?s hand, choose (\d+) cards? and discard (?:it|them)",
            |c| {
                Some(eff(
                    on_hit(),
                    vec![discard_choose(num(c, 1), CardFilter::default())],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        // Recur from discard -> hand (task #122). Broadened from cards/atk-only to a
        // selector: "card(s)" (any), order/atk (count_filter), or name-substring
        // ("with \"X\" in the name"). `recur_filter` declines shapes we don't model
        // (e.g. "stop"), which then fall through to Unsupported. AddFromDiscard adds
        // ONE (count ignored, as the prior cards/atk rules did).
        rule(
            r"Add (\d+) (.+?) from your discard pile to your hand",
            |c| {
                Some(eff(
                    on_hit(),
                    vec![Action::AddFromDiscard {
                        filter: recur_filter(&c[2])?,
                    }],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        rule(
            r"Shuffle (?:up to )?(\d+) cards? from your discard pile into your deck",
            |_| {
                Some(eff(
                    on_hit(),
                    vec![Action::ShuffleIntoDeck {
                        selector: CardFilter::default(),
                        source: ShuffleSource::Discard,
                    }],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        // "Shuffle N <X> you have in play into your deck" (schema v83, ShuffleSource::
        // InPlay) — return a matching in-play card to the deck (Cardona Re-boot). Count
        // simplified to one, as the whole shuffle-into-deck family does.
        rule(
            r"Shuffle (?:up to )?\d+ (?:other )?(.+?) you have in play into your deck",
            |c| {
                Some(eff(
                    on_hit(),
                    vec![Action::ShuffleIntoDeck {
                        selector: recur_filter(&c[1])?,
                        source: ShuffleSource::InPlay,
                    }],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        // "Take N cards from your discard pile and shuffle them into your deck" — an
        // alias phrasing of the ShuffleIntoDeck recur.
        rule(
            r"Take (\d+) cards? from your discard pile and shuffle them into your deck",
            |_| {
                Some(eff(
                    on_hit(),
                    vec![Action::ShuffleIntoDeck {
                        selector: CardFilter::default(),
                        source: ShuffleSource::Discard,
                    }],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        // Recur from discard -> deck top, with the same selector as the hand recur.
        rule(
            r"Put (?:up to )?(\d+) (.+?) from your discard pile on top of your deck",
            |c| {
                Some(eff(
                    on_hit(),
                    vec![Action::RecurToDeckTop {
                        selector: recur_filter(&c[2])?,
                        count: num(c, 1),
                    }],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        // Conditional recur: "If you have a(nother) <X> in play, add/shuffle/put N …".
        rule(
            r"If you have a(?:nother)? (.+?) in play, add (\d+) (.+?) from your discard pile to your hand",
            |c| {
                Some(eff(
                    Trigger::OnPlay,
                    vec![Action::AddFromDiscard {
                        filter: recur_filter(&c[3])?,
                    }],
                    has_in_play_desc(&c[1])?,
                    Duration::Instant,
                ))
            },
        ),
        rule(
            r"If you have a(?:nother)? (.+?) in play, shuffle (?:up to )?(\d+) (.+?) from your discard pile into your deck",
            |c| {
                Some(eff(
                    Trigger::OnPlay,
                    vec![Action::ShuffleIntoDeck {
                        selector: recur_filter(&c[3])?,
                        source: ShuffleSource::Discard,
                    }],
                    has_in_play_desc(&c[1])?,
                    Duration::Instant,
                ))
            },
        ),
        rule(
            r"If you have a(?:nother)? (.+?) in play, put (?:up to )?(\d+) (.+?) from your discard pile on top of your deck",
            |c| {
                Some(eff(
                    Trigger::OnPlay,
                    vec![Action::RecurToDeckTop {
                        selector: recur_filter(&c[3])?,
                        count: num(c, 2),
                    }],
                    has_in_play_desc(&c[1])?,
                    Duration::Instant,
                ))
            },
        ),
        rule(
            &format!(
                r"If you have another {ATK} in play, put (?:up to )?(\d+) cards? from your discard pile on top of your deck"
            ),
            |c| {
                Some(eff(
                    Trigger::OnPlay,
                    vec![Action::RecurToDeckTop {
                        selector: CardFilter::default(),
                        count: num(c, 2),
                    }],
                    has_in_play(Who::SelfSide, cf_atk(atk(&c[1])), 1),
                    Duration::Instant,
                ))
            },
        ),
        rule(
            &format!(
                r"If you have another {ATK} in play, draw (\d+) cards? and your next turn roll is \+(\d+)"
            ),
            |c| {
                Some(eff(
                    Trigger::OnPlay,
                    vec![
                        draw(num(c, 2), Who::SelfSide, DeckEnd::Top, None, Who::SelfSide),
                        modify_roll(Who::SelfSide, num(c, 3), RollWhen::Next, None, Who::Opp),
                    ],
                    has_in_play(Who::SelfSide, cf_atk(atk(&c[1])), 1),
                    Duration::Instant,
                ))
            },
        ),
        // "(This card) cannot be stopped by <order>" — unstoppable against a stopper
        // of that play order (extends the original Follow-Ups-only rule to Leads and
        // Finishes and the "This card " lead-in).
        rule(
            r"(?:This card )?[Cc]annot be stopped by (Follow[ -]?Ups?|Leads?|Finish(?:es)?)",
            |c| {
                Some(eff(
                    Trigger::Static,
                    vec![unstoppable(Some(stopper_order(&c[1])), None)],
                    Condition::Always,
                    Duration::WhileInPlay,
                ))
            },
        ),
        // "(This card) cannot be stopped by \"X\"" — unstoppable against a stopper
        // whose name is X.
        rule(r#"(?:This card )?[Cc]annot be stopped by "([^"]+)""#, |c| {
            Some(eff(
                Trigger::Static,
                vec![unstoppable(None, Some(c[1].to_owned()))],
                Condition::Always,
                Duration::WhileInPlay,
            ))
        }),
        // "(This card / Your cards) cannot be stopped by Skill Requirement cards" —
        // unstoppable against a stopper carrying a skill requirement. Authored on a
        // main-deck card it shields that card; on a gimmick/entrance ("Your cards …")
        // the engine's standing scan applies it to every one of the owner's cards.
        rule(
            r"(?:This card |Your cards )?[Cc]annot be stopped by (?:cards with [Ss]kill [Rr]equirements|[Ss]kill [Rr]equirement cards)",
            |_| {
                Some(eff(
                    Trigger::Static,
                    vec![unstoppable_skillreq()],
                    Condition::Always,
                    Duration::WhileInPlay,
                ))
            },
        ),
        // "If/When <cond>, this card cannot be stopped [by <order>]": a condition-gated
        // Unstoppable. The guard is parsed by `stop_condition`; the engine evaluates it
        // from the card owner's side at stop time.
        rule(
            r"(?:If|When) (.+?),? this card cannot be stopped(?: by (Follow[ -]?Ups?|Leads?|Finish(?:es)?))?",
            |c| {
                let by_order = c.get(2).map(|m| stopper_order(m.as_str()));
                Some(eff(
                    Trigger::Static,
                    vec![unstoppable(by_order, None)],
                    stop_condition(&c[1])?,
                    Duration::WhileInPlay,
                ))
            },
        ),
        rule(
            r"This card counts as (\d+) (Lead|Follow [Uu]p|Finish) (Strike|Grapple|Submission)s? in play",
            |c| {
                let filter = count_filter(&format!("{} {}", &c[2], &c[3])).unwrap_or_default();
                Some(eff(
                    Trigger::Static,
                    vec![Action::CountsAsInPlay {
                        selector: filter,
                        count: num(c, 1),
                    }],
                    Condition::Always,
                    Duration::WhileInPlay,
                ))
            },
        ),
        rule(
            r"Your next turn roll is \+(\d+) for each (.+?) your opponent has in play",
            |c| per_roll(num(c, 1), &c[2], Who::Opp, on_hit()),
        ),
        rule(
            r"Your next turn roll is \+(\d+) for each (.+?) you have in play",
            |c| per_roll(num(c, 1), &c[2], Who::SelfSide, Trigger::OnPlay),
        ),
        rule(
            r"Draw (\d+) cards? for each (?:other )?(.+?) you have in play",
            |c| per_draw(num(c, 1), &c[2], Who::SelfSide),
        ),
        rule(
            r"Draw (\d+) cards? for each (?:other )?(.+?) your opponent has in play",
            |c| per_draw(num(c, 1), &c[2], Who::Opp),
        ),
        // --- Draw riders (task #49): deck-position, conditional, compare ------
        rule(r"[Dd]raw the bottom card of your deck", |_| {
            Some(eff(
                on_hit(),
                vec![draw(1, Who::SelfSide, DeckEnd::Bottom, None, Who::SelfSide)],
                Condition::Always,
                Duration::Instant,
            ))
        }),
        rule(r"[Dd]raw the top and bottom cards? of your deck", |_| {
            Some(eff(
                on_hit(),
                vec![
                    draw(1, Who::SelfSide, DeckEnd::Top, None, Who::SelfSide),
                    draw(1, Who::SelfSide, DeckEnd::Bottom, None, Who::SelfSide),
                ],
                Condition::Always,
                Duration::Instant,
            ))
        }),
        // "If you have another <atk>/<order> in play, draw N" (gated, OnPlay).
        rule(
            &format!(r"If you have another {ATK} in play, draw (\d+) cards?"),
            |c| {
                Some(eff(
                    Trigger::OnPlay,
                    vec![draw(
                        num(c, 2),
                        Who::SelfSide,
                        DeckEnd::Top,
                        None,
                        Who::SelfSide,
                    )],
                    has_in_play(Who::SelfSide, cf_atk(atk(&c[1])), 1),
                    Duration::Instant,
                ))
            },
        ),
        rule(
            r"If you have another (Lead|Follow Up|Finish) in play, draw (\d+) cards?",
            |c| {
                Some(eff(
                    Trigger::OnPlay,
                    vec![draw(
                        num(c, 2),
                        Who::SelfSide,
                        DeckEnd::Top,
                        None,
                        Who::SelfSide,
                    )],
                    has_in_play(Who::SelfSide, cf_order(order(&c[1])), 1),
                    Duration::Instant,
                ))
            },
        ),
        // "If your <S> skill is greater than your opponent's <S> skill, draw N"
        // (same- or cross-skill via vs_skill). The "... draw N instead" replacement
        // form is intentionally NOT matched (anchored $): it replaces a base draw.
        rule(
            &format!(
                r"If your {SK}(?: skill)? is greater than your opponent'?s {SK}(?: skill)?, draw (\d+) cards?"
            ),
            |c| {
                let own = skill(&c[1]);
                let other = skill(&c[2]);
                Some(eff(
                    Trigger::OnPlay,
                    vec![draw(
                        num(c, 3),
                        Who::SelfSide,
                        DeckEnd::Top,
                        None,
                        Who::SelfSide,
                    )],
                    Condition::SkillCompare {
                        skill: own,
                        cmp: Comparator::Gt,
                        who: Who::SelfSide,
                        vs: Vs::OppSame,
                        value: None,
                        vs_skill: (own != other).then_some(other),
                    },
                    Duration::Instant,
                ))
            },
        ),
        // "If you have fewer cards in your hand than your opponent, draw N."
        rule(
            r"If you have fewer cards in your hand than your opponent, draw (\d+) cards?",
            |c| {
                Some(eff(
                    Trigger::OnPlay,
                    vec![draw(
                        num(c, 1),
                        Who::SelfSide,
                        DeckEnd::Top,
                        None,
                        Who::SelfSide,
                    )],
                    Condition::HandSizeCompare {
                        cmp: Comparator::Lt,
                        vs: Vs::Opp,
                        value: None,
                        who: Who::SelfSide,
                    },
                    Duration::Instant,
                ))
            },
        ),
        // OnRoll draws: a standing "when you / your opponent roll <S> for the turn
        // roll, draw N" — fires while the card is in play (standing_effects scans it).
        rule(
            &format!(r"When you roll {SK} for your turn roll, draw (\d+) cards?"),
            |c| {
                Some(eff(
                    on_roll(skill(&c[1]), Who::SelfSide),
                    vec![draw(
                        num(c, 2),
                        Who::SelfSide,
                        DeckEnd::Top,
                        None,
                        Who::SelfSide,
                    )],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        rule(
            &format!(r"When your opponent rolls {SK} for their turn roll, draw (\d+) cards?"),
            |c| {
                Some(eff(
                    on_roll(skill(&c[1]), Who::Opp),
                    vec![draw(
                        num(c, 2),
                        Who::SelfSide,
                        DeckEnd::Top,
                        None,
                        Who::SelfSide,
                    )],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        rule(
            r"Your opponent discards (\d+) cards?(?: from their hand)? for each (.+?) you have in play",
            |c| per_discard(num(c, 1), &c[2]),
        ),
        rule(
            r"Your opponent randomly reveals (\d+) cards?(?: in their hand)? and discards all revealed [Ss]tops",
            |c| {
                Some(eff(
                    on_hit(),
                    vec![Action::RevealAndDiscard {
                        count: num(c, 1),
                        who: Who::Opp,
                    }],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        rule(
            r"If you have no other cards in your hand, this card is also a Lead",
            |_| {
                Some(eff(
                    Trigger::Static,
                    vec![Action::AlsoLead {
                        condition: Condition::HandSizeCompare {
                            cmp: Comparator::Le,
                            vs: Vs::Value,
                            value: Some(1),
                            who: Who::SelfSide,
                        },
                        order: PlayOrder::Lead,
                    }],
                    Condition::Always,
                    Duration::WhileInPlay,
                ))
            },
        ),
        rule(
            &format!(r"If you rolled {SK} for your turn roll,? this card is also a Follow Up"),
            |c| {
                Some(eff(
                    Trigger::Static,
                    vec![Action::AlsoLead {
                        condition: Condition::RollWasSkill {
                            skill: skill(&c[1]),
                            who: Who::SelfSide,
                        },
                        order: PlayOrder::Followup,
                    }],
                    Condition::Always,
                    Duration::WhileInPlay,
                ))
            },
        ),
        rule(
            r"If you bumped on the (?:previous|last) turn roll, this card is also a Follow Up",
            |_| {
                Some(eff(
                    Trigger::Static,
                    vec![Action::AlsoLead {
                        condition: Condition::BumpedLastTurnRoll,
                        order: PlayOrder::Followup,
                    }],
                    Condition::Always,
                    Duration::WhileInPlay,
                ))
            },
        ),
        rule(
            r"If you bumped on the last turn roll, double these bonuses",
            |_| {
                Some(eff(
                    Trigger::Static,
                    vec![Action::DoubleFinishIfBumped],
                    Condition::Always,
                    Duration::WhileInPlay,
                ))
            },
        ),
        rule(
            &format!(
                r"If either play(?:er)? rolls {SK} for their Finish roll, their roll is \+(\d+)"
            ),
            |c| {
                Some(eff(
                    Trigger::Static,
                    vec![finish_bonus(num(c, 2), Some(skill(&c[1])), true)],
                    Condition::Always,
                    Duration::WhileInPlay,
                ))
            },
        ),
        // "If you roll <S> for your Finish roll, it is +N" — a rolled-skill-gated
        // Finish bonus (self only). The consequent phrasing varies (it is / your roll
        // is / your Finish roll is); the delta must be SIGNED (+N add / -N reduce) —
        // a bare "N" would be a SET (different mechanic), left Unsupported.
        rule(
            &format!(
                r"If you roll {SK} for your Finish roll, (?:it is|your roll is|your Finish roll is) ([+-]\d+)"
            ),
            |c| {
                Some(eff(
                    Trigger::Static,
                    vec![finish_bonus(num(c, 2), Some(skill(&c[1])), false)],
                    Condition::Always,
                    Duration::WhileInPlay,
                ))
            },
        ),
        // "Your <S> skill is +N during Finish rolls" — a +N to the Finish roll when
        // that skill is rolled, i.e. the same rolled-skill-gated FinishRollBonus.
        rule(
            &format!(r"Your {SK} skill is ([+-]\d+) during Finish [Rr]olls"),
            |c| {
                Some(eff(
                    Trigger::Static,
                    vec![finish_bonus(num(c, 2), Some(skill(&c[1])), false)],
                    Condition::Always,
                    Duration::WhileInPlay,
                ))
            },
        ),
        // "+N to <S> during your breakout rolls" — a rolled-skill-gated breakout bonus
        // (Pineapple/Trash Can/Sledgehammer). Mirror of the Finish-roll rules above but
        // keyed on the breakout roll (BreakoutModifier.when_skill, v79). Self only; a
        // bare "+N" is unsigned (add) — breakout modifiers are always positive help.
        rule(
            &format!(r"\+(\d+) to {SK} during your [Bb]reakout [Rr]olls"),
            |c| {
                Some(eff(
                    Trigger::Static,
                    vec![breakout_mod(num(c, 1), Some(skill(&c[2])))],
                    Condition::Always,
                    Duration::WhileInPlay,
                ))
            },
        ),
        // "Your <S> (skill) is +N during (your) breakout rolls" — same bonus, alternate
        // phrasing (The SRG Boss V3's "Your Power is +N during … breakout rolls").
        rule(
            &format!(r"Your {SK} (?:skill )?is ([+-]\d+) during (?:your )?[Bb]reakout [Rr]olls"),
            |c| {
                Some(eff(
                    Trigger::Static,
                    vec![breakout_mod(num(c, 2), Some(skill(&c[1])))],
                    Condition::Always,
                    Duration::WhileInPlay,
                ))
            },
        ),
        // "Your Finish roll is +N for each <order/atk> you have in play" — a per-count
        // Finish bonus. `count_filter` declines name-based / capped forms (they stay
        // Unsupported), so only the clean order/atk-in-play shapes match here.
        rule(
            r"Your Finish rolls? (?:is|are) ([+-]\d+) for each (?:other )?(.+?) you have in play",
            |c| {
                let per = count_filter(&c[2])?;
                Some(eff(
                    Trigger::Static,
                    vec![Action::FinishRollBonus {
                        delta: num(c, 1),
                        when_skill: None,
                        either: false,
                        when_base_le: None,
                        when_base_ge: None,
                        per: Some(per),
                        per_who: Who::SelfSide,
                        per_zone: CountZone::InPlay,
                        per_divisor: None,
                    }],
                    Condition::Always,
                    Duration::WhileInPlay,
                ))
            },
        ),
        // Same, phrased "for every N <X> you have in play" — the divisor floors the
        // count ("+1 for every 3 Strikes you have in play", The Ride Along).
        rule(
            r"Your Finish rolls? (?:is|are) ([+-]\d+) for every (\d+) (?:other )?(.+?) you have in play",
            |c| {
                let per = count_filter(&c[3])?;
                Some(eff(
                    Trigger::Static,
                    vec![Action::FinishRollBonus {
                        delta: num(c, 1),
                        when_skill: None,
                        either: false,
                        when_base_le: None,
                        when_base_ge: None,
                        per: Some(per),
                        per_who: Who::SelfSide,
                        per_zone: CountZone::InPlay,
                        per_divisor: Some(num(c, 2)),
                    }],
                    Condition::Always,
                    Duration::WhileInPlay,
                ))
            },
        ),
        // Per-count Finish bonus over cards FLIPPED this turn: "your Finish roll is
        // +1 for each Strike card flipped" (Five Star Frog Splash). The filter reads
        // the flipped set — an attack type, a stop, or a name substring.
        rule(
            r#"Your Finish rolls? (?:is|are) ([+-]\d+) for each (.+?) (?:card )?(?:you )?flipped(?: for your [Gg]immick)?"#,
            |c| {
                let per = flipped_filter(&c[2])?;
                Some(eff(
                    Trigger::Static,
                    vec![Action::FinishRollBonus {
                        delta: num(c, 1),
                        when_skill: None,
                        either: false,
                        when_base_le: None,
                        when_base_ge: None,
                        per: Some(per),
                        per_who: Who::SelfSide,
                        per_zone: CountZone::FlippedThisTurn,
                        per_divisor: None,
                    }],
                    Condition::Always,
                    Duration::WhileInPlay,
                ))
            },
        ),
        // Skill buff scaled by the turn's flips: "For each Strike flipped: +1 to
        // Strike and Power" (Five Star Heart Punch). Multi-skill; folds into the
        // finish/turn roll via the derived stats over CountZone::FlippedThisTurn.
        rule(r"For each (.+?) flipped:? \+(\d+) to (.+)", |c| {
            let per = flipped_filter(&c[1])?;
            let skills = skill_list(&c[3]);
            if skills.is_empty() {
                return None;
            }
            let delta = num(c, 2);
            let actions = skills
                .into_iter()
                .map(|s| Action::BuffSkill {
                    skill: s,
                    delta,
                    who: Who::SelfSide,
                    duration: Duration::WhileInPlay,
                    target_highest: false,
                    per_crowd: false,
                    cap: None,
                    per: Some(per.clone()),
                    per_zone: CountZone::FlippedThisTurn,
                })
                .collect();
            Some(eff(
                Trigger::Static,
                actions,
                Condition::Always,
                Duration::WhileInPlay,
            ))
        }),
        // Base-roll-gated Finish bonus: "If your Finish roll is N or less/greater,
        // it is +M". The N-or-less/greater reads the BASE roll (skill stat pre-bonus);
        // +M is a SIGNED additive bonus (a bare "M" is a SET, left Unsupported).
        rule(
            r"If your Finish roll is (\d+) or less,? (?:it is|your Finish roll is) ([+-]\d+)",
            |c| {
                Some(eff(
                    Trigger::Static,
                    vec![Action::FinishRollBonus {
                        delta: num(c, 2),
                        when_skill: None,
                        either: false,
                        when_base_le: Some(num(c, 1)),
                        when_base_ge: None,
                        per: None,
                        per_who: Who::SelfSide,
                        per_zone: CountZone::InPlay,
                        per_divisor: None,
                    }],
                    Condition::Always,
                    Duration::WhileInPlay,
                ))
            },
        ),
        rule(
            r"If your Finish roll is (\d+) or greater,? (?:it is|your Finish roll is) ([+-]\d+)",
            |c| {
                Some(eff(
                    Trigger::Static,
                    vec![Action::FinishRollBonus {
                        delta: num(c, 2),
                        when_skill: None,
                        either: false,
                        when_base_le: None,
                        when_base_ge: Some(num(c, 1)),
                        per: None,
                        per_who: Who::SelfSide,
                        per_zone: CountZone::InPlay,
                        per_divisor: None,
                    }],
                    Condition::Always,
                    Duration::WhileInPlay,
                ))
            },
        ),
        rule(r"Stop any (.+)", |c| stop_eff(&c[1], Condition::Always)),
        rule(
            &format!(
                r"If your {SK}(?: skill)? is greater than your opponent'?s {SK}(?: skill)?, stop any (.+)"
            ),
            |c| {
                stop_eff(
                    &c[3],
                    Condition::SkillCompare {
                        skill: skill(&c[1]),
                        cmp: Comparator::Gt,
                        who: Who::SelfSide,
                        vs: Vs::OppSame,
                        value: None,
                        vs_skill: None,
                    },
                )
            },
        ),
        // "at least N greater than your opponent's <S>" = `self >= opp + N` (Ge,
        // value=N); the engine's SkillCompare vs-opponent branch adds the delta.
        rule(
            &format!(
                r"If your {SK}(?: skill)? is at least (\d+) greater than your opponent'?s {SK}(?: skill)?, stop any (.+)"
            ),
            |c| {
                let (s1, s2) = (skill(&c[1]), skill(&c[3]));
                stop_eff(
                    &c[4],
                    Condition::SkillCompare {
                        skill: s1,
                        cmp: Comparator::Ge,
                        who: Who::SelfSide,
                        vs: Vs::OppSame,
                        value: Some(num(c, 2)),
                        vs_skill: (s1 != s2).then_some(s2),
                    },
                )
            },
        ),
        rule(
            &format!(r"If your opponent has another {ATK} in play, stop any (.+)"),
            |c| stop_eff(&c[2], has_in_play(Who::Opp, cf_atk(atk(&c[1])), 1)),
        ),
        rule(
            &format!(r"If your opponent has (\d+) other {ATK}s in play,? stop any (.+)"),
            |c| stop_eff(&c[3], has_in_play(Who::Opp, cf_atk(atk(&c[2])), num(c, 1))),
        ),
        rule(
            r"If the [Cc]rowd [Mm]eter is (\d+) or greater, stop any (.+)",
            |c| {
                stop_eff(
                    &c[2],
                    Condition::CrowdMeterCompare {
                        cmp: Comparator::Ge,
                        value: num(c, 1),
                    },
                )
            },
        ),
        rule(
            r"If the [Cc]rowd [Mm]eter is (\d+) or less,? stop any (.+)",
            |c| {
                stop_eff(
                    &c[2],
                    Condition::CrowdMeterCompare {
                        cmp: Comparator::Le,
                        value: num(c, 1),
                    },
                )
            },
        ),
        // "does not have a <order type> in play" → the opponent's count of that
        // filter is 0 (`< 1`); the stop is live only when they hold none.
        rule(
            r"If your opponent does not have (?:an? )?(.+?) in play, stop any (.+)",
            |c| {
                let filter = count_filter(&c[1])?;
                stop_eff(
                    &c[2],
                    Condition::HasInPlay {
                        who: Who::Opp,
                        filter,
                        count: 1,
                        cmp: Comparator::Lt,
                    },
                )
            },
        ),
        rule(
            &format!(
                r"If the [Cc]rowd [Mm]eter is (\d+) or greater and your opponent has another {ATK} in play, stop any (.+)"
            ),
            |c| {
                stop_eff(
                    &c[3],
                    Condition::And {
                        items: vec![
                            Condition::CrowdMeterCompare {
                                cmp: Comparator::Ge,
                                value: num(c, 1),
                            },
                            has_in_play(Who::Opp, cf_atk(atk(&c[2])), 1),
                        ],
                    },
                )
            },
        ),
    ]
}

static RULES: LazyLock<Vec<(Regex, Builder)>> = LazyLock::new(build_rules);

// ---------------------------------------------------------------------------
// Clause splitting, frequency headers, metadata
// ---------------------------------------------------------------------------

/// Split rules text into clauses on newlines and sentence boundaries (a period
/// followed by whitespace). Mirrors `re.split(r"[\n\r]+|(?<=[.])\s+", text)`.
pub fn split_clauses(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.split(['\n', '\r']) {
        let mut cur = String::new();
        let mut chars = line.chars().peekable();
        while let Some(ch) = chars.next() {
            cur.push(ch);
            if ch == '.' && chars.peek().is_some_and(|w| w.is_whitespace()) {
                out.push(cur.trim().to_owned());
                cur.clear();
                while chars.peek().is_some_and(|w| w.is_whitespace()) {
                    chars.next();
                }
            }
        }
        out.push(cur.trim().to_owned());
    }
    out.into_iter().filter(|p| !p.is_empty()).collect()
}

/// A frequency-guard header ("Once per match:", "N times per match:") scoping the
/// clauses that follow, or `None`.
fn freq_header(clause: &str) -> Option<(Frequency, Option<i64>)> {
    static ONCE_MATCH: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^Once (?:per|a) match:?$").unwrap());
    static ONCE_TURN: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^Once (?:per|a) turn:?$").unwrap());
    static N_MATCH: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^(\d+) times per match:?$").unwrap());
    let stripped = clause.trim();
    if ONCE_MATCH.is_match(stripped) {
        return Some((Frequency::OncePerMatch, None));
    }
    if ONCE_TURN.is_match(stripped) {
        return Some((Frequency::OncePerTurn, None));
    }
    if let Some(m) = N_MATCH.captures(stripped) {
        return Some((Frequency::NPerMatch, Some(m[1].parse().unwrap())));
    }
    None
}

/// Non-effect metadata (a deck-build "Skill Requirement:" line): recognized and
/// skipped, neither an effect nor Unsupported.
fn is_metadata(clause: &str) -> bool {
    static META: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^Skill Requirement:").unwrap());
    META.is_match(clause.trim())
}

fn match_grammar(clause: &str) -> Option<Effect> {
    let stripped = clause.trim().trim_end_matches('.').trim();
    for (re, builder) in RULES.iter() {
        if let Some(caps) = re.captures(stripped) {
            if let Some(eff) = builder(&caps) {
                return Some(eff); // a builder may decline (unmodelled target/desc)
            }
        }
    }
    None
}

fn compile(clause: &str, source: EffectSource, freq: Frequency, n: Option<i64>) -> Effect {
    let g = FrequencyGuard {
        node_type: FrequencyGuardTag,
        kind: freq,
        n,
    };
    if let Some(mut eff) = match_grammar(clause) {
        eff.raw_clause = clause.to_owned();
        eff.source = source;
        eff.frequency = g;
        return eff;
    }
    Effect {
        node_type: EffectTag,
        trigger: Trigger::OnPlay,
        condition: Condition::Always,
        actions: vec![Action::Unsupported {
            raw_text: clause.to_owned(),
            reason: "no grammar match".to_owned(),
        }],
        duration: Duration::Instant,
        frequency: g,
        raw_clause: clause.to_owned(),
        source,
        optional: false,
    }
}

/// Compile `text` into Effects: overrides win, then grammar, then Unsupported.
pub fn parse_text(
    text: &str,
    source: EffectSource,
    db_uuid: Option<&str>,
    overrides: Option<&Overrides>,
) -> Vec<Effect> {
    if let (Some(ov), Some(uuid)) = (overrides, db_uuid) {
        if let Some(entries) = ov.get(uuid) {
            return entries.clone();
        }
    }
    let mut effects = Vec::new();
    let mut freq = Frequency::Unlimited;
    let mut n = None;
    for clause in split_clauses(text) {
        if let Some((f, nn)) = freq_header(&clause) {
            freq = f;
            n = nn;
            continue;
        }
        if is_metadata(&clause) {
            continue;
        }
        effects.push(compile(&clause, source, freq, n));
    }
    effects
}

/// Sum every `FinishBonus` action into `(skill, delta)` pairs (for a [`Card`]).
pub fn finish_bonuses(effects: &[Effect]) -> BTreeMap<Skill, i64> {
    let mut totals: BTreeMap<Skill, i64> = BTreeMap::new();
    for eff in effects {
        for action in &eff.actions {
            if let Action::FinishBonus { skill, delta } = action {
                *totals.entry(*skill).or_insert(0) += *delta;
            }
        }
    }
    totals
}

// ---------------------------------------------------------------------------
// Overrides + enrichment (bridge to the loader)
// ---------------------------------------------------------------------------

/// Load the pre-expanded override table (`db_uuid -> [full Effect]`) from JSON.
pub fn load_overrides(json: &str) -> crate::Result<Overrides> {
    Ok(serde_json::from_str(json)?)
}

/// Attach compiled effects and finish bonuses to a loader-built [`Card`].
pub fn enrich_card(mut card: Card, overrides: Option<&Overrides>) -> Card {
    let effects = parse_text(
        &card.raw_text,
        EffectSource::Card,
        Some(&card.db_uuid),
        overrides,
    );
    card.finish_bonuses = finish_bonuses(&effects);
    card.effects = effects;
    card
}

/// Attach compiled gimmick effects to a [`Competitor`].
pub fn enrich_competitor(mut comp: Competitor, overrides: Option<&Overrides>) -> Competitor {
    comp.effects = parse_text(
        &comp.gimmick_text,
        EffectSource::Gimmick,
        Some(&comp.db_uuid),
        overrides,
    );
    comp
}

/// Attach compiled entrance effects to an [`EntranceCard`].
pub fn enrich_entrance(mut ent: EntranceCard, overrides: Option<&Overrides>) -> EntranceCard {
    ent.effects = parse_text(
        &ent.raw_text,
        EffectSource::Entrance,
        Some(&ent.db_uuid),
        overrides,
    );
    ent
}

/// Compile every card / competitor / entrance in a deck into playable IR.
pub fn enrich_deck(deck: Deck, overrides: Option<&Overrides>) -> Deck {
    Deck {
        competitor: enrich_competitor(deck.competitor, overrides),
        entrance: enrich_entrance(deck.entrance, overrides),
        cards: deck
            .cards
            .into_iter()
            .map(|c| enrich_card(c, overrides))
            .collect(),
    }
}

// ---------------------------------------------------------------------------
// Coverage report (DESIGN.md §4)
// ---------------------------------------------------------------------------

/// Clause-level coverage over a record set (DESIGN.md §4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverageReport {
    pub total: usize,
    pub grammar: usize,
    pub override_: usize,
    pub unsupported: usize,
    pub top_unparsed: Vec<(String, usize)>,
}

impl CoverageReport {
    pub fn parsed(&self) -> usize {
        self.grammar + self.override_
    }

    pub fn rate(&self) -> f64 {
        if self.total == 0 {
            1.0
        } else {
            self.parsed() as f64 / self.total as f64
        }
    }
}

fn normalize_shape(clause: &str) -> String {
    static DIGITS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b\d+\b").unwrap());
    static SKILLS: LazyLock<Regex> = LazyLock::new(|| Regex::new(SK).unwrap());
    let shape = DIGITS.replace_all(clause, "N");
    SKILLS.replace_all(&shape, "<S>").trim().to_owned()
}

/// One record for the coverage tally: its text and (optional) db_uuid.
pub struct CoverageRecord<'a> {
    pub text: &'a str,
    pub db_uuid: Option<&'a str>,
}

/// Tally grammar / override / unsupported clauses across `records`.
pub fn coverage(records: &[CoverageRecord], overrides: Option<&Overrides>) -> CoverageReport {
    let (mut total, mut grammar, mut override_, mut unsupported) = (0, 0, 0, 0);
    // Insertion-ordered shape counts, so the count-desc sort below breaks ties by
    // first-seen order — matching Python's `Counter.most_common`.
    let mut shape_order: Vec<String> = Vec::new();
    let mut shape_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for rec in records {
        let clauses: Vec<String> = split_clauses(rec.text)
            .into_iter()
            .filter(|c| freq_header(c).is_none() && !is_metadata(c))
            .collect();
        let is_override =
            matches!((overrides, rec.db_uuid), (Some(ov), Some(u)) if ov.contains_key(u));
        if is_override {
            total += clauses.len();
            override_ += clauses.len();
            continue;
        }
        for clause in &clauses {
            total += 1;
            if match_grammar(clause).is_some() {
                grammar += 1;
            } else {
                unsupported += 1;
                let shape = normalize_shape(clause);
                shape_counts
                    .entry(shape.clone())
                    .and_modify(|c| *c += 1)
                    .or_insert_with(|| {
                        shape_order.push(shape.clone());
                        1
                    });
            }
        }
    }
    let mut top: Vec<(String, usize)> = shape_order
        .into_iter()
        .map(|s| {
            let c = shape_counts[&s];
            (s, c)
        })
        .collect();
    // Stable sort by count descending; ties keep first-seen (insertion) order.
    top.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    top.truncate(20);
    CoverageReport {
        total,
        grammar,
        override_,
        unsupported,
        top_unparsed: top,
    }
}
