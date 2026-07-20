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
use crate::ir::{Action, CardFilter, Condition, CountZone, Duration, Effect, Skill, Trigger, Who};
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

/// One live timed skill buff on a player (DESIGN.md §3, `Duration::UntilEndOfTurn` /
/// `UntilStartOfYourNextTurn`).
///
/// Unlike the continuous `Static` buffs — re-derived from the board on every stats
/// read — a timed buff is granted imperatively at the moment its effect fires and
/// persists as state until its sweep. `source` is the granting clause's identity:
/// re-firing the SAME clause accumulates into the existing entry (clamped to `cap`)
/// rather than appending a second one, which is what makes "(Max +5 to each)" a real
/// ceiling across repeat triggers. `granted_turn` lets the
/// `UntilStartOfYourNextTurn` sweep tell "the turn I was granted on" from "the
/// owner's next active turn".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimedBuff {
    pub skill: Skill,
    pub delta: i64,
    pub until: Duration,
    /// Stacking identity — the granting effect's `raw_clause`. Same source + same
    /// skill + same expiry = one accumulating entry.
    pub source: String,
    pub cap: Option<i64>,
    pub granted_turn: i64,
}

/// A queued one-shot "added text" waiting for its target's next matching card
/// (DESIGN.md §3, [`Action::AddTextToNext`] — the Madness trio).
///
/// Held on the TARGET player, not the source card, which is what makes it survive the
/// source leaving the board (srgpc: poison "stays active until fulfilled even if
/// removed from the board"). Consumed by `resolve_play` when a matching card is
/// played, whether or not that card is then stopped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingText {
    pub selector: CardFilter,
    pub effects: Vec<Effect>,
    /// The granting clause, for the log.
    pub source: String,
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
    /// Live TIMED skill buffs granted to THIS player (stored on the target, not the
    /// granter) by a `BuffSkill` under `UntilEndOfTurn` / `UntilStartOfYourNextTurn`.
    /// Folded into the derived stats alongside the continuous `Static` buffs and swept
    /// at the matching turn boundary. See [`TimedBuff`].
    #[serde(default)]
    pub timed_buffs: Vec<TimedBuff>,
    /// The option bound by [`Action::ChooseName`] ("Choose 1: 'Kendo Stick', 'Steel
    /// Chair', or 'Trash Can'" — Raven), fixed for the rest of the match and read by
    /// [`Condition::ChosenNameIs`]. `None` until the choice is made.
    #[serde(default)]
    pub chosen_name: Option<String>,
    /// Queued one-shot "added text" for this player's next matching card. See
    /// [`PendingText`]; survives the source card leaving play.
    #[serde(default)]
    pub pending_text: Vec<PendingText>,
    /// Set when THIS player's gimmick was blanked "until their next turn" (Stiff Right
    /// Hand) — the turn the blank was granted on. Swept, with `gimmick_blanked`, at the
    /// start of this player's next ACTIVE turn (`sweep_next_turn_buffs`). Stored state,
    /// so like every poison it outlives the source card leaving the board. A timed
    /// blank and a stored permanent blank do not compose: last writer wins.
    #[serde(default)]
    pub blank_until_next_turn: Option<i64>,
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
    /// `db_uuid`s whose text is blanked for the REST OF THIS TURN by a
    /// [`Action::BlankStoppedText`] ("the stopped card has blank text until the end of
    /// the turn"). Card-identity scoped, not selector scoped, and cleared by the
    /// turn-boundary sweep — the blanking stop card stays in play, so a continuous
    /// blank would never end. Consulted by [`GameState::is_text_blanked`].
    #[serde(default)]
    pub blanked_text: std::collections::BTreeSet<String>,
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
            blanked_text: Default::default(),
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
    /// that targets `key` *whose condition holds* (DESIGN.md §3/§5). Derived like a
    /// Static buff, so a `WHILE_IN_PLAY` blank clears the moment its source leaves
    /// play or its condition stops holding. A gimmick MAY blank the opponent's
    /// gimmick (GM Calace V2, Mr. Snap V1): the owner's own Static competitor
    /// effects are scanned too, but only while that owner's gimmick is itself
    /// active. The re-entrancy guard defends the resulting blank<->blank loop and
    /// the pathological case of a blank gated on a stat comparison (whose
    /// evaluation reads `effective_stats` -> buff sources -> here again).
    /// `(effects, active)` for each of `owner`'s Static-declaration sources: the
    /// competitor gimmick (inert while blanked), the entrance, then every card in
    /// play. The single walk behind every PASSIVE RULE DECLARATION — the suppression
    /// flags, `ConsideredCompare`, and the disqualification rules — so a blanked
    /// gimmick silences all of them alike. Hand-adjudicated 2026-07-20.
    pub fn declaration_sources(&self, owner: &str) -> Vec<(&[Effect], bool)> {
        let player = &self.players[owner];
        let mut out = vec![
            (
                player.competitor.effects.as_slice(),
                !self.is_gimmick_blanked(owner),
            ),
            (player.entrance.effects.as_slice(), true),
        ];
        out.extend(player.in_play.iter().map(|c| (c.effects.as_slice(), true)));
        out
    }

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
            // A gimmick-sourced continuous blank ("while you have 5 X in play, your
            // opponent's Gimmick is blank" — GM Calace V2, Mr. Snap V1) fires only
            // while the owner's OWN gimmick is active; entrance/in-play blanks always
            // apply. The blank<->blank recursion is bounded by `blank_guard` (a
            // re-entrant is_gimmick_blanked returns false). Only a `Static` blank is
            // continuous here — a *triggered* BlankGimmick (OnRoll/OnHit) latches the
            // flag via the executor instead, so it must not be re-read as continuous.
            let gimmick: &[Effect] = if self.is_gimmick_blanked(owner) {
                &[]
            } else {
                &player.competitor.effects
            };
            let sources = std::iter::once(gimmick)
                .chain(std::iter::once(player.entrance.effects.as_slice()))
                .chain(player.in_play.iter().map(|c| c.effects.as_slice()));
            for effects in sources {
                for eff in effects {
                    if !matches!(eff.trigger, Trigger::Static) {
                        continue;
                    }
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
        // A card blanked by a stop this turn stays blanked regardless of zone.
        if self.blanked_text.contains(&card.db_uuid) {
            return true;
        }
        for (decl_owner, player) in &self.players {
            // (effects, is_discard) per source zone. A `WhileInDiscard` effect is active
            // only from the discard pile ("when this card is in your discard pile, …");
            // every other duration is active only while the source is in play/entrance.
            let live = std::iter::once(&player.entrance.effects)
                .chain(player.in_play.iter().map(|c| &c.effects))
                .map(|e| (e, false));
            let dead = player.discard.iter().map(|c| (&c.effects, true));
            for (effects, is_discard) in live.chain(dead) {
                for eff in effects {
                    let in_discard_scoped = eff.duration == Duration::WhileInDiscard;
                    if in_discard_scoped != is_discard {
                        continue; // effect not active from this zone
                    }
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
        // TIMED buffs are already resolved (condition checked, delta accumulated and
        // capped at grant time), so they fold in unconditionally. Folding here — the
        // one derived-stats chokepoint — is what makes them apply to turn rolls,
        // Finish rolls and breakout rolls alike (DESIGN.md §3/§5); a stop that becomes
        // a Finish can roll on the opponent's turn, while the buff is still live.
        for buff in &self.players[key].timed_buffs {
            stats = stats.with(buff.skill, stats.get(buff.skill) + buff.delta);
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
    pub fn count_in_zone(&self, filter: &CardFilter, zone: CountZone, target: &str) -> i64 {
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
