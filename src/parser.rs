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
    CountZone, DeckEnd, Dest, Direction, DqScope, Duration, Effect, EffectSource, EffectTag,
    Frequency, FrequencyGuard, FrequencyGuardTag, LoseKind, MatchType, PlayOrder, RequireKind,
    RerollCost, RerollCostKind, RerollCostTag, RevealSource, RollWhen, ScryRest, SearchSource,
    ShuffleSource, Skill, Trigger, Vs, Who,
};
use regex::{Captures, Regex};
use std::collections::BTreeMap;
use std::sync::LazyLock;

/// The hand-authored override table: `db_uuid -> compiled effects`.
pub type Overrides = BTreeMap<String, Vec<Effect>>;

// --------------------------------------------------------------------------
// Effect / action / filter constructors — the small builders the grammar
// rules call. Grouped by concern so a rule author can scan for an existing
// helper before writing a new one (sections below, in order): effect/trigger,
// card filters, reveal, draw/discard, roll mods, flip, scry, search, bury,
// skill buffs, re-rolls, DQ/lose, hand size, conditions, trigger-body & gates,
// text util.
// --------------------------------------------------------------------------

// --------------------------------------------------------------------------
// Effect & trigger constructors
// --------------------------------------------------------------------------

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
        from_hand: false,
    }
}

/// "When you hit a `<atk_type>`" — an [`Trigger::OnHit`] gated on the hit card's type.
fn on_hit_type(atk_type: AtkType) -> Trigger {
    on_hit_type_who(atk_type, Who::SelfSide)
}

/// "When your opponent hits a `<atk_type>`" — the opponent-side [`Trigger::OnHit`]
/// (Stung: "when your opponent hits a Strike, …").
fn on_hit_type_who(atk_type: AtkType, who: Who) -> Trigger {
    Trigger::OnHit {
        atk_type: Some(atk_type),
        order: None,
        name_contains: Vec::new(),
        text_contains: Vec::new(),
        on_any: false,
        who,
        from_hand: false,
    }
}

/// "When you hit a [`<atk_type>`] card with 'X' [or 'Y'] in the name/text" — an
/// [`Trigger::OnHit`] gated on the hit card's title (`in_text = false`) or rules text
/// (`in_text = true`), optionally AND-ed with an attack type.
fn on_hit_named(atk_type: Option<AtkType>, names: Vec<String>, in_text: bool) -> Trigger {
    let (name_contains, text_contains) = if in_text {
        (Vec::new(), names)
    } else {
        (names, Vec::new())
    };
    Trigger::OnHit {
        atk_type,
        order: None,
        name_contains,
        text_contains,
        on_any: false,
        who: Who::SelfSide,
        from_hand: false,
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

/// "When you flip [any number of | N or more] cards, <action>" — a STANDING flip
/// trigger (`on_self: false`), fired by `run_on_flip` from an in-play card. `count`
/// = `None` for "any number", `Some(n)` with `at_least` for an "n or more" threshold.
fn on_flip_standing(count: Option<i64>, at_least: bool) -> Trigger {
    Trigger::OnFlip {
        who: Who::SelfSide,
        count,
        at_least,
        on_self: false,
    }
}

/// "When/If this card is flipped, <body>" — the per-card flip self-trigger
/// (`on_self: true`), dispatched by `run_self_flips` for each just-flipped card. The
/// self-action family ([`flip_self`]) carries a bespoke action; this bare trigger pairs
/// with an arbitrary grammar body (draw / opponent bury / discard-pile shuffle) via
/// [`trigger_body`].
fn on_flip_self() -> Trigger {
    Trigger::OnFlip {
        who: Who::SelfSide,
        count: None,
        at_least: false,
        on_self: true,
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

/// The stopper's side: "when you stop a card" — the effect fires for the player who
/// PLAYED the stop (Direction::Theirs, "they stopped a card"), the mirror of
/// [`on_your_stop`].
fn on_their_stop() -> Trigger {
    Trigger::OnStop {
        dir: Direction::Theirs,
        order: None,
    }
}

// --------------------------------------------------------------------------
// Card filters (`CardFilter`)
// --------------------------------------------------------------------------

fn cf_atk(a: AtkType) -> CardFilter {
    CardFilter {
        atk_type: Some(a),
        ..Default::default()
    }
}

fn cf_order(o: PlayOrder) -> CardFilter {
    CardFilter {
        play_order: Some(o),
        ..Default::default()
    }
}

fn cf_name(names: Vec<String>) -> CardFilter {
    CardFilter {
        name_contains: names,
        ..Default::default()
    }
}

/// Blank the matching card(s) wherever they sit (the in-play Spotlight/named-card form).
fn blank_text(selector: CardFilter, who: Who) -> Action {
    Action::BlankText {
        selector,
        who,
        discard_only: false,
    }
}

/// Selector for a "(Their|Your opponent's) `<desc>` have blank text" clause — the
/// opponent-scoped continuous blank family. Covers a name-substring OR-list ("cards with
/// \"X\"[ or \"Y\"] in the name"), a play order ("Finishes"), the SkillRequirement tag
/// ("Skill Requirement cards"), and the unscoped "cards in play"/"cards" (any). Returns
/// `None` (the rule then declines → stays Unsupported) for anything else, so a "without …"
/// negation or an ambiguous "skill cards" is never silently mis-blanked.
fn opp_blank_selector(desc: &str) -> Option<CardFilter> {
    static NAMES: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?i)^cards with ("[^"]+"(?:,? (?:and |or )?"[^"]+")*) in the name$"#).unwrap()
    });
    let d = desc.trim();
    if let Some(c) = NAMES.captures(d) {
        let names = quoted_names(&c[1]);
        if !names.is_empty() {
            return Some(cf_name(names));
        }
    }
    match d.to_ascii_lowercase().as_str() {
        "finishes" | "finish cards" => Some(cf_order(PlayOrder::Finish)),
        "skill requirement cards" => Some(cf_tag(crate::cards::SKILL_REQUIREMENT_TAG)),
        "cards in play" | "cards" => Some(CardFilter::default()),
        _ => None,
    }
}

/// Blank every card in `who`'s discard pile — "cards in your opponent's discard pile
/// have blank text" (neutralises their WhileInDiscard abilities).
fn blank_discard(who: Who) -> Action {
    Action::BlankText {
        selector: CardFilter::default(),
        who,
        discard_only: true,
    }
}

/// Un-blank (restore) the matching card(s) — the inverse of [`blank_text`].
fn unblank(selector: CardFilter, who: Who) -> Action {
    Action::Unblank { selector, who }
}

/// Quoted names from a `with "X" [or "Y"] in the name` phrase (case-insensitive
/// OR-substring — same convention as the name-substring override family).
fn quoted_names(text: &str) -> Vec<String> {
    static Q: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#""([^"]+)""#).unwrap());
    Q.captures_iter(text).map(|c| c[1].to_owned()).collect()
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
    // A trailing " card"/" cards" is noise on a typed selector ("Lead cards" -> "Lead");
    // strip it so count_filter sees the bare type.
    let d = d
        .strip_suffix(" cards")
        .or_else(|| d.strip_suffix(" card"))
        .unwrap_or(d);
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

/// "If you have a(nother) `<desc>` in play, …" as a `HasInPlay(SELF, …, ≥1)` gate.
/// `None` for descriptors with no CardFilter (e.g. "stop").
fn has_in_play_desc(desc: &str) -> Option<Condition> {
    Some(has_in_play(Who::SelfSide, count_filter(desc.trim())?, 1))
}

// --------------------------------------------------------------------------
// Reveal / flip-reveal
// --------------------------------------------------------------------------

/// The match test inside a reveal-then clause — the "`<X>`" in "if `<X>`, `<consequence>`":
/// a name/text substring ("it has \"Guitar\" in the name") or an attack type ("it is a
/// Strike"). `None` for a predicate this node can't express (parity, number), so the
/// clause stays Unsupported.
fn reveal_filter(phrase: &str) -> Option<CardFilter> {
    let p = phrase.trim();
    if p.contains("in the name") || p.contains("in the text") {
        let names = quoted_names(p);
        if names.is_empty() {
            return None;
        }
        let attr = if p.contains("in the text") {
            "text"
        } else {
            "name"
        };
        return Some(name_or_text_filter(attr, names));
    }
    // "it is a[n] <type>" / "the card is a <type>" — an attack type, play order, or
    // stop, routed through `count_filter` (Strike/Grapple/Submission/Lead/Follow Up/
    // Finish/Stop). Anything it can't type ("non-finish", "odd-numbered") declines.
    static IS: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^(?:it|the card|that card) is an? (.+)$").unwrap());
    count_filter(&IS.captures(p)?[1])
}

/// Parse a reveal-then CONSEQUENCE ("`<consequence>`" in "if `<X>`, `<consequence>`") into
/// `(take_matched, then, then_optional)`: a leading "add that card to your hand" sets
/// `take_matched` (mandatory on a match), any remaining body is parsed through the normal
/// grammar for the extra actions, and a "you may" prefix on that body sets `then_optional`.
/// `None` if a non-empty body has no grammar, so the whole clause stays Unsupported.
fn reveal_consequence(text: &str) -> Option<(bool, Vec<Action>, bool)> {
    // "add that card / the revealed card / it to your hand" — the matched revealed card
    // to hand. In a reveal-then consequence "it" always refers to the revealed card.
    static TAKE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^(?:you )?add (?:that card|the revealed card|it) to your hand").unwrap()
    });
    let c = text.trim().trim_end_matches('.').trim();
    let (take, rest) = match TAKE.find(c) {
        Some(m) => {
            let r = c[m.end()..].trim().trim_start_matches(',').trim();
            (true, r.strip_prefix("and ").unwrap_or(r).trim())
        }
        None => (false, c),
    };
    let (opt, body) = match rest
        .strip_prefix("you may ")
        .or_else(|| rest.strip_prefix("You may "))
    {
        Some(b) => (true, b.trim()),
        None => (false, rest),
    };
    if body.is_empty() {
        return take.then_some((true, Vec::new(), false));
    }
    // Body clauses are lowercase mid-sentence; the grammar expects sentence case.
    let cap = capitalize_first(body);
    let eff = match_grammar(&cap)
        .or_else(|| compound_body(&cap))
        .or_else(|| choice_body(&cap))?;
    Some((take, eff.actions, opt || eff.optional))
}

/// Build the `RevealThen` effect shared by the inline colon form and the split
/// follow-up clause: reveal `count` from `reveal_from`, match a `<filter phrase>`, and
/// run a `<consequence>`. `None` if either the filter or the consequence has no grammar.
fn reveal_then_effect(
    reveal_from: RevealSource,
    count: i64,
    filter_phrase: &str,
    consequence: &str,
) -> Option<Effect> {
    let filter = reveal_filter(filter_phrase)?;
    let (take_matched, then, then_optional) = reveal_consequence(consequence)?;
    Some(eff(
        on_hit(),
        vec![Action::RevealThen {
            reveal_from,
            count,
            filter,
            take_matched,
            then,
            then_optional,
        }],
        Condition::Always,
        Duration::Instant,
    ))
}

/// A bare "Reveal the top/bottom card of your deck:" header carrying NO inline
/// consequence — the split form whose "If `<filter>`, `<consequence>`" lands on the
/// next clause. Returns the reveal source it opens.
fn reveal_header(clause: &str) -> Option<RevealSource> {
    static H: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^Reveal the (top|bottom) card of your deck:?$").unwrap());
    H.captures(clause.trim()).map(|c| {
        if c[1].eq_ignore_ascii_case("bottom") {
            RevealSource::DeckBottom
        } else {
            RevealSource::DeckTop
        }
    })
}

/// The follow-up clause of a split reveal header: "If `<filter>`, `<consequence>`",
/// built into a deck `RevealThen` under the header's `reveal_from`. `None` when the
/// clause isn't a filtered consequence (so the header falls through to Unsupported).
fn reveal_followup(reveal_from: RevealSource, clause: &str) -> Option<Effect> {
    static F: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^If (.+?), (.+)$").unwrap());
    let c = F.captures(clause.trim().trim_end_matches('.').trim())?;
    reveal_then_effect(reveal_from, 1, &c[1], &c[2])
}

/// "If this is a `<match-type>` match, you may flip both cards instead." — the optional,
/// match-type-gated REPLACEMENT of a paired "each player reveals the top card of their
/// deck and adds it to their hand" (Friends and Rivals family). Returns the match-type
/// gate; `parse_text` rewrites the preceding add-to-hand into an add-or-flip `Choice`.
fn flip_both_instead(clause: &str) -> Option<Condition> {
    static RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^If (this is an? .+? match), you may flip both cards? instead$").unwrap()
    });
    let c = RE.captures(clause.trim().trim_end_matches('.').trim())?;
    gate_condition(&c[1])
}

/// "If `<gate>`, `<body>` instead" — a gated clause that REPLACES the preceding sibling
/// effect's consequence when `<gate>` holds ("Draw 1 card. If your Power skill is greater
/// than your opponent's Power skill, draw 2 cards instead."; "Your opponent buries 2 cards
/// in their hand. If the Crowd Meter is 3 or greater, your opponent buries 3 cards in their
/// hand instead."). The gate is anything [`gate_condition`] parses — a skill compare, a
/// Crowd-Meter threshold, a has-in-play count, a turn-roll gate, … Returns `(gate,
/// replacement actions)`; `parse_text` gates the preceding base effect on `Not(gate)` and
/// pushes the replacement — sharing the base's trigger — gated on `gate`, so exactly one
/// fires (mirrors [`flip_both_instead`]), and only when the base's first action is the SAME
/// variant as the replacement (a draw replaces a draw, a bury a bury). The word "instead"
/// may sit at the body's end ("draw 2 cards instead") or mid-body ("your next turn roll is
/// instead +2"); it is stripped before compiling. `None` when the gate doesn't parse or the
/// body has no grammar — those fall through to the generic gate / Unsupported.
fn gated_instead(clause: &str, source: EffectSource) -> Option<(Condition, Vec<Action>)> {
    static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^If (.+?)[,:;] (.+)$").unwrap());
    let caps = RE.captures(clause.trim().trim_end_matches('.').trim())?;
    if !caps[2].to_lowercase().contains("instead") {
        return None;
    }
    let cond = gate_condition(&caps[1])?;
    // Strip "instead" wherever it sits, then collapse the doubled space it leaves.
    let body = caps[2]
        .replacen("instead ", "", 1)
        .replacen(" instead", "", 1);
    let body = body.split_whitespace().collect::<Vec<_>>().join(" ");
    let is_unsupported = |e: &Effect| {
        e.actions
            .iter()
            .any(|a| matches!(a, Action::Unsupported { .. }))
    };
    let mut compiled = compile(&body, source, Frequency::Unlimited, None);
    // The body is the lowercase tail of the clause ("draw 2 cards"); retry sentence-cased
    // so capital-anchored rules match (as `inline_freq` does for a promoted body).
    if is_unsupported(&compiled) {
        compiled = compile(&uppercase_first(&body), source, Frequency::Unlimited, None);
    }
    if is_unsupported(&compiled) {
        return None;
    }
    Some((cond, compiled.actions))
}

/// Is `eff` the "each player reveals the top card of their deck and adds it to their
/// hand" effect — an unconditional `OnHit` pair of top-of-deck Draws to SELF and OPP?
/// The anchor a `flip both cards instead` clause rewrites.
fn is_reveal_top_both(eff: &Effect) -> bool {
    matches!(eff.trigger, Trigger::OnHit { .. })
        && eff.condition == Condition::Always
        && !eff.optional
        && eff.actions.len() == 2
        && matches!(
            eff.actions[0],
            Action::Draw {
                who: Who::SelfSide,
                source: DeckEnd::Top,
                ..
            }
        )
        && matches!(
            eff.actions[1],
            Action::Draw {
                who: Who::Opp,
                source: DeckEnd::Top,
                ..
            }
        )
}

// --------------------------------------------------------------------------
// Draw & discard actions
// --------------------------------------------------------------------------

fn draw(n: i64, who: Who, source: DeckEnd, per: Option<CardFilter>, per_who: Who) -> Action {
    Action::Draw {
        cap: None,
        per_excludes_trigger: false,
        n,
        source,
        who,
        per,
        per_who,
        from_crowd: false,
    }
}

/// "Draw cards equal to the Crowd Meter [+`offset`] [(Max +`cap`)]" — a self-draw whose
/// count is the live Crowd Meter plus `offset`, clamped to `cap`. `n` carries the offset.
fn draw_crowd(offset: i64, cap: Option<i64>) -> Action {
    Action::Draw {
        cap,
        per_excludes_trigger: false,
        n: offset,
        source: DeckEnd::Top,
        who: Who::SelfSide,
        per: None,
        per_who: Who::SelfSide,
        from_crowd: true,
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
        all: false,
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
        all: false,
    }
}

// --------------------------------------------------------------------------
// Roll modifiers
// --------------------------------------------------------------------------

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
        on_skill: None,
    }
}

/// Skill-keyed pending turn-roll bonus: "the next time you roll `<S>` for your turn
/// roll, it is +N" — waits until `skill` is next rolled, applies once, is consumed.
fn modify_roll_on_skill(delta: i64, skill: Skill) -> Action {
    Action::ModifyRoll {
        who: Who::SelfSide,
        delta,
        when: RollWhen::Next,
        per: None,
        per_who: Who::SelfSide,
        per_zone: CountZone::InPlay,
        on_skill: Some(skill),
    }
}

/// A plain single-card `ShuffleIntoDeck` recur (`all`/`then_draw` off) — the common
/// case. The multi-card "take any number … then draw the same number" recur sets those
/// two flags on the struct literal directly.
fn shuffle_into(selector: CardFilter, source: ShuffleSource) -> Action {
    shuffle_into_who(selector, source, Who::SelfSide)
}

/// [`shuffle_into`] with an explicit actor — `Opp`/each-player recur of a chosen zone
/// back into that player's deck ("each player shuffles 1 Grapple from their discard pile
/// into their deck" emits one per side). schema v143
fn shuffle_into_who(selector: CardFilter, source: ShuffleSource, who: Who) -> Action {
    Action::ShuffleIntoDeck {
        selector,
        source,
        who,
        all: false,
        then_draw: false,
        then_bury: false,
    }
}

/// Map a play-requirement noun ("cards" / "Leads" / "Follow Ups") to its [`RequireKind`]
/// — the counted quantity in a `FinishRequires` gimmick.
fn require_kind(word: &str) -> Option<RequireKind> {
    let w = word.trim().to_ascii_lowercase();
    if w.starts_with("card") {
        Some(RequireKind::Cards)
    } else if w.starts_with("lead") {
        Some(RequireKind::Leads)
    } else if w.starts_with("follow") {
        Some(RequireKind::FollowUps)
    } else {
        None
    }
}

// --------------------------------------------------------------------------
// Flip actions
// --------------------------------------------------------------------------

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
                at_least: false,
                on_self: true, // per-card "if THIS card is flipped"
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

// --------------------------------------------------------------------------
// Scry
// --------------------------------------------------------------------------

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
        to_hand_filter: None,
    }
}

/// "Look at the top N of your deck, add `to_hand` to hand, put `bury` away, keep the
/// rest on top" — the peek-and-sort scry (D3 V1's Contact Juggling: top 3 / add 1 /
/// "discard" 1 / other on top). Two documented simplifications, standard for the scry
/// family: the kept card is the BEST (scry_value), not random; and `bury` sends to the
/// deck BOTTOM (`Scry.bury`), a near-equivalent of "put in your discard pile".
fn scry_keep(top: i64, to_hand: i64, bury: i64) -> Action {
    Action::Scry {
        deck: Who::SelfSide,
        top,
        bottom: 0,
        reveal: false,
        to_hand,
        bury,
        rest: ScryRest::Return,
        to_hand_filter: None,
    }
}

/// "Look at the bottom N cards of your deck, then randomly bury them" (Papa Nequaquam's
/// Flying Holmgang) — peek the deck bottom and send all N back to the bottom. `bottom=N`
/// pulls the window off the back and `bury=N` returns every card to the deck bottom, so
/// the net effect is a re-sort of the bottom N (a near-inert deck manipulation, the
/// weakest member of the look-at-bottom family). Simplifications, standard for the scry
/// family: "any player's deck" -> your own (`SelfSide`), and "randomly" -> the value
/// sort `Scry.bury` already applies.
fn scry_bottom_bury(n: i64) -> Action {
    Action::Scry {
        deck: Who::SelfSide,
        top: 0,
        bottom: n,
        reveal: false,
        to_hand: 0,
        bury: n,
        rest: ScryRest::Return,
        to_hand_filter: None,
    }
}

/// "Look at the bottom N cards of your deck, add `to_hand` to your hand and randomly bury
/// the others" (Shattered Split's Bonk!) — the bottom-window counterpart of [`scry_keep`]:
/// peek the deck bottom, take the `to_hand` BEST to hand, and re-bury the remaining
/// `N - to_hand` to the deck bottom. Same family simplifications: kept = best (`scry_value`)
/// not free-choice, "randomly bury" -> the value sort, buried cards go to the deck bottom.
fn scry_bottom_keep(n: i64, to_hand: i64) -> Action {
    Action::Scry {
        deck: Who::SelfSide,
        top: 0,
        bottom: n,
        reveal: false,
        to_hand,
        bury: (n - to_hand).max(0),
        rest: ScryRest::Return,
        to_hand_filter: None,
    }
}

/// "Look at / Reveal the top card of `deck`'s deck, you may flip it" — a single-card
/// peek with an *optional* flip ([`ScryRest::MayFlip`]): the actor sees the top card,
/// then mills it only when worthwhile (deny an opponent their Finish/stop, or shed
/// your own junk) and otherwise leaves it on top.
fn scry_may_flip(reveal: bool, deck: Who) -> Action {
    Action::Scry {
        deck,
        top: 1,
        bottom: 0,
        reveal,
        to_hand: 0,
        bury: 0,
        rest: ScryRest::MayFlip,
        to_hand_filter: None,
    }
}

// --------------------------------------------------------------------------
// Search
// --------------------------------------------------------------------------

fn search(filter: CardFilter, dest: Dest, count: i64) -> Action {
    Action::Search {
        filter,
        dest,
        count,
        source: SearchSource::Deck,
    }
}

/// A [`Action::Search`] that tutors from the deck OR the discard pile ("search your deck
/// or discard pile for X").
fn search_both(filter: CardFilter, dest: Dest, count: i64) -> Action {
    Action::Search {
        filter,
        dest,
        count,
        source: SearchSource::DeckOrDiscard,
    }
}

/// Parse a `Search` selector — "a Finish", "2 cards", "up to 3 cards", "1 card with
/// \"Ladder\" in the name" — into `(filter, count)`, reusing [`recur_filter`] for the
/// typed/named descriptor. `None` for a selector with no CardFilter (Spotlight, Skill
/// Requirement, …), so those clauses stay Unsupported rather than mis-modeling.
fn search_target(sel: &str) -> Option<(CardFilter, i64)> {
    static LEAD: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^(?:up to )?(a|an|\d+) (.+)$").unwrap());
    let sel = sel.trim();
    // A bare quoted card name with no count ("Clothesline") -> that one named card.
    if sel.starts_with('"') {
        let names = quoted_names(sel);
        if !names.is_empty() {
            return Some((cf_name(names), 1));
        }
    }
    let caps = LEAD.captures(sel)?;
    let head = caps[1].to_lowercase();
    let count = if head == "a" || head == "an" {
        1
    } else {
        head.parse().ok()?
    };
    Some((recur_filter(&caps[2])?, count))
}

// --------------------------------------------------------------------------
// Bury
// --------------------------------------------------------------------------

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
        per_zone: CountZone::InPlay,
        all: false,
    }
}

/// "Bury N [`<selector>`] cards in any/either player's discard pile" — the actor picks
/// `count` cards matching `selector` from EITHER discard pile (`choose: true`; `who`
/// is ignored). Each buried card returns to ITS OWNER's deck bottom.
fn bury_choose(count: i64, selector: CardFilter) -> Action {
    Action::Bury {
        choose: true,
        selector,
        count,
        who: Who::SelfSide,
        random: false,
        source: BuryFrom::Discard,
        per: None,
        per_who: Who::SelfSide,
        per_zone: CountZone::InPlay,
        all: false,
    }
}

/// "Bury `count` per `per`-matching card in `per_zone`" (schema v83; `per_zone` v130).
/// `random` is forced on for a HAND source (the hand owner sheds without choosing). The
/// per-count ranges over the SELF side — cards in play ("… for each `<X>` you have in
/// play", Cardona) or flipped this turn ("… for each Strike flipped", Five Star Heart
/// Punch).
fn bury_per(
    count: i64,
    who: Who,
    source: BuryFrom,
    per: CardFilter,
    per_zone: CountZone,
    random: bool,
) -> Action {
    Action::Bury {
        choose: false,
        selector: CardFilter::default(),
        count,
        who,
        random: random || source == BuryFrom::Hand,
        source,
        per: Some(per),
        per_who: Who::SelfSide,
        per_zone,
        all: false,
    }
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
        vec![bury_per(count, who, source, per, CountZone::InPlay, random)],
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
        per_zone: CountZone::InPlay,
        all: false,
    }
}

/// "They bury all `<type>` cards" — bury EVERY hand card matching `selector` (schema
/// v90, `all`). The hand owner sheds without choosing; `count` is a placeholder (the
/// dispatch derives it from the hand size).
fn bury_all_hand(selector: CardFilter, who: Who) -> Action {
    Action::Bury {
        choose: false,
        selector,
        count: 0,
        who,
        random: false,
        source: BuryFrom::Hand,
        per: None,
        per_who: Who::SelfSide,
        per_zone: CountZone::InPlay,
        all: true,
    }
}

/// "They discard all `<type>`" — discard EVERY hand card matching `selector` (schema
/// v90, `all`). Sibling of [`bury_all_hand`].
fn discard_all_hand(selector: CardFilter, who: Who) -> Action {
    Action::Discard {
        selector,
        count: 0,
        who,
        random: false,
        per: None,
        per_who: Who::SelfSide,
        choose: false,
        all: true,
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
        per_zone: CountZone::InPlay,
        all: false,
    }
}

// --------------------------------------------------------------------------
// Skill buffs & scaling
// --------------------------------------------------------------------------

fn buff(skill: Skill, delta: i64, who: Who) -> Action {
    Action::BuffSkill {
        skill,
        delta,
        who,
        duration: Duration::WhileInPlay,
        target_highest: false,
        target_lowest: false,
        target_chosen: false,
        per_crowd: false,
        cap: None,
        per: None,
        per_zone: CountZone::InPlay,
        per_excludes_self: false,
    }
}

/// One standing [`Action::TurnRollBonus`] per skill — "Your Power and Strike are +N
/// during turn rolls" fans out to a bonus on each named skill.
fn turn_roll_bonuses(skills: Vec<Skill>, delta: i64) -> Vec<Action> {
    skills
        .into_iter()
        .map(|skill| Action::TurnRollBonus {
            skill,
            delta,
            who: Who::SelfSide,
            either: false,
            per_crowd: false,
            cap: None,
        })
        .collect()
}

/// "+N to your lowest/highest skill" -> a [`Action::BuffSkill`] whose target skill is
/// resolved dynamically at derived-stats time (`resolve_buff`). The `skill` field is a
/// placeholder (never read when `target_lowest`/`target_highest` is set).
fn buff_extreme(highest: bool, delta: i64, who: Who) -> Action {
    Action::BuffSkill {
        skill: Skill::ALL[0],
        delta,
        who,
        duration: Duration::WhileInPlay,
        target_highest: highest,
        target_lowest: !highest,
        target_chosen: false,
        per_crowd: false,
        cap: None,
        per: None,
        per_zone: CountZone::InPlay,
        per_excludes_self: false,
    }
}

/// A `BuffSkill` scaled by the count of the owner's in-play cards matching `per`
/// (clamped to `cap`) — "your Technique and Grapple are +1 for each card you have
/// in play with 'Breaker' in the name". `per: None` = a flat +`delta`. `exclude_self`
/// drops the source card from the count ("for each OTHER card …").
fn buff_per(
    skill: Skill,
    delta: i64,
    per: Option<CardFilter>,
    cap: Option<i64>,
    exclude_self: bool,
) -> Action {
    Action::BuffSkill {
        skill,
        delta,
        who: Who::SelfSide,
        duration: Duration::WhileInPlay,
        target_highest: false,
        target_lowest: false,
        target_chosen: false,
        per_crowd: false,
        cap,
        per,
        per_zone: CountZone::InPlay,
        per_excludes_self: exclude_self,
    }
}

/// Build a Static multi-skill [`BuffSkill`] scaled by the count of a card TYPE on the
/// owner's board — "Your X and Y are +N for each `<type>` you have in play (Max +M)".
/// `skills_text` is a skill/and/comma list, `per_text` a type descriptor routed through
/// [`count_filter`] (atk / play order / stop). Declines if the skill list is empty or
/// the type descriptor isn't a countable type (e.g. bare "card" — owned by the name
/// rules), so a non-type "for each …" falls through to Unsupported.
fn type_count_buff(
    skills_text: &str,
    delta: i64,
    per_text: &str,
    cap: Option<regex::Match>,
    exclude_self: bool,
) -> Option<Effect> {
    let skills = skill_list(skills_text);
    let per = count_filter(per_text)?;
    if skills.is_empty() {
        return None;
    }
    let cap = cap.map(|m| m.as_str().parse::<i64>().unwrap());
    let actions = skills
        .into_iter()
        .map(|s| buff_per(s, delta, Some(per.clone()), cap, exclude_self))
        .collect();
    Some(eff(
        Trigger::Static,
        actions,
        Condition::Always,
        Duration::WhileInPlay,
    ))
}

/// Build a Static multi-skill [`BuffSkill`] whose delta is the live Crowd Meter —
/// "Your X [and Y] is/are + the Crowd Meter [(Max +M)]" (`per_crowd`, the same dynamic
/// delta Copy Kat uses, previously override-only). Declines when the skill list is empty
/// (so "Your Finish roll …" / "Your breakout rolls …" — different mechanisms — fall
/// through), keeping this to plain skill buffs.
fn crowd_meter_buff(skills_text: &str, cap: Option<regex::Match>) -> Option<Effect> {
    let skills = skill_list(skills_text);
    if skills.is_empty() {
        return None;
    }
    let cap = cap.map(|m| m.as_str().parse::<i64>().unwrap());
    let actions = skills
        .into_iter()
        .map(|s| Action::BuffSkill {
            skill: s,
            delta: 1,
            who: Who::SelfSide,
            duration: Duration::WhileInPlay,
            target_highest: false,
            target_lowest: false,
            target_chosen: false,
            per_crowd: true,
            cap,
            per: None,
            per_zone: CountZone::InPlay,
            per_excludes_self: false,
        })
        .collect();
    Some(eff(
        Trigger::Static,
        actions,
        Condition::Always,
        Duration::WhileInPlay,
    ))
}

// --------------------------------------------------------------------------
// Re-rolls
// --------------------------------------------------------------------------

/// A `Reroll` of the owner's (`SelfSide`) or opponent's turn/finish roll. `when` picks
/// the current roll (`This`, structural) vs a one-shot for the NEXT turn roll; `finish`
/// scopes it to the Finish roll. The action pre-existed (override-only) — this is the
/// first grammar for it. `once`/`choose`/`cost` stay at their defaults.
fn reroll(who: Who, when: RollWhen, finish: bool) -> Action {
    Action::Reroll {
        who,
        once: false,
        choose: false,
        when,
        cost: None,
        finish,
        breakout: false,
    }
}

/// A `Reroll` of the DEFENDER's breakout roll — `who: SelfSide` (the defender re-rolls
/// their own: "re-roll your Breakout roll") or `Opp` ("force your opponent to re-roll
/// their Breakout roll", the finisher forcing the defender). Always `This` (structural,
/// read in the breakout loop); never a `Next` grant.
fn reroll_breakout(who: Who) -> Action {
    Action::Reroll {
        who,
        once: false,
        choose: false,
        when: RollWhen::This,
        cost: None,
        finish: false,
        breakout: true,
    }
}

/// Parse a re-roll COST prefix into a [`RerollCost`] (schema v103): "bury N cards in
/// your hand" → `BuryFromHand`; "discard N `<object>` [from your hand]" →
/// `DiscardFromHand` (object via `recur_filter`: bare "card(s)" = any, else a typed /
/// named filter). `None` for shapes we don't model here — reveal costs, "discard this
/// card" self-discard (hand-activated), "up to N".
fn reroll_cost(text: &str) -> Option<RerollCost> {
    static BURY: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^bury (\d+|a|one) cards? in your hand$").unwrap());
    static DISCARD: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^discard (\d+|a|one) (.+?)(?: from your hand)?$").unwrap()
    });
    static REVEAL: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^reveal (\d+|a|one) (.+?)(?: from your hand)?$").unwrap()
    });
    let node = |kind, count, filter| RerollCost {
        node_type: RerollCostTag,
        kind,
        count: Some(count),
        filter,
    };
    let t = text.trim();
    if let Some(c) = BURY.captures(t) {
        return Some(node(
            RerollCostKind::BuryFromHand,
            count_or_word(&c[1]),
            None,
        ));
    }
    if let Some(c) = DISCARD.captures(t) {
        let obj = c[2].trim();
        if obj.eq_ignore_ascii_case("this card") {
            return None; // self-discard: hand-activated, out of scope
        }
        let filter = recur_filter(obj)?;
        // A bare "card"/"cards" object is the default (match-any) filter → carry None.
        let filter = (filter != CardFilter::default()).then_some(filter);
        return Some(node(
            RerollCostKind::DiscardFromHand,
            count_or_word(&c[1]),
            filter,
        ));
    }
    if let Some(c) = REVEAL.captures(t) {
        // Reveal is a SOFT cost: prove you hold the cards, keep them. A bare
        // "card(s)" object carries no filter (match-any).
        let filter = recur_filter(c[2].trim())?;
        let filter = (filter != CardFilter::default()).then_some(filter);
        return Some(node(
            RerollCostKind::RevealFromHand,
            count_or_word(&c[1]),
            filter,
        ));
    }
    None
}

/// Route an OnReroll trigger body: normalize its roll-modifier phrasings so the shared
/// grammar matches — "your/their roll is ±N" → "…turn roll is ±N", and the "would
/// re-roll … that roll is +N" self case's "that roll" → "your turn roll" — then delegate
/// to [`trigger_body`], which handles draw / roll-mod / shuffle-self / "you may" bodies.
/// The roll-mod body becomes a `ModifyRoll{This}` the engine folds into the re-rolled
/// value; every OnReroll clause's roll-mod always targets the re-rolling player.
fn on_reroll_body(who: Who, body: &str) -> Option<Effect> {
    static THAT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bthat roll is").unwrap());
    let norm = THAT.replace_all(body, "your turn roll is").into_owned();
    trigger_body(Trigger::OnReroll { who }, &norm_roll_is_phrasing(&norm))
}

/// Normalize a bare "your/their roll is ±N" roll-modifier phrasing to the canonical
/// "…turn roll is ±N" the [`ModifyRoll`] grammar (`Your turn roll is …` /
/// `Their turn roll is …`) matches. Shared by the OnReroll and OnRoll body routers,
/// whose clauses write the roll modifier as "their roll is -1" mid-body.
fn norm_roll_is_phrasing(body: &str) -> String {
    static BARE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)\b(your|their) roll is").unwrap());
    BARE.replace_all(body, "${1} turn roll is").into_owned()
}

