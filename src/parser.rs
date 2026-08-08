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
    Frequency, FrequencyGuard, FrequencyGuardTag, LoseKind, MatchType, PlayOrder, RerollCost,
    RerollCostKind, RerollCostTag, RevealSource, RollWhen, ScryRest, ShuffleSource, Skill, Trigger,
    Vs, Who,
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

/// "When you hit a `<atk_type>`" — an [`Trigger::OnHit`] gated on the hit card's type.
fn on_hit_type(atk_type: AtkType) -> Trigger {
    Trigger::OnHit {
        atk_type: Some(atk_type),
        order: None,
        name_contains: Vec::new(),
        text_contains: Vec::new(),
        on_any: false,
        who: Who::SelfSide,
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

/// Uppercase the first character (body clauses are lowercase mid-sentence, but the
/// grammar's rules expect sentence case).
fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

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
    } else if ["When ", "After ", "If ", "Each ", "At "]
        .iter()
        .any(|p| r.starts_with(p))
    {
        r.to_owned()
    } else {
        return None; // passive body -> deferred to the family-A slice
    };
    let mut effect = match_grammar(&inner)
        .or_else(|| compound_body(&inner))
        .or_else(|| choice_body(&inner))?;
    // Fidelity gate: only WhileInDiscard triggers whose dispatch site fires from the
    // discard pile (with the self_card referent bound) may be emitted; the rest decline
    // and stay Unsupported rather than become silently-inert IR. Wired so far (task #115):
    // OnRoll (slice 1, run_on_roll), OnHit (slice 2a, run_hit_gimmicks_inner), OnStop
    // (slice 2b, run_on_stop_gimmicks), OnBreakout (slice 2b, on_broken_out). Passive
    // bodies and OnBreakoutRoll/OnFlip remain gated out until their readers land.
    if !matches!(
        effect.trigger,
        Trigger::OnRoll { .. }
            | Trigger::OnHit { .. }
            | Trigger::OnStop { .. }
            | Trigger::OnBreakout { .. }
            | Trigger::OnReroll { .. }
    ) {
        return None;
    }
    effect.duration = Duration::WhileInDiscard;
    Some(effect)
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
        Regex::new(&format!(
            r"(?i)^your opponent rolled {SK} for their turn roll$"
        ))
        .unwrap()
    });
    static ROLL_VAL: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^you rolled (\d+) for your turn roll$").unwrap());
    static HAVE_NAME: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r#"(?i)^you have (?:a card )?(?:in play )?with "([^"]+)" in the name(?: in play)?$"#,
        )
        .unwrap()
    });
    static HAVE_INPLAY: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^you have (?:another |(\d+) (?:or more )?)?(.+?) in play$").unwrap()
    });
    static HIT: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^you hit (?:a |an |another )?(.+?) (this|last) turn$").unwrap()
    });
    static OPP_PLAY: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^your opponent has (\d+)(?: or more)? (.+?) in play$").unwrap()
    });
    static OPP_PLAY_NONE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^your opponent has (?:no|0) (.+?) in play$").unwrap());
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
        if let (Some(f), Ok(n)) = (recur_filter(c[2].trim()), c[1].parse::<i64>()) {
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
    if let Some(c) = ROLL_VAL.captures(t) {
        return Some(Condition::RollValue {
            cmp: Comparator::Eq,
            value: c[1].parse().ok()?,
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
            return Some(Condition::EndedTurnNoPlay)
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
    }
}

fn search(filter: CardFilter, dest: Dest, count: i64) -> Action {
    Action::Search {
        filter,
        dest,
        count,
    }
}

/// Parse a `Search` selector — "a Finish", "2 cards", "up to 3 cards", "1 card with
/// \"Ladder\" in the name" — into `(filter, count)`, reusing [`recur_filter`] for the
/// typed/named descriptor. `None` for a selector with no CardFilter (Spotlight, Skill
/// Requirement, …), so those clauses stay Unsupported rather than mis-modeling.
fn search_target(sel: &str) -> Option<(CardFilter, i64)> {
    static LEAD: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^(?:up to )?(a|an|\d+) (.+)$").unwrap());
    let caps = LEAD.captures(sel.trim())?;
    let head = caps[1].to_lowercase();
    let count = if head == "a" || head == "an" {
        1
    } else {
        head.parse().ok()?
    };
    Some((recur_filter(&caps[2])?, count))
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
        all: false,
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
        all: false,
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
        all: false,
    }
}

fn buff(skill: Skill, delta: i64, who: Who) -> Action {
    Action::BuffSkill {
        skill,
        delta,
        who,
        duration: Duration::WhileInPlay,
        target_highest: false,
        target_lowest: false,
        per_crowd: false,
        cap: None,
        per: None,
        per_zone: CountZone::InPlay,
    }
}

/// One standing [`Action::TurnRollBonus`] per skill — "Your Power and Strike are +N
/// during turn rolls" fans out to a bonus on each named skill.
fn turn_roll_bonuses(skills: Vec<Skill>, delta: i64) -> Vec<Action> {
    skills
        .into_iter()
        .map(|skill| Action::TurnRollBonus { skill, delta })
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
        per_crowd: false,
        cap: None,
        per: None,
        per_zone: CountZone::InPlay,
    }
}

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
        target_lowest: false,
        per_crowd: false,
        cap,
        per,
        per_zone: CountZone::InPlay,
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
) -> Option<Effect> {
    let skills = skill_list(skills_text);
    let per = count_filter(per_text)?;
    if skills.is_empty() {
        return None;
    }
    let cap = cap.map(|m| m.as_str().parse::<i64>().unwrap());
    let actions = skills
        .into_iter()
        .map(|s| buff_per(s, delta, Some(per.clone()), cap))
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
            per_crowd: true,
            cap,
            per: None,
            per_zone: CountZone::InPlay,
        })
        .collect();
    Some(eff(
        Trigger::Static,
        actions,
        Condition::Always,
        Duration::WhileInPlay,
    ))
}

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

