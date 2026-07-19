//! GameState / PlayerState with serializable snapshots (DESIGN.md §5).
//!
//! A faithful port of `state.py`. The state is **mutable** (the engine advances
//! it in place) but fully snapshottable: [`GameState`] serde-serializes every
//! zone plus the RNG's internal state, so any position is reproducible and
//! diffable. The event log is a separate JSONL stream and is intentionally *not*
//! part of a snapshot.
//!
//! **Derived stats (DESIGN.md §5).** There is no stored `static_buffs`. A
//! player's effective skills are *computed on demand* from base competitor stats
//! plus every active `Static` `BuffSkill`: those on cards in `in_play`, on the
//! entrance (present all match), and on the competitor gimmick *unless* it is
//! blanked. This one view feeds turn rolls, stop checks, and breakout rolls, so
//! a card leaving play or a gimmick being blanked simply drops out of the
//! recomputation.

use crate::cards::{Card, Competitor, EntranceCard};
use crate::conditions;
use crate::ir::{Action, CardFilter, Condition, CountZone, Skill, Trigger, Who};
use crate::rng::SeededRNG;
use crate::skills::Skills;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet};

/// A condition evaluator the engine can supply so conditional `Static` buffs
/// resolve against live state; without one, only unconditional (`Always`) buffs
/// apply.
pub type ConditionHolds<'a> = dyn Fn(&Condition) -> bool + 'a;

/// `{this, next}` turn-roll deltas held on a [`PlayerState`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollMods {
    #[serde(rename = "this")]
    pub this_turn: i64,
    #[serde(rename = "next")]
    pub next_turn: i64,
}

/// One side's competitor, entrance, and card zones (DESIGN.md §5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayerState {
    pub competitor: Competitor,
    pub entrance: EntranceCard,
    #[serde(default)]
    pub hand: Vec<Card>,
    #[serde(default)]
    pub deck: Vec<Card>,
    #[serde(default)]
    pub discard: Vec<Card>,
    #[serde(default)]
    pub in_play: Vec<Card>,
    #[serde(default)]
    pub pending_roll_mods: RollMods,
    /// One-shot "re-roll your NEXT turn roll" grants (King Brian Cage). `next` is
    /// set when the granting effect fires; promoted to `this` at the owner's next
    /// turn start; an unused grant expires (never accumulates).
    #[serde(default)]
    pub reroll_grants: RollMods,
    #[serde(default)]
    pub freq_counters: BTreeMap<String, i64>,
    #[serde(default)]
    pub gimmick_blanked: bool,
    #[serde(default)]
    pub gimmick_flipped: bool,
    #[serde(default)]
    pub flags: Map<String, Value>,
}

impl PlayerState {
    /// Move up to `n` cards from the top of `deck` to `hand`; return them.
    pub fn draw(&mut self, n: usize) -> Vec<Card> {
        let take = n.min(self.deck.len());
        let drawn: Vec<Card> = self.deck.drain(..take).collect();
        self.hand.extend(drawn.iter().cloned());
        drawn
    }
}

/// Both players plus the shared match state (DESIGN.md §5).
///
/// `active` is the player key whose turn it is; `rng` is the single seeded
/// generator. The event log is *not* part of a snapshot and is added by the
/// engine, not stored here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameState {
    pub players: BTreeMap<String, PlayerState>,
    pub rng: SeededRNG,
    #[serde(default)]
    pub crowd_meter: i64,
    #[serde(default = "default_active")]
    pub active: String,
    #[serde(default)]
    pub turn_no: i64,
    /// The previous turn's roll-off winner (`None` before turn 1), for a re-roll
    /// gimmick gated on "your opponent won the last turn roll" (Dunn).
    #[serde(default)]
    pub last_roll_winner: Option<String>,
    /// Re-entrancy guard for stat-gated gimmick blanks (DESIGN.md §5). Transient
    /// engine bookkeeping — never serialized.
    #[serde(skip)]
    blank_guard: RefCell<HashSet<String>>,
}

fn default_active() -> String {
    "A".to_owned()
}