/// Route an OnRoll trigger body where the roller is the effect owner's OPPONENT ("When
/// your opponent rolls `<S>` for their turn roll, `<body>`"). Normalizes the "their roll
/// is ±N" roll-modifier phrasing, then delegates to [`trigger_body`] with `OnRoll{Opp}`
/// — the "you"/"they" subjects in the body resolve to owner/opponent through the shared
/// grammar exactly as the self mirror does.
fn on_roll_opp_body(s: Skill, body: &str) -> Option<Effect> {
    trigger_body(on_roll(s, Who::Opp), &norm_roll_is_phrasing(body))
}

// --------------------------------------------------------------------------
// DQ / lose-the-match
// --------------------------------------------------------------------------

/// A Static "no disqualifications" match-rule toggle (`DisqualificationRule` was
/// previously override-only). `Match` scope = "the match has no disqualifications";
/// `SelfSide` = "you cannot be disqualified".
fn dq_rule(scope: DqScope) -> Effect {
    eff(
        Trigger::Static,
        vec![Action::DisqualificationRule {
            enabled: false,
            scope,
        }],
        Condition::Always,
        Duration::WhileInPlay,
    )
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
            all: false,
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

// --------------------------------------------------------------------------
// Hand size
// --------------------------------------------------------------------------

fn max_hand(delta: i64, who: Who) -> Action {
    Action::MaxHandSize {
        delta,
        who,
        duration: Duration::WhileInPlay,
        set: None,
    }
}

/// An ABSOLUTE maximum-handsize set ("your opponent's maximum handsize is N") — the cap
/// becomes `n` (lowest active set wins) rather than shifting by a delta.
fn max_hand_set(n: i64, who: Who, duration: Duration) -> Action {
    Action::MaxHandSize {
        delta: 0,
        who,
        duration,
        set: Some(n),
    }
}

fn min_hand(delta: i64, who: Who) -> Action {
    Action::MinHandSize {
        delta,
        who,
        duration: Duration::WhileInPlay,
    }
}

// --------------------------------------------------------------------------
// Conditions
// --------------------------------------------------------------------------

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

/// `who`'s hand size `cmp` the OPPONENT's, relatively — "you have fewer cards in your hand
/// than your opponent" (`Lt`, SELF), "the same number … as your opponent" (`Eq`).
/// `vs: Vs::Opp, value: None` (the eval reads the other seat's hand as the right operand).
fn hand_size_vs_opp(cmp: Comparator, who: Who) -> Condition {
    Condition::HandSizeCompare {
        cmp,
        vs: Vs::Opp,
        value: None,
        who,
    }
}

/// `who`'s count of ALL cards in play `cmp` `vs_who`'s — "you have fewer cards in play than
/// your opponent" (`Lt`, SELF vs Opp). Match-all filter (every card in play counts).
fn in_play_vs(cmp: Comparator, who: Who, vs_who: Who) -> Condition {
    Condition::InPlayCompare {
        filter: CardFilter::default(),
        cmp,
        who,
        vs_who,
    }
}

/// The comparator in a relative "fewer/less/more … than" gate: `more` -> `Gt`, `fewer` /
/// `less` -> `Lt`.
fn fewer_more_cmp(word: &str) -> Comparator {
    if word.eq_ignore_ascii_case("more") {
        Comparator::Gt
    } else {
        Comparator::Lt
    }
}

// --------------------------------------------------------------------------
// Trigger-body composition & gate parsers
// --------------------------------------------------------------------------

/// Trigger-body split: re-parse a clause's BODY through the whole grammar and attach
/// `trigger`, so a "<trigger prefix>, <body>" clause reuses every body rule (draw /
/// bury / turn-roll / recur / …). A leading "you may " sets [`Effect::optional`]; the
/// body's first letter is capitalized before matching. Returns `None` when the body
/// itself has no grammar (the whole clause then falls through to `Unsupported`).
fn trigger_body(trigger: Trigger, body: &str) -> Option<Effect> {
    let body = body.trim();
    let (optional, body) = match body.strip_prefix("you may ") {
        Some(rest) => (true, rest),
        None => (false, body),
    };
    let cap = capitalize_first(body);
    let mut effect = match_grammar(&cap)
        .or_else(|| compound_body(&cap))
        .or_else(|| choice_body(&cap))?;
    effect.trigger = trigger;
    effect.optional = effect.optional || optional;
    Some(effect)
}

/// A compound trigger body — "<action A> and/then <action B>[ and <C>…]" — parsed as a
/// single effect whose action list is the concatenation of the parts. Each part must
/// parse on its own to a plain (Always, non-optional, Instant) action-effect, which both
/// (a) validates the split — a spurious "and" inside one action leaves a part that does
/// not parse, so the whole thing declines — and (b) keeps compounding to simple
/// sequential actions. `None` when there is no split or any part is not such an effect.
fn compound_body(body: &str) -> Option<Effect> {
    static CONN: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r",? (?:and|then) ").expect("compound connector regex"));
    let parts: Vec<&str> = CONN.split(body).collect();
    if parts.len() < 2 {
        return None;
    }
    let mut base: Option<Effect> = None;
    for part in parts {
        let e = match_grammar(&capitalize_first(part.trim()))?;
        if e.condition != Condition::Always || e.optional || e.duration != Duration::Instant {
            return None; // only fold simple sequential Instant actions
        }
        match base.as_mut() {
            Some(b) if b.trigger == e.trigger => b.actions.extend(e.actions),
            Some(_) => return None, // don't merge parts that fire on different triggers
            None => base = Some(e),
        }
    }
    base
}

/// An "X or Y[ or Z]" choice body — "flip 1 card or draw 2 cards" — parsed as one effect
/// carrying a single [`Action::Choice`] whose options are the parts. Mirrors
/// [`compound_body`]'s guards: each part must parse to a plain (Always, non-optional,
/// Instant) action-effect on the same trigger, which validates the split (a spurious
/// " or " inside one action declines it) and keeps each branch a simple action list.
fn choice_body(body: &str) -> Option<Effect> {
    static OR: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r" or ").expect("choice connector regex"));
    let parts: Vec<&str> = OR.split(body).collect();
    if parts.len() < 2 {
        return None;
    }
    let mut options = Vec::new();
    let mut trigger: Option<Trigger> = None;
    for part in parts {
        let label = capitalize_first(part.trim());
        let e = match_grammar(&label)?;
        if e.condition != Condition::Always || e.optional || e.duration != Duration::Instant {
            return None;
        }
        match &trigger {
            Some(t) if *t != e.trigger => return None, // branches must share a trigger
            None => trigger = Some(e.trigger.clone()),
            _ => {}
        }
        options.push(ChoiceOption {
            node_type: ChoiceOptionTag,
            label,
            actions: e.actions,
        });
    }
    Some(eff(
        trigger?,
        vec![Action::Choice { options }],
        Condition::Always,
        Duration::Instant,
    ))
}

/// A bare "When the Crowd Meter increases:" trigger header on its own clause — Khloe
/// Mai's gimmick splits the body onto the next line. Consumed by the parse loop, which
/// re-parses the following clause under [`Trigger::OnCrowdMeterIncrease`]. The inline
/// "When the Crowd Meter increases, <body>" form is handled by a single-clause rule.
fn cm_increase_header(clause: &str) -> bool {
    static RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^When the Crowd Meter increases:?$").unwrap());
    RE.is_match(clause.trim())
}

/// A BARE, standalone "When this card is in your discard pile:" header — the colon-form
/// on its own line, its body arriving as the FOLLOWING clause(s) rather than inline. It
/// opens a persistent [`Duration::WhileInDiscard`] scope (task #115 slice 4): the discard
/// section is always the last block of a card's text, so every clause after the header is
/// re-parsed through [`while_in_discard_effect`] to end of text. The inline form ("… pile:
/// `<body>`" on one line) is NOT this — it carries its own body and is handled by the
/// whole-clause grammar rule; this matches only the header alone, so the two never collide.
fn discard_header(clause: &str) -> bool {
    static RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^When this card is in your discard pile:?$").unwrap());
    RE.is_match(clause.trim())
}

/// A standalone "Choose one[ of the following]:" header. Unlike the inline "X or Y"
/// that [`choice_body`] splits, its options arrive as the FOLLOWING clauses — composed
/// by [`choice_from_following`] in the parse loop.
fn is_choose_one_header(clause: &str) -> bool {
    static RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^Choose one(?: of the following)?:?$").unwrap());
    RE.is_match(clause.trim())
}

/// Build a `Choice` from the option clauses following a "Choose one:" header. Consumes
/// consecutive clauses that each parse as an unconditional, instant, non-optional
/// action sharing one trigger (the same shape [`choice_body`] requires of "X or Y"
/// branches), stopping at the first that does not. Returns the effect and how many
/// clauses it consumed; `None` if fewer than two options parse, so the header falls
/// through to `Unsupported` rather than silently dropping the choice.
fn choice_from_following(clauses: &[String]) -> Option<(Effect, usize)> {
    let mut options = Vec::new();
    let mut trigger: Option<Trigger> = None;
    for clause in clauses {
        let label = capitalize_first(clause.trim().trim_end_matches('.').trim());
        let Some(e) = match_grammar(&label) else {
            break;
        };
        if e.condition != Condition::Always || e.optional || e.duration != Duration::Instant {
            break;
        }
        match &trigger {
            Some(t) if *t != e.trigger => break, // options must share a trigger
            None => trigger = Some(e.trigger.clone()),
            _ => {}
        }
        options.push(ChoiceOption {
            node_type: ChoiceOptionTag,
            label,
            actions: e.actions,
        });
    }
    if options.len() < 2 {
        return None;
    }
    let consumed = options.len();
    let effect = eff(
        trigger?,
        vec![Action::Choice { options }],
        Condition::Always,
        Duration::Instant,
    );
    Some((effect, consumed))
}

/// A VERSATILE "<offensive> or <stop>" card: play it offensively (each player flips /
/// shuffles / adds the bottom card / buries) OR use it defensively as a Stop. The two
/// capabilities never both fire — the offensive branch is `OnHit` (attacking, when the
/// card leads and hits), the Stop is a capability consulted only by the engine's
/// `card_can_stop` (defending, when the card is played as a stop) — so they model as two
/// independent effects, NOT a `Choice`. Both branches reuse the existing grammar: the
/// stop-body ("Stop any <X>", "If your <S> skill is greater …, stop any <X>", the
/// opponent-has-another gate …) whose condition the engine already honors at stop time,
/// and the offensive body (each-player flip/shuffle/bottom-draw/bury).
///
/// Splits on " or " and requires the two sides to fully parse with EXACTLY ONE being a
/// pure Stop capability and the other a non-stop effect — so both flip orderings
/// ("<offensive> or stop …" and "stop … or <offensive>") work and false positives are
/// unlikely. Emits the offensive effect first (as the old flip-first composer did).
/// Declines (leaving the clause Unsupported) on any other shape. Reached only as a rescue
/// after normal compilation fails, so it can never shadow a clause with real grammar.
fn versatile_or_stop(clause: &str) -> Option<Vec<Effect>> {
    let c = clause.trim().trim_end_matches('.').trim();
    let lower = c.to_ascii_lowercase();
    let mut from = 0;
    while let Some(rel) = lower[from..].find(" or ") {
        let idx = from + rel;
        if let Some(pair) = versatile_split(&c[..idx], &c[idx + 4..]) {
            return Some(pair);
        }
        from = idx + 4;
    }
    None
}

/// "Stop any <A> that cannot be stopped or any <B> that is not the first card played
/// this turn" — TWO stop capabilities with DIFFERENT gates (an unconditional
/// even-unstoppable Stop on <A>, plus a `HitThisTurn{Opp}`-gated Stop on <B>, whose
/// opening card is safe). A single multi-target Stop effect shares one condition, so
/// this can't be one effect — it emits the two independently gated stop effects.
/// Guarded " & "-split rescue (Leader of the Unit JT Dunn): "Your cards with \"Elbow\"
/// … cannot be stopped by Skill Requirements & you can stop cards that cannot be stopped"
/// joins two independent abilities with an ampersand. `split_clauses` deliberately keeps
/// `&` intact — it also joins noun phrases inside ONE clause ("3 Grapples & 3 other
/// Strikes", "your Strike & Grapple cards") — so this runs only once a clause is
/// otherwise Unsupported and commits ONLY when EVERY " & "-separated part parses on its
/// own (a mid-phrase `&` leaves a fragment that doesn't, so it stays Unsupported). The
/// caller stamps the shared frequency/window onto the returned effects.
fn ampersand_compound(clause: &str, source: EffectSource) -> Option<Vec<Effect>> {
    let parts: Vec<&str> = clause.trim().trim_end_matches('.').split(" & ").collect();
    if parts.len() < 2 {
        return None;
    }
    let mut out = Vec::new();
    for part in parts {
        let e = compile(part.trim(), source, Frequency::Unlimited, None);
        if e.actions
            .iter()
            .any(|a| matches!(a, Action::Unsupported { .. }))
        {
            return None;
        }
        out.push(e);
    }
    Some(out)
}

fn stop_first_card_compound(clause: &str) -> Option<Vec<Effect>> {
    static RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"^Stop any (.+?) that cannot be stopped or any (.+?) that is not the first card played this turn$",
        )
        .unwrap()
    });
    let c = RE.captures(clause.trim().trim_end_matches('.').trim())?;
    let unstoppable = stop_eff(
        &format!("{} that cannot be stopped", &c[1]),
        Condition::Always,
    )?;
    let gated = stop_eff(&c[2], Condition::HitThisTurn { who: Who::Opp })?;
    Some(vec![unstoppable, gated])
}

/// "Stop any `<target>`: that card has blank text until the end of the turn" — the Jurassic
/// "If Stopped" family (the stop's `<target>` is usually "… with \"If Stopped\" in the
/// text"). TWO effects: the Stop capability (OnPlay, read by `card_can_stop`) plus a
/// `BlankStoppedText` on `OnStop{Theirs}` — the same shape the overrides use. The blank
/// resolves BEFORE the stopped card's own `OnStop`, suppressing its "If Stopped" text.
fn stop_then_blank_stopped(clause: &str) -> Option<Vec<Effect>> {
    static RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^Stop any (.+?): that card has blank text until the end of the turn$")
            .unwrap()
    });
    let c = RE.captures(clause.trim().trim_end_matches('.').trim())?;
    let stops = stop_targets(&c[1])?;
    Some(vec![
        eff(Trigger::OnPlay, stops, Condition::Always, Duration::Instant),
        eff(
            on_their_stop(),
            vec![Action::BlankStoppedText],
            Condition::Always,
            Duration::Instant,
        ),
    ])
}

/// "Stop any `<target>` and end the current turn" — a Stop capability (OnPlay) plus an
/// `EndTurn` on `OnStop{Theirs}` (fires when this card stops), which cancels the stopped
/// player's remaining `PlayExtraCard` grants. Boot Off the Apron / Capture Headlock / Take
/// You for a Ride, stopping a "Double Team" card.
fn stop_then_end_turn(clause: &str) -> Option<Vec<Effect>> {
    static RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^Stop any (.+?) and end the current turn$").unwrap());
    let c = RE.captures(clause.trim().trim_end_matches('.').trim())?;
    let stops = stop_targets(&c[1])?;
    Some(vec![
        eff(Trigger::OnPlay, stops, Condition::Always, Duration::Instant),
        eff(
            on_their_stop(),
            vec![Action::EndTurn],
            Condition::Always,
            Duration::Instant,
        ),
    ])
}

/// "Choose a skill: your opponent's skill of that type is -N" (Catch These Hands family):
/// TWO effects — an OnHit [`Action::ChooseSkill`] that binds the referenced skill, plus a
/// Static `BuffSkill{target_chosen, who: Opp}` that reads that binding live in derived
/// stats. Split because the choice is executed once while the debuff is a standing fold.
/// Tolerant of the DB's authoring drift: apostrophe count (`opponent'*s`), `Skill`/`skill`
/// casing, the `sklil` typo, and a `your`/`their`/absent possessor — the whole family maps
/// without editing the living DB.
fn choose_skill_opp_debuff(clause: &str) -> Option<Vec<Effect>> {
    static RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?i)^Choose a skill: (?:your |their )?opponent'*s (?:skill|sklil) of that type is -(\d+)$",
        )
        .unwrap()
    });
    let c = RE.captures(clause.trim().trim_end_matches('.').trim())?;
    let delta: i64 = c[1].parse().ok()?;
    Some(vec![
        eff(
            on_hit(),
            vec![Action::ChooseSkill],
            Condition::Always,
            Duration::Instant,
        ),
        eff(
            Trigger::Static,
            vec![Action::BuffSkill {
                skill: Skill::ALL[0], // placeholder — target_chosen picks the bound skill
                delta: -delta,
                who: Who::Opp,
                duration: Duration::WhileInPlay,
                target_highest: false,
                target_lowest: false,
                target_chosen: true,
                per_crowd: false,
                cap: None,
                per: None,
                per_zone: CountZone::InPlay,
                per_excludes_self: false,
            }],
            Condition::Always,
            Duration::WhileInPlay,
        ),
    ])
}

/// One candidate " or " split for [`versatile_or_stop`]: parse each side, require exactly
/// one to be a pure Stop capability, return `[offensive, stop]`.
fn versatile_split(left: &str, right: &str) -> Option<Vec<Effect>> {
    let le = match_grammar(&capitalize_first(left.trim()))?;
    let re = match_grammar(&capitalize_first(right.trim()))?;
    let has_stop = |e: &Effect| e.actions.iter().any(|a| matches!(a, Action::Stop { .. }));
    match (has_stop(&le), has_stop(&re)) {
        (false, true) => Some(vec![le, re]),
        (true, false) => Some(vec![re, le]),
        _ => None, // neither or both are stops — not a versatile-or card
    }
}

/// "The player with the fewest/most cards in hand draws/discards N" — the actor is
/// the hand-size extreme, decided at resolution. Composed as TWO conditional effects
/// (one per seat) using a NON-STRICT compare (`<=` for fewest, `>=` for most) so a
/// TIE resolves for BOTH players: each side draws/discards iff its hand is `<=`/`>=`
/// the opponent's. No new IR — reuses `HandSizeCompare` + `Draw`/`Discard`.
fn hand_extreme_effects(clause: &str) -> Option<Vec<Effect>> {
    static RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?i)^The player with (?:the )?(fewest|most) cards in (?:their )?hand (draws|discards) (\d+) cards?(?: from their hand)?$",
        )
        .unwrap()
    });
    let c = RE.captures(clause.trim().trim_end_matches('.').trim())?;
    let most = c[1].eq_ignore_ascii_case("most");
    let draws = c[2].eq_ignore_ascii_case("draws");
    let n: i64 = c[3].parse().ok()?;
    let cmp = if most { Comparator::Ge } else { Comparator::Le };
    let mk = |who: Who| {
        let action = if draws {
            draw(n, who, DeckEnd::Top, None, Who::SelfSide)
        } else {
            discard(n, who, false, None, Who::SelfSide)
        };
        eff(
            on_hit(),
            vec![action],
            Condition::HandSizeCompare {
                cmp,
                vs: Vs::Opp,
                value: None,
                who,
            },
            Duration::Instant,
        )
    };
    Some(vec![mk(Who::SelfSide), mk(Who::Opp)])
}

/// `a AND b`, dropping a trivially-true `b` ("the body has no gate of its own").
fn and_conds(a: Condition, b: Condition) -> Condition {
    match b {
        Condition::Always => a,
        other => Condition::And {
            items: vec![a, other],
        },
    }
}

/// [`trigger_body`] with an extra gate AND-ed onto the body's own condition — used by
/// the multi-skill roll split, where the trigger fires on any roll (`OnRoll{None}`) and
/// `cond` restricts it to the named skills.
fn trigger_body_cond(trigger: Trigger, cond: Condition, body: &str) -> Option<Effect> {
    let mut effect = trigger_body(trigger, body)?;
    effect.condition = and_conds(cond, effect.condition);
    Some(effect)
}

/// A condition-GATED body: re-parse the body through the whole grammar (like
/// [`trigger_body`]) but KEEP its natural trigger, AND-ing `cond` onto its condition.
/// Used for standalone gate prefixes that restrict WHEN a body applies without changing
/// the event it hangs off — "If you rolled `<skill>` for your turn roll, `<body>`"
/// (a `RollWasSkill` gate resolved against the play-time turn-roll context) and "If you
/// have a card with 'X' in the name in play, `<body>`" (a `HasInPlay` gate). Returns
/// `None` when the body itself has no grammar.
fn gate_body(cond: Condition, body: &str) -> Option<Effect> {
    let body = body.trim();
    let (optional, body) = match body.strip_prefix("you may ") {
        Some(rest) => (true, rest),
        None => (false, body),
    };
    let cap = capitalize_first(body);
    let mut effect = match_grammar(&cap)
        .or_else(|| compound_body(&cap))
        .or_else(|| choice_body(&cap))?;
    effect.optional = effect.optional || optional;
    effect.condition = and_conds(cond, effect.condition);
    Some(effect)
}

/// The tail of a "When this card is in your discard pile …" clause, after the prefix.
/// The discard prefix is a [`Duration::WhileInDiscard`] DURATION marker — the card
/// declares this effect only while it sits in the discard pile — so `remainder` is a
/// normal trigger clause we re-parse through the whole grammar and then re-stamp the
/// duration on. Scope (task #115 slice 1): the TRIGGERED forms only — an inline "and
/// `<event>`, `<body>`" (rewritten to "When `<event>`, `<body>`") or a nested
/// "When/After/If … `<body>`" after the separator. A bare passive body (family A: "…
/// your maximum handsize is +N") declines here and stays Unsupported until the passive
/// discard-readers land, so we never emit an effect the engine would silently not fire.
fn while_in_discard_effect(remainder: &str) -> Option<Effect> {
    let r = remainder.trim();
    let inner = if let Some(rest) = r.strip_prefix("and ") {
        format!("When {rest}")
    } else {
        // Sentence-case the remainder so a lowercase mid-clause "if"/"when" ("…, if you
        // drew 1 or more cards this turn, …") still matches the capital-anchored triggers.
        let cap = uppercase_first(r);
        if ["When ", "After ", "If ", "Each ", "At "]
            .iter()
            .any(|p| cap.starts_with(p))
        {
            cap
        } else {
            // Passive body (family A): only bodies whose consumer scans the discard zone
            // may be emitted. Hand-size mods now have that reader (owner_hand_mods scans
            // discard), so a WhileInDiscard MaxHandSize/MinHandSize is landed; the rest stay
            // Unsupported rather than become silently-inert IR.
            return passive_discard_effect(&cap);
        }
    };
    let mut effect = match_grammar(&inner)
        .or_else(|| compound_body(&inner))
        .or_else(|| choice_body(&inner))?;
    // Fidelity gate: only WhileInDiscard triggers whose dispatch site fires from the
    // discard pile (with the self_card referent bound) may be emitted; the rest decline
    // and stay Unsupported rather than become silently-inert IR. Wired so far (task #115):
    // OnRoll (slice 1, run_on_roll), OnHit (slice 2a, run_hit_gimmicks_inner), OnStop
    // (slice 2b, run_on_stop_gimmicks), OnBreakout (slice 2b, on_broken_out),
    // OnBreakoutRoll (run_on_breakout_roll — the reactive breakout re-roll, My Most
    // Powerful Spell). Passive bodies and OnFlip remain gated out until their readers land.
    if !matches!(
        effect.trigger,
        Trigger::OnRoll { .. }
            | Trigger::OnHit { .. }
            | Trigger::OnStop { .. }
            | Trigger::OnBreakout { .. }
            | Trigger::OnBreakoutRoll { .. }
            | Trigger::OnReroll { .. }
            | Trigger::OnDraw { .. }
            | Trigger::OnLoseTurn { .. }
    ) {
        return None;
    }
    effect.duration = Duration::WhileInDiscard;
    Some(effect)
}

/// A passive family-A discard body — a bare Static effect (no trigger word) that the
/// engine reads directly from the discard pile. Only hand-size mods have a discard
/// reader (`owner_hand_mods` scans the pile), so an effect whose actions are all
/// `MaxHandSize`/`MinHandSize` is emitted with `WhileInDiscard`; anything else declines
/// and stays Unsupported. schema v136
fn passive_discard_effect(body: &str) -> Option<Effect> {
    let mut effect = match_grammar(body)?;
    let readable = matches!(effect.trigger, Trigger::Static)
        && !effect.actions.is_empty()
        && effect
            .actions
            .iter()
            .all(|a| matches!(a, Action::MaxHandSize { .. } | Action::MinHandSize { .. }));
    if !readable {
        return None;
    }
    effect.duration = Duration::WhileInDiscard;
    Some(effect)
}

/// "you have `<name-and-list>` in play" / "you have cards with `<list>` in play" — EACH
/// named card present in play, an AND of per-name `HasInPlay` gates. Distinct from a name
/// OR-list (one card matching any of the names); requires ≥2 quoted names, so a single-name
/// gate keeps its own branch. Names are matched as name-substrings. `None` otherwise.
fn multi_name_and_in_play(t: &str) -> Option<Condition> {
    static RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r#"(?i)^you have (?:cards? with )?("[^"]+"(?:,? (?:and )?"[^"]+")+)(?: in the name)? in play$"#,
        )
        .unwrap()
    });
    let names = quoted_names(&RE.captures(t)?[1]);
    if names.len() < 2 {
        return None;
    }
    Some(Condition::And {
        items: names
            .into_iter()
            .map(|n| has_in_play(Who::SelfSide, cf_name(vec![n]), 1))
            .collect(),
    })
}

/// "you have N `<Competitor>` Finishes in play" — the owner's competitor-finish set (its
/// `related_finishes`), NOT every Finish (a deck may run logoless finishes that are not the
/// competitor's), so it maps to [`Condition::RelatedFinishesInPlay`]. The `<Competitor>`
/// qualifier is descriptive — the engine reads the OWNER's competitor — so a bare type/order
/// qualifier ("Grapple Finishes", "Finish … cards") is DECLINED here and left to the ordinary
/// in-play gate. Syzygy's "2 Syzygy Finishes in play"; Void/Fortress name their own sets too.
fn related_finishes_in_play(t: &str) -> Option<Condition> {
    static RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^you have (\d+) ([A-Za-z][A-Za-z0-9' ]*?) [Ff]inishes in play$").unwrap()
    });
    let c = RE.captures(t)?;
    let name = c[2].trim();
    // A play-order / attack-type / skill qualifier is a Finish-TYPE count, not a competitor
    // set — decline so it never masquerades as a related-finishes gate.
    const SKILLS: [&str; 6] = [
        "power",
        "technique",
        "agility",
        "strike",
        "submission",
        "grapple",
    ];
    let is_skill = SKILLS.iter().any(|s| name.eq_ignore_ascii_case(s));
    if count_filter(name).is_some() || is_skill {
        return None;
    }
    Some(Condition::RelatedFinishesInPlay {
        count: c[1].parse().ok()?,
    })
}

/// Parse a turn-roll-DELTA gate — "your turn roll was [exactly|at least] N (greater|less)
/// than your opponent's[ turn roll]" — into a signed roll-gap [`Condition`]. The context's
/// `gap` is `opp - self`, so a self-GREATER roll is a NEGATIVE gap:
///   - "at least N greater" -> `RollLeadAtLeast{N}` (self leads by ≥ N)
///   - "at least N less"     -> `RollGapAtLeast{N}`  (self trails by ≥ N)
///   - "exactly N greater"   -> `RollGapExactly{-N}`
///   - "exactly N less"      -> `RollGapExactly{N}`
///
/// A bare "N greater/less" (no `exactly`/`at least` qualifier) DECLINES rather than guess
/// exact-vs-threshold, so it stays `Unsupported`.
fn roll_delta_gate(t: &str) -> Option<Condition> {
    static RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?i)^your turn roll was (exactly|at least) (\d+) (greater|less) than your opponent'?s(?: turn roll)?$",
        )
        .unwrap()
    });
    let c = RE.captures(t)?;
    let n: i64 = c[2].parse().ok()?;
    let at_least = c[1].eq_ignore_ascii_case("at least");
    let greater = c[3].eq_ignore_ascii_case("greater");
    Some(match (at_least, greater) {
        (true, true) => Condition::RollLeadAtLeast { k: n },
        (true, false) => Condition::RollGapAtLeast { k: n },
        (false, true) => Condition::RollGapExactly { k: -n },
        (false, false) => Condition::RollGapExactly { k: n },
    })
}

/// Parse a turn-roll-VALUE gate — "your turn roll is N" / "your opponent's turn roll is
/// N", with an optional "or greater" (`Ge`), "or less" (`Le`), or a second value "or M"
/// (an `Or` of two `Eq`s — Shade's "9 or 10"). The opponent form ("your opponent's turn
/// roll is N") is the head of a 12-clause family keyed by Scott Prime's The Loaded Glove;
/// the opp's value is derived from the actor's roll context (`value + gap`) at evaluation
/// time. Returns `None` for any other shape. schema v130.
fn turn_roll_value_gate(t: &str) -> Option<Condition> {
    static RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?i)^your (opponent's )?turn roll (?:is|was) (\d+)(?: or (greater|less|(\d+)))?$",
        )
        .unwrap()
    });
    let c = RE.captures(t)?;
    let who = if c.get(1).is_some() {
        Who::Opp
    } else {
        Who::SelfSide
    };
    let n: i64 = c[2].parse().ok()?;
    let val = |cmp, value| Condition::RollValue { cmp, value, who };
    match c.get(3).map(|m| m.as_str()) {
        None => Some(val(Comparator::Eq, n)),
        Some("greater") => Some(val(Comparator::Ge, n)),
        Some("less") => Some(val(Comparator::Le, n)),
        Some(_) => {
            let m: i64 = c[4].parse().ok()?;
            Some(Condition::Or {
                items: vec![val(Comparator::Eq, n), val(Comparator::Eq, m)],
            })
        }
    }
}

