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

use crate::cards::{Card, Competitor, Deck, EntranceCard};
use crate::ir::{
    Action, AtkType, BuryFrom, CardFilter, Comparator, Condition, CountZone, DeckEnd, Direction,
    Duration, Effect, EffectSource, EffectTag, Frequency, FrequencyGuard, FrequencyGuardTag,
    LoseKind, PlayOrder, RollWhen, Skill, Trigger, Vs, Who,
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

fn max_hand(delta: i64, who: Who) -> Action {
    Action::MaxHandSize {
        delta,
        who,
        duration: Duration::WhileInPlay,
    }
}

fn has_in_play(who: Who, filter: CardFilter, count: i64) -> Condition {
    Condition::HasInPlay {
        who,
        filter,
        count,
        cmp: Comparator::Ge,
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

/// Parse a "stop any …" target into `Stop` actions, or `None` if any part is not
/// a plain `<type>` / `<order> <type>` (handles the "X or Y" two-target form).
fn stop_targets(text: &str) -> Option<Vec<Action>> {
    static OR_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+or\s+").unwrap());
    let mut stops = Vec::new();
    for part in OR_RE.split(text.trim()) {
        let m = STOP_PART_RE.captures(part.trim())?;
        stops.push(Action::Stop {
            order: m.get(1).map(|g| order(g.as_str())),
            atk_type: Some(atk(&m[2])),
            source_is_skillreq: false,
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

fn per_draw(n: i64, desc: &str) -> Option<Effect> {
    let per = count_filter(desc)?;
    Some(eff(
        Trigger::OnPlay,
        vec![draw(
            n,
            Who::SelfSide,
            DeckEnd::Top,
            Some(per),
            Who::SelfSide,
        )],
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
    vec![Action::FinishRollBonus {
        delta,
        when_skill: None,
        either: false,
        per: None,
        per_who: Who::SelfSide,
        per_zone: CountZone::InPlay,
    }]
}

#[allow(clippy::too_many_lines)]
fn build_rules() -> Vec<(Regex, Builder)> {
    vec![
        rule(r"\+(\d+) to (?:your )?Finish rolls?", |c| {
            Some(eff(
                Trigger::Static,
                finish_roll_bonus(num(c, 1)),
                Condition::Always,
                Duration::WhileInPlay,
            ))
        }),
        rule(r"Your Finish rolls? (?:is|are) \+(\d+)", |c| {
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
        rule(&format!(r"Your {SK} is \+(\d+)"), |c| {
            Some(eff(
                Trigger::Static,
                vec![buff(skill(&c[1]), num(c, 2), Who::SelfSide)],
                Condition::Always,
                Duration::WhileInPlay,
            ))
        }),
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
        rule(
            r"If stopped, you lose the match via disqualification",
            |_| {
                Some(eff(
                    Trigger::OnStop {
                        dir: Direction::Yours,
                        order: None,
                    },
                    vec![Action::LoseBy {
                        kind: LoseKind::Disqualification,
                        who: Who::SelfSide,
                    }],
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
        rule(r"Flip (\d+) cards?", |c| {
            Some(eff(
                on_hit(),
                vec![Action::Flip {
                    n: num(c, 1),
                    who: Who::SelfSide,
                }],
                Condition::Always,
                Duration::Instant,
            ))
        }),
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
        rule(
            r"Add (\d+) cards? from your discard pile to your hand",
            |_| {
                Some(eff(
                    on_hit(),
                    vec![Action::AddFromDiscard {
                        filter: CardFilter::default(),
                    }],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        rule(
            &format!(r"Add (\d+) {ATK} from your discard pile to your hand"),
            |c| {
                Some(eff(
                    on_hit(),
                    vec![Action::AddFromDiscard {
                        filter: cf_atk(atk(&c[2])),
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
                    }],
                    Condition::Always,
                    Duration::Instant,
                ))
            },
        ),
        rule(
            r"Put (?:up to )?(\d+) cards? from your discard pile on top of your deck",
            |c| {
                Some(eff(
                    on_hit(),
                    vec![Action::RecurToDeckTop {
                        selector: CardFilter::default(),
                        count: num(c, 1),
                    }],
                    Condition::Always,
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
        rule(r"Cannot be stopped by Follow ?Ups?", |_| {
            Some(eff(
                Trigger::Static,
                vec![Action::Unstoppable {
                    by_order: Some(PlayOrder::Followup),
                }],
                Condition::Always,
                Duration::WhileInPlay,
            ))
        }),
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
            |c| per_draw(num(c, 1), &c[2]),
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
                    vec![Action::FinishRollBonus {
                        delta: num(c, 2),
                        when_skill: Some(skill(&c[1])),
                        either: true,
                        per: None,
                        per_who: Who::SelfSide,
                        per_zone: CountZone::InPlay,
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