fn max_hand(delta: i64, who: Who) -> Action {
    Action::MaxHandSize {
        delta,
        who,
        duration: Duration::WhileInPlay,
    }
}

fn min_hand(delta: i64, who: Who) -> Action {
    Action::MinHandSize {
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

/// The stopper's side: "when you stop a card" — the effect fires for the player who
/// PLAYED the stop (Direction::Theirs, "they stopped a card"), the mirror of
/// [`on_your_stop`].
fn on_their_stop() -> Trigger {
    Trigger::OnStop {
        dir: Direction::Theirs,
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

/// A count that may be a digit or the words "a"/"an"/"one" (all -> 1). Used by the
/// reveal-and-discard family, where "reveals a card" and "reveals 1 card" are the same.
fn count_or_word(s: &str) -> i64 {
    match s.to_ascii_lowercase().as_str() {
        "a" | "an" | "one" => 1,
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
    }
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
    }
}

/// A rolled-skill-gated SELF breakout-roll bonus ("+1 to Strike during your breakout
/// rolls", Pineapple). `when_skill` = None applies to every breakout roll. schema v79
fn breakout_mod(delta: i64, when_skill: Option<Skill>) -> Action {
    breakout_mod_who(delta, Who::SelfSide, None, when_skill)
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
                        target_lowest: false,
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
        // Type-counted per-count buff (task #131): "Your X [and Y] is/are +N for each
        // <type> you have in play [(Max +M)]" — the same buff_per scaling as the
        // name-count rules above, but the count ranges over a card TYPE (atk / play
        // order / stop) via count_filter instead of a name substring. Own board only
        // (BuffSkill.per counts the buffed player's board; opponent-board and "for each
        // OTHER" forms need per_who/exclude and stay Unsupported). "for each card …"
        // declines here (count_filter has no bare-card filter) — the name rules own it.
        rule(
            r"Your (.+?) (?:is|are) \+(\d+) for each (.+?) you have in play(?: \(Max \+?(\d+)\))?",
            |c| type_count_buff(&c[1], num(c, 2), &c[3], c.get(4)),
        ),
        // Same, phrased "+N to X [and Y] for each <type> you have in play [(Max +M)]".
        rule(
            r"\+(\d+) to (.+?) for each (.+?) you have in play(?: \(Max \+?(\d+)\))?",
            |c| type_count_buff(&c[2], num(c, 1), &c[3], c.get(4)),
        ),
        // Crowd-Meter skill buff (task #131): "Your X [and Y] is/are + the Crowd Meter
        // [(Max +M)]" -> BuffSkill{per_crowd} (Copy Kat's dynamic delta, was override-
        // only). skill_list declines "Finish roll"/"breakout rolls" (own mechanisms) and
        // "+ double/triple the Crowd Meter" fails the literal "+ the" so it stays tail.
        rule(
            r"Your (.+?) (?:is|are) \+ the [Cc]rowd [Mm]eter(?: \((?:Max|max) \+?(\d+)\))?",
            |c| crowd_meter_buff(&c[1], c.get(2)),
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
        // Deck tutor (Search, previously override-only): "Search your deck for <SEL>
        // and <route>". Three destinations — hand, top of the shuffled deck, discard
        // pile. Compound tails ("… , or each player buries", "…: add 1 …", "search your
        // deck OR discard pile") decline here and stay Unsupported for now.
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
                    },
                    Action::Reveal {
                        who: Who::Opp,
                        count: num(c, 1),
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
        // MinHandSize mirror (previously override-only). Same three shapes / Static
        // WhileInPlay convention as the maximum. The per-count "… +N for each Lead you
        // have in play" form has no MinHandSize.per and stays Unsupported.
        rule(r"Each player's minimum hand ?size is ([+-]\d+)", |c| {
            let d = num(c, 1);
            Some(eff(
                Trigger::Static,
                vec![min_hand(d, Who::SelfSide), min_hand(d, Who::Opp)],
                Condition::Always,
                Duration::WhileInPlay,
            ))
        }),
        rule(
            r"(?:Your opponent's|Your target's|Their) minimum hand ?size is ([+-]\d+)",
            |c| {
                Some(eff(
                    Trigger::Static,
                    vec![min_hand(num(c, 1), Who::Opp)],
                    Condition::Always,
                    Duration::WhileInPlay,
                ))
            },
        ),
        rule(r"Your minimum hand ?size is ([+-]\d+)", |c| {
            Some(eff(
                Trigger::Static,
                vec![min_hand(num(c, 1), Who::SelfSide)],
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
        // "[If/When <cond>,] this card cannot be stopped [by <order>]": an optionally
        // condition-gated Unstoppable. The guard is parsed by `gate_condition` (the
        // superset gate parser — Crowd Meter, skill/hand compare, opp-roll, in-play,
        // hit-history, …); a bare clause (no gate) is unconditional. The engine evaluates
        // the guard from the card owner's side at stop time.
        rule(
            r"(?:(?:If|When) (.+?),? )?[Tt]his card cannot be stopped(?: by (Follow[ -]?Ups?|Leads?|Finish(?:es)?))?",
            |c| {
                let by_order = c.get(2).map(|m| stopper_order(m.as_str()));
                let condition = match c.get(1) {
                    Some(m) => gate_condition(m.as_str())?,
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
                    target_lowest: false,
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
                        }],
                        Condition::Always,
                        Duration::Instant,
                    )
                })
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
        rule(r"[Ii]f your opponent breaks out[,:] (.+)", |c| {
            trigger_body(
                Trigger::OnBreakout {
                    who: Some(Who::Opp),
                },
                &c[1],
            )
        }),
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
            |c| trigger_body(Trigger::OnBreakoutRoll { who: Who::Opp }, &c[1]),
        ),
        rule(r"At the start of the match[,:] (.+)", |c| {
            trigger_body(Trigger::StartOfMatch, &c[1])
        }),
        rule(r"At the start of your turn[,:] (.+)", |c| {
            trigger_body(Trigger::StartOfTurn, &c[1])
        }),
        rule(r"When you win (?:the|a) turn roll[,:;] (.+)", |c| {
            trigger_body(Trigger::OnWinTurn, &c[1])
        }),
        rule(r"[Ii]f (?:this card is |this is )?stopped[,:] (.+)", |c| {
            trigger_body(on_your_stop(), &c[1])
        }),
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
    static INLINE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?i)^(Once (?:per|a) match|Once (?:per|a) turn|(\d+) times per match)[:,]\s+(.+)$",
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

/// A window header ("During your turn:", "During your opponent's turn:") scoping the
/// clauses that follow to a turn phase. Returns the [`Condition::DuringTurn`] it opens,
/// which persists (like a [`freq_header`]) until another header replaces it — the whole
/// text after the header hangs off that turn window. `None` for any non-header clause.
fn window_header(clause: &str) -> Option<Condition> {
    static WINDOW: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^During your (turn|(?:target's|opponent's) turn):?$").unwrap()
    });
    let who = if WINDOW.captures(clause.trim())?[1].eq_ignore_ascii_case("turn") {
        Who::SelfSide
    } else {
        Who::Opp
    };
    Some(Condition::DuringTurn { who })
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
    let clauses = split_clauses(text);
    let mut effects = Vec::new();
    let mut freq = Frequency::Unlimited;
    let mut n = None;
    let mut window = Condition::Always;
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
        if let Some(cond) = window_header(clause) {
            window = cond;
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
        effects.push(scope(compile(clause, source, freq, n), &window));
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