/// Parse a common conditional-gate phrase — the "`<gate>`" in "If `<gate>`, double these
/// bonuses" and kin — into a [`Condition`] the engine already evaluates. Covers turn-roll
/// (skill / value / same-as-opponent), the re-roll / bump / ended-turn flags, the no-DQ
/// match state, and in-play gates (type / order / name-substring). Returns `None` for a
/// gate not yet modeled, so the caller declines and the clause stays `Unsupported`.
fn gate_condition(text: &str) -> Option<Condition> {
    static ROLL_SELF: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(&format!(r"(?i)^you rolled {SK} for your turn roll$")).unwrap()
    });
    static ROLL_OPP: LazyLock<Regex> = LazyLock::new(|| {
        // "rolled" (past, a completed-roll gate) and "rolls" (present, "if your opponent
        // rolls X for their turn roll") — both are gate phrasings; the event family is
        // "When your opponent rolls X" and never routes through gate_condition.
        Regex::new(&format!(
            r"(?i)^your opponent roll(?:ed|s) {SK} for their turn roll$"
        ))
        .unwrap()
    });
    // "your [opponent's] turn roll was <S>" — the same rolled-skill gate written in the
    // "turn roll was" idiom (the value twin is `turn_roll_value_gate`'s "is|was").
    static ROLL_WAS_SK: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(&format!(r"(?i)^your (opponent's )?turn roll was {SK}$")).unwrap()
    });
    static ROLL_VAL: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^you rolled (\d+) for your turn roll$").unwrap());
    static HAVE_NAME: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r#"(?i)^you have (?:a card )?(?:in play )?with "([^"]+)" in the name(?: in play)?$"#,
        )
        .unwrap()
    });
    // "you have \"X\"[, \"Y\"][ or \"Z\"] in play" — a card named X on YOUR board (an
    // OR-list of quoted card titles; ≥1 present). Distinct from HAVE_NAME's "with X in the
    // name" (a name-substring qualifier) and NAMED_CARD_IN_PLAY's ownerless "the card X".
    static HAVE_TITLE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?i)^you have ("[^"]+"(?:(?:,\s*(?:or\s+)?|\s+or\s+)"[^"]+")*) in play$"#)
            .unwrap()
    });
    // "the card X is in play" (X quoted or a bare name) — an UNqualified named-card gate.
    // Per Brandon, an unqualified "the card X is in play" counts EITHER player's board, so
    // it maps to an Or of a self- and an opponent-side name gate (Roll Up / Senton Splash /
    // Full Mounted Choke Hold's combo enablers).
    static NAMED_CARD_IN_PLAY: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"(?i)^the card "?([^"]+?)"? is in play$"#).unwrap());
    static HAVE_INPLAY: LazyLock<Regex> = LazyLock::new(|| {
        // The article "a"/"an" ("you have a Stop in play") reads as count 1, like a bare
        // count; "another" keeps its own branch (tried first so "another" never matches
        // the "an" article). The count group stays group 1 (None -> 1).
        Regex::new(r"(?i)^you have (?:another |an? |(\d+) (?:or more )?)?(.+?) in play$").unwrap()
    });
    static HIT: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^you hit (?:a |an |another )?(.+?) (this|last) turn$").unwrap()
    });
    // The opponent-side twin of HIT ("if your opponent hit a Submission last turn, …") —
    // same HitCard, `who = Opp`, resolved against the opponent's hit history.
    static HIT_OPP: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^your opponent hit (?:a |an |another )?(.+?) (this|last) turn$").unwrap()
    });
    static OPP_PLAY: LazyLock<Regex> = LazyLock::new(|| {
        // Count is group 1 (a bare number, "or more" optional); the article "a"/"an"
        // ("your opponent has a Stop in play") is a countless branch that reads as 1.
        Regex::new(r"(?i)^your opponent has (?:(\d+)(?: or more)?|an?) (.+?) in play$").unwrap()
    });
    static OPP_PLAY_NONE: LazyLock<Regex> = LazyLock::new(|| {
        // "your opponent has no/0 X in play" and the "does not have [a/an] X in play" twin.
        Regex::new(r"(?i)^your opponent (?:has (?:no|0)|does not have(?: an?)?) (.+?) in play$")
            .unwrap()
    });
    static MATCH_TYPE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^this is an? (.+?) match$").unwrap());

    // Each regex-then-`recur_filter` branch FALLS THROUGH when the inner descriptor
    // doesn't parse (rather than `?`-returning), so a shape one branch's regex loosely
    // matches but can't map ("4 other Submission cards") still reaches `stop_condition`.
    let t = text.trim().trim_end_matches([',', ';', '.']).trim();
    if let Some(f) = OPP_PLAY_NONE
        .captures(t)
        .and_then(|c| recur_filter(c[1].trim()))
    {
        return Some(Condition::HasInPlay {
            who: Who::Opp,
            filter: f,
            count: 1,
            cmp: Comparator::Lt,
        });
    }
    if let Some(c) = OPP_PLAY.captures(t) {
        // Count group absent = the "a"/"an" article branch -> 1.
        let n = c
            .get(1)
            .map_or(1, |m| m.as_str().parse::<i64>().unwrap_or(1));
        if let Some(f) = recur_filter(c[2].trim()) {
            return Some(has_in_play(Who::Opp, f, n));
        }
    }
    if let Some(c) = HIT.captures(t) {
        if let Some(f) = recur_filter(c[1].trim()) {
            return Some(Condition::HitCard {
                filter: f,
                who: Who::SelfSide,
                last_turn: c[2].eq_ignore_ascii_case("last"),
            });
        }
    }
    if let Some(c) = HIT_OPP.captures(t) {
        if let Some(f) = recur_filter(c[1].trim()) {
            return Some(Condition::HitCard {
                filter: f,
                who: Who::Opp,
                last_turn: c[2].eq_ignore_ascii_case("last"),
            });
        }
    }
    if let Some(c) = ROLL_SELF.captures(t) {
        return Some(Condition::RollWasSkill {
            skill: skill(&c[1]),
            who: Who::SelfSide,
        });
    }
    if let Some(c) = ROLL_OPP.captures(t) {
        return Some(Condition::RollWasSkill {
            skill: skill(&c[1]),
            who: Who::Opp,
        });
    }
    if let Some(c) = ROLL_WAS_SK.captures(t) {
        let who = if c.get(1).is_some() {
            Who::Opp
        } else {
            Who::SelfSide
        };
        return Some(Condition::RollWasSkill {
            skill: skill(&c[2]),
            who,
        });
    }
    if let Some(c) = roll_delta_gate(t) {
        return Some(c);
    }
    if let Some(c) = ROLL_VAL.captures(t) {
        return Some(Condition::RollValue {
            cmp: Comparator::Eq,
            value: c[1].parse().ok()?,
            who: Who::SelfSide,
        });
    }
    match t.to_lowercase().as_str() {
        "you re-rolled your turn roll" | "you re-rolled your last turn roll" => {
            return Some(Condition::RerolledTurnRoll)
        }
        "you bumped on the last turn roll" | "you bumped on the previous turn roll" => {
            return Some(Condition::BumpedLastTurnRoll)
        }
        "you ended the last turn without playing a card" => {
            return Some(Condition::EndedTurnNoPlay { who: Who::SelfSide })
        }
        "this is the first turn of the game" | "this is the first turn of the match" => {
            return Some(Condition::FirstTurn)
        }
        "the stopped card did not have a competitor logo or skill requirement"
        | "the stopped card did not have a competitor logo or a skill requirement" => {
            return Some(Condition::StoppedCardNoLogoNoReq)
        }
        "you broke out last turn" => {
            return Some(Condition::BrokeOutLastTurn { who: Who::SelfSide })
        }
        "your opponent broke out last turn" => {
            return Some(Condition::BrokeOutLastTurn { who: Who::Opp })
        }
        "either player broke out last turn" | "any player broke out last turn" => {
            return Some(Condition::Or {
                items: vec![
                    Condition::BrokeOutLastTurn { who: Who::SelfSide },
                    Condition::BrokeOutLastTurn { who: Who::Opp },
                ],
            })
        }
        "you stopped a card last turn" => {
            return Some(Condition::StoppedCard {
                who: Who::SelfSide,
                last_turn: true,
            })
        }
        "your opponent stopped a card last turn" | "your opponent stopped your card last turn" => {
            return Some(Condition::StoppedCard {
                who: Who::Opp,
                last_turn: true,
            })
        }
        "your opponent stopped a card this turn" => {
            return Some(Condition::StoppedCard {
                who: Who::Opp,
                last_turn: false,
            })
        }
        "either player stopped a card last turn" | "any player played a stop card last turn" => {
            return Some(Condition::Or {
                items: vec![
                    Condition::StoppedCard {
                        who: Who::SelfSide,
                        last_turn: true,
                    },
                    Condition::StoppedCard {
                        who: Who::Opp,
                        last_turn: true,
                    },
                ],
            })
        }
        "you and your opponent rolled the same skill for your turn roll"
        | "you rolled the same skill as your opponent for your turn roll"
        | "you rolled the same skill as your opponent" => return Some(Condition::SameRolledSkill),
        "this is a no dq match" => return Some(Condition::MatchHasNoDisqualifications),
        _ => {}
    }
    if let Some(c) = MATCH_TYPE.captures(t) {
        if let Some(types) = match_type_set(&c[1]) {
            return Some(Condition::IsMatchType { types });
        }
    }
    if let Some(c) = HAVE_NAME.captures(t) {
        return Some(has_in_play(
            Who::SelfSide,
            cf_name(vec![c[1].to_owned()]),
            1,
        ));
    }
    if let Some(c) = HAVE_TITLE.captures(t) {
        let names = quoted_names(&c[1]);
        if !names.is_empty() {
            return Some(has_in_play(Who::SelfSide, cf_name(names), 1));
        }
    }
    if let Some(c) = NAMED_CARD_IN_PLAY.captures(t) {
        let filter = || cf_name(vec![c[1].to_owned()]);
        return Some(Condition::Or {
            items: vec![
                has_in_play(Who::SelfSide, filter(), 1),
                has_in_play(Who::Opp, filter(), 1),
            ],
        });
    }
    if let Some(c) = HAVE_INPLAY.captures(t) {
        let count = c
            .get(1)
            .map_or(1, |m| m.as_str().parse::<i64>().unwrap_or(1));
        if let Some(f) = recur_filter(c[2].trim()) {
            return Some(has_in_play(Who::SelfSide, f, count));
        }
    }
    // Fall back to the richer `stop_condition` parser (Crowd-Meter / skill-compare /
    // hand-compare / play-count / name-list / negation / tag gates), so every gated
    // family that routes through `gate_condition` (double-bonuses, the generic gate
    // rule, "also a <order>") shares its whole vocabulary.
    if let Some(c) = stop_condition(t) {
        return Some(c);
    }
    // "you have X and Y[ and Z] in play" — each named card present (an AND of per-name
    // gates). Tried before the generic compound splitter, whose " and " split would
    // bisect the name list into halves that don't parse on their own.
    if let Some(c) = multi_name_and_in_play(t) {
        return Some(c);
    }
    if let Some(c) = related_finishes_in_play(t) {
        return Some(c);
    }
    // Compound gate: "<A> or <B>" / "<A> and <B>" where each half is itself a modeled
    // gate — e.g. "you have another Follow Up in play and this is a Steel Cage match".
    // Tried ONLY after the whole phrase fails to parse atomically, so single conditions
    // that literally contain " and " / " or " ("you and your opponent rolled the same
    // skill", "the Crowd Meter is 2 or greater") are matched as a unit first and never
    // split. `or` splits BEFORE `and` so `and` binds tighter (A or B and C = A or (B and
    // C)). Composes existing `Condition`s under the existing And / Or — no schema change.
    if let Some(c) = split_compound_gate(t, " or ", |items| Condition::Or { items }) {
        return Some(c);
    }
    split_compound_gate(t, " and ", |items| Condition::And { items })
}

/// Split a gate phrase on a top-level `sep` (" and " / " or ", outside quoted names) and
/// parse each half via [`gate_condition`], combining them with `make` iff BOTH parse.
/// Tries each split point left-to-right and takes the first that fully parses, so a
/// leading atomic phrase that itself contains `sep` ("the Crowd Meter is 2 or greater or
/// …") is kept whole. A trailing half combined under the SAME connective flattens into
/// one flat node (A or (B or C) -> Or[A,B,C]); a differently-combined half stays nested,
/// preserving precedence. `None` (clause stays `Unsupported`) if no split point parses.
fn split_compound_gate(
    text: &str,
    sep: &str,
    make: fn(Vec<Condition>) -> Condition,
) -> Option<Condition> {
    for pos in top_level_split_points(text, sep) {
        let left = text[..pos].trim();
        let right = text[pos + sep.len()..].trim();
        if let (Some(a), Some(b)) = (gate_condition(left), gate_condition(right)) {
            let mut items = vec![a];
            match b {
                Condition::Or { items: more } if sep == " or " => items.extend(more),
                Condition::And { items: more } if sep == " and " => items.extend(more),
                other => items.push(other),
            }
            return Some(make(items));
        }
    }
    None
}

/// Byte offsets of every top-level `sep` in `text` — occurrences inside a `"…"` quoted
/// name (e.g. a card titled `"Bar and Grill"`) are skipped so a compound split never
/// bisects a name.
fn top_level_split_points(text: &str, sep: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut in_quote = false;
    for (i, ch) in text.char_indices() {
        if ch == '"' {
            in_quote = !in_quote;
        } else if !in_quote && text[i..].starts_with(sep) {
            out.push(i);
        }
    }
    out
}

/// Map one match-stipulation keyword to its [`MatchType`]. Handles the recurring
/// canonical names and their obvious spelling variants ("liger's den" / "ligers den",
/// "lumber jack"). Player-count / billing phrases ("singles", "main event") aren't
/// stipulations and return `None`, so their clauses stay `Unsupported`.
fn match_type_name(word: &str) -> Option<MatchType> {
    match word.trim().to_lowercase().replace('\'', "").as_str() {
        "steel cage" => Some(MatchType::SteelCage),
        "ligers den" | "liger den" => Some(MatchType::LigersDen),
        "ring of fire" => Some(MatchType::RingOfFire),
        "triad" => Some(MatchType::Triad),
        "tag team" => Some(MatchType::TagTeam),
        "steel chain" => Some(MatchType::SteelChain),
        "lumberjack" | "lumber jack" => Some(MatchType::Lumberjack),
        _ => None,
    }
}

/// Parse a match-stipulation phrase — the "`<X>`" in "this is a `<X>` Match" — into the
/// set of [`MatchType`]s it names, splitting an OR-list ("Steel Cage or Liger's Den").
/// `None` if ANY keyword is unrecognized, so a mixed phrase declines cleanly.
fn match_type_set(phrase: &str) -> Option<Vec<MatchType>> {
    let parts: Vec<&str> = phrase.split(" or ").collect();
    let types: Vec<MatchType> = parts.iter().filter_map(|p| match_type_name(p)).collect();
    (types.len() == parts.len() && !types.is_empty()).then_some(types)
}

/// "Strike, Submission, or Grapple" -> `RollWasSkill` OR-set for a `who=SELF` turn roll
/// (used as the gate on an `OnRoll{None}` multi-skill trigger). `None` if fewer than two
/// skills parse.
fn roll_was_any(list: &str) -> Option<Condition> {
    let normalized = list.replace(", or ", ", ").replace(" or ", ", ");
    let skills = skill_list(&normalized);
    if skills.len() < 2 {
        return None;
    }
    Some(Condition::Or {
        items: skills
            .into_iter()
            .map(|s| Condition::RollWasSkill {
                skill: s,
                who: Who::SelfSide,
            })
            .collect(),
    })
}

// --------------------------------------------------------------------------
// Text utilities
// --------------------------------------------------------------------------

/// Uppercase the first character (body clauses are lowercase mid-sentence, but the
/// grammar's rules expect sentence case).
fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
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
    // Bare "skills" (optionally "all"/"your") names EVERY skill — "your skills are +N",
    // Death by Elbow's "Your skills are +1 for each other card … with 'Elbow' in the name".
    // Handled before the "skill(s)"-stripping normalization (which would leave it empty).
    if matches!(
        text.trim().to_lowercase().as_str(),
        "skills" | "all skills" | "your skills" | "all of your skills"
    ) {
        return Skill::ALL.to_vec();
    }
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
    // Case-insensitive: a few stop-target clauses lowercase the word ("lead Strike"),
    // and the `(?i)` STOP_PART_RE can hand back any casing.
    match text.to_ascii_lowercase().as_str() {
        "strike" => AtkType::Strike,
        "grapple" => AtkType::Grapple,
        "submission" => AtkType::Submission,
        other => unreachable!("atk regex admitted {other:?}"),
    }
}

fn order(text: &str) -> PlayOrder {
    // Case-insensitive (see `atk`): admits "lead"/"Lead", "follow up"/"Follow Up", …
    match text.to_ascii_lowercase().as_str() {
        "lead" => PlayOrder::Lead,
        "follow up" => PlayOrder::Followup,
        "finish" => PlayOrder::Finish,
        other => unreachable!("order regex admitted {other:?}"),
    }
}

/// Integer capture group `i` (handles a leading `+`/`-` sign).
fn num(c: &Captures, i: usize) -> i64 {
    c[i].parse().expect("numeric capture parses")
}

/// A count that may be a digit or the words "a"/"an"/"one" (all -> 1). Used by the
/// reveal-and-discard family, where "reveals a card" and "reveals 1 card" are the same.
fn count_or_word(s: &str) -> i64 {
    match s.to_ascii_lowercase().as_str() {
        "a" | "an" | "one" => 1,
        "two" => 2,
        "three" => 3,
        d => d.parse().unwrap_or(1),
    }
}

// ---------------------------------------------------------------------------
// Count / stop-target helper parsers
// ---------------------------------------------------------------------------

static COUNT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:(lead|follow up|finish) )?(strike|grapple|submission)$").unwrap()
});
static STOP_PART_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:(Lead|Follow Up|Finish) )?(Strike|Grapple|Submission)$").unwrap()
});
/// A stop-target OR-part that is a bare order with NO type — "Follow Up" in "stop any
/// Follow Up or Finish Grapple", where the type on a later part distributes leftward.
static ORDER_ONLY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(Lead|Follow Up|Finish)$").unwrap());

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
    // Normalize the "Follow-Up" hyphen (the DB writes both "Follow Up" and "Follow-Up")
    // so COUNT_RE's "follow up" matches either — e.g. "3 Follow-Up Strikes in play".
    let lower = text.trim().to_lowercase().replace("follow-up", "follow up");
    let t = lower.trim_end_matches('s');
    // "skill requirement": the synthetic SkillRequirement tag (folded from a card's
    // `requirements:` block at load), a first-class selector everywhere count_filter is
    // read — HasInPlay gates, per-counts, recurs ("5 skill requirement cards in play").
    if t == "skill requirement" {
        return Some(cf_tag(crate::cards::SKILL_REQUIREMENT_TAG));
    }
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
    // "Spotlight": the synthetic Spotlight tag (folded from the DB `spotlight`
    // flag at load). A first-class selector everywhere count_filter is read —
    // Spotlight searches/recurs/per-counts ("a Spotlight card", "for each
    // Spotlight you have in play"). `t` is already lowercased + de-pluralized.
    if t == "spotlight" {
        return Some(cf_tag("Spotlight"));
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
    // "printed Finish" = a Finish by its printed play order, which is exactly what a
    // `Stop{order:Finish}` already matches (the engine keys on `attack.play_order`); the
    // word is emphasis, so drop it.
    let p = p.strip_prefix("printed ").unwrap_or(p);
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
        applies_name: None,
        player_scope: false,
    }
}

/// "Your cards cannot be stopped by `<order>`" — the player-scope twin of
/// [`unstoppable`]: shields EVERY one of the owner's cards against a stopper of that
/// play order (Cat/Dog/Sheep Uprising's "printed Finishes"). `player_scope` lets the
/// engine read it even from an in-play main-deck source.
fn unstoppable_player_scope(by_order: Option<PlayOrder>) -> Action {
    Action::Unstoppable {
        by_order,
        by_name: None,
        by_skillreq: false,
        applies_name: None,
        player_scope: true,
    }
}

/// "This card can only be stopped by `<order>`" — the WHITELIST inverse of the
/// "cannot be stopped by `<order>`" shield. The attack is `Unstoppable` against a stopper
/// of every OTHER play order, so only a stopper of `only` gets through. Modeled with NO
/// new IR as one `Unstoppable{by_order}` per complementary order (stoppers always carry a
/// real Lead/Follow Up/Finish order).
fn only_stopped_by(only: PlayOrder) -> Vec<Action> {
    [PlayOrder::Lead, PlayOrder::Followup, PlayOrder::Finish]
        .into_iter()
        .filter(|o| *o != only)
        .map(|o| unstoppable(Some(o), None))
        .collect()
}

/// "Cannot be stopped by Skill Requirement cards" — an `Unstoppable` keyed on the
/// stopper carrying a skill requirement. `player_scope` distinguishes the "Your cards
/// …" declaration (covers every owner card, read from in play) from "This card …".
fn unstoppable_skillreq(player_scope: bool) -> Action {
    Action::Unstoppable {
        by_order: None,
        by_name: None,
        by_skillreq: true,
        applies_name: None,
        player_scope,
    }
}

/// "Your cards with \"X\" in the name cannot be stopped[ by Skill Requirements]" — a
/// player-scope shield (`player_scope`) that protects only the owner's attacks whose
/// name contains `x`. Read by the engine's standing-effect scan
/// (`attack_is_unstoppable_by`), which AND-s `applies_name` against the attack; `by_skillreq`
/// further narrows the shield to stoppers carrying a skill requirement (Leader of the Unit
/// JT Dunn's "… with \"Elbow\" in the name cannot be stopped by Skill Requirements").
fn unstoppable_applies_name(x: &str, by_skillreq: bool) -> Action {
    Action::Unstoppable {
        by_order: None,
        by_name: None,
        by_skillreq,
        applies_name: Some(x.to_owned()),
        player_scope: true,
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

/// A FINISH-OFF-STOP effect: "[if the stopped card had no logo/req,] this card is also a
/// Finish [when played as a Stop]". An `OnStop{Theirs}` effect (fires when THIS card
/// stops something) carrying an optional Crowd-Meter swing plus the `FinishIfStop` marker;
/// the engine's `apply_stop` runs the finish sequence off the successful stop. The gate
/// (`Always` for "if played as a Stop", `StoppedCardNoLogoNoReq` for the logo/skill-req
/// variant) rides on the effect condition, so it also gates the Crowd-Meter swing.
fn finish_off_stop(cm_delta: Option<i64>, condition: Condition) -> Effect {
    let mut actions = Vec::new();
    if let Some(d) = cm_delta {
        actions.push(Action::CrowdMeter { delta: d });
    }
    actions.push(Action::FinishIfStop);
    eff(on_their_stop(), actions, condition, Duration::Instant)
}

/// Parse the guard of a conditional "If/When `<cond>`, this card cannot be stopped"
/// into a [`Condition`], covering the common gate shapes (Crowd Meter, skill-vs-opp,
/// hand size, in-play count / name-count / none, turn-roll value/skill, same skill).
/// `None` (the rule declines → stays `Unsupported`) for any shape not covered. The
/// engine evaluates this from the CARD OWNER's side with their turn roll context.
fn stop_condition(text: &str) -> Option<Condition> {
    static CROWD: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^the [Cc]rowd [Mm]eter is (\d+) or (greater|higher|less|lower)$").unwrap()
    });
    // "greater than" / "higher than" (synonyms) vs the opponent's same-or-other
    // skill; an optional "or equal to" promotes the comparator Gt -> Ge. Group 2 is
    // the "or equal to" flag, so the two skills are groups 1 and 3.
    static SKILL_GT: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(&format!(
            r"^your {SK}(?: skill)? is (?:greater|higher) than (or equal to )?your opponent'?s {SK}(?: skill)?$"
        ))
        .unwrap()
    });
    // Self-vs-self: two of the SAME player's skills — "your Agility skill is greater
    // than your Strike skill" (the #13/#14/#15 "equal-8" stops). No "opponent's", so
    // it never overlaps SKILL_GT. Group 2 is the "or equal to" flag; skills are 1, 3.
    static SKILL_GT_SELF: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(&format!(
            r"^your {SK}(?: skill)? is (?:greater|higher) than (or equal to )?your {SK}(?: skill)?$"
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
    static HAND_OPP_MORE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^your opponent has (\d+) or more cards in their hand$").unwrap()
    });
    // Relative hand-size gates vs the opponent ("fewer"/"less"/"more"), and the equal form.
    static HAND_REL: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^you have (fewer|less|more) cards in (?:your )?hand than your opponent$")
            .unwrap()
    });
    static HAND_SAME: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^you have the same number of cards in (?:your )?hand as your opponent$")
            .unwrap()
    });
    static HAND_OPP_REL: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^your opponent has (fewer|less|more) cards in their hand than you$")
            .unwrap()
    });
    // Relative in-play count vs the opponent ("you have fewer cards in play than your opponent").
    static PLAY_REL: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^you have (fewer|less|more) cards in play than your opponent$").unwrap()
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

    static IN_DISCARD_NAME: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"^"([^"]+)" is in your discard pile$"#).unwrap());

    let t = text.trim();
    if let Some(c) = CROWD.captures(t) {
        let cmp = if matches!(&c[2], "greater" | "higher") {
            Comparator::Ge
        } else {
            Comparator::Le
        };
        return Some(Condition::CrowdMeterCompare {
            cmp,
            value: c[1].parse().ok()?,
        });
    }
    // "\"X\" is in your discard pile" — a name-in-discard gate (a 4-card family: School
    // Boy Legend, Salt the Wound, Walk the Line, Spell 656). Combines under the compound
    // splitter for Spell 656's "the Crowd Meter is 2 or greater and \"School Boy\" is in
    // your discard pile".
    if let Some(c) = IN_DISCARD_NAME.captures(t) {
        return Some(Condition::HasInDiscard {
            who: Who::SelfSide,
            filter: cf_name(vec![c[1].to_owned()]),
            count: 1,
        });
    }
    if let Some(c) = SKILL_GT.captures(t) {
        let (s1, s2) = (skill(&c[1]), skill(&c[3]));
        let cmp = if c.get(2).is_some() {
            Comparator::Ge
        } else {
            Comparator::Gt
        };
        return Some(Condition::SkillCompare {
            skill: s1,
            cmp,
            who: Who::SelfSide,
            vs: Vs::OppSame,
            value: None,
            vs_skill: (s1 != s2).then_some(s2),
        });
    }
    if let Some(c) = SKILL_GT_SELF.captures(t) {
        let (s1, s2) = (skill(&c[1]), skill(&c[3]));
        let cmp = if c.get(2).is_some() {
            Comparator::Ge
        } else {
            Comparator::Gt
        };
        return Some(Condition::SkillCompare {
            skill: s1,
            cmp,
            who: Who::SelfSide,
            vs: Vs::SelfOther,
            value: None,
            vs_skill: Some(s2),
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
    if let Some(c) = HAND_OPP_MORE.captures(t) {
        return Some(hand_size(Comparator::Ge, Who::Opp, c[1].parse().ok()?));
    }
    if let Some(c) = HAND_REL.captures(t) {
        return Some(hand_size_vs_opp(fewer_more_cmp(&c[1]), Who::SelfSide));
    }
    if HAND_SAME.is_match(t) {
        return Some(hand_size_vs_opp(Comparator::Eq, Who::SelfSide));
    }
    if let Some(c) = HAND_OPP_REL.captures(t) {
        return Some(hand_size_vs_opp(fewer_more_cmp(&c[1]), Who::Opp));
    }
    if let Some(c) = PLAY_REL.captures(t) {
        return Some(in_play_vs(fewer_more_cmp(&c[1]), Who::SelfSide, Who::Opp));
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
            who: Who::SelfSide,
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
    // "without a Competitor logo" / "that doesn't|does not have a Competitor logo" — a
    // logo qualifier. The DB tags logoless cards `Logoless`; the attack must carry it, so
    // the target filter matches that synthetic tag (engine `card_matches` reads it).
    static LOGO_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^(.*?),? (?:that (?:doesn'?t|does not) have|without) a competitor logo$")
            .unwrap()
    });
    // "with a skill requirement" — a skill-requirement qualifier. The loader folds a
    // card's `requirements:` block into the synthetic `SkillRequirement` tag, so the
    // attack must carry it (engine `card_matches` reads `.tag`). Tolerates the DB's
    // "Requireement" typo (an extra e).
    static SKILLREQ_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^(.*?),? with an? skill requiree?ments?$").unwrap());
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
    if let Some(c) = LOGO_RE.captures(p) {
        return (c.get(1).unwrap().as_str(), Some(cf_tag("Logoless")));
    }
    if let Some(c) = SKILLREQ_RE.captures(p) {
        return (
            c.get(1).unwrap().as_str(),
            Some(cf_tag(crate::cards::SKILL_REQUIREMENT_TAG)),
        );
    }
    (p, None)
}

/// Peel a trailing "that is also a `<order>`[ or [a] `<order>`]" off a stop-target body,
/// returning the bare body and the ALSO-order list — "Stop any Finish Strike that is also
/// a Lead or a Follow Up". Peeled BEFORE the target OR-split so the inner " or " does not
/// masquerade as a second stop target. Only Lead/Follow Up appear (a Finish that is "also a
/// Finish" is meaningless).
fn strip_also_order(body: &str) -> (&str, Vec<PlayOrder>) {
    static RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?i)^(.*?) that is also (?:an? )?(Lead|Follow Up)(?: or (?:an? )?(Lead|Follow Up))?$",
        )
        .unwrap()
    });
    let word = |s: &str| match s.to_ascii_lowercase().as_str() {
        "lead" => PlayOrder::Lead,
        _ => PlayOrder::Followup, // the regex only admits "Lead" | "Follow Up"
    };
    if let Some(c) = RE.captures(body.trim()) {
        let mut orders = vec![word(&c[2])];
        if let Some(m) = c.get(3) {
            orders.push(word(m.as_str()));
        }
        return (c.get(1).unwrap().as_str(), orders);
    }
    (body, Vec::new())
}

/// Peel a trailing `with <name-list> in the (name|text)` qualifier — an OR/AND-list of TWO
/// OR MORE quoted names ("with \"Flying\" or \"Splash\" in the name") — off a stop-target
/// body at the BODY level, so the list's inner " or " is not mistaken for a second stop
/// target. Returns the bare head and a `name_contains`/`text_contains` filter. A SINGLE
/// quoted name declines here (returns `None`) so the per-part `strip_target_filter` keeps
/// handling it unchanged.
fn strip_target_names(body: &str) -> (&str, Option<CardFilter>) {
    static RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r#"(?i)^(.*?) with ("[^"]+"(?:(?:,\s*(?:or\s+)?|\s+or\s+)"[^"]+")+) in the (name|text)$"#,
        )
        .unwrap()
    });
    if let Some(c) = RE.captures(body.trim()) {
        let names = quoted_names(&c[2]);
        if names.len() >= 2 {
            return (
                c.get(1).unwrap().as_str(),
                Some(name_or_text_filter(&c[3], names)),
            );
        }
    }
    (body, None)
}

/// Parse a "stop any …" target into `Stop` actions, or `None` if any part is not
/// a plain `<type>` / `<order> <type>` (handles the "X or Y" two-target form). A
/// trailing "(that / even if it) cannot be stopped" flags every Stop to bypass the
/// attack's `Unstoppable`; a `with "X"[ or "Y"] in the name/text` qualifier sets `target`;
/// a trailing "that is also a Lead[ or a Follow Up]" sets `also_order` (a multi-order attack).
fn stop_targets(text: &str) -> Option<Vec<Action>> {
    static OR_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+or\s+").unwrap());
    let (body, even_unstoppable) = strip_stop_override(text.trim());
    let (body, also_order) = strip_also_order(body);
    // A multi-name qualifier is peeled at body level (its inner " or " must not split into a
    // second target); it applies to every type-part below. A single name stays per-part.
    let (body, list_target) = strip_target_names(body);
    // First pass: each OR-part is a full `<order?> <type>` or a BARE ORDER (whose type is
    // supplied by a later part — "Follow Up or Finish Grapple" = a Follow Up AND a Finish
    // Grapple). Anything else declines the whole target.
    let mut parts: Vec<StopPart> = Vec::new();
    for part in OR_RE.split(body) {
        let (head, part_target) = strip_target_filter(part);
        let norm = norm_stop_part(head);
        if let Some(m) = STOP_PART_RE.captures(norm) {
            parts.push(StopPart::Full {
                order: m.get(1).map(|g| order(g.as_str())),
                atk_type: atk(&m[2]),
                target: part_target,
            });
        } else {
            // A bare order ("Follow Up") with no type of its own; otherwise the whole
            // target is not a plain stop-part list and declines.
            let m = ORDER_ONLY_RE.captures(norm)?;
            parts.push(StopPart::BareOrder(order(&m[1]), part_target));
        }
    }
    // Second pass: a bare order inherits the type of the nearest FULL part (following ones
    // first, then preceding). If no part carries a type at all, the target is undetermined.
    let mut stops = Vec::new();
    for (i, p) in parts.iter().enumerate() {
        let (order, atk_type, target) = match p {
            StopPart::Full {
                order,
                atk_type,
                target,
            } => (*order, *atk_type, target.clone()),
            StopPart::BareOrder(o, target) => {
                let atk_type = parts[i + 1..]
                    .iter()
                    .chain(parts[..i].iter().rev())
                    .find_map(StopPart::atk_type)?;
                (Some(*o), atk_type, target.clone())
            }
        };
        stops.push(Action::Stop {
            order,
            atk_type: Some(atk_type),
            source_is_skillreq: false,
            even_unstoppable,
            target: list_target.clone().or(target),
            also_order: also_order.clone(),
        });
    }
    if stops.is_empty() {
        None
    } else {
        Some(stops)
    }
}

/// One parsed OR-part of a "stop any …" target (see [`stop_targets`]).
enum StopPart {
    Full {
        order: Option<PlayOrder>,
        atk_type: AtkType,
        target: Option<CardFilter>,
    },
    /// A bare order with no type of its own; its type is distributed from a `Full` part.
    BareOrder(PlayOrder, Option<CardFilter>),
}