impl GameState {
    /// A fresh game state over the given players and RNG (crowd meter 0, turn 0,
    /// player `A` active).
    pub fn new(players: BTreeMap<String, PlayerState>, rng: SeededRNG) -> Self {
        Self {
            players,
            rng,
            crowd_meter: 0,
            active: default_active(),
            turn_no: 0,
            last_roll_winner: None,
            blank_guard: RefCell::new(HashSet::new()),
        }
    }

    /// The other player's key (two-player game).
    pub fn opponent_of(&self, key: &str) -> String {
        self.players
            .keys()
            .find(|k| k.as_str() != key)
            .expect("two-player game")
            .clone()
    }

    // --- gimmick blank (derived) -------------------------------------------

    /// Whether `key`'s competitor gimmick is currently suppressed — by the stored
    /// flag (a one-shot / StartOfMatch blank) OR by any active `BlankGimmick`
    /// that targets `key` from an entrance or in-play card *whose condition
    /// holds* (DESIGN.md §3/§5). Derived like a Static buff, so a `WHILE_IN_PLAY`
    /// blank clears the moment its source leaves play or its condition stops
    /// holding. A gimmick never blanks itself, so competitor effects are not
    /// scanned. The re-entrancy guard defends the pathological case of a blank
    /// gated on a stat comparison (whose evaluation reads `effective_stats` ->
    /// buff sources -> here again).
    pub fn is_gimmick_blanked(&self, key: &str) -> bool {
        if self.players[key].gimmick_blanked {
            return true;
        }
        if self.blank_guard.borrow().contains(key) {
            return false; // re-entrant stat-gated blank: fall back to no blank
        }
        self.blank_guard.borrow_mut().insert(key.to_owned());
        let result = self.blank_scan(key);
        self.blank_guard.borrow_mut().remove(key);
        result
    }

    fn blank_scan(&self, key: &str) -> bool {
        for (owner, player) in &self.players {
            let sources = std::iter::once(&player.entrance.effects)
                .chain(player.in_play.iter().map(|c| &c.effects));
            for effects in sources {
                for eff in effects {
                    let targets = eff.actions.iter().any(|a| {
                        if let Action::BlankGimmick { who, .. } = a {
                            self.who_key(owner, *who) == key
                        } else {
                            false
                        }
                    });
                    if targets && conditions::holds(&eff.condition, self, owner, None) {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Whether `card` (owned by `owner`) has its printed text blanked — some player
    /// has an active Static `BlankText` declaration (on an in-play or entrance card)
    /// whose `who` targets `owner` and whose `selector` matches the card. "Your
    /// opponent's Spotlights are blank" (the source stays in play). A blanked card
    /// fires none of its own effects and cannot stop.
    pub fn is_text_blanked(&self, card: &Card, owner: &str) -> bool {
        for (decl_owner, player) in &self.players {
            let sources = std::iter::once(&player.entrance.effects)
                .chain(player.in_play.iter().map(|c| &c.effects));
            for effects in sources {
                for eff in effects {
                    let hit = eff.actions.iter().any(|a| {
                        matches!(a, Action::BlankText { selector, who }
                            if self.who_key(decl_owner, *who) == owner
                                && conditions::card_matches(card, selector))
                    });
                    if hit && conditions::holds(&eff.condition, self, decl_owner, None) {
                        return true;
                    }
                }
            }
        }
        false
    }

    fn who_key(&self, owner: &str, who: Who) -> String {
        match who {
            Who::SelfSide => owner.to_owned(),
            Who::Opp => self.opponent_of(owner),
        }
    }

    // --- derived stats ------------------------------------------------------

    /// Derived skills for `key` (base + active Static buffs). `holds` optionally
    /// resolves conditional Static buffs against live state; without it, only
    /// unconditional buffs contribute (DESIGN.md §5).
    pub fn effective_stats(&self, key: &str, holds: Option<&ConditionHolds>) -> Skills {
        let mut stats = self.players[key].competitor.stats;
        for (owner, player) in &self.players {
            self.apply_owner_buffs(&mut stats, key, owner, player, holds);
        }
        stats
    }

    /// The single derived value for `skill`.
    pub fn effective_stat(&self, key: &str, skill: Skill, holds: Option<&ConditionHolds>) -> i64 {
        self.effective_stats(key, holds).get(skill)
    }

    fn apply_owner_buffs(
        &self,
        stats: &mut Skills,
        target: &str,
        owner: &str,
        player: &PlayerState,
        holds: Option<&ConditionHolds>,
    ) {
        let gimmick_active = !self.is_gimmick_blanked(owner);
        self.fold_buffs(
            stats,
            &player.competitor.effects,
            gimmick_active,
            target,
            owner,
            holds,
        );
        self.fold_buffs(stats, &player.entrance.effects, true, target, owner, holds);
        for card in &player.in_play {
            self.fold_buffs(stats, &card.effects, true, target, owner, holds);
        }
    }

    fn fold_buffs(
        &self,
        stats: &mut Skills,
        effects: &[crate::ir::Effect],
        active: bool,
        target: &str,
        owner: &str,
        holds: Option<&ConditionHolds>,
    ) {
        if !active {
            return;
        }
        for eff in effects {
            if !matches!(eff.trigger, Trigger::Static) {
                continue;
            }
            for action in &eff.actions {
                if let Action::BuffSkill {
                    skill,
                    delta,
                    who,
                    target_highest,
                    per_crowd,
                    cap,
                    per,
                    per_zone,
                    ..
                } = action
                {
                    if targets(owner, *who, target) && condition_ok(&eff.condition, holds) {
                        let (sk, d) = self.resolve_buff(
                            *skill,
                            *target_highest,
                            *per_crowd,
                            *cap,
                            *delta,
                            per.as_ref(),
                            *per_zone,
                            target,
                        );
                        *stats = stats.with(sk, stats.get(sk) + d);
                    }
                }
            }
        }
    }

    /// The `(skill, delta)` a buff contributes, expanding Copy Kat's dynamic
    /// variants: `target_highest` retargets to the target's highest base skill
    /// (ties broken by canonical skill order), `per_crowd` uses the Crowd Meter
    /// as the delta, clamped to `cap` when set.
    #[allow(clippy::too_many_arguments)]
    fn resolve_buff(
        &self,
        skill: Skill,
        target_highest: bool,
        per_crowd: bool,
        cap: Option<i64>,
        delta: i64,
        per: Option<&CardFilter>,
        per_zone: CountZone,
        target: &str,
    ) -> (Skill, i64) {
        let sk = if target_highest {
            let base = self.players[target].competitor.stats;
            let mut best = Skill::ALL[0];
            for &s in &Skill::ALL[1..] {
                if base.get(s) > base.get(best) {
                    best = s; // strictly greater keeps the first max (ties -> earlier)
                }
            }
            best
        } else {
            skill
        };
        let d = if per_crowd {
            cap.map_or(self.crowd_meter, |c| self.crowd_meter.min(c))
        } else if let Some(filter) = per {
            // "+delta for each card in `per_zone` matching `filter`", clamped to cap.
            let n = self.count_in_zone(filter, per_zone, target);
            let raw = n * delta;
            cap.map_or(raw, |c| raw.min(c))
        } else {
            delta
        };
        (sk, d)
    }

    /// Count the target's cards in `zone` matching `filter` (Static per-count buffs).
    fn count_in_zone(&self, filter: &CardFilter, zone: CountZone, target: &str) -> i64 {
        let player = &self.players[target];
        match zone {
            CountZone::InPlay => conditions::count_in_play(&player.in_play, filter, None),
            CountZone::Discard => player
                .discard
                .iter()
                .filter(|c| conditions::card_matches(c, filter))
                .count() as i64,
        }
    }

    /// Derived maximum hand size for `key` (`base` + active Static hand mods),
    /// folding every `MaxHandSize` the way [`effective_stats`](Self::effective_stats)
    /// folds Static buffs. Clamped at zero.
    pub fn effective_hand_cap(&self, key: &str, base: i64, holds: Option<&ConditionHolds>) -> i64 {
        let mut cap = base;
        for (owner, player) in &self.players {
            cap += self.owner_hand_mods(key, owner, player, holds);
        }
        cap.max(0)
    }

    fn owner_hand_mods(
        &self,
        target: &str,
        owner: &str,
        player: &PlayerState,
        holds: Option<&ConditionHolds>,
    ) -> i64 {
        let gimmick_active = !self.is_gimmick_blanked(owner);
        let mut total = 0;
        total += self.fold_hand_mods(
            &player.competitor.effects,
            gimmick_active,
            target,
            owner,
            holds,
        );
        total += self.fold_hand_mods(&player.entrance.effects, true, target, owner, holds);
        for card in &player.in_play {
            total += self.fold_hand_mods(&card.effects, true, target, owner, holds);
        }
        total
    }

    fn fold_hand_mods(
        &self,
        effects: &[crate::ir::Effect],
        active: bool,
        target: &str,
        owner: &str,
        holds: Option<&ConditionHolds>,
    ) -> i64 {
        if !active {
            return 0;
        }
        let mut total = 0;
        for eff in effects {
            if !matches!(eff.trigger, Trigger::Static) {
                continue;
            }
            for action in &eff.actions {
                if let Action::MaxHandSize { delta, who, .. } = action {
                    if targets(owner, *who, target) && condition_ok(&eff.condition, holds) {
                        total += *delta;
                    }
                }
            }
        }
        total
    }

    // --- information model --------------------------------------------------

    /// What `viewer` may legitimately see (DESIGN.md §7). Public: both
    /// competitors, entrances, `in_play` boards, `discard` piles, and
    /// gimmick-blank status. Private: a player sees only the *size* of the
    /// opponent's hand, and every deck is a size only (order hidden from
    /// everyone). The viewer's own hand is fully visible; an opponent's hand is
    /// revealed while an active `Peek` grants a look this turn. RNG, `flags`,
    /// `freq_counters`, and `pending_roll_mods` are engine bookkeeping and are
    /// omitted. Unlike [`to_dict`](Self::to_dict) this is a lossy projection.
    pub fn observable(&self, viewer: &str) -> Value {
        let players: Map<String, Value> = self
            .players
            .keys()
            .map(|k| (k.clone(), self.observe_player(k, viewer)))
            .collect();
        json!({
            "viewer": viewer,
            "crowd_meter": self.crowd_meter,
            "active": self.active,
            "turn_no": self.turn_no,
            "players": players,
        })
    }

    fn observe_player(&self, key: &str, viewer: &str) -> Value {
        let player = &self.players[key];
        let mut view = json!({
            "competitor": serde_json::to_value(&player.competitor).expect("competitor"),
            "entrance": serde_json::to_value(&player.entrance).expect("entrance"),
            "in_play": serde_json::to_value(&player.in_play).expect("in_play"),
            "discard": serde_json::to_value(&player.discard).expect("discard"),
            "gimmick_blanked": self.is_gimmick_blanked(key),
            "deck_size": player.deck.len(),
        });
        // Own hand always full; an opponent's hand is a count only unless a Peek
        // is revealing it this turn.
        if key == viewer || self.peeked(viewer, key) {
            view["hand"] = serde_json::to_value(&player.hand).expect("hand");
        } else {
            view["hand_size"] = json!(player.hand.len());
        }
        view
    }

    /// Whether `viewer` has an active peek on `key`'s hand: a `Peek` grants a
    /// look for the rest of the peeker's turn, stored in `flags["peek"]` as
    /// `{key: turn_no}`, so it expires automatically once `turn_no` advances.
    fn peeked(&self, viewer: &str, key: &str) -> bool {
        if viewer == key {
            return false;
        }
        match self.players[viewer].flags.get("peek") {
            Some(Value::Object(peek)) => {
                peek.get(key).and_then(Value::as_i64) == Some(self.turn_no)
            }
            _ => false,
        }
    }

    // --- snapshots ----------------------------------------------------------

    /// Snapshot the position (players, crowd meter, turn, RNG). Excludes the log.
    pub fn to_dict(&self) -> Value {
        serde_json::to_value(self).expect("state serializes")
    }

    /// Rebuild a position from [`to_dict`](Self::to_dict) output.
    pub fn from_dict(value: Value) -> serde_json::Result<Self> {
        serde_json::from_value(value)
    }
}

/// True iff an effect owned by `owner` with `who` lands on `target`
/// (`SELF` = owner, `OPP` = the other player).
fn targets(owner: &str, who: Who, target: &str) -> bool {
    (who == Who::SelfSide) == (owner == target)
}

/// Unconditional buffs always apply; conditional ones need a `holds` evaluator.
fn condition_ok(condition: &Condition, holds: Option<&ConditionHolds>) -> bool {
    if matches!(condition, Condition::Always) {
        return true;
    }
    holds.is_some_and(|h| h(condition))
}