impl StopPart {
    fn atk_type(&self) -> Option<AtkType> {
        match self {
            StopPart::Full { atk_type, .. } => Some(*atk_type),
            StopPart::BareOrder(..) => None,
        }
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

/// Per-count draw. The optional `name` descriptor (the "with 'X' in the name"
/// suffix that trails "you have in play") routes through `in_play_filter` — a
/// name-substring filter when present, else the `<desc>` selector (card / type /
/// order / stop). "Draw 1 card for each card you have in play with 'Table' in the
/// name."
fn per_draw(n: i64, desc: &str, per_who: Who, name: Option<&str>) -> Option<Effect> {
    let per = in_play_filter(desc, name)?;
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
    remove_opp_to(count, selector, false)
}

/// Remove N of the opponent's in-play cards, `to_deck` sending them to the deck BOTTOM
/// (a "bury") instead of the discard — "choose N cards your opponent has in play and
/// bury it/them" (JT Dunn's gimmick).
fn remove_opp_to(count: i64, selector: CardFilter, to_deck: bool) -> Action {
    Action::RemoveFromPlay {
        selector,
        who: Who::Opp,
        count,
        choose: false,
        to_deck,
        all: false,
    }
}

/// Wrap the per-player halves of an "each player …" board effect so a competitor with
/// a matching [`Action::RedirectAuthority`] (Emo Mam) may pick who they affect. Inert
/// (applies every half) when no such competitor is in the match. schema v135
fn redirect_board_effect(actions: Vec<Action>) -> Action {
    Action::RedirectBoardEffect { actions }
}

/// One player's half of Derailed: shuffle their WHOLE hand into their deck, then draw
/// `count` (`hand_count: None` = the whole hand).
fn shuffle_hand_draw(who: Who, count: i64) -> Action {
    Action::ShuffleHandDraw {
        who,
        count,
        choose: false,
        hand_count: None,
    }
}

/// Discard EVERY in-play card of `who` at once — "Discard all cards in play" (per the
/// controller's side / the opponent's side). No per-card pick.
fn discard_all_in_play(who: Who) -> Action {
    Action::RemoveFromPlay {
        selector: CardFilter::default(),
        who,
        count: 0,
        choose: false,
        to_deck: false,
        all: true,
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
/// Non-capturing skill alternation — for patterns that repeat a skill (multi-skill OR
/// lists) where capturing groups would shift the body's index.
const SKNC: &str = r"(?:Power|Technique|Agility|Strike|Submission|Grapple)";
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
        cap: None,
        per_excludes_self: false,
        per_crowd: false,
    }
}

/// A Static per-count [`Action::FinishRollBonus`]: `delta * floor(count of `per_who`'s
/// in-play `per`-matching cards / divisor)`, clamped to `cap`, dropping the source card
/// when `exclude_self` ("for each OTHER …"). The `per`-count Finish family builder.
fn finish_per(
    delta: i64,
    per: CardFilter,
    per_who: Who,
    cap: Option<i64>,
    divisor: Option<i64>,
    exclude_self: bool,
) -> Effect {
    eff(
        Trigger::Static,
        vec![Action::FinishRollBonus {
            delta,
            when_skill: None,
            either: false,
            when_base_le: None,
            when_base_ge: None,
            per: Some(per),
            per_who,
            per_zone: CountZone::InPlay,
            per_divisor: divisor,
            cap,
            per_excludes_self: exclude_self,
            per_crowd: false,
        }],
        Condition::Always,
        Duration::WhileInPlay,
    )
}

/// A breakout-roll modifier on `who`'s breakout rolls (`SelfSide` = the owner's own,
/// `Opp` = "your opponent's breakout rolls …"), gated to `attempts` (a single attempt
/// index; `None` = every attempt) and `when_skill` (`None` = any rolled skill). schema v94
fn breakout_mod_who(
    delta: i64,
    who: Who,
    attempts: Option<i64>,
    when_skill: Option<Skill>,
) -> Action {
    Action::BreakoutModifier {
        delta,
        attempts,
        when_skill,
        who,
        either: false,
        per: None,
        per_who: Who::SelfSide,
        per_zone: CountZone::InPlay,
        per_divisor: None,
        cap: None,
        per_excludes_self: false,
    }
}

/// A Static per-count [`Action::BreakoutModifier`] on `who`'s breakout rolls: `delta *
/// (count of `per_who`'s in-play `per`-matching cards)`, clamped to `cap`, dropping the
/// source card when `exclude_self` ("for each OTHER …"). `attempts` gates a single roll
/// index (`None` = every attempt) — an ordinal clause emits one per index. The breakout
/// parallel of [`finish_per`]. schema v112
#[allow(clippy::too_many_arguments)]
fn breakout_per(
    delta: i64,
    who: Who,
    attempts: Option<i64>,
    per: CardFilter,
    per_who: Who,
    per_zone: CountZone,
    cap: Option<i64>,
    exclude_self: bool,
) -> Action {
    Action::BreakoutModifier {
        delta,
        attempts,
        when_skill: None,
        who,
        either: false,
        per: Some(per),
        per_who,
        per_zone,
        per_divisor: None,
        cap,
        per_excludes_self: exclude_self,
    }
}

/// Resolve the "for each <sel> …" tail shared by the per-count breakout rules into
/// `(filter, per_who, per_zone)`. Mirrors the per-count Finish rule: a TYPE selector
/// routes through `count_filter`; a bare "card … with 'Y' in the name/text" routes
/// through `name_or_text_filter` (a combined type+name isn't one `CardFilter`, so it
/// declines). Exactly one of `inplay`/`disc` is `Some` — `inplay` names whose in-play
/// board to count ("you have"=self, else opp); `disc` names whose discard pile
/// ("your"=self, "their"/"your opponent's"=opp). Returns `None` when the selector
/// can't be mapped, so an unhandled shape stays Unsupported.
/// Parse an ordinal token — "1st"/"2nd"/"3rd"/… or the word forms "first"/"second"/
/// "third" — to its 1-based index, for the attempt-indexed breakout rules.
fn ordinal_num(tok: &str) -> Option<i64> {
    match tok.to_lowercase().as_str() {
        "first" => Some(1),
        "second" => Some(2),
        "third" => Some(3),
        t => t
            .trim_end_matches(|c: char| c.is_alphabetic())
            .parse::<i64>()
            .ok(),
    }
}

fn breakout_per_target(
    sel: &str,
    inplay: Option<&str>,
    disc: Option<&str>,
    names: Option<&str>,
    kind: Option<&str>,
) -> Option<(CardFilter, Who, CountZone)> {
    let per = match names {
        Some(list_text) => {
            let bare = sel.trim_end_matches('s').eq_ignore_ascii_case("card");
            let list = quoted_names(list_text);
            if !bare || list.is_empty() {
                return None;
            }
            name_or_text_filter(kind.unwrap_or("name"), list)
        }
        None => count_filter(sel)?,
    };
    let (per_who, per_zone) = match (inplay, disc) {
        (Some(g), _) => {
            let w = if g.eq_ignore_ascii_case("you have") {
                Who::SelfSide
            } else {
                Who::Opp
            };
            (w, CountZone::InPlay)
        }
        (_, Some(g)) => {
            let w = if g.eq_ignore_ascii_case("your") {
                Who::SelfSide
            } else {
                Who::Opp
            };
            (w, CountZone::Discard)
        }
        _ => return None,
    };
    Some((per, per_who, per_zone))
}

/// The affected side of a breakout-attempt-count clause, from the owner's POV: "you get
/// …" -> `SelfSide`; "your opponent"/"they get …" -> `Opp` (in this family "they" is the
/// opponent). Used by the [`Action::BreakoutAttempts`] rules.
fn attempts_who(subject: &str) -> Who {
    if subject.trim().eq_ignore_ascii_case("you") {
        Who::SelfSide
    } else {
        Who::Opp
    }
}

/// A [`Action::BreakoutAttempts`] modifying `who`'s breakout-roll COUNT — `set` overrides
/// the base ("gets N Breakout rolls"), else `delta` shifts it ("gets N additional/fewer").
/// `per` (with per_who/per_zone/cap/exclude_self) scales `delta` per counted card. The
/// count-family sibling of [`breakout_per`]. schema v113
fn breakout_attempts_action(
    delta: i64,
    set: Option<i64>,
    who: Who,
    per: Option<(CardFilter, Who, CountZone)>,
    cap: Option<i64>,
    exclude_self: bool,
) -> Action {
    let (per, per_who, per_zone) = match per {
        Some((f, w, z)) => (Some(f), w, z),
        None => (None, Who::SelfSide, CountZone::InPlay),
    };
    Action::BreakoutAttempts {
        delta,
        set,
        who,
        per,
        per_who,
        per_zone,
        per_divisor: None,
        cap,
        per_excludes_self: exclude_self,
    }
}

/// A rolled-skill-gated SELF breakout-roll bonus ("+1 to Strike during your breakout
/// rolls", Pineapple). `when_skill` = None applies to every breakout roll. schema v79
fn breakout_mod(delta: i64, when_skill: Option<Skill>) -> Action {
    breakout_mod_who(delta, Who::SelfSide, None, when_skill)
}

// ---------------------------------------------------------------------------
// Grammar rule table (regex -> Effect builder).
//
// `match_grammar` takes the FIRST rule whose regex matches, so ORDER IS SEMANTIC:
// more-specific patterns must precede the general fallbacks. The table is split
// into domain sub-tables purely for navigability; both `build_rules` (the live
// `RULES`) and `rule_catalog` (the grammar-catalog tooling) read the SAME ordered
// `domain_tables`, so the catalog order matches `RULES` by construction. Do not
// reorder `domain_tables`, or rules within a sub-table, without re-checking the
// parser golden (fixtures/parser/cards.ir.json via tests/parser_parity.rs).
// ---------------------------------------------------------------------------

/// One domain sub-table: a short name and one-line description (both surfaced in
/// the generated grammar catalog) plus its ordered `(regex, builder)` rules.
type DomainTable = (&'static str, &'static str, Vec<(Regex, Builder)>);

/// The ordered grammar domains — THE single source of rule order. `build_rules`
/// flattens it into `RULES`; `rule_catalog` walks it for the catalog. Keep both
/// this list and each `build_*_rules` body in precedence order (first match wins).
fn domain_tables() -> Vec<DomainTable> {
    vec![
        ("skill_buff", "Finish- and skill-roll buffs: flat, per-count, extreme (lowest/highest), and Crowd-Meter-scaled skill bonuses.", build_skill_buff_rules()),
        ("draw_search", "Draw, discard, reveal, search, shuffle, and peek — self / opponent / each-player.", build_draw_search_rules()),
        ("turn_roll", "Turn-roll modifiers (per-count, conditional, multi-turn, opponent-directed) and hand-size caps.", build_turn_roll_rules()),
        ("dq_loss", "If-stopped / breakout-roll loss family (pay-or-lose, discard-or-lose, pinfall).", build_dq_loss_rules()),
        ("flip_crowd_reroll", "Flip N, Crowd-Meter swings, turn/finish/breakout/costed re-rolls, and extra-card grants.", build_flip_crowd_reroll_rules()),
        ("flip_trigger", "Gimmick-blank, flip-until, flip self-triggers, flip-pool selects, and standing flip triggers.", build_flip_trigger_rules()),
        ("bury_discard", "Provenance-gated flip triggers, per-count bury, and bury/discard across discard piles.", build_bury_discard_rules()),
        ("removal_hand", "In-play removal (discard the opponent's board) and hand disruption (bury/discard from hand).", build_removal_hand_rules()),
        ("recur", "Recursion: discard->hand, shuffle-into-deck, recur-to-deck-top, and conditional recur.", build_recur_rules()),
        ("unstoppable_draw", "Unstoppable-by gates and draw riders (deck-position, conditional, on-roll).", build_unstoppable_draw_rules()),
        ("reveal_alsolead", "Reveal-and-discard, the also-a-<order> family, and no-DQ / cannot-be-disqualified rules.", build_reveal_alsolead_rules()),
        ("finish_breakout", "Symmetric roll mods, Finish-roll skill gates, breakout bonuses/attempts, and per-count Finish.", build_finish_breakout_rules()),
        ("stop_trigger", "Stop rules and generic trigger-body splits (on hit/roll/breakout/stop/start) plus the catch-all gate.", build_stop_trigger_rules()),
    ]
}

fn build_rules() -> Vec<(Regex, Builder)> {
    domain_tables()
        .into_iter()
        .flat_map(|(_, _, table)| table)
        .collect()
}

/// A single grammar rule, as surfaced by [`rule_catalog`].
pub struct RuleInfo {
    /// The domain sub-table this rule belongs to.
    pub domain: &'static str,
    /// One-line description of the domain.
    pub description: &'static str,
    /// Global precedence index into `RULES` (lower = matched first).
    pub index: usize,
    /// The rule's anchored regex source.
    pub pattern: String,
}

/// Every grammar rule in precedence order, tagged with its domain — the inventory
/// behind the generated grammar catalog. Built from the same `domain_tables` as
/// `RULES`, so `index` lines up with the live table.
pub fn rule_catalog() -> Vec<RuleInfo> {
    let mut out = Vec::new();
    for (domain, description, table) in domain_tables() {
        for (re, _) in &table {
            let index = out.len();
            out.push(RuleInfo {
                domain,
                description,
                index,
                pattern: re.as_str().to_owned(),
            });
        }
    }
    out
}

/// The canonical rule-inventory JSON (one `{index, domain, pattern}` object per line,
/// in precedence order): the DB-free contract that guards the grammar catalog against
/// silent drift. `srg grammar-catalog` writes it to `fixtures/parser/rule_index.json`;
/// `tests/grammar_catalog.rs` asserts the committed copy still equals this, so adding
/// or reordering a rule without regenerating the catalog fails the suite.
pub fn rule_index_json() -> String {
    let rows: Vec<String> = rule_catalog()
        .iter()
        .map(|r| {
            serde_json::json!({
                "index": r.index,
                "domain": r.domain,
                "pattern": r.pattern,
            })
            .to_string()
        })
        .collect();
    format!("[\n{}\n]\n", rows.join(",\n"))
}

fn build_skill_buff_rules() -> Vec<(Regex, Builder)> {
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
        rule(r"\+(\d+) to (?:your )?Finish [Rr]olls?", |c| {
            Some(eff(
                Trigger::Static,
                finish_roll_bonus(num(c, 1)),
                Condition::Always,
                Duration::WhileInPlay,
            ))
        }),
        // "[Rr]oll" — the game term "Finish Roll" is capitalized on ~7 cards (Stung's The
        // Bee Sting: "Your Finish Roll is +3"); accept either case.
        rule(r"Your Finish [Rr]olls? (?:is|are) ([+-]\d+)", |c| {
            Some(eff(
                Trigger::Static,
                finish_roll_bonus(num(c, 1)),
                Condition::Always,
                Duration::WhileInPlay,
            ))
        }),
        // Bare skill finish-roll bonus stat line — the printed "+N to <S>" (and the
        // negative "-N to <S>", a finish-roll penalty; both signs appear together on a
        // card, e.g. "-3 to Technique / +4 to Grapple"). Folds into the card's
        // finish_bonuses. The `$` anchor leaves the "… during turn rolls" phrasing to its
        // own later rule.
        rule(&format!(r"([+-]\d+) to {SK}"), |c| {
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
                        target_lowest: false,
                        target_chosen: false,
                        per_crowd: false,
                        cap,
                        per: Some(cf_name(names)),
                        per_zone: CountZone::InPlay,
                        per_excludes_self: false,
                    }],
                    Condition::Always,
                    Duration::WhileInPlay,
                ))
            },
        ),
        // Phase-scoped skill buff: "Your <skills> [is|are] +N during turn rolls" (one
        // or more skills) -> a standing TurnRollBonus per skill, applied only when the
        // turn roll comes up that skill (the roll-off parallel of FinishRollBonus).
        // Unlike a plain BuffSkill it does NOT affect finish rolls / stops / skill
        // comparisons. `skill_list` handles the "skill" word + and/comma lists and
        // declines cleanly on any unknown token.
        rule(
            r"Your (.+?) (?:is|are) \+(\d+) during (?:your )?turn rolls",
            |c| {
                let skills = skill_list(&c[1]);
                if skills.is_empty() {
                    return None;
                }
                Some(eff(
                    Trigger::Static,
                    turn_roll_bonuses(skills, num(c, 2)),
                    Condition::Always,
                    Duration::WhileInPlay,
                ))
            },
        ),
        // The "+N to <S> during turn rolls" phrasing (would otherwise fall to the
        // finish-roll-only "+N to <S>" default) -> the same TurnRollBonus.
        rule(
            &format!(r"\+(\d+) to {SK} during (?:your )?turn rolls"),
            |c| {
                Some(eff(
                    Trigger::Static,
                    turn_roll_bonuses(vec![skill(&c[2])], num(c, 1)),
                    Condition::Always,
                    Duration::WhileInPlay,
                ))
            },
        ),
        rule(&format!(r"Your {SK}(?: skill)? is \+(\d+)"), |c| {
            Some(eff(
                Trigger::Static,
                vec![buff(skill(&c[1]), num(c, 2), Who::SelfSide)],
                Condition::Always,
                Duration::WhileInPlay,
            ))
        }),
        // Standing skill buff gated on a SECOND card of two play orders sharing an
        // attack type: "If you have another Follow Up or Finish <ATK> in play, your
        // <SK> skill is +N." Every printed card with this clause is itself a Follow Up
        // of the gated attack type, and the effect is Static (fires only while in
        // play), so the engine's HasInPlay — which counts the source card — reads
        // "another" faithfully as count >= 2 (this card plus at least one other). The
        // buff re-evaluates live off the board, so a qualifying card entering later
        // turns it on.
        rule(
            &format!(
                r"If you have another (Lead|Follow Up|Finish) or (Lead|Follow Up|Finish) {ATK} in play, your {SK}(?: skill)? is \+(\d+)"
            ),
            |c| {
                let filter = CardFilter {
                    play_orders: vec![
                        count_order(&c[1].to_lowercase()),
                        count_order(&c[2].to_lowercase()),
                    ],
                    atk_type: Some(atk(&c[3])),
                    ..Default::default()
                };
                Some(eff(
                    Trigger::Static,
                    vec![buff(skill(&c[4]), num(c, 5), Who::SelfSide)],
                    has_in_play(Who::SelfSide, filter, 2),
                    Duration::WhileInPlay,
                ))
            },
        ),
        // "+N to your lowest/highest skill" and "Your lowest/highest skill is +N" -> a
        // dynamic-target skill buff, resolved to the extreme BASE skill at derived-stats
        // time (`resolve_buff`), mirroring Copy Kat's `target_highest`.
        rule(r"\+(\d+) to your (lowest|highest) skill", |c| {
            Some(eff(
                Trigger::Static,
                vec![buff_extreme(&c[2] == "highest", num(c, 1), Who::SelfSide)],
                Condition::Always,
                Duration::WhileInPlay,
            ))
        }),
        rule(r"Your (lowest|highest) skill is \+(\d+)", |c| {
            Some(eff(
                Trigger::Static,
                vec![buff_extreme(&c[1] == "highest", num(c, 2), Who::SelfSide)],
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
        // Multi-skill per-count buff, "Your X [and Y] are +N for each [other] card you
        // have in play with 'Z' in the name/text" (Evee's Spell Breaker). The single-skill
        // "Your X skill is +N for each …" rule above wins first for its exact shape.
        // "other" sets per_excludes_self (the source card drops from the count).
        rule(
            r#"Your (.+?) (?:is|are) \+(\d+) for each (other )?card you have in play with (.+?) in the (name|text)(?: \(Max \+(\d+)\))?"#,
            |c| {
                let skills = skill_list(&c[1]);
                let names = quoted_names(&c[4]);
                if skills.is_empty() || names.is_empty() {
                    return None;
                }
                let delta = num(c, 2);
                let excl = c.get(3).is_some();
                let cap = c.get(6).map(|m| m.as_str().parse::<i64>().unwrap());
                let filter = name_or_text_filter(&c[5], names);
                let actions = skills
                    .into_iter()
                    .map(|s| buff_per(s, delta, Some(filter.clone()), cap, excl))
                    .collect();
                Some(eff(
                    Trigger::Static,
                    actions,
                    Condition::Always,
                    Duration::WhileInPlay,
                ))
            },
        ),
        // Same per-count buff phrased "+N to X [and Y] for each [other] card … in the
        // name/text" (Witch's My Most Powerful Spell; Postal's "…in the text (Max +4)").
        rule(
            r#"\+(\d+) to (.+?) for each (other )?card you have in play with (.+?) in the (name|text)(?: \(Max \+(\d+)\))?"#,
            |c| {
                let skills = skill_list(&c[2]);
                let names = quoted_names(&c[4]);
                if skills.is_empty() || names.is_empty() {
                    return None;
                }
                let delta = num(c, 1);
                let excl = c.get(3).is_some();
                let cap = c.get(6).map(|m| m.as_str().parse::<i64>().unwrap());
                let filter = name_or_text_filter(&c[5], names);
                let actions = skills
                    .into_iter()
                    .map(|s| buff_per(s, delta, Some(filter.clone()), cap, excl))
                    .collect();
                Some(eff(
                    Trigger::Static,
                    actions,
                    Condition::Always,
                    Duration::WhileInPlay,
                ))
            },
        ),
        // Type-counted per-count buff (task #131): "Your X [and Y] is/are +N for each
        // [other] <type> you have in play [(Max +M)]" — the same buff_per scaling as the
        // name-count rules above, but the count ranges over a card TYPE (atk / play
        // order / stop) via count_filter instead of a name substring. Own board only
        // (BuffSkill.per counts the buffed player's board; opponent-board forms need
        // per_who and stay Unsupported). "other" sets per_excludes_self. "for each card …"
        // declines here (count_filter has no bare-card filter) — the name rules own it.
        rule(
            r"Your (.+?) (?:is|are) \+(\d+) for each (other )?(.+?) you have in play(?: \(Max \+?(\d+)\))?",
            |c| type_count_buff(&c[1], num(c, 2), &c[4], c.get(5), c.get(3).is_some()),
        ),
        // Same, phrased "+N to X [and Y] for each [other] <type> you have in play [(Max +M)]".
        rule(
            r"\+(\d+) to (.+?) for each (other )?(.+?) you have in play(?: \(Max \+?(\d+)\))?",
            |c| type_count_buff(&c[2], num(c, 1), &c[4], c.get(5), c.get(3).is_some()),
        ),
        // Crowd-Meter skill buff (task #131): "Your X [and Y] is/are + the Crowd Meter
        // [(Max +M)]" -> BuffSkill{per_crowd} (Copy Kat's dynamic delta, was override-
        // only). skill_list declines "Finish roll"/"breakout rolls" (own mechanisms) and
        // "+ double/triple the Crowd Meter" fails the literal "+ the" so it stays tail.
        rule(
            r"Your (.+?) (?:is|are) \+ the [Cc]rowd [Mm]eter(?: \((?:Max|max) \+?(\d+)\))?",
            |c| crowd_meter_buff(&c[1], c.get(2)),
        ),
    ]
}

fn build_draw_search_rules() -> Vec<(Regex, Builder)> {
    vec![
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
        // Deck-OR-discard tutor: "Search your deck or discard pile for <SEL> and add
        // it/them to your hand" -> Search{source:DeckOrDiscard} (the pool is deck ∪
        // discard; the found card leaves whichever zone holds it). Placed before the plain
        // deck rule. Compound tails ("…, or you may force a re-roll", "…: add 1 …") still
        // decline and stay Unsupported.
        rule(
            r#"Search your deck or discard pile for (.+?) and add (?:it|them) to your hand"#,
            |c| {
                let (filter, count) = search_target(&c[1])?;
                Some(eff(
                    on_hit(),
                    vec![search_both(filter, Dest::Hand, count)],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        // Deck tutor (Search, previously override-only): "Search your deck for <SEL>
        // and <route>". Three destinations — hand, top of the shuffled deck, discard
        // pile. Compound tails ("… , or each player buries", "…: add 1 …") decline here
        // and stay Unsupported for now.
        rule(
            r#"Search your deck for (.+?) and add (?:it|them) to your hand"#,
            |c| {
                let (filter, count) = search_target(&c[1])?;
                Some(eff(
                    on_hit(),
                    vec![search(filter, Dest::Hand, count)],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        rule(
            r#"Search your deck for (.+?) and put (?:it|them) on top of your shuffled deck"#,
            |c| {
                let (filter, count) = search_target(&c[1])?;
                Some(eff(
                    on_hit(),
                    vec![search(filter, Dest::DeckTop, count)],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        // Self-type gated tutor: "If you have another <order|atk> in play, search your
        // deck for <SEL> and put it on top of your shuffled deck" (Thunderous Punch, Double
        // Throw, Blood in the Water, the "another Follow Up" trio). Every printed card with
        // this clause is itself of the gated order/type, and the tutor fires OnHit (after
        // the card entered play — `record_landed_hit` precedes the OnHit pass), so the
        // engine's HasInPlay, which counts the source card, must read "another" as count>=2
        // — this card plus at least one other. Placed before the generic gate rule (whose
        // gate_condition emits the always-true count=1) so this faithful model wins. Mirrors
        // the count>=2 convention of the "another … your next turn roll is +N" rule below.
        rule(
            r#"If you have another (.+?) in play, search your deck for (.+?),? and put (?:it|them) on top of your shuffled deck"#,
            |c| {
                let gate = count_filter(&c[1])?;
                let (filter, count) = search_target(&c[2])?;
                Some(eff(
                    on_hit(),
                    vec![search(filter, Dest::DeckTop, count)],
                    has_in_play(Who::SelfSide, gate, 2),
                    Duration::Instant,
                ))
            },
        ),
        // Self-type gated hand cycle: "If you have another <order> in play, bury up to N
        // cards in your hand to draw the same number of cards +M" (Stolen Valor, Back
        // Cracker Potion, Win When You Can…). BuryToDraw sheds up to N and draws (buried+M).
        // Every printed card is itself of the gated order and fires OnHit (after landing),
        // so "another" is count>=2. Placed before the generic gate rule (its count=1) so
        // this faithful model wins. Mirrors the search-to-top family above.
        rule(
            r#"If you have another (.+?) in play, bury up to (\d+) cards? in your hand to draw the same number of cards \+(\d+)"#,
            |c| {
                let gate = count_filter(&c[1])?;
                Some(eff(
                    on_hit(),
                    vec![Action::BuryToDraw {
                        max: num(c, 2),
                        bonus: num(c, 3),
                        who: Who::SelfSide,
                    }],
                    has_in_play(Who::SelfSide, gate, 2),
                    Duration::Instant,
                ))
            },
        ),
        rule(
            r#"Search your deck for (.+?) and put (?:it|them) in(?:to)? your discard pile"#,
            |c| {
                let (filter, count) = search_target(&c[1])?;
                Some(eff(
                    on_hit(),
                    vec![search(filter, Dest::Discard, count)],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        // Split tutor (Stung's The Buzzkill): "Search your deck for 2 <X>: Add 1 to your
        // hand and put the other in your discard pile" — two 1-card searches of the same
        // type, one to hand, one to discard.
        rule(
            r#"Search your deck for (.+?): [Aa]dd 1 to your hand and put the other in your discard pile"#,
            |c| {
                let (filter, _count) = search_target(&c[1])?;
                Some(eff(
                    on_hit(),
                    vec![
                        search(filter.clone(), Dest::Hand, 1),
                        search(filter, Dest::Discard, 1),
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
                vec![redirect_board_effect(vec![
                    bury_whole_discard(Who::SelfSide),
                    bury_whole_discard(Who::Opp),
                ])],
                Condition::Always,
                Duration::Instant,
            ))
        }),
        // "Discard all cards in play" (Apocalypse) — a board clear, one half per player
        // (the controller's board, the opponent's board), so Emo Mam's redirect can
        // spare either. Absent that gimmick both halves apply. schema v135
        rule(r"Discard all cards in play", |_| {
            Some(eff(
                on_hit(),
                vec![redirect_board_effect(vec![
                    discard_all_in_play(Who::SelfSide),
                    discard_all_in_play(Who::Opp),
                ])],
                Condition::Always,
                Duration::Instant,
            ))
        }),
        // "Each player shuffles their hand into their deck, then adds the top N cards of
        // their deck to their hand" (Derailed) — a symmetric hand refresh, one
        // ShuffleHandDraw half per player, wrapped for Emo Mam's redirect. schema v135
        rule(
            r"Each player shuffles their hand into their deck, then adds the top (\d+) cards? of their deck to their hand",
            |c| {
                let n = num(c, 1);
                Some(eff(
                    on_hit(),
                    vec![redirect_board_effect(vec![
                        shuffle_hand_draw(Who::SelfSide, n),
                        shuffle_hand_draw(Who::Opp, n),
                    ])],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        // Emo Mam's gimmick: "When you or your opponent hit '<cards>', you may choose who
        // it affects (one, both, or neither player)." A passive redirect authority over
        // the listed cards' board effects (their `RedirectBoardEffect`), keyed by the
        // resolving card's name. The `(.+?)` captures the quoted card list. schema v135
        rule(
            r"When you or your opponent hit (.+?), you may choose who it affects \(one, both, or neither player\)",
            |c| {
                let groups = quoted_names(&c[1]);
                if groups.is_empty() {
                    return None;
                }
                Some(eff(
                    Trigger::Static,
                    vec![Action::RedirectAuthority { groups }],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
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
        // "Each player discards N cards from their hand" — the non-random twin: each
        // player sheds their OWN choice (choose=false, random=false), so who=SelfSide
        // + who=Opp both mean the hand owner picks. Two Discard actions.
        rule(r"Each player discards (\d+) cards? from their hand", |c| {
            Some(eff(
                on_hit(),
                vec![
                    discard(num(c, 1), Who::SelfSide, false, None, Who::SelfSide),
                    discard(num(c, 1), Who::Opp, false, None, Who::SelfSide),
                ],
                Condition::Always,
                Duration::Instant,
            ))
        }),
        // "Each player discards their hand" — both players shed their ENTIRE hand
        // (discard-all, count derived from hand size). Two Discard{all} actions.
        rule(r"Each player discards their hand", |_| {
            Some(eff(
                on_hit(),
                vec![
                    discard_all_hand(CardFilter::default(), Who::SelfSide),
                    discard_all_hand(CardFilter::default(), Who::Opp),
                ],
                Condition::Always,
                Duration::Instant,
            ))
        }),
        // "Each player discards the bottom card of their deck" — a deck-to-discard mill
        // from the BOTTOM for both players (MillDeck, not a Flip). "play" is a common DB
        // typo for "player". Two MillDeck actions.
        rule(
            r"Each play(?:er)? discards the (top|bottom) card of their deck",
            |c| {
                let from = if &c[1] == "top" {
                    DeckEnd::Top
                } else {
                    DeckEnd::Bottom
                };
                Some(eff(
                    on_hit(),
                    vec![
                        Action::MillDeck {
                            who: Who::SelfSide,
                            count: 1,
                            from,
                        },
                        Action::MillDeck {
                            who: Who::Opp,
                            count: 1,
                            from,
                        },
                    ],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        // "Reveal your hand to your opponent" (Bermuda Triangle) — fog-of-war: expose
        // EVERY card in your own hand at once (`whole_hand`, no per-card choice). The
        // optional "to your opponent" / "entire|whole" wording is informational.
        rule(
            r"Reveal your (?:entire |whole )?hand(?: to your opponent)?",
            |_c| {
                Some(eff(
                    on_hit(),
                    vec![Action::Reveal {
                        who: Who::SelfSide,
                        count: 0,
                        whole_hand: true,
                    }],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        // "Each player reveals N card(s) in their hand" — fog-of-war: each player
        // reveals N of their own hand cards to the opponent (their own choice, resolved
        // by the engine's `reveal` decision). Two Reveal actions (SELF + OPP).
        rule(r"Each player reveals (\d+) cards? in their hand", |c| {
            Some(eff(
                on_hit(),
                vec![
                    Action::Reveal {
                        who: Who::SelfSide,
                        count: num(c, 1),
                        whole_hand: false,
                    },
                    Action::Reveal {
                        who: Who::Opp,
                        count: num(c, 1),
                        whole_hand: false,
                    },
                ],
                Condition::Always,
                Duration::Instant,
            ))
        }),
        // Impact is Family (V2) entrance: blank the opponent's Spotlight Finishes
        // Reveal-then family (RevealThen, schema v95): "Reveal the top/bottom card of
        // your deck[:,] if <filter>, <consequence>" and "Randomly reveal N card(s) in
        // your hand[:;] if <filter>, <consequence>". `reveal_filter` parses the name/atk
        // match; `reveal_consequence` splits off a "add that card to your hand" take and
        // parses the rest through the grammar (declines if that body has none). The bare
        // "Reveal the … card of your deck:" header (no inline "if") stays Unsupported —
        // it splits from its consequence across a newline (a separate follow-up).
        rule(
            r"Reveal the (top|bottom) card of your deck[:,] [Ii]f (.+?), (.+)",
            |c| {
                let reveal_from = if &c[1] == "bottom" {
                    RevealSource::DeckBottom
                } else {
                    RevealSource::DeckTop
                };
                reveal_then_effect(reveal_from, 1, &c[2], &c[3])
            },
        ),
        rule(
            r"Randomly reveal (\d+) cards? in your hand[:;,] [Ii]f (.+?), (.+)",
            |c| reveal_then_effect(RevealSource::HandRandom, num(c, 1), &c[2], &c[3]),
        ),
        // Impact is Family entrances: blank the opponent's Spotlights (continuous
        // selector scan; mirrors A Trip to the Upside Down's Spotlight blank). V2
        // (9ee10069) scopes to "Spotlight Finishes" (play_order Finish); V1 (37a75d37)
        // is the broader "Spotlight cards" (any order) — one rule, the noun picks the
        // play-order constraint.
        rule(
            r"Your opponent'?s Spotlight (Finishes|cards) have blank text",
            |c| {
                let play_order = (&c[1] == "Finishes").then_some(PlayOrder::Finish);
                Some(eff(
                    Trigger::Static,
                    vec![blank_text(
                        CardFilter {
                            play_order,
                            tag: Some("Spotlight".to_owned()),
                            ..Default::default()
                        },
                        Who::Opp,
                    )],
                    Condition::Always,
                    Duration::WhileInPlay,
                ))
            },
        ),
        // Opponent-scoped continuous blank: "(Their|Your opponent's) <desc> have blank
        // text" -> BlankText{OPP, <selector>}. `opp_blank_selector` maps <desc> (a
        // name-substring OR-list, a play order, the SkillRequirement tag, or "cards in
        // play") and DECLINES anything else (incl. "without …" negations), so an unmodeled
        // descriptor stays Unsupported. Placed after the Spotlight rule (which wins first).
        rule(r"(?:Their|Your opponent'?s) (.+?) have blank text", |c| {
            Some(eff(
                Trigger::Static,
                vec![blank_text(opp_blank_selector(&c[1])?, Who::Opp)],
                Condition::Always,
                Duration::WhileInPlay,
            ))
        }),
        // "When hit: Your opponent's cards with <name-list> in the name have blank text"
        // (Ultra Dracula) — a STATEFUL blank: only once this competitor is hit does the
        // opponent's themed set go blank. OnHit{who:Opp, on_any} fires whenever the opponent
        // lands ANY card (= "when hit") and stamps a rest-of-match BlankTextPermanent on the
        // opponent's named cards. A blanked gimmick is excluded from the OnHit scan, so it
        // stops stamping when hit.
        rule(
            r#"When hit:\s+Your opponent'?s cards with ("[^"]+"(?:,? (?:and |or )?"[^"]+")*) in the name have blank text"#,
            |c| {
                let names = quoted_names(&c[1]);
                if names.is_empty() {
                    return None;
                }
                Some(eff(
                    Trigger::OnHit {
                        order: None,
                        atk_type: None,
                        name_contains: Vec::new(),
                        text_contains: Vec::new(),
                        on_any: true,
                        who: Who::Opp,
                        from_hand: false,
                    },
                    vec![Action::BlankTextPermanent {
                        selector: cf_name(names),
                        who: Who::Opp,
                    }],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        // "When your opponent hits a card with <name-list> in the name, that card has blank
        // text and their next turn roll is <±N>" (Jax, Pet of the Year) — OnHit{who:Opp,
        // name_contains} fires when the opponent hits a themed card; blank THAT card
        // (BlankHitCard, the hit referent) and debuff the opponent's next turn roll.
        rule(
            r#"When your opponent hits a card with ("[^"]+"(?:,? (?:and |or )?"[^"]+")*) in the name, that card has blank text and their next turn roll is ([+-]\d+)"#,
            |c| {
                let names = quoted_names(&c[1]);
                if names.is_empty() {
                    return None;
                }
                let delta: i64 = c[2].parse().ok()?;
                Some(eff(
                    Trigger::OnHit {
                        order: None,
                        atk_type: None,
                        name_contains: names,
                        text_contains: Vec::new(),
                        on_any: false,
                        who: Who::Opp,
                        from_hand: false,
                    },
                    vec![
                        Action::BlankHitCard,
                        modify_roll(Who::Opp, delta, RollWhen::Next, None, Who::SelfSide),
                    ],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        // Impact is Family (V1) once-per-match rider: when the opponent STALLED (ended
        // their turn without playing) AND buried a Spotlight, arm +1 to your next turn
        // roll. Modeled as an OnTurnStart effect (fires before the roll-off, so the armed
        // MultiTurnRollBonus lands on this turn's roll) gated on the two just-ended-turn
        // conditions. The "and this card is blanked" self-consumption is subsumed by the
        // once-per-match frequency (both cap it to a single use). Bespoke — a single card.
        rule(
            r"(?:Once per match:\s+)?When your opponent ended their turn without playing a card and they buried a Spotlight card, your next turn roll is \+1 and this card is blanked",
            |_| {
                let mut e = eff(
                    Trigger::OnTurnStart,
                    vec![Action::MultiTurnRollBonus {
                        who: Who::SelfSide,
                        rolls: 1,
                        delta: 1,
                    }],
                    and_conds(
                        Condition::EndedTurnNoPlay { who: Who::Opp },
                        Condition::BuriedSpotlightLastTurn { who: Who::Opp },
                    ),
                    Duration::Instant,
                );
                e.frequency = FrequencyGuard {
                    node_type: FrequencyGuardTag,
                    kind: Frequency::OncePerMatch,
                    n: None,
                };
                Some(e)
            },
        ),
        // Discard-pile blank ("Cards in your opponent's discard pile have blank text"):
        // every card in that pile is blank -> BlankText{any, discard_only}. "in the
        // discard pile" (Liger's Den) is BOTH boards -> two actions. The "opponet's" typo
        // is tolerated. Neutralises the opponent's WhileInDiscard abilities.
        rule(
            r"Cards in (?:your opponen?t'?s|your target'?s|their) discard pile have blank text",
            |_| {
                Some(eff(
                    Trigger::Static,
                    vec![blank_discard(Who::Opp)],
                    Condition::Always,
                    Duration::WhileInPlay,
                ))
            },
        ),
        rule(r"Cards in the discard pile have blank text", |_| {
            Some(eff(
                Trigger::Static,
                vec![blank_discard(Who::SelfSide), blank_discard(Who::Opp)],
                Condition::Always,
                Duration::WhileInPlay,
            ))
        }),
        // Owner-scoped named-card blank ("Your \"Backslide\" and \"School Boy\" have blank
        // text"): the OWNER's cards of those names are blank -> BlankText{SELF} only. Placed
        // before the ownerless rule below (which would else blank both boards). "has"/"have"
        // both appear (singular/plural by the name count).
        rule(
            r#"Your ("[^"]+"(?:,? (?:and |or )?"[^"]+")*) (?:has|have) blank text"#,
            |c| {
                let names = quoted_names(&c[1]);
                if names.is_empty() {
                    return None;
                }
                Some(eff(
                    Trigger::Static,
                    vec![blank_text(cf_name(names), Who::SelfSide)],
                    Condition::Always,
                    Duration::WhileInPlay,
                ))
            },
        ),
        // Named-card blank ("\"Apocalypse\" has blank text", "\"X\" or \"Y\" have blank
        // text"): the named card(s) are blank whoever holds them. The clause names no
        // owner (these target opponent counters — Apocalypse / Rejected / Derailed, or an
        // opponent's tag-team cards — but say only "X has/have blank text"), so blank the
        // name on BOTH boards: BlankText{SELF} + BlankText{OPP} with the same name-substring
        // OR-list selector (mirrors the two-action "each player" pattern; Who has no Both).
        rule(
            r#"("[^"]+"(?:,? (?:and |or )?"[^"]+")*) (?:has|have) blank text"#,
            |c| {
                let names = quoted_names(&c[1]);
                if names.is_empty() {
                    return None;
                }
                let sel = cf_name(names);
                Some(eff(
                    Trigger::Static,
                    vec![
                        blank_text(sel.clone(), Who::SelfSide),
                        blank_text(sel, Who::Opp),
                    ],
                    Condition::Always,
                    Duration::WhileInPlay,
                ))
            },
        ),
        // "Un-blank your Finishes." — the inverse of the blank family: restore the text
        // of your OWN Finish cards, overriding any blank on them for the rest of the
        // match (the 6 Splash / "your opponent buries … un-blank your Finishes"
        // Followups). Fires on hit like the sibling bury clause on the same card.
        rule(r"Un-?blank your Finishes", |_| {
            Some(eff(
                on_hit(),
                vec![unblank(cf_order(PlayOrder::Finish), Who::SelfSide)],
                Condition::Always,
                Duration::Instant,
            ))
        }),
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
        // "Draw cards equal to the Crowd Meter [+N] [(Max +M)]" (task #131) — a self-draw
        // whose count is the live Crowd Meter plus the offset, clamped. Compound forms
        // ("… or shuffle …") break the anchor and stay Unsupported; gated/triggered forms
        // ("If stopped, draw …") compose via the generic gate/trigger split over this body.
        rule(
            r"Draw cards? equal to the [Cc]rowd [Mm]eter(?: \+(\d+))?(?: \(Max \+?(\d+)\))?",
            |c| {
                let offset = c.get(1).map_or(0, |m| m.as_str().parse().unwrap());
                let cap = c.get(2).map(|m| m.as_str().parse::<i64>().unwrap());
                Some(eff(
                    on_hit(),
                    vec![draw_crowd(offset, cap)],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        // Draw from the BOTTOM of the deck. "Add the bottom N cards of your deck to
        // your hand" (a "Choose one:" option on Booty Drop Chop and kin) is the same
        // action as "Draw the bottom N cards of your deck", just phrased as an add.
        rule(
            r"(?:Draw|Add) the bottom (\d+) cards? of your deck(?: to your hand)?",
            |c| {
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
            },
        ),
        // Singular "add the top/bottom card of your deck to your hand" (Leader of the
        // Postal Nation gimmick body) — a 1-card draw from the named end.
        rule(
            r"(?:Draw|Add) the (top|bottom) card of your deck(?: to your hand)?",
            |c| {
                let end = if &c[1] == "bottom" {
                    DeckEnd::Bottom
                } else {
                    DeckEnd::Top
                };
                Some(eff(
                    on_hit(),
                    vec![draw(1, Who::SelfSide, end, None, Who::SelfSide)],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        rule(r"Shuffle your deck", |_| {
            Some(eff(
                on_hit(),
                vec![Action::ShuffleDeck { who: Who::SelfSide }],
                Condition::Always,
                Duration::Instant,
            ))
        }),
        // Gag "no-op" look: "Look at your opponent's hand, choose N cards and put them
        // back in your opponent's hand" (Medieval Prankster's "I Got Your Nose!") — the
        // choose-and-put-right-back changes nothing, so it is functionally just a Peek
        // (its only value is fog-of-war). User-confirmed 2026-08-08. Placed before the
        // bare Peek rule (whose `$` anchor can't reach past the trailing gag text).
        rule(
            r"Look at your opponent'?s hand, choose \d+ cards? and put (?:it|them) back in your opponent'?s hand",
            |_| {
                Some(eff(
                    on_hit(),
                    vec![Action::Peek { who: Who::Opp }],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        rule(r"Look at your opponent'?s hand", |_| {
            Some(eff(
                on_hit(),
                vec![Action::Peek { who: Who::Opp }],
                Condition::Always,
                Duration::Instant,
            ))
        }),
    ]
}

fn build_turn_roll_rules() -> Vec<(Regex, Builder)> {
    vec![
        // Per-count self turn-roll bonus. The optional `with "X" in the name` suffix
        // (which trails "you have in play", so a single capture can't reach it) routes
        // through `in_play_filter` — a name-substring filter when present, else the
        // `<pre>` descriptor (card / type / order / stop). "+1 for each card you have
        // in play with 'Steel Chain' in the name."
        rule(
            r#"Your next turn roll is ([+-]\d+) for each (?:other )?(.+?) you have in play(?: with (.+?) in the name)?"#,
            |c| {
                let per = in_play_filter(&c[2], c.get(3).map(|m| m.as_str()))?;
                Some(eff(
                    on_hit(),
                    vec![modify_roll(
                        Who::SelfSide,
                        c[1].parse().ok()?,
                        RollWhen::Next,
                        Some(per),
                        Who::SelfSide,
                    )],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        // Both-boards per-count next-roll bonus: "Your next turn roll is +N for each
        // <X> in play" with NO owner qualifier counts <X> on BOTH boards. Modeled without
        // a schema change as two stacked ModifyRolls on SELF's next roll — per_who=Self
        // (your board) + per_who=Opp (theirs) — whose per-counts sum to the total. Placed
        // AFTER the "you have"/"opponent has" rules; those phrasings decline recur_filter
        // here and fall through to the qualified rules ("... you have in play" is not a
        // bare selector).
        rule(
            r"Your next turn roll is ([+-]\d+) for each (.+?) in play",
            |c| {
                let delta: i64 = c[1].parse().ok()?;
                let per = recur_filter(c[2].trim())?;
                Some(eff(
                    on_hit(),
                    vec![
                        modify_roll(
                            Who::SelfSide,
                            delta,
                            RollWhen::Next,
                            Some(per.clone()),
                            Who::SelfSide,
                        ),
                        modify_roll(Who::SelfSide, delta, RollWhen::Next, Some(per), Who::Opp),
                    ],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        // Skill-keyed pending turn-roll bonus: "The next time you roll <S> [for your
        // turn roll][,] it is +N" — a mod that waits until <S> is next rolled, applies
        // once, and is consumed (schema v99, engine pending_skill_roll_mods). The
        // "for your turn roll" phrase and the comma are both optional.
        rule(
            &format!(r"The next time you roll {SK}(?: for your turn roll)?,? it is \+?(\d+)"),
            |c| {
                Some(eff(
                    on_hit(),
                    vec![modify_roll_on_skill(num(c, 2), skill(&c[1]))],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        // Roll-conditional one-shot draw (cluster d): "If your next turn roll is <S>,
        // draw N" arms a pending draw that fires if the owner's NEXT turn roll comes up
        // <S>; the opponent-watch mirror ("If your opponent's next turn roll is <S>")
        // draws for the owner when the OPPONENT's next turn roll is <S> (schema v109,
        // engine pending_roll_draws). Fires-or-fizzles on that one turn roll.
        rule(
            &format!(r"If your next turn roll is {SK},? draw (\d+) cards?"),
            |c| {
                Some(eff(
                    on_hit(),
                    vec![Action::RollDraw {
                        who: Who::SelfSide,
                        skill: skill(&c[1]),
                        count: num(c, 2),
                    }],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        rule(
            &format!(r"If your opponent's next turn roll is {SK},? draw (\d+) cards?"),
            |c| {
                Some(eff(
                    on_hit(),
                    vec![Action::RollDraw {
                        who: Who::Opp,
                        skill: skill(&c[1]),
                        count: num(c, 2),
                    }],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        // Chosen-skill draw: "The next time you roll that skill[,] draw N card(s)" (Catch
        // These Hands and its siblings). Pairs with the "Choose a skill: your opponent's
        // skill of that type is -1" clause (see `choose_skill_opp_debuff`), whose ChooseSkill
        // binds the referenced skill. RollDrawChosen arms a PERSISTENT one-shot keyed to the
        // owner's bound skill — it waits until that skill is rolled rather than fizzling.
        rule(
            r"The next time you roll that skill,? draw (\d+) cards?",
            |c| {
                Some(eff(
                    on_hit(),
                    vec![Action::RollDrawChosen {
                        who: Who::SelfSide,
                        count: num(c, 1),
                    }],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        // One-turn skill-gated turn-roll bonus (cluster b): "+N to <S>, <S>, and <S>
        // during your next turn roll" (single or multi-skill via skill_list) —
        // NextRollSkillBonus{SELF} applied to the immediately-next turn roll if it comes
        // up a listed skill, then drained (a one-turn window; schema v110, engine
        // pending_next_roll_skill_mods). Declines (-> Unsupported) if the middle isn't a
        // pure skill list (e.g. "+1 to <S>, +1 to <S> …", per-skill deltas).
        rule(r"\+(\d+) to (.+?) during your next turn roll", |c| {
            let skills = skill_list(&c[2]);
            if skills.is_empty() {
                return None;
            }
            Some(eff(
                on_hit(),
                vec![Action::NextRollSkillBonus {
                    who: Who::SelfSide,
                    skills,
                    delta: num(c, 1),
                }],
                Condition::Always,
                Duration::Instant,
            ))
        }),
        // Self single-skill: "If your next turn roll is <S>[,] it is +N" (the value-keyed
        // "is 10, it is +1" declines via SK — a separate roll-VALUE variant).
        rule(
            &format!(r"If your next turn roll is {SK},? it is \+(\d+)"),
            |c| {
                Some(eff(
                    on_hit(),
                    vec![Action::NextRollSkillBonus {
                        who: Who::SelfSide,
                        skills: vec![skill(&c[1])],
                        delta: num(c, 2),
                    }],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        // Opponent single-skill penalty: "If your opponent's next turn roll is <S>, their
        // [turn ]roll is -N" -> NextRollSkillBonus{OPP} (stored on the opponent, whose roll
        // it modifies).
        rule(
            &format!(r"If your opponent's next turn roll is {SK},? their (?:turn )?roll is (-\d+)"),
            |c| {
                Some(eff(
                    on_hit(),
                    vec![Action::NextRollSkillBonus {
                        who: Who::Opp,
                        skills: vec![skill(&c[1])],
                        delta: c[2].parse().ok()?,
                    }],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        // Multi-turn duration bonus (cluster b): "Your [opponent's] next N turn rolls are
        // +/-N" -> MultiTurnRollBonus applied to the next N turn rolls of the affected
        // side (schema v111, engine multi_turn_roll_mods). The Finish-roll-gated variant
        // ("If your Finish roll is odd, ...") keeps its "If" prefix and declines here.
        rule(
            r"Your (opponent's )?next (\d+) turn rolls are ([+-]\d+)",
            |c| {
                let who = if c.get(1).is_some() {
                    Who::Opp
                } else {
                    Who::SelfSide
                };
                Some(eff(
                    on_hit(),
                    vec![Action::MultiTurnRollBonus {
                        who,
                        rolls: num(c, 2),
                        delta: c[3].parse().ok()?,
                    }],
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
                        on_skill: None,
                    }],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        // Opponent's next turn roll lowered by a per-count of YOUR in-play cards ("your
        // opponent's next turn roll is -N for each [other] Lead you have in play") — the
        // opp-directed mirror of the self per-count rule above. who=Opp (their roll),
        // per_who=SelfSide (the cards YOU have in play).
        rule(
            r#"Your opponent's next turn roll is -(\d+) for each (?:other )?(.+?) you have in play(?: with (.+?) in the name)?"#,
            |c| {
                let per = in_play_filter(&c[2], c.get(3).map(|m| m.as_str()))?;
                Some(eff(
                    on_hit(),
                    vec![modify_roll(
                        Who::Opp,
                        -num(c, 1),
                        RollWhen::Next,
                        Some(per),
                        Who::SelfSide,
                    )],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        // Gated flat next-turn-roll bonus: "If you have another <order|atk> in play[,]
        // your next turn roll is +N." Every printed card with this clause is itself a
        // card of the gated order/type, and the bonus fires OnHit (after the card has
        // entered play), so "another" reads as HasInPlay count>=2 — this card plus at
        // least one other. A name-gated variant ("another Saber of Light card") has no
        // count_filter and declines to Unsupported. Placed before the generic gate rule
        // (which would emit the always-on count=1) so this count=2 model wins.
        rule(
            r"If you have another (.+?) in play,? your next turn roll is \+(\d+)",
            |c| {
                let filter = count_filter(&c[1])?;
                Some(eff(
                    on_hit(),
                    vec![modify_roll(
                        Who::SelfSide,
                        num(c, 2),
                        RollWhen::Next,
                        None,
                        Who::Opp,
                    )],
                    has_in_play(Who::SelfSide, filter, 2),
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
        // Opponent-directed current-roll modifier ("Their turn roll is -N" / "Your
        // opponent's turn roll is -N") — the This-scope sibling of the Self rule above,
        // and the roll-mod body for an OnReroll{Opp} clause ("when your opponent re-rolls
        // their turn roll, their roll is -1"). Signed, so a rare "+N" also maps.
        rule(r"(?:Their|Your opponent's) turn roll is ([+-]?\d+)", |c| {
            Some(eff(
                on_hit(),
                vec![modify_roll(
                    Who::Opp,
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
        // The bare "next roll" phrasing of the same debuff (task #131): "Your opponent's
        // next roll is -N" == "…next turn roll is -N" -> ModifyRoll{Opp, Next}. Anchored so
        // the per-count ("… for each …") and Crowd-Meter ("… - the Crowd Meter") forms fall
        // through to their own rules rather than being flattened to a plain delta.
        rule(r"^Your opponent's next roll is -(\d+)$", |c| {
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
        // Absolute maximum-handsize SET ("... maximum handsize is N", no sign) — the cap
        // becomes N (vs the signed delta rules just below). Standing (WhileInPlay) only:
        // the "until the end of the turn" timed variant needs the timed-buff path (not this
        // board-standing fold) and stays Unsupported rather than modeled as permanent.
        // Placed before the delta rules (which require a [+-] sign, so an unsigned digit
        // never reaches them).
        rule(
            r"(?:Your opponent's|Your target's|Their) max(?:imum)? hand ?size is (\d+)",
            |c| {
                Some(eff(
                    Trigger::Static,
                    vec![max_hand_set(num(c, 1), Who::Opp, Duration::WhileInPlay)],
                    Condition::Always,
                    Duration::WhileInPlay,
                ))
            },
        ),
        rule(r"Your max(?:imum)? hand ?size is (\d+)", |c| {
            Some(eff(
                Trigger::Static,
                vec![max_hand_set(
                    num(c, 1),
                    Who::SelfSide,
                    Duration::WhileInPlay,
                )],
                Condition::Always,
                Duration::WhileInPlay,
            ))
        }),
        rule(r"Each player's max(?:imum)? hand ?size is ([+-]\d+)", |c| {
            let d = num(c, 1);
            Some(eff(
                Trigger::Static,
                vec![max_hand(d, Who::SelfSide), max_hand(d, Who::Opp)],
                Condition::Always,
                Duration::WhileInPlay,
            ))
        }),
        rule(
            r"(?:Your opponent's|Your target's|Their) max(?:imum)? hand ?size is ([+-]\d+)",
            |c| {
                Some(eff(
                    Trigger::Static,
                    vec![max_hand(num(c, 1), Who::Opp)],
                    Condition::Always,
                    Duration::WhileInPlay,
                ))
            },
        ),
        rule(r"Your max(?:imum)? hand ?size is ([+-]\d+)", |c| {
            Some(eff(
                Trigger::Static,
                vec![max_hand(num(c, 1), Who::SelfSide)],
                Condition::Always,
                Duration::WhileInPlay,
            ))
        }),
        // MinHandSize mirror (previously override-only). Same three shapes / Static
        // WhileInPlay convention as the maximum. The per-count "… +N for each Lead you
        // have in play" form has no MinHandSize.per and stays Unsupported.
        rule(r"Each player's min(?:imum)? hand ?size is ([+-]\d+)", |c| {
            let d = num(c, 1);
            Some(eff(
                Trigger::Static,
                vec![min_hand(d, Who::SelfSide), min_hand(d, Who::Opp)],
                Condition::Always,
                Duration::WhileInPlay,
            ))
        }),
        rule(
            r"(?:Your opponent's|Your target's|Their) min(?:imum)? hand ?size is ([+-]\d+)",
            |c| {
                Some(eff(
                    Trigger::Static,
                    vec![min_hand(num(c, 1), Who::Opp)],
                    Condition::Always,
                    Duration::WhileInPlay,
                ))
            },
        ),
        rule(r"Your min(?:imum)? hand ?size is ([+-]\d+)", |c| {
            Some(eff(
                Trigger::Static,
                vec![min_hand(num(c, 1), Who::SelfSide)],
                Condition::Always,
                Duration::WhileInPlay,
            ))
        }),
    ]
}

fn build_dq_loss_rules() -> Vec<(Regex, Builder)> {
    vec![
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
                    Trigger::OnBreakoutRoll {
                        who: Who::Opp,
                        attempts: Vec::new(),
                    },
                    vec![Action::LoseBy {
                        kind: LoseKind::Disqualification,
                        who: Who::SelfSide,
                    }],
                    Condition::RollValue {
                        cmp: Comparator::Eq,
                        value: num(c, 1),
                        who: Who::SelfSide,
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
    ]
}

fn build_flip_crowd_reroll_rules() -> Vec<(Regex, Builder)> {
    vec![
        rule(r"Flip (?:up to )?(\d+) cards?", |c| {
            Some(eff(
                on_hit(),
                vec![flip(num(c, 1), Who::SelfSide)],
                Condition::Always,
                Duration::Instant,
            ))
        }),
        rule(
            r"(?:[Yy]our opponent flips|[Tt]hey flip) (\d+) cards?",
            |c| {
                Some(eff(
                    on_hit(),
                    vec![flip(num(c, 1), Who::Opp)],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        rule(r"Each player flips (\d+) cards?", |c| {
            let n = num(c, 1);
            Some(eff(
                on_hit(),
                vec![flip(n, Who::SelfSide), flip(n, Who::Opp)],
                Condition::Always,
                Duration::Instant,
            ))
        }),
        // "The Crowd Meter is +N" / "-N" -> a direct swing (`CrowdMeter{delta}`), the
        // printed sign verbatim (per-side orientation is handled globally, as for the
        // "The Crowd Meter is +1" override). Trigger-prefixed variants ("If stopped, the
        // Crowd Meter is +2") reach this body via the trigger/gate split machinery.
        rule(r"[Tt]he Crowd Meter is ([+-]\d+)", |c| {
            Some(eff(
                Trigger::OnPlay,
                vec![Action::CrowdMeter {
                    delta: c[1].parse().ok()?,
                }],
                Condition::Always,
                Duration::Instant,
            ))
        }),
        // "Increase/Raise the Crowd Meter by N" — a positive CM swing body (Imaginary
        // Dragon DDT's "If stopped, increase the Crowd Meter by 1" reaches it via the
        // "If stopped, <body>" split; a bare clause fires on play). Companion to the
        // "The Crowd Meter is +N" swing above, for the imperative phrasing.
        rule(r"(?:[Ii]ncrease|[Rr]aise) the Crowd Meter by (\d+)", |c| {
            Some(eff(
                Trigger::OnPlay,
                vec![Action::CrowdMeter { delta: num(c, 1) }],
                Condition::Always,
                Duration::Instant,
            ))
        }),
        // Standalone "bury this card" body -> `BuryThisCard` (self-recycle to the deck
        // bottom). Reaches its gated form via the generic gate split (Imaginary Dragon
        // DDT: "If the Crowd Meter is 5 or greater, bury this card"). Full-anchored, so
        // the compound "discard N and bury this card or lose …" cost clauses (longer
        // text) never match it.
        rule(r"[Bb]ury this card", |_| {
            Some(eff(
                Trigger::OnPlay,
                vec![Action::BuryThisCard],
                Condition::Always,
                Duration::Instant,
            ))
        }),
        // Re-roll grammar (the `Reroll` action pre-existed but was override-only). A
        // leading "You may" -> `Effect::optional`; "next" -> the one-shot NEXT turn-roll
        // grant, bare "your turn roll" -> the current roll (`This`, structural). Trigger/
        // gate-prefixed variants ("If stopped, you may re-roll …") reach these bodies via
        // the split machinery, which strips "you may " and sets optional itself.
        rule(r"(?:(You may) )?[Rr]e-?roll your (next )?turn roll", |c| {
            let when = if c.get(2).is_some() {
                RollWhen::Next
            } else {
                RollWhen::This
            };
            let mut e = eff(
                Trigger::OnPlay,
                vec![reroll(Who::SelfSide, when, false)],
                Condition::Always,
                Duration::Instant,
            );
            e.optional = c.get(1).is_some();
            Some(e)
        }),
        rule(
            r"(?:(You may) )?[Ff]orce your opponent to re-?roll (?:their )?(next )?turn roll",
            |c| {
                let when = if c.get(2).is_some() {
                    RollWhen::Next
                } else {
                    RollWhen::This
                };
                let mut e = eff(
                    Trigger::OnPlay,
                    vec![reroll(Who::Opp, when, false)],
                    Condition::Always,
                    Duration::Instant,
                );
                e.optional = c.get(1).is_some();
                Some(e)
            },
        ),
        rule(r"(?:(You may) )?[Rr]e-?roll your [Ff]inish roll", |c| {
            let mut e = eff(
                Trigger::OnPlay,
                vec![reroll(Who::SelfSide, RollWhen::This, true)],
                Condition::Always,
                Duration::Instant,
            );
            e.optional = c.get(1).is_some();
            Some(e)
        }),
        // "When you roll <S>[ or <S>], you may re-roll" — a turn-roll re-roll gated on
        // the rolled skill (Brock Smith V2's gimmick, wrapped in "Once per turn roll:").
        // The gate becomes a `RollWasSkill` condition (or an OR-set) read against the
        // roll context in `offer_reroll`; the bare "re-roll" re-rolls the current turn
        // roll (`This`). Anchored past the "re-roll your (next) turn roll" bodies, so no
        // overlap. Self-side only — the "force your opponent to re-roll" phrasing lacks
        // "you may re-roll" and stays on its own rule.
        rule(
            &format!(r"When you roll ({SKNC}(?:,? (?:or )?{SKNC})*),? you may re-?roll"),
            |c| {
                let cond = roll_was_any(&c[1]).unwrap_or_else(|| Condition::RollWasSkill {
                    skill: skill(&c[1]),
                    who: Who::SelfSide,
                });
                let mut e = eff(
                    Trigger::OnPlay,
                    vec![reroll(Who::SelfSide, RollWhen::This, false)],
                    cond,
                    Duration::Instant,
                );
                e.optional = true;
                Some(e)
            },
        ),
        // "[You may] bury this card to re-roll your Breakout roll" (My Most Powerful
        // Spell, via WHILE_IN_DISCARD). The "bury this card" cost (a self-recycle out of
        // the discard) is a documented simplification — DROPPED — so the effect can
        // re-offer while the card sits in the discard; the optional breakout re-roll is
        // the faithful core. Placed before the plain re-roll rule (anchored, so no
        // ordering hazard) and the costed-re-roll rule (which rejects "bury this card").
        rule(
            r"(?:(You may) )?[Bb]ury this card to re-?roll your [Bb]reakout [Rr]oll",
            |c| {
                let mut e = eff(
                    Trigger::OnPlay,
                    vec![reroll_breakout(Who::SelfSide)],
                    Condition::Always,
                    Duration::Instant,
                );
                e.optional = c.get(1).is_some();
                Some(e)
            },
        ),
        // Breakout-roll re-roll (schema v102). Self ("re-roll your Breakout roll") and
        // force-opponent ("force/make your opponent re-roll their Breakout roll") both
        // re-roll the defender's die; who distinguishes which side owns the "you may".
        rule(
            r"(?:(You may) )?[Rr]e-?roll your [Bb]reakout [Rr]oll",
            |c| {
                let mut e = eff(
                    Trigger::OnPlay,
                    vec![reroll_breakout(Who::SelfSide)],
                    Condition::Always,
                    Duration::Instant,
                );
                e.optional = c.get(1).is_some();
                Some(e)
            },
        ),
        rule(
            r"(?:(You may) )?(?:[Ff]orce|[Mm]ake) your opponent (?:to )?re-?roll (?:their )?(?:a )?[Bb]reakout [Rr]oll",
            |c| {
                let mut e = eff(
                    Trigger::OnPlay,
                    vec![reroll_breakout(Who::Opp)],
                    Condition::Always,
                    Duration::Instant,
                );
                e.optional = c.get(1).is_some();
                Some(e)
            },
        ),
        // Costed re-roll (schema v103): "[You may] <hand-cost> to <re-roll body>" —
        // "bury 4 cards in your hand to re-roll your Finish roll", "discard 1 Finish
        // from your hand to force your opponent to re-roll their breakout roll". Parse
        // the cost prefix and re-parse the body through the whole grammar; the body must
        // resolve to a single Reroll, onto which the cost is attached. Declines (falls to
        // Unsupported) when the cost or body doesn't parse — never a silent cost drop.
        rule(
            r"(?:(You may) )?(.+?) to ((?:[Ff]orce|[Mm]ake) your opponent .*re-?roll .+|[Rr]e-?roll .+)",
            |c| {
                let cost = reroll_cost(&c[2])?;
                let mut e = match_grammar(&c[3])?;
                match e.actions.as_mut_slice() {
                    [Action::Reroll { cost: slot, .. }] => *slot = Some(cost),
                    _ => return None,
                }
                e.optional = c.get(1).is_some();
                Some(e)
            },
        ),
        // OnReroll trigger (schema v104): fires when a TURN roll is re-rolled. "When you
        // [would] re-roll your turn roll[,:] <body>" -> OnReroll{Self}; "When your
        // opponent/target re-rolls their turn roll[,:] <body>" -> OnReroll{Opp}. The body
        // (draw, roll-mod, shuffle-self) routes via `on_reroll_body`; the WHILE_IN_DISCARD
        // variants reach here through `while_in_discard_effect` after its prefix is stripped.
        rule(
            r"(?:When|If|Whenever) you (?:would )?re-?roll your turn roll[,:] (.+)",
            |c| on_reroll_body(Who::SelfSide, &c[1]),
        ),
        rule(
            r"(?:When|If|Whenever) your (?:opponent|target) re-?rolls their turn roll[,:] (.+)",
            |c| on_reroll_body(Who::Opp, &c[1]),
        ),
        // Extra-card grant (PlayExtraCard, previously override-only): "You may play an
        // additional card this turn". `order=None` (any card); N>1 grants loop as N
        // separate PlayExtraCard actions (each bumps the extra-plays counter). The
        // optional "You may" makes the effect optional; this base rule cascades through
        // the generic gate rule for the "If <gate>, you may play …" forms.
        rule(
            r"(?:(You may) )?[Pp]lay (an?|\d+) (?:additional|extra) cards? this turn",
            |c| {
                let count = match &c[2].to_lowercase()[..] {
                    "a" | "an" => 1usize,
                    n => n.parse().ok()?,
                };
                let mut e = eff(
                    on_hit(),
                    vec![Action::PlayExtraCard { order: None }; count],
                    Condition::Always,
                    Duration::Instant,
                );
                e.optional = c.get(1).is_some();
                Some(e)
            },
        ),
    ]
}

fn build_flip_trigger_rules() -> Vec<(Regex, Builder)> {
    vec![
        // "[Your opponent's] Gimmick is blank until they hit a card" (Sleep Paralysis)
        // -> an event-swept BlankGimmick lifted the instant the TARGET next lands a hit
        // (`UntilTargetHitsCard`). Unlike the bare "…is blank" form below (a continuous
        // Static blank read by `blank_scan`), a timed blank must be LATCHED by the
        // executor, so it is dispatched on_hit (the finish's connect), not Static.
        // Placed before the bare rule; both are anchored, so no ordering hazard.
        rule(
            r"Your ([Oo]pponent's )?[Gg]immick is blank until they hit a card",
            |c| {
                let who = if c.get(1).is_some() {
                    Who::Opp
                } else {
                    Who::SelfSide
                };
                Some(eff(
                    on_hit(),
                    vec![Action::BlankGimmick {
                        who,
                        duration: Duration::UntilTargetHitsCard,
                    }],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        // "[Your opponent's] Gimmick is blank" -> a Static `BlankGimmick` marker (the
        // action pre-existed but was override-only). Self ("Your Gimmick is blank") vs
        // opponent; WhileInPlay. Gated variants ("If the Crowd Meter is N or greater, …")
        // cascade via the generic gate rule.
        rule(r"Your ([Oo]pponent's )?[Gg]immick is blank", |c| {
            let who = if c.get(1).is_some() {
                Who::Opp
            } else {
                Who::SelfSide
            };
            Some(eff(
                Trigger::Static,
                vec![Action::BlankGimmick {
                    who,
                    duration: Duration::WhileInPlay,
                }],
                Condition::Always,
                Duration::WhileInPlay,
            ))
        }),
        // Defender play-restriction: "Your opponent needs N <cards|Leads|Follow Ups> in
        // play to hit you with a Finish" (D3 V1) -> a Static FinishRequires marker read
        // in `playable_options`. On top of SRG's built-in FollowUps-1 default to land a
        // Finish; Stops bypass it (they resolve outside the play path).
        rule(
            r"Your opponent needs (\d+) (cards?|[Ll]eads?|[Ff]ollow ?[Uu]ps?) in play to hit you with a Finish",
            |c| {
                Some(eff(
                    Trigger::Static,
                    vec![Action::FinishRequires {
                        kind: require_kind(&c[2])?,
                        count: num(c, 1),
                    }],
                    Condition::Always,
                    Duration::WhileInPlay,
                ))
            },
        ),
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
        // Peek-and-sort scry: "Look at the top N cards of your deck, [randomly] add M to
        // your hand, put K in your discard pile, and put the other(s) on top" (D3 V1's
        // Contact Juggling, top 3 / add 1 / away 1 / other on top). -> `scry_keep`
        // (Scry rest=Return). "our deck" is a real DB typo. The "If stopped," prefix is
        // handled upstream by the OnStop trigger split.
        rule(
            r"[Ll]ook at the top (\d+) cards? of (?:your|our) deck, (?:randomly )?add (\d+)(?: cards?)? to your hand, put (\d+)(?: cards?)? in your discard pile,?(?: and)? put the others? (?:back )?on top(?: of your deck)?",
            |c| {
                Some(eff(
                    on_hit(),
                    vec![scry_keep(num(c, 1), num(c, 2), num(c, 3))],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        // Look at the deck BOTTOM and re-bury: "Look at the bottom N cards of [any
        // player's|your] deck, then randomly bury them" (Flying Holmgang) -> Scry with a
        // bottom window that buries every card back (near-inert; simplifications in
        // `scry_bottom_bury`). Anchored to the "randomly bury them" (bury-all) tail so it
        // never claims the family's "put 1 in hand, bury the others" variants.
        rule(
            r"Look at the bottom (\d+) cards? of (?:any player'?s|your) deck,? then randomly bury them",
            |c| {
                Some(eff(
                    on_hit(),
                    vec![scry_bottom_bury(num(c, 1))],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        // Look at the deck BOTTOM, keep some, re-bury the rest: "Look at the bottom N cards
        // of your deck, add one/M to your hand and randomly bury the others" (Shattered
        // Split's Bonk!; a 6-card family). Distinct tail from the bury-all rule above.
        rule(
            r"Look at the bottom (\d+) cards? of your deck, add (?:one|(\d+)) to your hand and randomly bury the others",
            |c| {
                let to_hand = c.get(2).map_or(1, |m| m.as_str().parse().unwrap());
                Some(eff(
                    on_hit(),
                    vec![scry_bottom_keep(num(c, 1), to_hand)],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        // Single-card peek with an optional flip: "Look at the top card of your
        // opponent's deck, you may flip it" -> Scry{top:1, rest:MayFlip}. "Look at"
        // keeps it private (reveal:false); "Reveal" is public. deck follows the
        // "your"/"your opponent's" possessive.
        rule(
            r"(Look at|Reveal) the top card of your (opponent'?s )?deck, you may flip it",
            |c| {
                let deck = if c.get(2).is_some() {
                    Who::Opp
                } else {
                    Who::SelfSide
                };
                Some(eff(
                    Trigger::OnPlay,
                    vec![scry_may_flip(&c[1] == "Reveal", deck)],
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
        // Bare self-referential recursion bodies — the enclosing trigger_body supplies
        // the trigger (flip / WHILE_IN_DISCARD roll etc.); "it"/"this card" is the
        // card carrying the clause. Fully anchored, so these only match a whole-clause
        // bare body (a "Search … and add it" / "Flip … add it" keeps its own specific
        // rule). AddSelfToHand / ShuffleSelfIntoDeck are no-ops unless a `self_card`
        // referent is bound at the fire site, so a stray match is inert, not wrong.
        rule(r"[Aa]dd (?:it|this card) to your hand", |_| {
            Some(eff(
                Trigger::Static,
                vec![Action::AddSelfToHand],
                Condition::Always,
                Duration::Instant,
            ))
        }),
        // The Gobstopper recur: "If you drew N or more cards this turn, you may add this
        // card to your hand" -> OnDraw{SELF} (offered right after a draw) gated on
        // DrewThisTurn{N} + optional AddSelfToHand. Reached via while_in_discard_effect
        // (its allow-list admits OnDraw); the OnDraw trigger, not the generic gate rule
        // (which would emit an inert Static), is what makes the WhileInDiscard recur fire.
        rule(
            r"If you drew (\d+) or more cards this turn, you may add this card to your hand",
            |c| {
                let mut e = eff(
                    Trigger::OnDraw { who: Who::SelfSide },
                    vec![Action::AddSelfToHand],
                    Condition::DrewThisTurn {
                        who: Who::SelfSide,
                        at_least: num(c, 1),
                    },
                    Duration::Instant,
                );
                e.optional = true;
                Some(e)
            },
        ),
        // Me Against the World recur: "When you lose N Turn Rolls in a row, you may add this
        // card to your hand" -> OnLoseTurn gated on LostTurnRollsInARow{N} + optional
        // AddSelfToHand. Reached via while_in_discard_effect (its allow-list admits
        // OnLoseTurn); the OnLoseTurn trigger fires the WhileInDiscard recur from the pile.
        rule(
            r"When you lose (\d+) [Tt]urn [Rr]olls in a row, you may add this card to your hand",
            |c| {
                let mut e = eff(
                    Trigger::OnLoseTurn { by: None },
                    vec![Action::AddSelfToHand],
                    Condition::LostTurnRollsInARow {
                        who: Who::SelfSide,
                        at_least: num(c, 1),
                    },
                    Duration::Instant,
                );
                e.optional = true;
                Some(e)
            },
        ),
        rule(
            r"[Ss]huffle ?(?:it|this card)(?: from your discard pile)?(?: back)? into your deck",
            |_| {
                Some(eff(
                    Trigger::Static,
                    vec![Action::ShuffleSelfIntoDeck],
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
            r"(?:If|When) this card is flipped,?(?: (you may))? add it to your hand",
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
            r"(?:If|When) this card is flipped,?(?: (you may))? shuffle ?it(?: from your discard pile)?(?: back)? into your deck",
            |c| {
                Some(flip_self(
                    Action::ShuffleSelfIntoDeck,
                    c.get(1).is_some(),
                    Condition::Always,
                ))
            },
        ),
        // Standalone self-recycle body: "shuffle it / this card into your deck" ->
        // ShuffleSelfIntoDeck on the trigger's bound referent. Reached as the BODY of a
        // triggered clause (the trigger + "you may" optionality come from trigger_body);
        // e.g. the WHILE_IN_DISCARD "if your opponent hits a Grapple, shuffle it into
        // your deck" family. Distinct from the flip-anchored rule above, which carries
        // its own OnFlip trigger. The placeholder OnPlay trigger is overwritten upstream.
        rule(r"[Ss]huffle (?:it|this card) into your deck", |_| {
            Some(eff(
                Trigger::OnPlay,
                vec![Action::ShuffleSelfIntoDeck],
                Condition::Always,
                Duration::Instant,
            ))
        }),
        // Standalone self-recycle body: "put this card / it on top of your deck" ->
        // PutSelfOnDeckTop on the trigger's bound referent (self_card, or stopped_card
        // for the "If stopped, …" family). Reached as a triggered/gated body — the
        // WHILE_IN_DISCARD "when you roll <S>, put this card on top" clause and the
        // "If stopped, put this card on top of your deck" clause. Trigger + "you may"
        // come from trigger_body/gate_body; the on_hit() placeholder is overwritten (no
        // card uses this body standalone). on_hit() (not OnPlay) so it folds with the
        // put-from-hand tail below under `compound_body`, which requires a shared trigger.
        rule(r"[Pp]ut (?:this card|it) on top of your deck", |_| {
            Some(eff(
                on_hit(),
                vec![Action::PutSelfOnDeckTop],
                Condition::Always,
                Duration::Instant,
            ))
        }),
        // "put N card(s) from your hand on top of your deck" -> PutFromHandOnDeckTop{N}.
        // The owner picks which. Two contexts: standalone on hit (Diving Headbutt's "Put
        // 1 card from your hand on top of your deck. Look at your opponent's hand …"), and
        // the tail of a self-recycle via compound_body ("put this card on top of your
        // deck, then put 1 card from your hand on top") — the "If stopped, …" and
        // WHILE_IN_DISCARD put-on-top families, whose trigger comes from upstream.
        rule(
            r"[Pp]ut (\d+) cards? from your hand on top of your deck",
            |c| {
                Some(eff(
                    on_hit(),
                    vec![Action::PutFromHandOnDeckTop {
                        count: c[1].parse().ok()?,
                    }],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        // "<cost> to put (this card|it) on top of your deck" -> a COST-gated self-recycle:
        // [<cost>, PutSelfOnDeckTop] folded on one trigger. The "you may" (always present)
        // is stripped upstream by trigger_body/gate_body and sets Effect::optional, so the
        // player controls whether to pay. The cost is re-parsed through the grammar and must
        // be a plain on-hit action (bury/discard N from hand) so the two fold — "bury 5
        // cards in your hand to …" (If stopped) / "discard 3 cards from your hand to …"
        // (WHILE_IN_DISCARD roll). NOTE the "to" is a cost modeled as a bundled optional
        // compound: a short hand pays LESS than the stated cost, an over-generosity the
        // "you may" keeps in the player's hands. schema v142
        rule(r"(.+?) to put (?:this card|it) on top of your deck", |c| {
            let cost = match_grammar(&capitalize_first(c[1].trim()))?;
            let plain = cost.trigger == on_hit()
                && !cost.optional
                && cost.condition == Condition::Always
                && cost.duration == Duration::Instant;
            if !plain {
                return None;
            }
            let mut actions = cost.actions;
            actions.push(Action::PutSelfOnDeckTop);
            Some(eff(on_hit(), actions, Condition::Always, Duration::Instant))
        }),
        // "you may play it[ as an additional card this turn]" -> PlaySelf (the play is
        // itself the bonus action, so "as an additional card" folds in).
        rule(
            r"(?:If|When) this card is flipped,?(?: (you may))? play it(?: as an additional card this turn)?",
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
        // it. ("all"/"the" -> all matching; "randomly" -> RNG pick.)
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
        // Standing flip trigger (schema v89 on_self split): "When/After you flip any
        // number of cards, [randomly] add M of the flipped cards to your hand" -> a
        // standing OnFlip (on_self=false, count=None) firing AddFlippedToHand. Distinct
        // from the per-card "if this card is flipped" self-trigger.
        rule(
            r"(?:When|After) you flip any number of cards,? (randomly )?add (\d+|all|the|[Oo]ne) (?:of the )?flipped (cards?|Strikes?|Grapples?|Submissions?) to your hand",
            |c| {
                Some(eff(
                    on_flip_standing(None, false),
                    vec![add_flipped_action(&c[2], &c[3], c.get(1).is_some())],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        // "When/After you flip N or more cards, [you may] add M of the flipped cards to
        // your hand" -> standing OnFlip with an at_least threshold; "you may" -> optional.
        rule(
            r"(?:When|After) you flip (\d+) or more cards,? (?:(you may) )?add (\d+|all|the|[Oo]ne) (?:of the )?flipped (cards?|Strikes?|Grapples?|Submissions?) to your hand",
            |c| {
                Some(Effect {
                    optional: c.get(2).is_some(),
                    ..eff(
                        on_flip_standing(Some(num(c, 1)), true),
                        vec![add_flipped_action(&c[3], &c[4], false)],
                        Condition::Always,
                        Duration::Instant,
                    )
                })
            },
        ),
        // Generic flip trigger-body split (schema v89 on_self): "When/After you flip
        // <count>, <body>" -> the body re-parsed through the grammar with a standing
        // OnFlip attached. Reuses every body rule (draw / bury / turn-roll / recur / …).
        // Placed AFTER the specific flip-add rules above, so those still claim the
        // add-flipped body; this catches the rest. A body with no grammar -> Unsupported.
        rule(r"(?:When|After) you flip any number of cards,? (.+)", |c| {
            trigger_body(on_flip_standing(None, false), &c[1])
        }),
        rule(r"(?:When|After) you flip (\d+) or more cards,? (.+)", |c| {
            trigger_body(on_flip_standing(Some(num(c, 1)), true), &c[2])
        }),
    ]
}

fn build_bury_discard_rules() -> Vec<(Regex, Builder)> {
    vec![
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
        // "Bury N [<selector>] cards in ANY/EITHER player's discard pile" — the actor
        // chooses from both piles (Bury.choose). The optional middle is a type/order
        // selector ("1 Grapple") or bare "cards" (any). `player.?s` absorbs the
        // apostrophe variants (player's / player’s / players). Placed before the
        // opponent/self discard rules so the both-piles pool wins.
        rule(
            r"Bury (?:up to )?(\d+) (.+?) in (?:any|either) player.?s discard pile",
            |c| {
                let selector = recur_filter(&c[2]).unwrap_or_default();
                Some(eff(
                    on_hit(),
                    vec![bury_choose(num(c, 1), selector)],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
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
        // "Your opponent chooses either player to randomly bury N card(s) in their
        // discard pile" (Papa Nequaquam's Norseman's Slam) — the OPPONENT picks the
        // target, so the adversarial choice falls on the actor's OWN discard (deny their
        // recursion; `random` sheds a random card). Simplification: the opponent's
        // "either player" choice -> SelfSide (worst case for the actor).
        rule(
            r"Your opponent chooses either player to (randomly )?bury (\d+) cards? in their discard pile",
            |c| {
                let mut a = bury(num(c, 2), Who::SelfSide);
                if c.get(1).is_some() {
                    if let Action::Bury { random, .. } = &mut a {
                        *random = true;
                    }
                }
                Some(eff(on_hit(), vec![a], Condition::Always, Duration::Instant))
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
            r"(?:[Yy]our opponent discards|[Tt]hey discard) (\d+) random cards?(?: (?:from|in) their hand)?",
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
            r"(?:[Yy]our opponent discards|[Tt]hey discard) (\d+) cards?(?: (?:from|in) their hand)?",
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
        // Generic per-card flip self-trigger with an arbitrary body: "When/If this card
        // is flipped, <body>" -> the body re-parsed through the whole grammar (draw /
        // opponent bury / discard-pile shuffle / …) with a per-card `OnFlip{on_self}`
        // attached, dispatched by `run_self_flips` with the flipped card as referent.
        // Placed LAST — after the specific self-action rules ([`flip_self`] add / shuffle
        // / play, in `flip_trigger`) and the provenance rules ("flipped by \"X\"" /
        // "flipped for your Gimmick", above), so those claim their clauses first; this
        // catches the rest. A body with no grammar declines -> Unsupported.
        rule(r"(?:When|If) this card is flipped,? (.+)", |c| {
            trigger_body(on_flip_self(), &c[1])
        }),
    ]
}

fn build_removal_hand_rules() -> Vec<(Regex, Builder)> {
    vec![
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
        // "Choose N cards your opponent has in play and BURY it/them" (JT Dunn's gimmick;
        // a 6-card family) -> RemoveFromPlay{to_deck}, sending the removed card to its
        // owner's deck bottom rather than their discard. Composes with the OnHit-Strike
        // split for the gimmick's "When you hit a Strike, <body>".
        rule(
            r"[Cc]hoose (\d+) cards? your opponent has in play and bury (?:it|them)",
            |c| {
                Some(eff(
                    on_hit(),
                    vec![remove_opp_to(num(c, 1), CardFilter::default(), true)],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        rule(r"[Dd]iscard (\d+) (.+?) your opponent has in play", |c| {
            remove_opp_play(num(c, 1), count_filter(&c[2])?)
        }),
        // "[Look at your opponent's hand,] choose 1 <selector> and put it on top of
        // their deck" (D3 V1's Claw) -> HandToDeckTop{who:Opp}: the actor sees the hand
        // and picks; the target redraws the denied card. `recur_filter` handles
        // "card" (any) / typed selectors. The "look at" prefix is informational.
        rule(
            r"(?:Look at your opponent'?s hand,? )?[Cc]hoose 1 (.+?),? and put it on top of their deck",
            |c| {
                Some(eff(
                    on_hit(),
                    vec![Action::HandToDeckTop {
                        who: Who::Opp,
                        selector: recur_filter(&c[1])?,
                    }],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
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
            r"(?:[Yy]our opponent randomly buries|[Tt]hey randomly bury) (\d+) cards? in their hand",
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
        rule(
            r"(?:[Yy]our opponent buries|[Tt]hey bury) (\d+) cards? in their hand",
            |c| {
                Some(eff(
                    on_hit(),
                    vec![bury_hand(num(c, 1), Who::Opp, false, false)],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        // "[Look at your opponent's hand,] they bury/discard all <type> [cards]" — the
        // opponent sheds EVERY hand card of a type (schema v90, `all`). The reveal
        // prefix is informational (full-info engine). recur_filter declines shapes with
        // no card filter ("cards of the chosen type" -> stays Unsupported).
        rule(
            r"(?:[Ll]ook at your opponent'?s hand[,;] )?[Tt]hey bury all (.+)",
            |c| {
                Some(eff(
                    on_hit(),
                    vec![bury_all_hand(recur_filter(&c[1])?, Who::Opp)],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        rule(
            r"(?:[Ll]ook at your opponent'?s hand[,;] )?(?:[Tt]hey discard all|[Dd]iscard all their) (.+)",
            |c| {
                Some(eff(
                    on_hit(),
                    vec![discard_all_hand(recur_filter(&c[1])?, Who::Opp)],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
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
        // Self-hand-bury and both-players. An optional "randomly" prefix sets random
        // selection (mirrors the opponent-side "your opponent randomly buries …" rule and
        // "each player randomly buries …"); the deterministic form lets the owner pick.
        // Unlocks the standalone / "If stopped, …" self random-buries and the put-on-top
        // tail "randomly bury 1 card in your hand then put this card on top".
        rule(r"([Rr]andomly )?[Bb]ury (\d+) cards? in your hand", |c| {
            Some(eff(
                on_hit(),
                vec![bury_hand(
                    num(c, 2),
                    Who::SelfSide,
                    c.get(1).is_some(),
                    false,
                )],
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
        // "Each player buries N card(s) in their discard pile" — the discard-pile twin of
        // the each-player hand bury above. `bury` recycles from EACH player's own discard
        // to their deck bottom (BuryFrom::Discard). A branch of the "Stop any X or each
        // player buries N in their discard pile" versatile-or shape (#120).
        rule(
            r"[Ee]ach player buries (\d+) cards? in their discard pile",
            |c| {
                let n = num(c, 1);
                Some(eff(
                    on_hit(),
                    vec![bury(n, Who::SelfSide), bury(n, Who::Opp)],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        // "Each player shuffles their deck" — both players reshuffle their own deck (the
        // per-player twin of the self-only "Shuffle your deck"). A branch of the "Each
        // player shuffles their deck or stop any X" versatile-or shape (#120).
        rule(r"[Ee]ach player shuffles their deck", |_| {
            Some(eff(
                on_hit(),
                vec![
                    Action::ShuffleDeck { who: Who::SelfSide },
                    Action::ShuffleDeck { who: Who::Opp },
                ],
                Condition::Always,
                Duration::Instant,
            ))
        }),
        // "Each player adds the bottom card of their deck to their hand" — a 1-card
        // bottom-draw for BOTH players (per-player twin of "Add the bottom card of your
        // deck to your hand"). A branch of the "Each player adds the bottom card … or
        // stop any X" versatile-or shape (#120).
        rule(
            r"[Ee]ach player adds the bottom card of their deck to their hand",
            |_| {
                Some(eff(
                    on_hit(),
                    vec![
                        draw(1, Who::SelfSide, DeckEnd::Bottom, None, Who::SelfSide),
                        draw(1, Who::Opp, DeckEnd::Bottom, None, Who::SelfSide),
                    ],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        // "Each player shuffles N <type> from their discard pile into their deck" — both
        // players recur a matching card from their OWN discard back into their OWN deck
        // (schema v143, ShuffleIntoDeck.who). A branch of the "Stop any X or each player
        // shuffles …" versatile-or shape (#120). Tolerates the "dsicard" DB typo and a
        // doubled space before "pile".
        rule(
            r"[Ee]ach player shuffles (?:up to )?\d+ (.+?) from their d(?:is|si)card\s+pile into their deck",
            |c| {
                let filter = recur_filter(&c[1])?;
                Some(eff(
                    on_hit(),
                    vec![
                        shuffle_into_who(filter.clone(), ShuffleSource::Discard, Who::SelfSide),
                        shuffle_into_who(filter, ShuffleSource::Discard, Who::Opp),
                    ],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
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
        // Standalone opp-hand choose-discard: "Choose N <selector> and discard it/them" —
        // the split-clause tail of "Look at your opponent's hand.\nChoose N X and discard
        // it" (the inline comma form parses via the rules above). Every standalone
        // choose-discard is either opp-HAND (here) or opp-BOARD ("... your opponent has in
        // play …", which `recur_filter` declines so it falls to the in-play removal rule);
        // none mean the actor's own hand. `recur_filter` parses the typed/order/name
        // selector -> `discard_choose` (opp hand, effect owner picks).
        rule(r"[Cc]hoose (\d+) (.+?) and discard (?:it|them)", |c| {
            let filter = recur_filter(&c[2])?;
            Some(eff(
                on_hit(),
                vec![discard_choose(num(c, 1), filter)],
                Condition::Always,
                Duration::Instant,
            ))
        }),
    ]
}

fn build_recur_rules() -> Vec<(Regex, Builder)> {
    vec![
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
        // Gag "no-op" recur: "Choose N card(s) in your discard pile and shuffle it/them
        // into your hand" (1 Jump, 1 Whistle & 1 Fart) — shuffling the HAND does nothing,
        // so this is just a recur from discard to hand (AddFromDiscard adds one). The
        // shuffle-your-hand flavor is a non-effect. User-confirmed 2026-08-08.
        rule(
            r"Choose \d+ (.+?) in your discard pile and shuffle (?:it|them) into your hand",
            |c| {
                Some(eff(
                    on_hit(),
                    vec![Action::AddFromDiscard {
                        filter: recur_filter(&c[1])?,
                    }],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        // "Take N <X> from your discard pile and add them to your hand" — the "Take …
        // and add" phrasing of the recur-from-discard rule above (AJ Styles' Lead
        // recursion). One added, as the whole AddFromDiscard family does.
        rule(
            r"Take (?:up to )?\d+ (.+?) from your discard pile and add (?:it|them) to your hand",
            |c| {
                Some(eff(
                    on_hit(),
                    vec![Action::AddFromDiscard {
                        filter: recur_filter(&c[1])?,
                    }],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        // "Shuffle N cards from your discard pile into your deck and then draw M card(s)"
        // (Dragon Flare Kick) — the recur followed by a FIXED redraw (not `then_draw`,
        // which couples the draw to the shuffled count). The shuffle count is simplified
        // as the whole discard-recur family does; the draw is a plain Instant draw. Placed
        // before the bare recur rule (whose `$` anchor can't reach the "and then draw" tail).
        rule(
            r"Shuffle (?:up to )?(\d+) cards? from your discard pile into your deck,? and then draw (\d+) cards?",
            |c| {
                Some(eff(
                    on_hit(),
                    vec![
                        shuffle_into(CardFilter::default(), ShuffleSource::Discard),
                        draw(num(c, 2), Who::SelfSide, DeckEnd::Top, None, Who::SelfSide),
                    ],
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
                    vec![shuffle_into(CardFilter::default(), ShuffleSource::Discard)],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        // "Choose N cards from your discard pile and randomly bury them" (a branch of
        // Khloe Mai's "when the Crowd Meter increases" gimmick) — pick N of your OWN
        // discard and recycle them to the deck. `random` honours the "randomly" wording;
        // the count is the whole-family simplification (top-N recycle).
        rule(
            r"Choose (\d+) cards? from your discard pile and randomly bury them",
            |c| {
                Some(eff(
                    on_hit(),
                    vec![Action::Bury {
                        choose: false,
                        selector: CardFilter::default(),
                        count: num(c, 1),
                        who: Who::SelfSide,
                        random: true,
                        source: BuryFrom::Discard,
                        per: None,
                        per_who: Who::SelfSide,
                        per_zone: CountZone::InPlay,
                        all: false,
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
                    vec![shuffle_into(recur_filter(&c[1])?, ShuffleSource::InPlay)],
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
                    vec![shuffle_into(CardFilter::default(), ShuffleSource::Discard)],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        // "Take any number of <X> cards from your discard pile and shuffle them into
        // your deck, then draw the same number of cards" (AJ Styles' Spiral Tap) — a
        // recycle-and-refill: shuffle EVERY matching discard card back (all=true, the
        // "any number" heuristic maxed) and draw as many as were shuffled (then_draw
        // couples the draw to the actual count).
        rule(
            r"Take any number of (.+?) from your discard pile and shuffle them into your deck, then draw the same number of cards",
            |c| {
                Some(eff(
                    on_hit(),
                    vec![Action::ShuffleIntoDeck {
                        selector: recur_filter(&c[1])?,
                        source: ShuffleSource::Discard,
                        who: Who::SelfSide,
                        all: true,
                        then_draw: true,
                        then_bury: false,
                    }],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        // The Dudebuster: "Shuffle any number of cards from your hand into your deck, then
        // draw the same number of cards" — a hand cycle. source=Hand + all (any number,
        // maxed = whole hand) + then_draw (redraw the shuffled count).
        rule(
            r"Shuffle any number of cards from your hand into your deck, then draw the same number of cards",
            |_| {
                Some(eff(
                    on_hit(),
                    vec![Action::ShuffleIntoDeck {
                        selector: CardFilter::default(),
                        source: ShuffleSource::Hand,
                        who: Who::SelfSide,
                        all: true,
                        then_draw: true,
                        then_bury: false,
                    }],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        // Double Leg Death Lock: "Shuffle any number of cards from your discard pile into
        // your deck, then bury the same number of cards from your hand" — recycle the
        // whole discard (all) then bury that many from hand (then_bury).
        rule(
            r"Shuffle any number of cards from your discard pile into your deck, then bury the same number of cards from your hand",
            |_| {
                Some(eff(
                    on_hit(),
                    vec![Action::ShuffleIntoDeck {
                        selector: CardFilter::default(),
                        source: ShuffleSource::Discard,
                        who: Who::SelfSide,
                        all: true,
                        then_draw: false,
                        then_bury: true,
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
                    vec![shuffle_into(recur_filter(&c[3])?, ShuffleSource::Discard)],
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
    ]
}

fn build_unstoppable_draw_rules() -> Vec<(Regex, Builder)> {
    vec![
        // "You can stop cards that cannot be stopped" — a player-scope enabler: while it
        // is in play (or declared on a competitor, e.g. JT Dunn), EVERY one of the owner's
        // stops may stop an otherwise-unstoppable attack. The per-`Stop` `even_unstoppable`
        // flag ("stop any X that cannot be stopped") is the single-card twin.
        rule(r"[Yy]ou can stop cards that cannot be stopped", |_| {
            Some(eff(
                Trigger::Static,
                vec![Action::CanStopUnstoppable { only_order: None }],
                Condition::Always,
                Duration::WhileInPlay,
            ))
        }),
        // "Ignore any \"Cannot be stopped\" text on your opponent's <order> cards" — the
        // same defender-scope bypass, but narrowed to the opponent's attacks of a single
        // PRINTED play order (Pineapple/Trash Can/Sledgehammer Uprising key on "Finish
        // cards"). While it is in play every one of the owner's stops beats an
        // otherwise-unstoppable attack of that order; other orders are unaffected.
        rule(
            r#"Ignore any "Cannot be stopped" text on your opponent'?s (Follow[ -]?Ups?|Leads?|Finish(?:es)?)(?: cards)?"#,
            |c| {
                Some(eff(
                    Trigger::Static,
                    vec![Action::CanStopUnstoppable {
                        only_order: Some(stopper_order(&c[1])),
                    }],
                    Condition::Always,
                    Duration::WhileInPlay,
                ))
            },
        ),
        // "Your cards cannot be stopped by [printed] <order>[ cards]" — the PLAYER-SCOPE
        // shield (Cat/Dog/Sheep Uprising's "printed Finishes"). Covers every one of the
        // owner's cards, so the engine reads it even from an in-play main-deck source
        // (`player_scope`). "printed <order>" is emphasis for the printed play order —
        // exactly what `by_order` keys on (a card printed at slot #28-30 is Finish);
        // a card merely playable AS a Finish is not, and never matches the gate.
        rule(
            r"Your cards cannot be stopped by (?:printed )?(Follow[ -]?Ups?|Leads?|Finish(?:es)?)(?: cards)?",
            |c| {
                Some(eff(
                    Trigger::Static,
                    vec![unstoppable_player_scope(Some(stopper_order(&c[1])))],
                    Condition::Always,
                    Duration::WhileInPlay,
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
            r"(This card |Your cards )?[Cc]annot be stopped by (?:cards with [Ss]kill [Rr]equirements|[Ss]kill [Rr]equirement cards)",
            |c| {
                let player_scope = c.get(1).is_some_and(|m| m.as_str().starts_with("Your"));
                Some(eff(
                    Trigger::Static,
                    vec![unstoppable_skillreq(player_scope)],
                    Condition::Always,
                    Duration::WhileInPlay,
                ))
            },
        ),
        // "[When you roll <S> for your turn roll: ]Your cards with \"X\" in the name
        // cannot be stopped" — a player-scope shield (competitor/gimmick/entrance) that
        // protects only the owner's attacks whose name contains X (`applies_name`),
        // optionally gated on the owner's turn roll being a given skill. The engine's
        // standing scan applies it across every one of the owner's cards.
        rule(
            &format!(
                r#"(?:When you roll {SK} for your turn roll: )?Your cards with "([^"]+)" in the name cannot be stopped( by (?:cards with [Ss]kill [Rr]equirements?|[Ss]kill [Rr]equirements?(?: cards)?))?\.?"#
            ),
            |c| {
                let condition = match c.get(1) {
                    Some(m) => Condition::RollWasSkill {
                        skill: skill(m.as_str()),
                        who: Who::SelfSide,
                    },
                    None => Condition::Always,
                };
                // The optional "by Skill Requirements" tail narrows the shield to
                // skill-requirement stoppers (JT Dunn) instead of every stopper.
                let by_skillreq = c.get(3).is_some();
                Some(eff(
                    Trigger::Static,
                    vec![unstoppable_applies_name(&c[2], by_skillreq)],
                    condition,
                    Duration::WhileInPlay,
                ))
            },
        ),
        // "[If/When <cond>,] this card cannot be stopped [by <order>]": an optionally
        // condition-gated Unstoppable. The guard is parsed by `turn_roll_value_gate`
        // ("[your opponent's] turn roll is N" — Scott Prime's The Loaded Glove; kept out
        // of the general `gate_condition` so its body-triggered generic-gate rule can't
        // mis-model the "When you roll <S>, <body>" event family) falling back to the
        // superset `gate_condition` (Crowd Meter, skill/hand compare, opp-roll, in-play,
        // hit-history, …). A bare clause (no gate) is unconditional; the engine evaluates
        // the guard from the card owner's side at stop time.
        rule(
            r"(?:(?:If|When) (.+?),? )?[Tt]his card cannot be stopped(?: by (Follow[ -]?Ups?|Leads?|Finish(?:es)?))?",
            |c| {
                let by_order = c.get(2).map(|m| stopper_order(m.as_str()));
                let condition = match c.get(1) {
                    Some(m) => {
                        turn_roll_value_gate(m.as_str()).or_else(|| gate_condition(m.as_str()))?
                    }
                    None => Condition::Always,
                };
                Some(eff(
                    Trigger::Static,
                    vec![unstoppable(by_order, None)],
                    condition,
                    Duration::WhileInPlay,
                ))
            },
        ),
        // "[If/When <cond>,] this card can only be stopped by [a] <order>" — the WHITELIST
        // inverse of "cannot be stopped by <order>": unstoppable against every OTHER play
        // order, so only the named order can stop it (see `only_stopped_by`). Optionally
        // condition-gated ("If your opponent has a card in play, …" / "If you have a card
        // with 'X' in the name in play, …") via the shared `gate_condition`.
        rule(
            r"(?:(?:If|When) (.+?),? )?[Tt]his card can only be stopped by (?:an? )?(Follow[ -]?Ups?|Leads?|Finish(?:es)?)",
            |c| {
                let only = stopper_order(&c[2]);
                let condition = match c.get(1) {
                    Some(m) => gate_condition(m.as_str())?,
                    None => Condition::Always,
                };
                Some(eff(
                    Trigger::Static,
                    only_stopped_by(only),
                    condition,
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
            r#"Draw (\d+) cards? for each (?:other )?(.+?) you have in play(?: with (.+?) in the name)?"#,
            |c| {
                per_draw(
                    num(c, 1),
                    &c[2],
                    Who::SelfSide,
                    c.get(3).map(|m| m.as_str()),
                )
            },
        ),
        rule(
            r#"Draw (\d+) cards? for each (?:other )?(.+?) your opponent has in play(?: with (.+?) in the name)?"#,
            |c| per_draw(num(c, 1), &c[2], Who::Opp, c.get(3).map(|m| m.as_str())),
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
        // Trent's gimmick body, after "Once on your turn," is stripped as an inline
        // frequency (OncePerTurn): "if you have fewer cards in your hand than your
        // opponent you may draw N" -> a StartOfTurn OPTIONAL draw gated on the hand
        // compare. The comma before "you may" is optional in the DB text.
        rule(
            r"[Ii]f you have fewer cards in your hand than your opponent,? you may draw (\d+) cards?",
            |c| {
                let mut e = eff(
                    Trigger::StartOfTurn,
                    vec![draw(
                        num(c, 1),
                        Who::SelfSide,
                        DeckEnd::Top,
                        None,
                        Who::SelfSide,
                    )],
                    hand_size_vs_opp(Comparator::Lt, Who::SelfSide),
                    Duration::Instant,
                );
                e.optional = true;
                Some(e)
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
            r"Your opponent randomly reveals (\d+) cards?(?: in their hands?)? and discards all(?: revealed)? [Ss]tops",
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
    ]
}

fn build_reveal_alsolead_rules() -> Vec<(Regex, Builder)> {
    vec![
        // Reveal-and-discard, single/conditional phrasing: "[Your opponent|They] randomly
        // reveal(s) N card(s) in their hand[;:,] if it is a Stop, they discard it" — the
        // opponent reveals N random hand cards and discards any that are stops. With N
        // cards revealed and each discarded if a stop, this is exactly RevealAndDiscard
        // (which discards all revealed stops). N is a digit or "a"/"one". The trigger is
        // whatever the enclosing prefix supplies (trigger_body overrides on_hit()).
        rule(
            r"(?i)^(?:Your opponent|They) randomly reveals? (\d+|an?|one) cards? in their hand[;:,]\s*if it(?:'s| is) a Stop,?\s*they discard it\.?$",
            |c| {
                Some(eff(
                    on_hit(),
                    vec![Action::RevealAndDiscard {
                        count: count_or_word(&c[1]),
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
        // FINISH-OFF-STOP (#120): "If played as a Stop, [the Crowd Meter is +N and] this
        // (card) is also a Finish" — when this stop lands, run a finish sequence off it (a
        // breakout attempt). Distinct from the AlsoLead "also a Finish" playability family
        // below; placed FIRST so it wins on the stop-context phrasings. See `finish_off_stop`.
        rule(
            r"(?i)If played as a stop,? (?:the [Cc]rowd [Mm]eter is \+(\d+) and )?this (?:card )?is also a Finish",
            |c| {
                let cm = c.get(1).and_then(|m| m.as_str().parse().ok());
                Some(finish_off_stop(cm, Condition::Always))
            },
        ),
        // Finish-off-stop, gated on the STOPPED card lacking a logo/skill-requirement
        // (Universal Dropkick / Sweeping Slam / Umbrella Hold V1/V2). Placed before the
        // general AlsoLead rule, which would otherwise fold it to an (engine-inert on the
        // stop path) AlsoLead{Finish}. The condition rides the effect, gating the CM swing too.
        rule(
            r"(?i)If the stopped card did not have a competitor logo or (?:a )?skill requirement,? (?:the [Cc]rowd [Mm]eter is \+(\d+) and )?this card is also a Finish",
            |c| {
                let cm = c.get(1).and_then(|m| m.as_str().parse().ok());
                Some(finish_off_stop(cm, Condition::StoppedCardNoLogoNoReq))
            },
        ),
        // General "[If/When <gate>,] this card is also a <order>" (a 264-clause family):
        // the card gains an extra play-order slot via `AlsoLead{condition, order}`, whose
        // OWN condition carries the gate (read by `also_lead_now`, independent of the
        // effect's trigger/condition). Subsumes the specific rules above; a gate that
        // `gate_condition` can't parse (compound "and", "played as a Stop", match-type)
        // declines, leaving the clause Unsupported. Bare (no gate) -> Always.
        rule(
            r"(?:(?:If|When) (.+?),? )?[Tt]his card is also an? (Lead|Follow Up|Finish)",
            |c| {
                let condition = match c.get(1) {
                    Some(m) => gate_condition(m.as_str())?,
                    None => Condition::Always,
                };
                Some(eff(
                    Trigger::Static,
                    vec![Action::AlsoLead {
                        condition,
                        order: order(&c[2]),
                    }],
                    Condition::Always,
                    Duration::WhileInPlay,
                ))
            },
        ),
        // No-DQ match rule (DisqualificationRule, previously override-only). "The/This
        // match has no disqualifications", optionally prefixed by the redundant static
        // window "When this card is in play" or "For the rest of the match" (a main-deck
        // card's Static already applies only while in play). Match scope.
        rule(
            r"(?:When this card is in play,? |For the rest of the match,? )?[Tt]h(?:is|e) match (?:now )?has no [Dd]isqualifications",
            |_| Some(dq_rule(DqScope::Match)),
        ),
        // "You cannot be disqualified" — the SELF-scope form.
        rule(r"You cannot be disqualified", |_| {
            Some(dq_rule(DqScope::SelfSide))
        }),
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
    ]
}

fn build_finish_breakout_rules() -> Vec<(Regex, Builder)> {
    vec![
        // Symmetric roll modifier (task #131): "If either player rolls <S> for their
        // {turn|breakout|Finish} roll, their [<roll> ]roll is ±N" — applies to WHOEVER
        // rolls that skill for that roll type, from either board. The delta is SIGNED
        // and the consequent's roll word is optional ("their roll is" / "their turn roll
        // is"). Turn -> TurnRollBonus{either}; breakout -> BreakoutModifier{either};
        // Finish -> FinishRollBonus{either} (now consumed by the opponent-board scan).
        rule(
            &format!(
                r"If [Ee]ither play(?:er)? rolls {SK} for their turn roll, their (?:turn )?roll is ([+-]\d+)"
            ),
            |c| {
                Some(eff(
                    Trigger::Static,
                    vec![Action::TurnRollBonus {
                        skill: skill(&c[1]),
                        delta: num(c, 2),
                        who: Who::SelfSide,
                        either: true,
                        per_crowd: false,
                        cap: None,
                    }],
                    Condition::Always,
                    Duration::WhileInPlay,
                ))
            },
        ),
        // Opponent-directed standing turn-roll modifier (task #131): "If your opponent
        // rolls <S> for their turn roll, their [turn ]roll is -N" -> TurnRollBonus{who:Opp}.
        // Read by turn_roll_bonus's opponent-board scan, so it bites the opponent's roll
        // only (never the owner's). Distinct from the one-shot "next turn roll" form
        // (NextRollSkillBonus): this is a STANDING modifier fired every time the opponent
        // rolls <S>. The self mirror is the who:SelfSide default.
        rule(
            &format!(
                r"If your opponent rolls {SK} for their turn roll, their (?:turn )?roll is ([+-]\d+)"
            ),
            |c| {
                Some(eff(
                    Trigger::Static,
                    vec![Action::TurnRollBonus {
                        skill: skill(&c[1]),
                        delta: num(c, 2),
                        who: Who::Opp,
                        either: false,
                        per_crowd: false,
                        cap: None,
                    }],
                    Condition::Always,
                    Duration::WhileInPlay,
                ))
            },
        ),
        rule(
            &format!(
                r"If [Ee]ither play(?:er)? rolls {SK} for their [Bb]reakout roll, their (?:[Bb]reakout )?roll is ([+-]\d+)"
            ),
            |c| {
                Some(eff(
                    Trigger::Static,
                    vec![Action::BreakoutModifier {
                        delta: num(c, 2),
                        attempts: None,
                        when_skill: Some(skill(&c[1])),
                        who: Who::SelfSide,
                        either: true,
                        per: None,
                        per_who: Who::SelfSide,
                        per_zone: CountZone::InPlay,
                        per_divisor: None,
                        cap: None,
                        per_excludes_self: false,
                    }],
                    Condition::Always,
                    Duration::WhileInPlay,
                ))
            },
        ),
        rule(
            &format!(
                r"If [Ee]ither play(?:er)? rolls {SK} for their Finish roll, their (?:Finish )?roll is ([+-]\d+)"
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
        // Flat breakout-roll bonus (no skill gate): "[Your opponent's] breakout rolls are
        // +N" / "+N to your breakout rolls". `who` picks whose breakout rolls; the "for
        // each …" per-count forms have no per-count on this action and stay Unsupported.
        rule(
            r"Your ([Oo]pponent's )?[Bb]reakout [Rr]olls? (?:is|are) ([+-]\d+)",
            |c| {
                let who = if c.get(1).is_some() {
                    Who::Opp
                } else {
                    Who::SelfSide
                };
                Some(eff(
                    Trigger::Static,
                    vec![breakout_mod_who(c[2].parse().ok()?, who, None, None)],
                    Condition::Always,
                    Duration::WhileInPlay,
                ))
            },
        ),
        rule(r"\+(\d+) to your [Bb]reakout [Rr]olls?", |c| {
            Some(eff(
                Trigger::Static,
                vec![breakout_mod(num(c, 1), None)],
                Condition::Always,
                Duration::WhileInPlay,
            ))
        }),
        // "Your opponent's 1st and 2nd breakout rolls are ±N" (Shattered Split's Why So
        // Serious?!?, as a reveal-then consequence) -> an IMPERATIVE `GrantBreakoutBonus`
        // on the opponent, not a Static per-ordinal `BreakoutModifier`: RevealThen applies
        // its `then` actions imperatively, so the penalty must be a timed grant to compose
        // there. Documented simplification: the ±N lands on ALL the opponent's breakout
        // rolls this turn, not strictly the 1st/2nd (a breakout rarely reaches a 3rd roll,
        // so the over-reach is marginal). Placed before the single-ordinal rule; the two
        // never overlap ("rolls are" vs "roll is"). schema v132.
        rule(
            r"Your opponent'?s 1st and 2nd [Bb]reakout [Rr]olls? (?:is|are) ([+-]\d+)",
            |c| {
                Some(eff(
                    on_hit(),
                    vec![Action::GrantBreakoutBonus {
                        delta: c[1].parse().ok()?,
                        who: Who::Opp,
                    }],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        // Attempt-indexed bonus: "[Your opponent's] 3rd breakout roll is +N" -> the bonus
        // applies only on that attempt (BreakoutModifier.attempts, an attempt-index gate).
        rule(
            r"Your ([Oo]pponent's )?(\d+)(?:st|nd|rd|th) [Bb]reakout [Rr]oll is ([+-]\d+)",
            |c| {
                let who = if c.get(1).is_some() {
                    Who::Opp
                } else {
                    Who::SelfSide
                };
                Some(eff(
                    Trigger::Static,
                    vec![breakout_mod_who(
                        c[3].parse().ok()?,
                        who,
                        Some(num(c, 2)),
                        None,
                    )],
                    Condition::Always,
                    Duration::WhileInPlay,
                ))
            },
        ),
        // Per-count breakout modifier: "[Your opponent's|Their] breakout rolls are ±N for
        // each [other] <X> [you have|they have|your opponent has] in play | in <x>'s
        // discard pile [with 'Y' in the name/text] [(Max +M)]" — the BreakoutModifier
        // analogue of the per-count Finish rule below (task #131, schema v112). The
        // subject prefix picks whose breakout rolls (who); the "for each" tail picks the
        // counted board/pile (per_who/per_zone) via breakout_per_target. A selector that
        // can't be mapped declines -> stays Unsupported.
        rule(
            r#"([Yy]our opponent's |[Yy]our |[Tt]heir )[Bb]reakout [Rr]olls? (?:is|are) ([+-]\d+) for each (other )?(.+?)(?: (you have|they have|your opponent has) in play| in (your opponent'?s|your|their) discard pile)(?: with (.+?) in the (name|text))?(?: \(Max \+?(\d+)\))?"#,
            |c| {
                let who = if c[1].trim().eq_ignore_ascii_case("your") {
                    Who::SelfSide
                } else {
                    Who::Opp
                };
                let (per, per_who, per_zone) = breakout_per_target(
                    &c[4],
                    c.get(5).map(|m| m.as_str()),
                    c.get(6).map(|m| m.as_str()),
                    c.get(7).map(|m| m.as_str()),
                    c.get(8).map(|m| m.as_str()),
                )?;
                let cap = c.get(9).map(|m| m.as_str().parse::<i64>().unwrap());
                Some(eff(
                    Trigger::Static,
                    vec![breakout_per(
                        num(c, 2),
                        who,
                        None,
                        per,
                        per_who,
                        per_zone,
                        cap,
                        c.get(3).is_some(),
                    )],
                    Condition::Always,
                    Duration::WhileInPlay,
                ))
            },
        ),
        // Ordinal per-count breakout modifier: "[Your opponent's] Nst [and Nnd] breakout
        // roll[s] is/are ±N for each <X> …" — the attempt-indexed form of the rule above.
        // Each ordinal emits one attempt-gated BreakoutModifier (BreakoutModifier.attempts
        // is a single index), so "1st and 2nd" -> two actions sharing the per-count.
        rule(
            r#"([Yy]our opponent's |[Yy]our |[Tt]heir )(\d+(?:st|nd|rd|th)|first|second|third)(?: and (\d+(?:st|nd|rd|th)|first|second|third))? [Bb]reakout [Rr]olls? (?:is|are) ([+-]\d+) for each (other )?(.+?)(?: (you have|they have|your opponent has) in play| in (your opponent'?s|your|their) discard pile)(?: with (.+?) in the (name|text))?(?: \(Max \+?(\d+)\))?"#,
            |c| {
                let who = if c[1].trim().eq_ignore_ascii_case("your") {
                    Who::SelfSide
                } else {
                    Who::Opp
                };
                let (per, per_who, per_zone) = breakout_per_target(
                    &c[6],
                    c.get(7).map(|m| m.as_str()),
                    c.get(8).map(|m| m.as_str()),
                    c.get(9).map(|m| m.as_str()),
                    c.get(10).map(|m| m.as_str()),
                )?;
                let cap = c.get(11).map(|m| m.as_str().parse::<i64>().unwrap());
                let delta = num(c, 4);
                let excl = c.get(5).is_some();
                let attempts: Vec<i64> = [c.get(2), c.get(3)]
                    .iter()
                    .filter_map(|m| m.and_then(|x| ordinal_num(x.as_str())))
                    .collect();
                let actions = attempts
                    .into_iter()
                    .map(|n| {
                        breakout_per(
                            delta,
                            who,
                            Some(n),
                            per.clone(),
                            per_who,
                            per_zone,
                            cap,
                            excl,
                        )
                    })
                    .collect();
                Some(eff(
                    Trigger::Static,
                    actions,
                    Condition::Always,
                    Duration::WhileInPlay,
                ))
            },
        ),
        // Per-count breakout-ATTEMPT-count modifier: "[Your opponent|You|They] gets N
        // (additional|fewer) Breakout roll[s] [this turn] for each [other] <X> [you have|
        // they have|your opponent has] in play | in <x>'s discard pile [with 'Y' in the
        // name/text] [(Max +M)]" — scales the roll COUNT (BreakoutAttempts), the count
        // sibling of the per-count VALUE rule above (task #131, schema v113). "fewer" ->
        // negative delta, else additive; the "for each" tail routes through the shared
        // breakout_per_target. A selector that can't map declines -> stays Unsupported.
        // Listed before the flat rule so the more-specific per-count form wins.
        rule(
            r#"([Yy]our opponent|[Yy]ou|[Tt]hey) gets? (\d+|one|two|three|a|an) (additional |more |fewer )?[Bb]reakout [Rr]olls?(?: this turn)? for each (other )?(.+?)(?: (you have|they have|your opponent has) in play| in (your opponent'?s|your|their) discard pile)(?: with (.+?) in the (name|text))?(?: \(Max \+?(\d+)\))?(?: this turn)?"#,
            |c| {
                let who = attempts_who(&c[1]);
                let n = count_or_word(&c[2]);
                let fewer = c
                    .get(3)
                    .is_some_and(|m| m.as_str().trim().eq_ignore_ascii_case("fewer"));
                let per = breakout_per_target(
                    &c[5],
                    c.get(6).map(|m| m.as_str()),
                    c.get(7).map(|m| m.as_str()),
                    c.get(8).map(|m| m.as_str()),
                    c.get(9).map(|m| m.as_str()),
                )?;
                let cap = c.get(10).map(|m| m.as_str().parse::<i64>().unwrap());
                Some(eff(
                    Trigger::Static,
                    vec![breakout_attempts_action(
                        if fewer { -n } else { n },
                        None,
                        who,
                        Some(per),
                        cap,
                        c.get(4).is_some(),
                    )],
                    Condition::Always,
                    Duration::WhileInPlay,
                ))
            },
        ),
        // Flat breakout-attempt-count modifier: "[Your opponent|You|They] gets N
        // [additional|more|fewer] Breakout roll[s] [on all breakouts] [this turn]" — the
        // "reduced / extra breakout rolls" family (task #131, schema v113). Bare "gets N
        // Breakout rolls" SETS the attempt count to N ("your opponent gets 2 Breakout
        // rolls this turn"); "additional"/"more" -> +N, "fewer" -> -N. `who` (Opp for
        // "your opponent"/"they", SelfSide for "you") names the affected side. Gated
        // forms ("If <state>, …") flow through the generic gate rule + gate_body.
        rule(
            r"([Yy]our opponent|[Yy]ou|[Tt]hey) gets? (\d+|one|two|three|a|an) (additional |more |fewer )?[Bb]reakout [Rr]olls?(?: on all breakouts)?(?: this turn)?",
            |c| {
                let who = attempts_who(&c[1]);
                let n = count_or_word(&c[2]);
                let (delta, set) = match c
                    .get(3)
                    .map(|m| m.as_str().trim().to_lowercase())
                    .as_deref()
                {
                    Some("additional") | Some("more") => (n, None),
                    Some("fewer") => (-n, None),
                    _ => (0, Some(n)), // bare "gets N Breakout rolls" = SET the count
                };
                Some(eff(
                    Trigger::Static,
                    vec![breakout_attempts_action(delta, set, who, None, None, false)],
                    Condition::Always,
                    Duration::WhileInPlay,
                ))
            },
        ),
        // "Your Finish roll[s] is/are +N for each [other] <X> [you have|your opponent has]
        // in play [with 'Y' in the name/text] [(Max +M)]" — the per-count Finish bonus
        // family (task #131, schema v106). Board picks per_who; a TYPE descriptor routes
        // through count_filter, a bare "card … with 'Y' in the name/text" through
        // name_or_text_filter (a combined type+name — "other Submission … with 'Bomb'" —
        // isn't expressible in one CardFilter, so it declines). "other" sets
        // per_excludes_self (drops the source card from the count); "(Max +M)" -> cap.
        rule(
            r#"Your Finish rolls? (?:is|are) ([+-]\d+) for each (other )?(.+?) (you have|your opponent has) in play(?: with (.+?) in the (name|text))?(?: \(Max \+?(\d+)\))?"#,
            |c| {
                let per = match c.get(5) {
                    Some(names) => {
                        // Name/text filter only when the descriptor is a bare "card".
                        let bare = c[3].trim_end_matches('s').eq_ignore_ascii_case("card");
                        let list = quoted_names(names.as_str());
                        if !bare || list.is_empty() {
                            return None;
                        }
                        name_or_text_filter(&c[6], list)
                    }
                    None => count_filter(&c[3])?,
                };
                let per_who = if &c[4] == "your opponent has" {
                    Who::Opp
                } else {
                    Who::SelfSide
                };
                let cap = c.get(7).map(|m| m.as_str().parse::<i64>().unwrap());
                Some(finish_per(
                    num(c, 1),
                    per,
                    per_who,
                    cap,
                    None,
                    c.get(2).is_some(),
                ))
            },
        ),
        // Same, phrased "for every N <X> you have in play" — the divisor floors the
        // count ("+1 for every 3 Strikes you have in play", The Ride Along).
        rule(
            r"Your Finish rolls? (?:is|are) ([+-]\d+) for every (\d+) (other )?(.+?) you have in play",
            |c| {
                let per = count_filter(&c[4])?;
                Some(finish_per(
                    num(c, 1),
                    per,
                    Who::SelfSide,
                    None,
                    Some(num(c, 2)),
                    c.get(3).is_some(),
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
                        cap: None,
                        per_excludes_self: false,
                        per_crowd: false,
                    }],
                    Condition::Always,
                    Duration::WhileInPlay,
                ))
            },
        ),
        // Crowd-Meter Finish bonus (task #131): "Your Finish roll[s] is/are + the Crowd
        // Meter [(Max +N)]" -> FinishRollBonus{per_crowd} — a SECOND live-Crowd-Meter
        // addend, ON TOP of the one the finish math already folds into every roll
        // (user-confirmed: additional, not redundant). "+ double/triple/half the Crowd
        // Meter" fails the literal "+ the" and stays tail (distinct multipliers).
        rule(
            r"Your Finish rolls? (?:is|are) \+ the Crowd Meter(?: \(Max \+?(\d+)\))?",
            |c| {
                Some(eff(
                    Trigger::Static,
                    vec![Action::FinishRollBonus {
                        delta: 0,
                        when_skill: None,
                        either: false,
                        when_base_le: None,
                        when_base_ge: None,
                        per: None,
                        per_who: Who::SelfSide,
                        per_zone: CountZone::InPlay,
                        per_divisor: None,
                        cap: c.get(1).map(|m| m.as_str().parse().unwrap()),
                        per_excludes_self: false,
                        per_crowd: true,
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
                    target_lowest: false,
                    target_chosen: false,
                    per_crowd: false,
                    cap: None,
                    per: Some(per.clone()),
                    per_zone: CountZone::FlippedThisTurn,
                    per_excludes_self: false,
                })
                .collect();
            Some(eff(
                Trigger::Static,
                actions,
                Condition::Always,
                Duration::WhileInPlay,
            ))
        }),
        // Opponent hand bury scaled by the turn's flips: "Your opponent buries N cards in
        // their hand for each Strike flipped" (Scott Prime's Five Star Heart Punch). The
        // per-count ranges over the finisher's own `flipped_this_turn`, which its "Flip N
        // cards" clause populates in the OnHit phase — so this fires OnHit too (later in
        // the card's effect list than the Flip, hence after it) and reads a full pool
        // rather than the empty pre-flip OnPlay one.
        rule(
            r"Your opponent buries (\d+) cards? in their hand for each (.+?) flipped",
            |c| {
                let per = flipped_filter(&c[2])?;
                Some(eff(
                    on_hit(),
                    vec![bury_per(
                        num(c, 1),
                        Who::Opp,
                        BuryFrom::Hand,
                        per,
                        CountZone::FlippedThisTurn,
                        true,
                    )],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        // "Your opponent [randomly] buries their hand" — bury the OPPONENT's WHOLE hand
        // (Scott Prime's The Loaded Glove, gated "If you roll Power, …" by the generic
        // gate rule; a 6-clause family). `all` buries every card, so the "randomly"
        // variant is identical (count == hand size either way).
        rule(
            r"Your opponent (?:randomly )?buries their (?:entire )?hand",
            |_| {
                Some(eff(
                    Trigger::OnPlay,
                    vec![bury_all_hand(CardFilter::default(), Who::Opp)],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
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
                        cap: None,
                        per_excludes_self: false,
                        per_crowd: false,
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
                        cap: None,
                        per_excludes_self: false,
                        per_crowd: false,
                    }],
                    Condition::Always,
                    Duration::WhileInPlay,
                ))
            },
        ),
    ]
}

fn build_stop_trigger_rules() -> Vec<(Regex, Builder)> {
    vec![
        // "Stop \"X\"[, \"Y\"][ or \"Z\"]" — stop a specifically-NAMED attack (no
        // order/type constraint), an OR-list of card names. One Stop whose `target`
        // name-filter matches any attack with one of those names (order/atk_type None
        // = any). Placed before "Stop any …" (which needs a type) — the leading quote
        // means it never overlaps that rule.
        rule(
            r#"Stop ("[^"]+"(?:(?:,\s*(?:or\s+)?|\s+or\s+)"[^"]+")*)"#,
            |c| {
                let names = quoted_names(&c[1]);
                (!names.is_empty()).then(|| {
                    eff(
                        Trigger::OnPlay,
                        vec![Action::Stop {
                            order: None,
                            atk_type: None,
                            source_is_skillreq: false,
                            even_unstoppable: false,
                            target: Some(cf_name(names)),
                            also_order: Vec::new(),
                        }],
                        Condition::Always,
                        Duration::Instant,
                    )
                })
            },
        ),
        // "Stop any <X> if it is not the first card played this turn" — the opponent's
        // OPENING card of the turn is safe; this card can stop <X> only once they have
        // already landed one. At the stop window (before the current card lands) the
        // attacker's per-turn hit count is >= 1 iff this is not their first card, so the
        // gate is `HitThisTurn{Opp}` (evaluated from the defender's view = the attacker).
        rule(
            r"Stop any (.+?) if it is not the first card played this turn",
            |c| stop_eff(&c[1], Condition::HitThisTurn { who: Who::Opp }),
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
        // NOTE: "If you have N <Competitor> Finishes in your discard pile, stop any <X>"
        // (Fortress's Tower of Strength, Void's front/back) is NOT grammar: "<Competitor>
        // Finishes" means that competitor's SIGNATURE finishes, not any Finish — a deck may
        // run Logoless/other finishes that must not count — and the Card model carries no
        // competitor linkage to filter on. Such clauses are bespoke overrides that name the
        // finishes (a `name_contains` filter). See overrides.yaml (Tower of Strength).
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
        // Generic gated stop: "(If|When) <gate>[,:] stop any <X>" — the catch-all for a
        // condition-gated stop whose gate is any shape `gate_condition` already models (Crowd
        // Meter, skill-vs-opp, in-play counts, roll/hit history, `and`/`or` compounds, …) and
        // whose <X> is a plain stop target. Placed AFTER the specific gated-stop rules above so
        // each keeps its exact phrasing; this only fires on the ones they don't claim. A gate
        // `gate_condition` can't map, or a target that isn't a plain stop list, declines (the
        // clause stays Unsupported). Ordered before the "When you roll …" trigger rules, whose
        // present-tense roll gates `gate_condition` does not match, so those still route there.
        rule(r"(?i)^(?:If|When) (.+?)[,:] stop any (.+)", |c| {
            stop_eff(&c[2], gate_condition(&c[1])?)
        }),
        // Generic roll trigger-body split: "When you roll <Skill>[ for your turn roll]
        // [:,] <body>" -> the body re-parsed through the grammar with OnRoll{skill}
        // attached. Single-skill only; the multi-skill OR ("roll Strike, Submission, or
        // Grapple") needs a multi-effect fan-out and stays tail. Placed LAST so every
        // specific "When you roll …" rule claims its exact phrasing first.
        rule(
            &format!(r"[Ww]hen you roll {SK}(?: for your turn roll)?[:,] (.+)"),
            |c| trigger_body(on_roll(skill(&c[1]), Who::SelfSide), &c[2]),
        ),
        // Multi-skill OR: "When you roll <S1>[, <S2>], or <Sn> [for your turn roll][:,]
        // <body>" -> OnRoll{None} (fires on any roll) gated by an OR of RollWasSkill on
        // the named skills, so the body fires on any of them. Requires 2+ skills; the
        // "for your Finish/Breakout roll" variants don't match (they aren't turn-roll
        // OnRoll) and stay tail. `SKNC` is a NON-capturing skill so groups stay 1=list,
        // 2=body. Placed after the single-skill rule.
        rule(
            &format!(
                r"[Ww]hen you roll ({SKNC}(?:,? (?:or )?{SKNC})+)(?: for your turn roll)?[:,] (.+)"
            ),
            |c| {
                let cond = roll_was_any(&c[1])?;
                trigger_body_cond(
                    Trigger::OnRoll {
                        skill: None,
                        who: Who::SelfSide,
                    },
                    cond,
                    &c[2],
                )
            },
        ),
        // Generic OPPONENT roll trigger-body split (task #131): "When your opponent rolls
        // <Skill>[ for their turn roll][:,] <body>" -> body re-parsed with OnRoll{Opp}
        // attached (the engine's run_on_roll already dispatches Opp-side rolls). The mirror
        // of the self rule above; "their roll is ±N" normalizes to a ModifyRoll{Opp,This}.
        // Placed LAST so every specific "When your opponent rolls …" rule wins its phrasing
        // first, and additive — a body with no grammar leaves the clause Unsupported.
        rule(
            &format!(r"When your opponent rolls {SK}(?: for their turn roll)?[:,] (.+)"),
            |c| on_roll_opp_body(skill(&c[1]), &c[2]),
        ),
        // Trigger-prefix body splits (task #119): reuse trigger_body for the standard
        // event/standing triggers, delegating the body to the whole grammar. All placed
        // LAST so every specific rule for these prefixes wins first; each catches the
        // previously-Unsupported clauses whose body parses. A body with no grammar ->
        // the whole clause stays Unsupported.
        rule(&format!(r"When you hit (?:an? )?{ATK}[,:] (.+)"), |c| {
            trigger_body(on_hit_type(atk(&c[1])), &c[2])
        }),
        // Opponent-side twin (Stung: "When your opponent hits a Strike, you may put 1
        // Strike from your discard pile on top of your deck") -> OnHit{Opp, atk} + body.
        rule(
            &format!(r"When your opponent hits (?:an? )?{ATK}[,:] (.+)"),
            |c| trigger_body(on_hit_type_who(atk(&c[1]), Who::Opp), &c[2]),
        ),
        // "If your opponent hits a <ATK>[,] <body>" — the "If"-phrased opponent OnHit
        // twin of the rule above, with an OPTIONAL separator: the in-discard self-recycle
        // family writes "hits a Grapple you may shuffle it …" (no comma) as well as "hits
        // a Submission, shuffle this card …". Reached via while_in_discard_effect for the
        // WHILE_IN_DISCARD forms (15b5b7e6, 93d7272d). The whole-clause "When …" rules win
        // first; this lands the leftover "If" gate-phrased trigger shape.
        rule(
            &format!(r"If your opponent hits (?:an? )?{ATK},? (.+)"),
            |c| trigger_body(on_hit_type_who(atk(&c[1]), Who::Opp), &c[2]),
        ),
        // Name/text-gated OnHit: "When you hit a [<type>] card with 'X' [or 'Y'] in the
        // name/text, <body>" -> OnHit{name_contains|text_contains[, atk_type]} + body.
        // "in the name" is the default when omitted. Group 1 = optional type, 2 = quoted
        // list, 3 = name|text, 4 = body.
        rule(
            &format!(
                r#"When you hit (?:an? )?(?:{ATK} )?(?:card )?with ("[^"]+"(?: or "[^"]+")*)(?: in the (name|text))?[,:] (.+)"#
            ),
            |c| {
                let atk_type = c.get(1).map(|m| atk(m.as_str()));
                let names = quoted_names(&c[2]);
                let in_text = c.get(3).is_some_and(|m| m.as_str() == "text");
                trigger_body(on_hit_named(atk_type, names, in_text), &c[4])
            },
        ),
        // Name-gated OnHit where the quoted title(s) come BEFORE the play-order word:
        // "When you hit a "X" [or "Y"] <Lead|Follow Up|Finish>, <body>" (Dr. Sleep's
        // "hit a Gangone Finish -> the Crowd Meter is +1" gimmick). The sibling rule
        // above handles the "with 'X' in the name" phrasing. Group 1 = quoted list,
        // 2 = play order, 3 = body.
        rule(
            r#"When you hit (?:an? )?("[^"]+"(?: or "[^"]+")*) (Lead|Follow ?-?Up|Finish)s?[,:] (.+)"#,
            |c| {
                let names = quoted_names(&c[1]);
                let po = match &c[2] {
                    m if m.starts_with("Lead") => PlayOrder::Lead,
                    m if m.starts_with("Finish") => PlayOrder::Finish,
                    _ => PlayOrder::Followup,
                };
                let trigger = Trigger::OnHit {
                    atk_type: None,
                    order: Some(po),
                    name_contains: names,
                    text_contains: Vec::new(),
                    on_any: false,
                    who: Who::SelfSide,
                    from_hand: false,
                };
                trigger_body(trigger, &c[3])
            },
        ),
        // From-HAND reactive (The Mailman Always Delivers): "When your opponent hits a
        // Finish: You may reveal this card from your hand and shuffle it into your deck to
        // add +N to your breakout rolls until the end of the turn." OnHit{Opp, Finish,
        // from_hand} dispatched by `hand_self_triggers`; the "you may" is `optional`; the
        // body shuffles the source away (`ShuffleSelfIntoDeck`) and banks a timed breakout
        // bonus (`GrantBreakoutBonus`). schema v128.
        rule(
            r"When your opponent hits a Finish: You may reveal this card from your hand and shuffle it into your deck to add \+(\d+) to your breakout rolls until the end of the turn",
            |c| {
                let mut e = eff(
                    Trigger::OnHit {
                        atk_type: None,
                        order: Some(PlayOrder::Finish),
                        name_contains: Vec::new(),
                        text_contains: Vec::new(),
                        on_any: false,
                        who: Who::Opp,
                        from_hand: true,
                    },
                    vec![
                        Action::ShuffleSelfIntoDeck,
                        Action::GrantBreakoutBonus {
                            delta: num(c, 1),
                            who: Who::SelfSide,
                        },
                    ],
                    Condition::Always,
                    Duration::Instant,
                );
                e.optional = true;
                Some(e)
            },
        ),
        rule(r"[Ii]f your opponent breaks out[,:] (.+)", |c| {
            trigger_body(
                Trigger::OnBreakout {
                    who: Some(Who::Opp),
                },
                &c[1],
            )
        }),
        // Bare self-loss body — "you lose the match via <Pinfall|Disqualification>". A body
        // rule for the loss-on-trigger family (Stung's The Bee Sting: "If your opponent
        // breaks out, you lose the match via Pinfall" -> OnBreakout{Opp} + LoseBy{Pinfall}).
        // The "If stopped, … via disqualification" family above matches its whole clause
        // first; this catches the other triggers' bodies.
        rule(
            r"[Yy]ou lose the match via (Pinfall|Disqualifications?)",
            |c| {
                let kind = if c[1].starts_with("Pinfall") {
                    LoseKind::Pinfall
                } else {
                    LoseKind::Disqualification
                };
                Some(eff(
                    Trigger::OnPlay,
                    vec![Action::LoseBy {
                        kind,
                        who: Who::SelfSide,
                    }],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        rule(r"When you break out[,:] (.+)", |c| {
            trigger_body(
                Trigger::OnBreakout {
                    who: Some(Who::SelfSide),
                },
                &c[1],
            )
        }),
        // "When either/any player breaks out, <body>" — fires on ANY breakout
        // (who: None). Common as a discard-pile self-recur ("… and either player breaks
        // out, you may shuffle it into your deck"); on_broken_out already dispatches
        // who=None from both sides.
        rule(
            r"(?:When|If) (?:either|any) player breaks out[,:] (.+)",
            |c| trigger_body(Trigger::OnBreakout { who: None }, &c[1]),
        ),
        rule(
            r"Each time your opponent rolls for a [Bb]reakout roll[,:] (.+)",
            |c| {
                trigger_body(
                    Trigger::OnBreakoutRoll {
                        who: Who::Opp,
                        attempts: Vec::new(),
                    },
                    &c[1],
                )
            },
        ),
        // Ordinal breakout-roll trigger (Return to Sender #30): "When your opponent rolls
        // their 1st [or 2nd] breakout roll, <body>" -> OnBreakoutRoll{Opp, attempts:[..]}
        // + body. The 1-based ordinals gate `run_on_breakout_roll`. schema v128.
        rule(
            r"When your opponent rolls their (\d+)(?:st|nd|rd|th)(?: or (\d+)(?:st|nd|rd|th))? breakout roll[,:] (.+)",
            |c| {
                let mut attempts = vec![num(c, 1)];
                if let Some(m) = c.get(2) {
                    attempts.push(m.as_str().parse().unwrap());
                }
                trigger_body(
                    Trigger::OnBreakoutRoll {
                        who: Who::Opp,
                        attempts,
                    },
                    &c[3],
                )
            },
        ),
        // Self-side ordinal breakout-roll trigger with a VALUE gate (My Most Powerful
        // Spell, via WHILE_IN_DISCARD): "When your Nth Breakout roll is X [or Y], <body>"
        // -> OnBreakoutRoll{SELF, attempts:[N]} gated on the roll VALUE (an `Or` of
        // `RollValue`s read from the breakout RollContext). Pairs with the "bury this card
        // to re-roll your Breakout roll" body.
        rule(
            r"When your (\d+)(?:st|nd|rd|th) [Bb]reakout roll is (\d+)(?: or (\d+))?, (.+)",
            |c| {
                let val = |v: i64| Condition::RollValue {
                    cmp: Comparator::Eq,
                    value: v,
                    who: Who::SelfSide,
                };
                let cond = match c.get(3) {
                    Some(m) => Condition::Or {
                        items: vec![val(num(c, 2)), val(m.as_str().parse().unwrap())],
                    },
                    None => val(num(c, 2)),
                };
                trigger_body_cond(
                    Trigger::OnBreakoutRoll {
                        who: Who::SelfSide,
                        attempts: vec![num(c, 1)],
                    },
                    cond,
                    &c[4],
                )
            },
        ),
        // OnShuffle trigger split (Leader of the Postal Nation gimmick): "After you
        // shuffle your deck, <body>". Pairs with the "add the top/bottom card of your
        // deck to your hand" body rule below. schema-neutral (OnShuffle pre-existed,
        // override-only).
        rule(r"After you shuffle your deck[,:] (.+)", |c| {
            trigger_body(Trigger::OnShuffle { who: Who::SelfSide }, &c[1])
        }),
        rule(r"At the start of the match[,:] (.+)", |c| {
            trigger_body(Trigger::StartOfMatch, &c[1])
        }),
        // Inline "When the Crowd Meter increases[,:] <body>" (3 DB cards; Khloe Mai's
        // gimmick instead splits the body onto the next line and is joined by the
        // `cm_increase_header` consumer in the parse loop). Both feed the same trigger.
        rule(r"When the Crowd Meter increases[,:] (.+)", |c| {
            trigger_body(Trigger::OnCrowdMeterIncrease, &c[1])
        }),
        rule(r"At the start of your turn[,:] (.+)", |c| {
            trigger_body(Trigger::StartOfTurn, &c[1])
        }),
        rule(r"When you win (?:the|a) turn roll[,:;] (.+)", |c| {
            trigger_body(Trigger::OnWinTurn, &c[1])
        }),
        rule(
            r"[Ii]f (?:this card is |this is )?[Ss]topped[,:] (.+)",
            |c| trigger_body(on_your_stop(), &c[1]),
        ),
        // "When your opponent stops a card, <body>" — your card was stopped
        // (Direction::Yours), same trigger as "if stopped". The body re-parses through the
        // grammar; the subject "they"/"your opponent" carries into RevealAndDiscard etc.
        rule(
            r"When your opponent stops (?:a|one|your) cards?[,:] (.+)",
            |c| trigger_body(on_your_stop(), &c[1]),
        ),
        // "When you stop a card, <body>" — the stopper's side (Direction::Theirs), the
        // mirror of "your opponent stops a card". Common as a discard-pile self-recur
        // ("… and you stop a card, add it to your hand"); run_on_stop_gimmicks already
        // dispatches the Theirs direction for the stopper.
        rule(r"When you stop (?:a|one) cards?[,:] (.+)", |c| {
            trigger_body(on_their_stop(), &c[1])
        }),
        // WHILE_IN_DISCARD self-trigger (task #115): "When this card is in your discard
        // pile[ and <event>][:,] <body>" — the prefix is a Duration::WhileInDiscard marker;
        // the remainder is a normal trigger clause re-parsed via while_in_discard_effect.
        // Separator: " " (before "and"), ":" or "," (nested "When …" / passive). Slice 1
        // fires the triggered forms (OnRoll self-recursion the biggest); passive bodies
        // decline. Placed with the trigger-prefix splits so specific rules win first.
        rule(
            r"(?i)When this card is in your discard pile(?: ?[:,] ?| )(.+)",
            |c| while_in_discard_effect(&c[1]),
        ),
        // Condition-gate prefixes (task #130): keep the body's natural trigger, AND a
        // gate onto it. Placed LAST so a specific rule for the whole clause wins first.
        // "If you rolled <skill> for your turn roll[;,] <body>" -> RollWasSkill{SELF}
        // gate (resolved at play time against the threaded turn-roll context).
        rule(
            &format!(r"If you rolled {SK} for your turn roll[;,] (.+)"),
            |c| {
                gate_body(
                    Condition::RollWasSkill {
                        skill: skill(&c[1]),
                        who: Who::SelfSide,
                    },
                    &c[2],
                )
            },
        ),
        // Present-tense "If you roll <S>[ or <S>], <body>" — a finish-rider roll gate
        // (Scott Prime's The Loaded Glove: "If you roll Power, your opponent buries their
        // hand"; Malleus Maleficarum: "If you roll Power or Strike, …"). Scoped to the
        // literal "If you roll" prefix so it never shadows the "When you roll <S>, …"
        // OnRoll event family; resolved against the play-time turn-roll context. A
        // two-skill list becomes an `Or`.
        rule(&format!(r"If you roll {SK}(?: or {SK})?, (.+)"), |c| {
            let was = |s: &str| Condition::RollWasSkill {
                skill: skill(s),
                who: Who::SelfSide,
            };
            let gate = match c.get(2) {
                Some(m) => Condition::Or {
                    items: vec![was(&c[1]), was(m.as_str())],
                },
                None => was(&c[1]),
            };
            gate_body(gate, &c[3])
        }),
        // "If you have a card with 'X' in the name in play, <body>" -> HasInPlay{SELF}
        // name-substring gate.
        rule(
            r#"If you have a card with "([^"]+)" in the name in play, (.+)"#,
            |c| {
                gate_body(
                    has_in_play(Who::SelfSide, cf_name(vec![c[1].to_owned()]), 1),
                    &c[2],
                )
            },
        ),
        // "If <gate>, double these bonuses" -> DoubleFinishIf{condition} (a 137-clause
        // family; "these bonuses" = the card's own FinishBonus sum, doubled at finish
        // time when the gate holds). Only the ×2 "double" form maps here — triple /
        // quadruple would need a factor field and stay Unsupported. `gate_condition`
        // declines gates we don't model, so those clauses stay Unsupported too.
        rule(r"If (.+?),? double (?:these|the) bonuses", |c| {
            Some(eff(
                Trigger::OnPlay,
                vec![Action::DoubleFinishIf {
                    condition: gate_condition(&c[1])?,
                }],
                Condition::Always,
                Duration::Instant,
            ))
        }),
        // Comma-less Crowd-Meter gate: "If/When the Crowd Meter is N or greater/less/more
        // <body>" — a recurring DB phrasing (~27 clauses, incl. JT Dunn's Fear of Diving
        // "… 4 or greater stop any Finish Strike") that drops the separator the generic
        // gate below requires. The "or greater/less/more" terminal delimits the gate
        // unambiguously, so no comma is needed; anything past it is the body. Placed just
        // before the generic gate so the punctuated form still wins when present.
        rule(
            r"(?:If|When) (the Crowd Meter is \d+ or (?:greater|less|more)) (.+)",
            |c| gate_body(gate_condition(&c[1])?, &c[2]),
        ),
        // Generic condition gate: "If/When <gate>[;,] <body>" — parse the gate via
        // `gate_condition` and the body via `gate_body` (natural trigger kept, gate
        // AND-ed on). Placed LAST so every specific rule wins first; it fires only when
        // BOTH the gate and the body are modelled, so it strictly adds coverage. The
        // non-greedy gate stops at the first `,`/`;`; a gate with an internal comma
        // (e.g. a multi-name list) fails `gate_condition` and the clause stays
        // Unsupported. "When" is accepted too: `gate_condition` only matches STATE gates
        // (Crowd Meter / in-play counts / roll-was / match type), for which "When <state>"
        // and "If <state>" are equivalent; event-trigger "When …" phrases either match a
        // specific trigger rule first or fail `gate_condition` and decline. Subsumes the
        // roll-gate / name-in-play prefixes above; those remain as more-specific twins.
        rule(r"(?:If|When) (.+?)[;,] (.+)", |c| {
            gate_body(gate_condition(&c[1])?, &c[2])
        }),
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

/// A parsed frequency cap: the [`Frequency`] kind plus its `n` (the count for
/// [`Frequency::NPerMatch`], `None` for the fixed kinds). Returned by the frequency
/// header/phrase parsers and threaded onto an [`Effect`]'s guard.
type FreqSpec = (Frequency, Option<i64>);

/// A frequency-guard header ("Once per match:", "N times per match:") scoping the
/// clauses that follow, or `None`.
fn freq_header(clause: &str) -> Option<FreqSpec> {
    static ONCE_MATCH: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^Once (?:per|a) match:?$").unwrap());
    // "Once per turn roll" == "Once per turn" as a guard: the turn-roll phase (with all its
    // bumps/re-rolls) is one phase per turn, and the per-turn counter clears at turn start,
    // so both cap a roll-phase (OnRoll/OnReroll/OnBump) effect to once per roll-off. Matched
    // before ONCE_TURN, whose `turn:?$` can't reach past "roll".
    static ONCE_TURN_ROLL: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^Once (?:per|a) turn roll:?$").unwrap());
    static ONCE_TURN: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^Once (?:per|a|on your) turn:?$").unwrap());
    static N_MATCH: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^(\d+) times per match:?$").unwrap());
    let stripped = clause.trim();
    if ONCE_MATCH.is_match(stripped) {
        return Some((Frequency::OncePerMatch, None));
    }
    if ONCE_TURN_ROLL.is_match(stripped) || ONCE_TURN.is_match(stripped) {
        return Some((Frequency::OncePerTurn, None));
    }
    if let Some(m) = N_MATCH.captures(stripped) {
        return Some((Frequency::NPerMatch, Some(m[1].parse().unwrap())));
    }
    None
}

/// Uppercase the first character of `s` (ASCII), leaving the rest untouched. Used to
/// sentence-case a body promoted out of an inline prefix so capital-anchored rules match.
fn uppercase_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// An INLINE frequency prefix — a [`freq_header`] fused to its body on one clause
/// ("Once per turn: <body>", "Once a turn, <body>", "N times per match: <body>").
/// Returns the frequency plus the trailing body, which the caller compiles on its own
/// and to which the frequency applies ALONE (unlike a standalone header, which persists
/// over the following clauses). `None` when the clause is not so prefixed.
fn inline_freq(clause: &str) -> Option<(Frequency, Option<i64>, &str)> {
    // "Once per turn roll" is listed BEFORE "Once per turn" so the longer form wins (else
    // "Once per turn" matches and the following `[:,]` fails on the " roll" that remains).
    static INLINE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?i)^(Once (?:per|a) match|Once (?:per|a) turn roll|Once (?:per|a|on your) turn|(\d+) times per match)[:,]\s+(.+)$",
        )
        .unwrap()
    });
    let caps = INLINE.captures(clause.trim())?;
    let head = caps.get(1)?.as_str().to_lowercase();
    let body = caps.get(3)?.as_str();
    let freq = if head.contains("match") && caps.get(2).is_none() {
        Frequency::OncePerMatch
    } else if caps.get(2).is_some() {
        return Some((Frequency::NPerMatch, Some(caps[2].parse().ok()?), body));
    } else {
        Frequency::OncePerTurn
    };
    Some((freq, None, body))
}

/// Parse a bare frequency PHRASE (no trailing `:` anchor) into a [`Frequency`]. Unlike
/// [`freq_header`] (which requires the phrase to be the whole clause), this reads just the
/// prefix ahead of another header — e.g. the "Once" / "Once per match" that precedes a
/// window header in "Once during your turn:". A bare "Once" (no "per match") is a
/// per-TURN cap: "once during your turn" == "once per turn". `None` when the phrase is not
/// a frequency (so the caller declines rather than swallowing arbitrary prefix text).
fn freq_phrase(s: &str) -> Option<FreqSpec> {
    static N_MATCH: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^(\d+) times per match$").unwrap());
    static PER_MATCH: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^Once (?:per|a) match$").unwrap());
    static ONCE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^Once$").unwrap());
    let s = s.trim().trim_end_matches([',', ':']).trim();
    if PER_MATCH.is_match(s) {
        return Some((Frequency::OncePerMatch, None));
    }
    if let Some(m) = N_MATCH.captures(s) {
        return Some((Frequency::NPerMatch, Some(m[1].parse().unwrap())));
    }
    if ONCE.is_match(s) {
        return Some((Frequency::OncePerTurn, None));
    }
    None
}

/// A window header ("During your turn:", "During your opponent's turn:") scoping the
/// clauses that follow to a turn phase. Returns an optional leading frequency (the
/// "Once" / "Once per match," in "Once during your turn:", parsed by [`freq_phrase`]) plus
/// the [`Condition::DuringTurn`] it opens. Both persist (like a [`freq_header`]) until
/// another header replaces them — the whole text after the header hangs off that turn
/// window (and, when present, that frequency cap). `None` for any non-header clause, and
/// for a clause whose leading text is not a frequency (so it stays its own clause).
fn window_header(clause: &str) -> Option<(Option<FreqSpec>, Condition)> {
    static WINDOW: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^(?:(.+?)\s+)?During your (turn|(?:target's|opponent's) turn):?$").unwrap()
    });
    let caps = WINDOW.captures(clause.trim())?;
    let freq = match caps.get(1) {
        Some(m) => Some(freq_phrase(m.as_str())?),
        None => None,
    };
    let who = if caps[2].eq_ignore_ascii_case("turn") {
        Who::SelfSide
    } else {
        Who::Opp
    };
    Some((freq, Condition::DuringTurn { who }))
}

/// A STANDALONE roll-phase trigger header ("When you roll `<S>` for your turn roll:")
/// whose body lands on the FOLLOWING clauses — the standalone twin of the inline
/// "When you roll `<S>` for your turn roll, `<body>`" grammar rule. Returns the
/// [`Trigger::OnRoll`] it opens plus an optional gate: for the multi-skill OR form the
/// trigger fires on any roll (`OnRoll{None}`) and the gate ([`roll_was_any`]) restricts
/// it to the named skills, mirroring the inline multi-skill rule. The window persists
/// over the clauses that follow (like [`window_header`]) until end of text. `None` for
/// any non-header clause (a "your lowest skill" header has no named skill and declines).
fn roll_header(clause: &str) -> Option<(Trigger, Option<Condition>)> {
    static SINGLE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(&format!(r"^When you roll {SK} for your turn roll:$")).unwrap()
    });
    static MULTI: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(&format!(
            r"^When you roll ({SKNC}(?:,? (?:or )?{SKNC})+) for your turn roll:$"
        ))
        .unwrap()
    });
    let stripped = clause.trim();
    if let Some(c) = SINGLE.captures(stripped) {
        return Some((on_roll(skill(&c[1]), Who::SelfSide), None));
    }
    if let Some(c) = MULTI.captures(stripped) {
        return Some((
            Trigger::OnRoll {
                skill: None,
                who: Who::SelfSide,
            },
            Some(roll_was_any(&c[1])?),
        ));
    }
    None
}

/// A standalone "During your turn roll:" header — opens a turn-roll scope over the
/// clauses that follow (like [`window_header`]/[`roll_header`], persists to end of
/// text). Its body's STANDING skill buffs are turn-roll-scoped by [`scope_to_turn_roll`];
/// every other body passes through as parsed (its own trigger already carries the
/// timing). Only the bare header line matches — the inline "During your turn roll,
/// <body>" comma forms stay their own clause.
fn turn_roll_header(clause: &str) -> bool {
    static RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^During your turn rolls?:?$").unwrap());
    RE.is_match(clause.trim())
}

/// Re-scope a turn-roll-header body: a Static, self-side skill buff ("your Power is +1",
/// "your Technique is + the Crowd Meter (Max +3)") is active only during the roll-off, so
/// rewrite it to a [`Action::TurnRollBonus`] (the roll-off parallel of a plain
/// [`Action::BuffSkill`], read by `turn_roll_bonus`), carrying the effect's condition and
/// the `per_crowd`/`cap` dynamic delta. Card-count (`per`) buffs, retargeted
/// (`target_highest`/`_lowest`) buffs, opponent-directed buffs, timed buffs, and every
/// non-`BuffSkill` action are left untouched — a triggered body already encodes its own
/// roll timing, and `TurnRollBonus` carries no card-count / retarget delta.
fn scope_to_turn_roll(mut eff: Effect) -> Effect {
    if eff.trigger != Trigger::Static {
        return eff;
    }
    eff.actions = eff
        .actions
        .into_iter()
        .map(|a| buff_to_turn_roll_bonus(&a).unwrap_or(a))
        .collect();
    eff
}

/// The roll-off parallel of a plain self-side skill buff: a flat ("your Power is +1") or
/// dynamic Crowd-Meter ("your Technique is + the Crowd Meter (Max +3)") [`Action::BuffSkill`]
/// maps to the [`Action::TurnRollBonus`] the turn-roll base reads, carrying `per_crowd`/`cap`.
/// `None` for anything `TurnRollBonus` can't express — a card-count (`per`) or retargeted
/// (`target_highest`/`_lowest`) buff, an opponent-directed buff, or a non-`BuffSkill` action.
fn buff_to_turn_roll_bonus(a: &Action) -> Option<Action> {
    match a {
        Action::BuffSkill {
            skill,
            delta,
            who: Who::SelfSide,
            duration: Duration::WhileInPlay,
            target_highest: false,
            target_lowest: false,
            target_chosen: false,
            per_crowd,
            cap,
            per: None,
            per_excludes_self: false,
            ..
        } => Some(Action::TurnRollBonus {
            skill: *skill,
            delta: *delta,
            who: Who::SelfSide,
            either: false,
            per_crowd: *per_crowd,
            cap: *cap,
        }),
        _ => None,
    }
}

/// "Your `<skills>` (skill) is/are +N \[+ the Crowd Meter] during your turn\[ and turn
/// rolls]" — a standing skill buff scoped to the OWNER's turn. The turn window is a
/// [`Condition::DuringTurn`] gate: the buff folds into derived stats only while it is your
/// turn, so it reaches your Finish rolls and your skill requirements but not your
/// opponent's turn, and the roll-off is excluded (`GameState::in_turn_roll`). "and turn
/// rolls" ADDS the roll: a parallel [`Action::TurnRollBonus`] per buffed skill in a second,
/// `Always`-gated effect (a `DuringTurn` gate would be suppressed in the roll-off, so it
/// can't carry the roll piece). The head reuses the plain buff grammar
/// ([`crowd_meter_buff`] and the flat "+N" rules), so single-skill, multi-skill, flat and
/// Crowd-Meter forms all ride through — this composer only re-scopes the result. `None`
/// when the head is not a self-side Static skill buff (falls through to Unsupported), or
/// when "and turn rolls" is present but a buff can't map to a `TurnRollBonus` (so the roll
/// piece is never silently dropped).
fn during_turn_skill_buff(clause: &str, source: EffectSource) -> Option<Vec<Effect>> {
    static RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^(.+?) during your turn( and turn rolls)?\.?$").unwrap());
    let caps = RE.captures(clause.trim())?;
    let and_rolls = caps.get(2).is_some();
    let base = compile(
        caps.get(1)?.as_str().trim(),
        source,
        Frequency::Unlimited,
        None,
    );
    if base.trigger != Trigger::Static
        || base.actions.is_empty()
        || !base.actions.iter().all(|a| {
            matches!(
                a,
                Action::BuffSkill {
                    who: Who::SelfSide,
                    ..
                }
            )
        })
    {
        return None;
    }
    // Effect A: the buff, gated to your turn.
    let mut turn_eff = base.clone();
    turn_eff.condition = Condition::DuringTurn { who: Who::SelfSide };
    let mut out = vec![turn_eff];
    // Effect B ("and turn rolls"): the roll-off piece — one TurnRollBonus per buffed skill,
    // Always-gated so it survives the roll-off. Decline the whole clause if any buff can't
    // map, rather than drop that skill's roll bonus.
    if and_rolls {
        let bonuses: Vec<Action> = base
            .actions
            .iter()
            .filter_map(buff_to_turn_roll_bonus)
            .collect();
        if bonuses.len() != base.actions.len() {
            return None;
        }
        out.push(eff(
            Trigger::Static,
            bonuses,
            Condition::Always,
            Duration::WhileInPlay,
        ));
    }
    Some(out)
}

/// One self-side flat [`Action::BuffSkill`] per skill named in a bare buff head —
/// "your `<skills>` is/are +N" or "+N to `<skills>`" — reused by the phase-scoped
/// composers below. `None` when the head is not a flat positive skill buff (a
/// Crowd-Meter/per-count/retargeted head, or non-skill text, declines).
fn self_flat_skill_buffs(head: &str) -> Option<Vec<Action>> {
    static YOUR: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^your (.+?) (?:is|are) \+(\d+)$").unwrap());
    static TO: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^\+(\d+) to (.+)$").unwrap());
    let (skills, delta) = if let Some(c) = YOUR.captures(head) {
        (skill_list(&c[1]), c[2].parse().ok()?)
    } else {
        let c = TO.captures(head)?;
        (skill_list(&c[2]), c[1].parse().ok()?)
    };
    if skills.is_empty() {
        return None;
    }
    Some(
        skills
            .into_iter()
            .map(|s| buff(s, delta, Who::SelfSide))
            .collect(),
    )
}

/// "During your opponent's turn, `<buff>`" / "`<buff>` during your opponent's turn" /
/// "+N to `<S>` during your opponent's turn" — a standing SELF skill buff scoped to the
/// OPPONENT's turn via a [`Condition::DuringTurn`]`{Opp}` gate. `effective_stats` folds a
/// player's own buffs with the gate resolved against THAT player (the derived-stats
/// closure is keyed to the buffed side), so `DuringTurn{Opp}` reads as "active == the
/// opponent" — the buff is live only on the opponent's turn, reaching the stops and
/// Finishes you make then, and never your own turn or the roll-off. There is no "and turn
/// rolls" tail in this direction (you do not take a turn roll on the opponent's turn), so
/// this composer emits a single `DuringTurn`-gated effect with one `BuffSkill` per named
/// skill. `None` when the body is not a self-side flat skill buff (falls through to
/// Unsupported).
fn during_opponent_turn_buff(clause: &str) -> Option<Vec<Effect>> {
    static RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?i)^(?:during your opponent.?s turn,?\s*(.+)|(.+?) during your opponent.?s turn)\.?$",
        )
        .unwrap()
    });
    let caps = RE.captures(clause.trim())?;
    // The prefix branch's greedy `(.+)` swallows a trailing "." ("your Technique is +2.");
    // the suffix branch does not. Strip it so the head's `\+(\d+)$` anchor lands.
    let head = caps
        .get(1)
        .or_else(|| caps.get(2))?
        .as_str()
        .trim()
        .trim_end_matches('.')
        .trim();
    let actions = self_flat_skill_buffs(head)?;
    Some(vec![eff(
        Trigger::Static,
        actions,
        Condition::DuringTurn { who: Who::Opp },
        Duration::WhileInPlay,
    )])
}

/// "Your opponent's `<skills>` is/are -N during their turn\[ and turn rolls]" — a standing
/// OPPONENT skill DEBUFF scoped to the opponent's turn, the opponent-directed mirror of
/// [`during_turn_skill_buff`]'s two-piece split. (A) a [`Action::BuffSkill`]`{who:Opp}`
/// gated on [`Condition::DuringTurn`]`{SELF}`: this buff folds onto the OPPONENT (the
/// target), and `effective_stats` resolves the gate against the buffed side, so
/// `DuringTurn{SELF}` reads as "active == the opponent" — the debuff bites the stops and
/// Finishes the opponent makes on their own turn and never the roll-off. (B) for "and turn
/// rolls", a [`Action::TurnRollBonus`]`{who:Opp}` (Always): `turn_roll_bonus` sums a
/// roller's opponent's `Opp` mods, so this reduces the opponent's turn roll. `None` when
/// the head is not an opponent skill debuff (falls through to Unsupported).
fn opponent_turn_debuff(clause: &str) -> Option<Vec<Effect>> {
    static RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?i)^your opponent.?s (.+?) (?:is|are) ([+-]?\d+) during their turn( and turn rolls)?\.?$",
        )
        .unwrap()
    });
    let caps = RE.captures(clause.trim())?;
    let skills = skill_list(&caps[1]);
    if skills.is_empty() {
        return None;
    }
    let delta: i64 = caps[2].parse().ok()?;
    // (A) the standing skill debuff, gated to the opponent's own turn (DuringTurn{SELF}
    // reads as the opponent's turn because effective_stats keys the gate to the buffed
    // side — see the doc comment).
    let buffs: Vec<Action> = skills.iter().map(|s| buff(*s, delta, Who::Opp)).collect();
    let mut out = vec![eff(
        Trigger::Static,
        buffs,
        Condition::DuringTurn { who: Who::SelfSide },
        Duration::WhileInPlay,
    )];
    // (B) "and turn rolls": the opponent's-turn-roll piece — one TurnRollBonus{who:Opp}
    // per skill, Always-gated (the roll-off timing is inherent to TurnRollBonus).
    if caps.get(3).is_some() {
        let rolls: Vec<Action> = skills
            .iter()
            .map(|s| Action::TurnRollBonus {
                skill: *s,
                delta,
                who: Who::Opp,
                either: false,
                per_crowd: false,
                cap: None,
            })
            .collect();
        out.push(eff(
            Trigger::Static,
            rolls,
            Condition::Always,
            Duration::WhileInPlay,
        ));
    }
    Some(out)
}

/// Non-effect metadata (a deck-build "Skill Requirement:" line): recognized and
/// skipped, neither an effect nor Unsupported.
fn is_metadata(clause: &str) -> bool {
    static META: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^Skill Requirement:").unwrap());
    META.is_match(clause.trim())
}

/// The first rule whose regex matches AND whose builder accepts the clause,
/// returned as `(index into RULES, built effect)`. Shared by `match_grammar` (the
/// parse path) and `matching_rule` (the grammar-catalog attribution path) so they
/// can never diverge on which rule wins.
fn first_match(clause: &str) -> Option<(usize, Effect)> {
    let stripped = clause.trim().trim_end_matches('.').trim();
    for (i, (re, builder)) in RULES.iter().enumerate() {
        if let Some(caps) = re.captures(stripped) {
            if let Some(eff) = builder(&caps) {
                return Some((i, eff)); // a builder may decline (unmodelled target/desc)
            }
        }
    }
    None
}

fn match_grammar(clause: &str) -> Option<Effect> {
    first_match(clause).map(|(_, eff)| eff)
}

/// Index into `RULES` of the first rule that handles `clause`, or `None` when no
/// single rule does (it may still parse via a composition, or be Unsupported).
/// Tooling-only: pairs with [`rule_catalog`] to attribute real clauses to rules.
fn matching_rule(clause: &str) -> Option<usize> {
    first_match(clause).map(|(i, _)| i)
}

/// Each grammar-relevant clause in `text` — frequency headers and metadata filtered
/// out, matching `coverage` — paired with the index of the first rule that matches
/// it (`None` = handled only by a composition, or unsupported). Feeds the grammar
/// catalog's per-rule examples.
pub fn clause_rule_hits(text: &str) -> Vec<(String, Option<usize>)> {
    split_clauses(text)
        .into_iter()
        .filter(|c| freq_header(c).is_none() && !is_metadata(c))
        .map(|c| {
            let hit = matching_rule(&c);
            (c, hit)
        })
        .collect()
}

fn compile(clause: &str, source: EffectSource, freq: Frequency, n: Option<i64>) -> Effect {
    let g = FrequencyGuard {
        node_type: FrequencyGuardTag,
        kind: freq,
        n,
    };
    // A single rule wins; otherwise try folding a top-level compound ("Draw 1 card and
    // bury 1 card" -> one effect with both actions). compound_body validates each part
    // parses to a plain Instant action-effect on the same trigger, so a spurious "and"
    // inside one action declines the split.
    if let Some(mut eff) = match_grammar(clause)
        .or_else(|| compound_body(clause))
        .or_else(|| choice_body(clause))
    {
        eff.raw_clause = clause.to_owned();
        eff.source = source;
        eff.frequency = g;
        return eff;
    }
    unsupported_effect(clause, source, g)
}

/// Build the fallback "no grammar match" [`Action::Unsupported`] effect for a clause the
/// grammar couldn't map — the never-silently-dropped floor of the parser.
fn unsupported_effect(clause: &str, source: EffectSource, freq: FrequencyGuard) -> Effect {
    Effect {
        node_type: EffectTag,
        trigger: Trigger::OnPlay,
        condition: Condition::Always,
        actions: vec![Action::Unsupported {
            raw_text: clause.to_owned(),
            reason: "no grammar match".to_owned(),
        }],
        duration: Duration::Instant,
        frequency: freq,
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
    let clauses = split_clauses(text);
    let mut effects = Vec::new();
    let mut freq = Frequency::Unlimited;
    let mut n = None;
    let mut window = Condition::Always;
    // An active standalone roll-phase trigger window ("When you roll <S> for your turn
    // roll:"): the OnRoll trigger to stamp on the following clauses, plus an optional gate
    // (the multi-skill OR restriction). Set by `roll_header`, persists until end of text.
    let mut roll_win: Option<(Trigger, Option<Condition>)> = None;
    // An active "During your turn roll:" header: standing self skill buffs in the
    // following clauses are re-scoped to the roll-off (`scope_to_turn_roll`). Persists
    // to end of text, like `window`/`roll_win`.
    let mut turn_roll_scope = false;
    // An active bare "When this card is in your discard pile:" header (`discard_header`):
    // every following clause is re-parsed through `while_in_discard_effect` and carries
    // Duration::WhileInDiscard. Persists to end of text — the discard section is always a
    // card's last block. Bodies whose trigger isn't a wired discard-dispatch site (or that
    // have no grammar) decline to Unsupported rather than leak a wrongly-in-play effect.
    let mut discard_scope = false;
    // AND the active turn-window onto an effect's condition (a no-op when Always).
    let scope = |mut eff: Effect, window: &Condition| {
        if !matches!(window, Condition::Always) {
            eff.condition = and_conds(window.clone(), eff.condition);
        }
        eff
    };
    let mut i = 0;
    while i < clauses.len() {
        let clause = &clauses[i];
        if let Some((f, nn)) = freq_header(clause) {
            freq = f;
            n = nn;
            i += 1;
            continue;
        }
        // Inline frequency prefix ("Once per turn: <body>", "Once a turn, <body>"):
        // apply the frequency to THIS body alone (a standalone header instead persists).
        // Take it only if the body actually parses; otherwise fall through so the whole
        // clause compiles to Unsupported (never a silent drop).
        if let Some((f, nn, body)) = inline_freq(clause) {
            let is_unsupported = |e: &Effect| {
                e.actions
                    .iter()
                    .any(|a| matches!(a, Action::Unsupported { .. }))
            };
            let mut e = compile(body, source, f, nn);
            // A body promoted from mid-clause may start lowercase ("Once a turn, draw
            // …"); retry sentence-cased so capital-anchored rules match.
            if is_unsupported(&e) {
                let cap = uppercase_first(body);
                if cap != body {
                    e = compile(&cap, source, f, nn);
                }
            }
            if !is_unsupported(&e) {
                e.raw_clause = clause.clone();
                effects.push(scope(e, &window));
                i += 1;
                continue;
            }
        }
        if let Some((freq_prefix, cond)) = window_header(clause) {
            window = cond;
            // "Once during your turn:" / "Once per match, during your turn:" carry a
            // frequency cap alongside the turn window; a bare "During your turn:" carries
            // none (leave any frequency a prior header set in place).
            if let Some((f, nn)) = freq_prefix {
                freq = f;
                n = nn;
            }
            i += 1;
            continue;
        }
        // Standalone roll-phase header: open an OnRoll window over the clauses that follow.
        if let Some(win) = roll_header(clause) {
            roll_win = Some(win);
            i += 1;
            continue;
        }
        // Standalone "During your turn roll:" header: open a turn-roll scope so the
        // following standing skill buffs are re-scoped to the roll-off.
        if turn_roll_header(clause) {
            turn_roll_scope = true;
            i += 1;
            continue;
        }
        // Bare "When this card is in your discard pile:" header: open a WhileInDiscard
        // scope over the rest of the text (the discard section is a card's last block).
        if discard_header(clause) {
            discard_scope = true;
            i += 1;
            continue;
        }
        // Under an active discard scope, every clause is a body the card declares only
        // from the pile: re-parse it as a WhileInDiscard effect. This runs BEFORE the
        // in-play handlers below so a discard-only passive (e.g. "your opponent's skill
        // cards have blank text") can't leak an active-in-play effect; an unwired or
        // grammarless body compiles to Unsupported instead (never a silent drop).
        if discard_scope {
            let g = FrequencyGuard {
                node_type: FrequencyGuardTag,
                kind: freq,
                n,
            };
            let e = while_in_discard_effect(clause)
                .map(|mut eff| {
                    eff.raw_clause = clause.clone();
                    eff.source = source;
                    eff.frequency = g.clone();
                    eff
                })
                .unwrap_or_else(|| unsupported_effect(clause, source, g));
            effects.push(scope(e, &window));
            i += 1;
            continue;
        }
        if is_metadata(clause) {
            i += 1;
            continue;
        }
        // Split reveal-then: a bare "Reveal the … card of your deck:" header whose
        // "If <filter>, <consequence>" lands on the NEXT clause. Consume both into one
        // RevealThen; if the follow-up doesn't parse, the header falls through below
        // (compiled to Unsupported, never silently dropped).
        if let Some(src) = reveal_header(clause) {
            if let Some(mut eff) = clauses.get(i + 1).and_then(|nxt| reveal_followup(src, nxt)) {
                eff.raw_clause = format!("{clause} {}", clauses[i + 1]);
                eff.source = source;
                eff.frequency = FrequencyGuard {
                    node_type: FrequencyGuardTag,
                    kind: freq,
                    n,
                };
                effects.push(scope(eff, &window));
                i += 2;
                continue;
            }
        }
        // "If this is a <match-type> match, you may flip both cards instead" REPLACES the
        // preceding "each player reveals top & adds to hand": outside the match type the
        // add stands; inside it, offer add-or-flip. (Friends and Rivals family.)
        if let Some(gate) = flip_both_instead(clause) {
            if effects.last().is_some_and(is_reveal_top_both) {
                let add = effects.pop().unwrap();
                let g = FrequencyGuard {
                    node_type: FrequencyGuardTag,
                    kind: freq,
                    n,
                };
                let option = |label: &str, actions: Vec<Action>| ChoiceOption {
                    node_type: ChoiceOptionTag,
                    label: label.to_owned(),
                    actions,
                };
                let choice = Action::Choice {
                    options: vec![
                        option("Add both to your hand", add.actions.clone()),
                        option(
                            "Flip both cards",
                            vec![flip(1, Who::SelfSide), flip(1, Who::Opp)],
                        ),
                    ],
                };
                let mut choice_eff = eff(on_hit(), vec![choice], gate.clone(), Duration::Instant);
                choice_eff.raw_clause = clause.to_owned();
                choice_eff.source = source;
                choice_eff.frequency = g;
                // The plain add-to-hand now applies only OUTSIDE the flip match types.
                let mut plain = add;
                plain.condition = Condition::Not {
                    item: Box::new(gate),
                };
                effects.push(scope(plain, &window));
                effects.push(scope(choice_eff, &window));
                i += 1;
                continue;
            }
        }
        // "If <gate>, <body> instead" REPLACES the preceding sibling: gate the base on
        // Not(gate) and add the replacement (sharing the base's trigger) on the gate, so
        // exactly one fires. Only when the base's first action is the SAME variant as the
        // replacement (a draw replaces a draw, a bury a bury) — otherwise the "instead" is
        // not about this base, so leave it (falls through to Unsupported).
        if let Some((cond, actions)) = gated_instead(clause, source) {
            let same_variant = effects.last().is_some_and(|base| {
                base.actions
                    .first()
                    .zip(actions.first())
                    .is_some_and(|(b, a)| std::mem::discriminant(b) == std::mem::discriminant(a))
            });
            if same_variant {
                let mut base = effects.pop().unwrap();
                let mut instead = eff(base.trigger.clone(), actions, cond.clone(), base.duration);
                instead.raw_clause = clause.clone();
                instead.source = source;
                instead.frequency = FrequencyGuard {
                    node_type: FrequencyGuardTag,
                    kind: freq,
                    n,
                };
                // The base now applies only when the compare does NOT hold. It was already
                // window-scoped when first pushed, so don't re-scope it.
                base.condition = and_conds(
                    Condition::Not {
                        item: Box::new(cond),
                    },
                    base.condition,
                );
                effects.push(base);
                effects.push(scope(instead, &window));
                i += 1;
                continue;
            }
        }
        // Bare "When the Crowd Meter increases:" header (a standalone line): re-parse
        // the following clause as the body under `OnCrowdMeterIncrease`. If it doesn't
        // parse, fall through so the header compiles to Unsupported (never a silent drop).
        if cm_increase_header(clause) {
            if let Some(mut eff) = clauses
                .get(i + 1)
                .and_then(|nxt| trigger_body(Trigger::OnCrowdMeterIncrease, nxt))
            {
                eff.raw_clause = format!("{clause} {}", clauses[i + 1]);
                eff.source = source;
                eff.frequency = FrequencyGuard {
                    node_type: FrequencyGuardTag,
                    kind: freq,
                    n,
                };
                effects.push(scope(eff, &window));
                i += 2;
                continue;
            }
        }
        // "Choose one:" header (a standalone line): compose the following option
        // clauses into a single Choice. If fewer than two options parse, fall through
        // so the header itself compiles to Unsupported (never a silent drop).
        if is_choose_one_header(clause) {
            if let Some((mut eff, consumed)) = choice_from_following(&clauses[i + 1..]) {
                eff.raw_clause = clauses[i..=i + consumed].join(" ");
                eff.source = source;
                eff.frequency = FrequencyGuard {
                    node_type: FrequencyGuardTag,
                    kind: freq,
                    n,
                };
                effects.push(scope(eff, &window));
                i += 1 + consumed;
                continue;
            }
        }
        // "Your <skill> is +N during your turn[ and turn rolls]" — a self-turn-scoped
        // standing buff: one DuringTurn-gated buff effect, plus (for "and turn rolls") a
        // second Always-gated TurnRollBonus effect for the roll-off.
        if let Some(effs) = during_turn_skill_buff(clause, source) {
            for mut e in effs {
                e.raw_clause = clause.clone();
                e.source = source;
                e.frequency = FrequencyGuard {
                    node_type: FrequencyGuardTag,
                    kind: freq,
                    n,
                };
                effects.push(scope(e, &window));
            }
            i += 1;
            continue;
        }
        // "During your opponent's turn, your <skill> is +N" (either order) — a self-side
        // standing buff gated to the OPPONENT's turn (DuringTurn{Opp}, resolved against the
        // buffed side by effective_stats). No roll-off piece (you don't roll on their turn).
        if let Some(effs) = during_opponent_turn_buff(clause) {
            for mut e in effs {
                e.raw_clause = clause.clone();
                e.source = source;
                e.frequency = FrequencyGuard {
                    node_type: FrequencyGuardTag,
                    kind: freq,
                    n,
                };
                effects.push(scope(e, &window));
            }
            i += 1;
            continue;
        }
        // "Your opponent's <skill> is -N during their turn[ and turn rolls]" — an opponent
        // skill DEBUFF: a DuringTurn-gated BuffSkill{Opp}, plus (for "and turn rolls") an
        // Always-gated TurnRollBonus{Opp} for the opponent's roll-off.
        if let Some(effs) = opponent_turn_debuff(clause) {
            for mut e in effs {
                e.raw_clause = clause.clone();
                e.source = source;
                e.frequency = FrequencyGuard {
                    node_type: FrequencyGuardTag,
                    kind: freq,
                    n,
                };
                effects.push(scope(e, &window));
            }
            i += 1;
            continue;
        }
        // "The player with the fewest/most cards in hand draws/discards N" — one
        // clause, TWO conditional effects (per-seat, tie resolves for both).
        if let Some(effs) = hand_extreme_effects(clause) {
            for mut e in effs {
                e.raw_clause = clause.clone();
                e.source = source;
                e.frequency = FrequencyGuard {
                    node_type: FrequencyGuardTag,
                    kind: freq,
                    n,
                };
                effects.push(scope(e, &window));
            }
            i += 1;
            continue;
        }
        // Under an active roll-phase window, re-parse the body clause with the OnRoll
        // trigger stamped on (and the multi-skill gate AND-ed onto its condition). Falls
        // through to the plain compile below when the body has no grammar, so an
        // unparseable body stays Unsupported rather than being silently dropped.
        if let Some((trigger, gate)) = &roll_win {
            if let Some(mut eff) = trigger_body(trigger.clone(), clause) {
                if let Some(g) = gate {
                    eff.condition = and_conds(g.clone(), eff.condition);
                }
                eff.raw_clause = clause.clone();
                eff.source = source;
                eff.frequency = FrequencyGuard {
                    node_type: FrequencyGuardTag,
                    kind: freq,
                    n,
                };
                effects.push(scope(eff, &window));
                i += 1;
                continue;
            }
        }
        let mut compiled = compile(clause, source, freq, n);
        // Rescue: a versatile "<offensive> or <stop>" card (each player flips/shuffles/
        // buries OR use it as a Stop) — two independent effects. Tried only once normal
        // grammar has failed, so it never shadows a clause with real grammar.
        if compiled
            .actions
            .iter()
            .any(|a| matches!(a, Action::Unsupported { .. }))
        {
            // "Stop any <A> that cannot be stopped or any <B> that is not the first card
            // played this turn" — two differently-gated stop effects (see the fn).
            if let Some(effs) = stop_first_card_compound(clause) {
                for mut e in effs {
                    e.raw_clause = clause.clone();
                    e.source = source;
                    e.frequency = FrequencyGuard {
                        node_type: FrequencyGuardTag,
                        kind: freq,
                        n,
                    };
                    effects.push(scope(e, &window));
                }
                i += 1;
                continue;
            }
            // "Stop any <target>: that card has blank text until the end of the turn" — the
            // Jurassic "If Stopped" family: a Stop capability + a BlankStoppedText OnStop.
            if let Some(effs) = stop_then_blank_stopped(clause) {
                for mut e in effs {
                    e.raw_clause = clause.clone();
                    e.source = source;
                    e.frequency = FrequencyGuard {
                        node_type: FrequencyGuardTag,
                        kind: freq,
                        n,
                    };
                    effects.push(scope(e, &window));
                }
                i += 1;
                continue;
            }
            // "Stop any <target> and end the current turn" — a Stop capability + an EndTurn
            // OnStop (cancels the stopped player's remaining extra-play grants).
            if let Some(effs) = stop_then_end_turn(clause) {
                for mut e in effs {
                    e.raw_clause = clause.clone();
                    e.source = source;
                    e.frequency = FrequencyGuard {
                        node_type: FrequencyGuardTag,
                        kind: freq,
                        n,
                    };
                    effects.push(scope(e, &window));
                }
                i += 1;
                continue;
            }
            if let Some(effs) = versatile_or_stop(clause) {
                for mut e in effs {
                    e.raw_clause = clause.clone();
                    e.source = source;
                    e.frequency = FrequencyGuard {
                        node_type: FrequencyGuardTag,
                        kind: freq,
                        n,
                    };
                    effects.push(scope(e, &window));
                }
                i += 1;
                continue;
            }
            // "Choose a skill: your opponent's skill of that type is -N" — a ChooseSkill
            // binding + a Static target_chosen debuff on the opponent (see the fn).
            if let Some(effs) = choose_skill_opp_debuff(clause) {
                for mut e in effs {
                    e.raw_clause = clause.clone();
                    e.source = source;
                    e.frequency = FrequencyGuard {
                        node_type: FrequencyGuardTag,
                        kind: freq,
                        n,
                    };
                    effects.push(scope(e, &window));
                }
                i += 1;
                continue;
            }
            // Two independent abilities joined by " & " (JT Dunn) — commit only when both
            // halves parse. Tried last so it never shadows a clause with real grammar.
            if let Some(effs) = ampersand_compound(clause, source) {
                for mut e in effs {
                    e.raw_clause = clause.clone();
                    e.source = source;
                    e.frequency = FrequencyGuard {
                        node_type: FrequencyGuardTag,
                        kind: freq,
                        n,
                    };
                    effects.push(scope(e, &window));
                }
                i += 1;
                continue;
            }
        }
        if turn_roll_scope {
            compiled = scope_to_turn_roll(compiled);
        }
        effects.push(scope(compiled, &window));
        i += 1;
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
        // Parse the WHOLE record (not clause-by-clause via `match_grammar`) so parse-
        // loop compositions — the "Choose one:" / reveal-header composers and the
        // hand-size-extreme two-effect split — count as MODELED rather than showing up
        // as unsupported headers. A clause the parser cannot map surfaces as exactly one
        // `Unsupported` effect whose `raw_clause` is that clause verbatim (same string
        // `split_clauses` yields), so the set membership below is a faithful per-clause
        // verdict.
        let effects = parse_text(rec.text, EffectSource::Card, rec.db_uuid, overrides);
        let unsupported_clauses: std::collections::HashSet<&str> = effects
            .iter()
            .filter(|e| {
                e.actions
                    .iter()
                    .any(|a| matches!(a, Action::Unsupported { .. }))
            })
            .map(|e| e.raw_clause.as_str())
            .collect();
        for clause in &clauses {
            total += 1;
            if unsupported_clauses.contains(clause.as_str()) {
                unsupported += 1;
                let shape = normalize_shape(clause);
                shape_counts
                    .entry(shape.clone())
                    .and_modify(|c| *c += 1)
                    .or_insert_with(|| {
                        shape_order.push(shape.clone());
                        1
                    });
            } else {
                grammar += 1;
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
